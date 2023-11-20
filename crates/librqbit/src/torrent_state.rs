// The main logic of rqbit is here - connecting to peers, reading and writing messages
// to them, tracking peer state etc.
//
// ## Architecture
// There are many tasks cooperating to download the torrent. Tasks communicate both with message passing
// and shared memory.
//
// ### Shared locked state
// Shared state is access by almost all actors through RwLocks.
//
// There's one source of truth (TorrentStateLocked) for which chunks we have, need, and what peers are we waiting them from.
//
// Peer states that are important to the outsiders (tasks other than manage_peer) are in a sharded hash-map (DashMap)
//
// ### Tasks (actors)
// Peer adder task:
// - spawns new peers as they become known. It pulls them from a queue. The queue is filled in by DHT and torrent trackers.
//   Also gets updated when peers are reconnecting after errors.
//
// Each peer has one main task "manage_peer". It's composed of 2 futures running as one task through tokio::select:
// - "manage_peer" - this talks to the peer over network and calls callbacks on PeerHandler. The callbacks are not async,
//   and are supposed to finish quickly (apart from writing to disk, which is accounted for as "spawn_blocking").
// - "peer_chunk_requester" - this continuously sends requests for chunks to the peer.
//   it may steal chunks/pieces from other peers.
//
// ## Peer lifecycle
// State transitions:
// - queued (initial state) -> connected
// - connected -> live
// - ANY STATE -> dead (on error)
// - ANY STATE -> not_needed (when we don't need to talk to the peer anymore)
//
// When the peer dies, it's rescheduled with exponential backoff.
//
// > NOTE: deadlock notice:
// > peers and stateLocked are behind 2 different locks.
// > if you lock them in different order, this may deadlock.
// >
// > so don't lock them both at the same time at all, or at the worst lock them in the
// > same order (peers one first, then the global one).

use std::{
    collections::HashMap,
    fs::File,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, Context};
use backoff::backoff::Backoff;
use buffers::{ByteBuf, ByteString};
use clone_to_owned::CloneToOwned;
use dashmap::DashMap;
use futures::{stream::FuturesUnordered, StreamExt};
use librqbit_core::{
    id20::Id20,
    lengths::{ChunkInfo, Lengths, ValidPieceIndex},
    torrent_metainfo::TorrentMetaV1Info,
};
use parking_lot::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use peer_binary_protocol::{
    extended::handshake::ExtendedHandshake, Handshake, Message, MessageOwned, Piece, Request,
};
use serde::Serialize;
use sha1w::Sha1;
use tokio::{
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        Notify, Semaphore,
    },
    time::timeout,
};
use tracing::{debug, error, info, span, trace, warn, Level};

use crate::{
    chunk_tracker::{ChunkMarkingResult, ChunkTracker},
    file_ops::FileOps,
    peer_connection::{
        PeerConnection, PeerConnectionHandler, PeerConnectionOptions, WriterRequest,
    },
    peer_state::{
        atomic_inc, AggregatePeerStatsAtomic, InflightRequest, LivePeerState, Peer, PeerCounters,
        PeerRx, PeerState, PeerStatsFilter, PeerStatsSnapshot, PeerTx, SendMany,
    },
    spawn_utils::{spawn, BlockingSpawner},
    type_aliases::{PeerHandle, BF},
};

pub struct InflightPiece {
    pub peer: PeerHandle,
    pub started: Instant,
}

#[derive(Default)]
pub struct PeerStates {
    stats: AggregatePeerStatsAtomic,
    states: DashMap<PeerHandle, Peer>,
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct AggregatePeerStats {
    pub queued: usize,
    pub connecting: usize,
    pub live: usize,
    pub seen: usize,
    pub dead: usize,
    pub not_needed: usize,
}

impl<'a> From<&'a AggregatePeerStatsAtomic> for AggregatePeerStats {
    fn from(s: &'a AggregatePeerStatsAtomic) -> Self {
        let ordering = Ordering::Relaxed;
        Self {
            queued: s.queued.load(ordering) as usize,
            connecting: s.connecting.load(ordering) as usize,
            live: s.live.load(ordering) as usize,
            seen: s.seen.load(ordering) as usize,
            dead: s.dead.load(ordering) as usize,
            not_needed: s.not_needed.load(ordering) as usize,
        }
    }
}

impl PeerStates {
    pub fn stats(&self) -> AggregatePeerStats {
        AggregatePeerStats::from(&self.stats)
    }

    pub fn add_if_not_seen(&self, addr: SocketAddr) -> Option<PeerHandle> {
        use dashmap::mapref::entry::Entry;
        match self.states.entry(addr) {
            Entry::Occupied(_) => None,
            Entry::Vacant(vac) => {
                vac.insert(Default::default());
                atomic_inc(&self.stats.queued);
                atomic_inc(&self.stats.seen);
                Some(addr)
            }
        }
    }
    pub fn with_peer<R>(&self, addr: PeerHandle, f: impl FnOnce(&Peer) -> R) -> Option<R> {
        self.states.get(&addr).map(|e| f(e.value()))
    }

    pub fn with_peer_mut<R>(
        &self,
        addr: PeerHandle,
        reason: &'static str,
        f: impl FnOnce(&mut Peer) -> R,
    ) -> Option<R> {
        timeit(reason, || self.states.get_mut(&addr))
            .map(|e| f(TimedExistence::new(e, reason).value_mut()))
    }
    pub fn with_live<R>(&self, addr: PeerHandle, f: impl FnOnce(&LivePeerState) -> R) -> Option<R> {
        self.states
            .get(&addr)
            .and_then(|e| match &e.value().state.get() {
                PeerState::Live(l) => Some(f(l)),
                _ => None,
            })
    }
    pub fn with_live_mut<R>(
        &self,
        addr: PeerHandle,
        reason: &'static str,
        f: impl FnOnce(&mut LivePeerState) -> R,
    ) -> Option<R> {
        self.with_peer_mut(addr, reason, |peer| peer.state.get_live_mut().map(f))
            .flatten()
    }

    pub fn drop_peer(&self, handle: PeerHandle) -> Option<Peer> {
        let p = self.states.remove(&handle).map(|r| r.1)?;
        self.stats.dec(p.state.get());
        Some(p)
    }

    pub fn mark_peer_interested(&self, handle: PeerHandle, is_interested: bool) -> Option<bool> {
        self.with_live_mut(handle, "mark_peer_interested", |live| {
            let prev = live.peer_interested;
            live.peer_interested = is_interested;
            prev
        })
    }
    pub fn update_bitfield_from_vec(&self, handle: PeerHandle, bitfield: Vec<u8>) -> Option<()> {
        self.with_live_mut(handle, "update_bitfield_from_vec", |live| {
            live.bitfield = BF::from_vec(bitfield);
        })
    }
    pub fn mark_peer_connecting(&self, h: PeerHandle) -> anyhow::Result<(PeerRx, PeerTx)> {
        let rx = self
            .with_peer_mut(h, "mark_peer_connecting", |peer| {
                peer.state
                    .queued_to_connecting(&self.stats)
                    .context("invalid peer state")
            })
            .context("peer not found in states")??;
        Ok(rx)
    }

    fn reset_peer_backoff(&self, handle: PeerHandle) {
        self.with_peer_mut(handle, "reset_peer_backoff", |p| {
            p.stats.backoff.reset();
        });
    }

    fn mark_peer_not_needed(&self, handle: PeerHandle) -> Option<PeerState> {
        let prev = self.with_peer_mut(handle, "mark_peer_not_needed", |peer| {
            peer.state.to_not_needed(&self.stats)
        })?;
        Some(prev)
    }
}

pub struct TorrentStateLocked {
    // What chunks we have and need.
    pub chunks: ChunkTracker,

    // At a moment in time, we are expecting a piece from only one peer.
    // inflight_pieces stores this information.
    pub inflight_pieces: HashMap<ValidPieceIndex, InflightPiece>,
}

#[derive(Default, Debug)]
struct AtomicStats {
    have_bytes: AtomicU64,
    downloaded_and_checked_bytes: AtomicU64,
    downloaded_and_checked_pieces: AtomicU64,
    uploaded_bytes: AtomicU64,
    fetched_bytes: AtomicU64,
    total_piece_download_ms: AtomicU64,
}

impl AtomicStats {
    fn average_piece_download_time(&self) -> Option<Duration> {
        let d = self.downloaded_and_checked_pieces.load(Ordering::Acquire);
        let t = self.total_piece_download_ms.load(Ordering::Acquire);
        if d == 0 {
            return None;
        }
        Some(Duration::from_secs_f64(t as f64 / d as f64 / 1000f64))
    }
}

#[derive(Debug, Serialize)]
pub struct StatsSnapshot {
    pub have_bytes: u64,
    pub downloaded_and_checked_bytes: u64,
    pub downloaded_and_checked_pieces: u64,
    pub fetched_bytes: u64,
    pub uploaded_bytes: u64,
    pub initially_needed_bytes: u64,
    pub remaining_bytes: u64,
    pub total_bytes: u64,
    #[serde(skip)]
    pub time: Instant,
    pub total_piece_download_ms: u64,
    pub peer_stats: AggregatePeerStats,
}

impl StatsSnapshot {
    pub fn average_piece_download_time(&self) -> Option<Duration> {
        let d = self.downloaded_and_checked_pieces;
        let t = self.total_piece_download_ms;
        if d == 0 {
            return None;
        }
        Some(Duration::from_secs_f64(t as f64 / d as f64 / 1000f64))
    }
}

#[derive(Default)]
pub struct TorrentStateOptions {
    pub peer_connect_timeout: Option<Duration>,
    pub peer_read_write_timeout: Option<Duration>,
}

pub struct TorrentState {
    peers: PeerStates,
    info: TorrentMetaV1Info<ByteString>,
    locked: Arc<RwLock<TorrentStateLocked>>,
    files: Vec<Arc<Mutex<File>>>,
    filenames: Vec<PathBuf>,
    info_hash: Id20,
    peer_id: Id20,
    lengths: Lengths,
    needed_bytes: u64,
    have_plus_needed_bytes: u64,
    stats: AtomicStats,
    options: TorrentStateOptions,

    // Limits how many active (occupying network resources) peers there are at a moment in time.
    peer_semaphore: Semaphore,

    // The queue for peer manager to connect to them.
    peer_queue_tx: UnboundedSender<SocketAddr>,

    finished_notify: Notify,
}

// Used during debugging to see if some locks take too long.
#[cfg(not(feature = "timed_existence"))]
mod timed_existence {
    use std::ops::{Deref, DerefMut};

    pub struct TimedExistence<T>(T);

    impl<T> TimedExistence<T> {
        #[inline(always)]
        pub fn new(object: T, _reason: &'static str) -> Self {
            Self(object)
        }
    }

    impl<T> Deref for TimedExistence<T> {
        type Target = T;

        #[inline(always)]
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl<T> DerefMut for TimedExistence<T> {
        #[inline(always)]
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }

    #[inline(always)]
    pub fn timeit<R>(_n: impl std::fmt::Display, f: impl FnOnce() -> R) -> R {
        f()
    }
}

#[cfg(feature = "timed_existence")]
mod timed_existence {
    use std::ops::{Deref, DerefMut};
    use std::time::{Duration, Instant};
    use tracing::warn;

    const MAX: Duration = Duration::from_millis(1);

    // Prints if the object exists for too long.
    // This is used to track long-lived locks for debugging.
    pub struct TimedExistence<T> {
        object: T,
        reason: &'static str,
        started: Instant,
    }

    impl<T> TimedExistence<T> {
        pub fn new(object: T, reason: &'static str) -> Self {
            Self {
                object,
                reason,
                started: Instant::now(),
            }
        }
    }

    impl<T> Drop for TimedExistence<T> {
        fn drop(&mut self) {
            let elapsed = self.started.elapsed();
            let reason = self.reason;
            if elapsed > MAX {
                warn!("elapsed on lock {reason:?}: {elapsed:?}")
            }
        }
    }

    impl<T> Deref for TimedExistence<T> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            &self.object
        }
    }

    impl<T> DerefMut for TimedExistence<T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.object
        }
    }

    pub fn timeit<R>(name: impl std::fmt::Display, f: impl FnOnce() -> R) -> R {
        let now = Instant::now();
        let r = f();
        let elapsed = now.elapsed();
        if elapsed > MAX {
            warn!("elapsed on \"{name:}\": {elapsed:?}")
        }
        r
    }
}

pub use timed_existence::{timeit, TimedExistence};

impl TorrentState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        info: TorrentMetaV1Info<ByteString>,
        info_hash: Id20,
        peer_id: Id20,
        files: Vec<Arc<Mutex<File>>>,
        filenames: Vec<PathBuf>,
        chunk_tracker: ChunkTracker,
        lengths: Lengths,
        have_bytes: u64,
        needed_bytes: u64,
        spawner: BlockingSpawner,
        options: Option<TorrentStateOptions>,
    ) -> Arc<Self> {
        let options = options.unwrap_or_default();
        let (peer_queue_tx, peer_queue_rx) = unbounded_channel();
        let state = Arc::new(TorrentState {
            info_hash,
            info,
            peer_id,
            peers: Default::default(),
            locked: Arc::new(RwLock::new(TorrentStateLocked {
                chunks: chunk_tracker,
                inflight_pieces: Default::default(),
            })),
            files,
            filenames,
            stats: AtomicStats {
                have_bytes: AtomicU64::new(have_bytes),
                ..Default::default()
            },
            needed_bytes,
            have_plus_needed_bytes: needed_bytes + have_bytes,
            lengths,
            options,

            peer_semaphore: Semaphore::new(128),
            peer_queue_tx,
            finished_notify: Notify::new(),
        });
        spawn(
            span!(Level::ERROR, "peer_adder"),
            state.clone().task_peer_adder(peer_queue_rx, spawner),
        );
        state
    }

    pub async fn task_manage_peer(
        self: Arc<Self>,
        addr: SocketAddr,
        spawner: BlockingSpawner,
    ) -> anyhow::Result<()> {
        let state = self;
        let (rx, tx) = state.peers.mark_peer_connecting(addr)?;

        let counters = state
            .peers
            .with_peer(addr, |p| p.stats.counters.clone())
            .context("bug: peer not found")?;

        let handler = PeerHandler {
            addr,
            on_bitfield_notify: Default::default(),
            unchoke_notify: Default::default(),
            locked: RwLock::new(PeerHandlerLocked {
                i_am_choked: true,
                previously_requested_pieces: BF::new(),
            }),
            requests_sem: Semaphore::new(0),
            state: state.clone(),
            tx,
            spawner,
            counters,
        };
        let options = PeerConnectionOptions {
            connect_timeout: state.options.peer_connect_timeout,
            read_write_timeout: state.options.peer_read_write_timeout,
            ..Default::default()
        };
        let peer_connection = PeerConnection::new(
            addr,
            state.info_hash,
            state.peer_id,
            &handler,
            Some(options),
            spawner,
        );
        let requester = handler.task_peer_chunk_requester(addr);

        handler
            .counters
            .connection_attempts
            .fetch_add(1, Ordering::Relaxed);
        let res = tokio::select! {
            r = requester => {r}
            r = peer_connection.manage_peer(rx) => {r}
        };

        handler.state.peer_semaphore.add_permits(1);

        match res {
            // We disconnected the peer ourselves as we don't need it
            Ok(()) => {
                handler.on_peer_died(None);
            }
            Err(e) => {
                debug!("error managing peer: {:#}", e);
                handler.on_peer_died(Some(e));
            }
        }
        Ok::<_, anyhow::Error>(())
    }

    pub async fn task_peer_adder(
        self: Arc<Self>,
        mut peer_queue_rx: UnboundedReceiver<SocketAddr>,
        spawner: BlockingSpawner,
    ) -> anyhow::Result<()> {
        let state = self;
        loop {
            let addr = peer_queue_rx.recv().await.unwrap();
            if state.is_finished() {
                debug!("ignoring peer {} as we are finished", addr);
                state.peers.mark_peer_not_needed(addr);
                continue;
            }

            let permit = state.peer_semaphore.acquire().await.unwrap();
            permit.forget();
            spawn(
                span!(parent: None, Level::ERROR, "manage_peer", peer = addr.to_string()),
                state.clone().task_manage_peer(addr, spawner),
            );
        }
    }

    pub fn info(&self) -> &TorrentMetaV1Info<ByteString> {
        &self.info
    }
    pub fn info_hash(&self) -> Id20 {
        self.info_hash
    }
    pub fn peer_id(&self) -> Id20 {
        self.peer_id
    }
    pub fn file_ops(&self) -> FileOps<'_, Sha1> {
        FileOps::new(&self.info, &self.files, &self.lengths)
    }
    pub fn initially_needed(&self) -> u64 {
        self.needed_bytes
    }
    pub fn lock_read(
        &self,
        reason: &'static str,
    ) -> TimedExistence<RwLockReadGuard<TorrentStateLocked>> {
        TimedExistence::new(timeit(reason, || self.locked.read()), reason)
    }
    pub fn lock_write(
        &self,
        reason: &'static str,
    ) -> TimedExistence<RwLockWriteGuard<TorrentStateLocked>> {
        TimedExistence::new(timeit(reason, || self.locked.write()), reason)
    }

    fn get_next_needed_piece(&self, peer_handle: PeerHandle) -> Option<ValidPieceIndex> {
        self.peers
            .with_live_mut(peer_handle, "l(get_next_needed_piece)", |live| {
                let g = self.lock_read("g(get_next_needed_piece)");
                let bf = &live.bitfield;
                for n in g.chunks.iter_needed_pieces() {
                    if bf.get(n).map(|v| *v) == Some(true) {
                        // in theory it should be safe without validation, but whatever.
                        return self.lengths.validate_piece_index(n as u32);
                    }
                }
                None
            })?
    }

    fn am_i_interested_in_peer(&self, handle: PeerHandle) -> bool {
        self.get_next_needed_piece(handle).is_some()
    }

    fn set_peer_live(&self, handle: PeerHandle, h: Handshake) {
        let result = self.peers.with_peer_mut(handle, "set_peer_live", |p| {
            p.state
                .connecting_to_live(Id20(h.peer_id), &self.peers.stats)
                .is_some()
        });
        match result {
            Some(true) => {
                debug!("set peer to live")
            }
            Some(false) => debug!("can't set peer live, it was in wrong state"),
            None => debug!("can't set peer live, it disappeared"),
        }
    }

    pub fn get_uploaded_bytes(&self) -> u64 {
        self.stats.uploaded_bytes.load(Ordering::Relaxed)
    }
    pub fn get_downloaded_bytes(&self) -> u64 {
        self.stats
            .downloaded_and_checked_bytes
            .load(Ordering::Acquire)
    }

    pub fn is_finished(&self) -> bool {
        self.get_left_to_download_bytes() == 0
    }

    pub fn get_left_to_download_bytes(&self) -> u64 {
        self.needed_bytes - self.get_downloaded_bytes()
    }

    fn maybe_transmit_haves(&self, index: ValidPieceIndex) {
        let mut futures = Vec::new();

        for pe in self.peers.states.iter() {
            match &pe.value().state.get() {
                PeerState::Live(live) => {
                    if !live.peer_interested {
                        continue;
                    }

                    if live
                        .bitfield
                        .get(index.get() as usize)
                        .map(|v| *v)
                        .unwrap_or(false)
                    {
                        continue;
                    }

                    let tx = live.tx.downgrade();
                    futures.push(async move {
                        if let Some(tx) = tx.upgrade() {
                            if tx
                                .send(WriterRequest::Message(Message::Have(index.get())))
                                .is_err()
                            {
                                // whatever
                            }
                        }
                    });
                }
                _ => continue,
            }
        }

        if futures.is_empty() {
            trace!("no peers to transmit Have={} to, saving some work", index);
            return;
        }

        let mut unordered: FuturesUnordered<_> = futures.into_iter().collect();
        spawn(
            span!(
                Level::ERROR,
                "transmit_haves",
                piece = index.get(),
                count = unordered.len()
            ),
            async move {
                while unordered.next().await.is_some() {}
                Ok(())
            },
        );
    }

    pub fn add_peer_if_not_seen(self: &Arc<Self>, addr: SocketAddr) -> bool {
        match self.peers.add_if_not_seen(addr) {
            Some(handle) => handle,
            None => return false,
        };

        self.peer_queue_tx.send(addr).unwrap();
        true
    }

    pub fn stats_snapshot(&self) -> StatsSnapshot {
        use Ordering::*;
        let downloaded_bytes = self.stats.downloaded_and_checked_bytes.load(Relaxed);
        let remaining = self.needed_bytes - downloaded_bytes;
        StatsSnapshot {
            have_bytes: self.stats.have_bytes.load(Relaxed),
            downloaded_and_checked_bytes: downloaded_bytes,
            downloaded_and_checked_pieces: self.stats.downloaded_and_checked_pieces.load(Relaxed),
            fetched_bytes: self.stats.fetched_bytes.load(Relaxed),
            uploaded_bytes: self.stats.uploaded_bytes.load(Relaxed),
            total_bytes: self.have_plus_needed_bytes,
            time: Instant::now(),
            initially_needed_bytes: self.needed_bytes,
            remaining_bytes: remaining,
            total_piece_download_ms: self.stats.total_piece_download_ms.load(Relaxed),
            peer_stats: self.peers.stats(),
        }
    }

    pub fn per_peer_stats_snapshot(&self, filter: PeerStatsFilter) -> PeerStatsSnapshot {
        PeerStatsSnapshot {
            peers: self
                .peers
                .states
                .iter()
                .filter(|e| filter.state.matches(e.value().state.get()))
                .map(|e| (e.key().to_string(), e.value().into()))
                .collect(),
        }
    }

    pub async fn wait_until_completed(&self) {
        if self.is_finished() {
            return;
        }
        self.finished_notify.notified().await;
    }
}

struct PeerHandlerLocked {
    pub i_am_choked: bool,

    // This is used to only request a piece from a peer once when stealing from others.
    // So that you don't steal then re-steal the same piece in a loop.
    pub previously_requested_pieces: BF,
}

// All peer state that would never be used by other actors should pe put here.
// This state tracks a live peer.
struct PeerHandler {
    state: Arc<TorrentState>,
    counters: Arc<PeerCounters>,
    // Semantically, we don't need an RwLock here, as this is only requested from
    // one future (requester + manage_peer).
    //
    // However as PeerConnectionHandler takes &self everywhere, we need shared mutability.
    // RefCell would do, but tokio is unhappy when we use it.
    locked: RwLock<PeerHandlerLocked>,

    // This is used to unpause chunk requester once the bitfield
    // is received.
    on_bitfield_notify: Notify,

    // This is used to unpause after we were choked.
    unchoke_notify: Notify,

    // This is used to limit the number of chunk requests we send to a peer at a time.
    requests_sem: Semaphore,

    addr: SocketAddr,
    spawner: BlockingSpawner,

    tx: PeerTx,
}

impl<'a> PeerConnectionHandler for &'a PeerHandler {
    fn on_connected(&self, connection_time: Duration) {
        self.counters.connections.fetch_add(1, Ordering::Relaxed);
        self.counters
            .total_time_connecting_ms
            .fetch_add(connection_time.as_millis() as u64, Ordering::Relaxed);
    }
    fn on_received_message(&self, message: Message<ByteBuf<'_>>) -> anyhow::Result<()> {
        match message {
            Message::Request(request) => {
                self.on_download_request(request)
                    .context("on_download_request")?;
            }
            Message::Bitfield(b) => self
                .on_bitfield(b.clone_to_owned())
                .context("on_bitfield")?,
            Message::Choke => self.on_i_am_choked(),
            Message::Unchoke => self.on_i_am_unchoked(),
            Message::Interested => self.on_peer_interested(),
            Message::Piece(piece) => self.on_received_piece(piece).context("on_received_piece")?,
            Message::KeepAlive => {
                debug!("keepalive received");
            }
            Message::Have(h) => self.on_have(h),
            Message::NotInterested => {
                info!("received \"not interested\", but we don't care yet")
            }
            message => {
                warn!("received unsupported message {:?}, ignoring", message);
            }
        };
        Ok(())
    }

    fn get_have_bytes(&self) -> u64 {
        self.state.stats.have_bytes.load(Ordering::Relaxed)
    }

    fn serialize_bitfield_message_to_buf(&self, buf: &mut Vec<u8>) -> Option<usize> {
        let g = self.state.lock_read("serialize_bitfield_message_to_buf");
        let msg = Message::Bitfield(ByteBuf(g.chunks.get_have_pieces().as_raw_slice()));
        let len = msg.serialize(buf, None).unwrap();
        debug!("sending: {:?}, length={}", &msg, len);
        Some(len)
    }

    fn on_handshake(&self, handshake: Handshake) -> anyhow::Result<()> {
        self.state.set_peer_live(self.addr, handshake);
        Ok(())
    }

    fn on_uploaded_bytes(&self, bytes: u32) {
        self.state
            .stats
            .uploaded_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn read_chunk(&self, chunk: &ChunkInfo, buf: &mut [u8]) -> anyhow::Result<()> {
        self.state.file_ops().read_chunk(self.addr, chunk, buf)
    }

    fn on_extended_handshake(&self, _: &ExtendedHandshake<ByteBuf>) -> anyhow::Result<()> {
        Ok(())
    }
}

impl PeerHandler {
    fn on_peer_died(self, error: Option<anyhow::Error>) {
        let peers = &self.state.peers;
        let pstats = &peers.stats;
        let handle = self.addr;
        let mut pe = match peers.states.get_mut(&handle) {
            Some(peer) => TimedExistence::new(peer, "on_peer_died"),
            None => {
                warn!("bug: peer not found in table. Forgetting it forever");
                return;
            }
        };
        let prev = pe.value_mut().state.take(pstats);

        match prev {
            PeerState::Connecting(_) => {}
            PeerState::Live(live) => {
                let mut g = self.state.lock_write("mark_chunk_requests_canceled");
                for req in live.inflight_requests {
                    debug!(
                        "peer dead, marking chunk request cancelled, index={}, chunk={}",
                        req.piece.get(),
                        req.chunk
                    );
                    g.chunks.mark_chunk_request_cancelled(req.piece, req.chunk);
                }
            }
            PeerState::NotNeeded => {
                // Restore it as std::mem::take() replaced it above.
                pe.value_mut().state.set(PeerState::NotNeeded, pstats);
                return;
            }
            s @ PeerState::Queued | s @ PeerState::Dead => {
                warn!("bug: peer was in a wrong state {s:?}, ignoring it forever");
                // Prevent deadlocks.
                drop(pe);
                self.state.peers.drop_peer(handle);
                return;
            }
        };

        if error.is_none() {
            debug!("peer died without errors, not re-queueing");
            pe.value_mut().state.set(PeerState::NotNeeded, pstats);
            return;
        } else {
            self.counters.errors.fetch_add(1, Ordering::Relaxed);
        }

        if self.state.is_finished() {
            debug!("torrent finished, not re-queueing");
            pe.value_mut().state.set(PeerState::NotNeeded, pstats);
            return;
        }

        pe.value_mut().state.set(PeerState::Dead, pstats);
        let backoff = pe.value_mut().stats.backoff.next_backoff();

        // Prevent deadlocks.
        drop(pe);

        if let Some(dur) = backoff {
            spawn(
                span!(
                    parent: None,
                    Level::ERROR,
                    "wait_for_peer",
                    peer = handle.to_string(),
                    duration = format!("{dur:?}")
                ),
                async move {
                    tokio::time::sleep(dur).await;
                    self.state
                        .peers
                        .with_peer_mut(handle, "dead_to_queued", |peer| {
                            match peer.state.get() {
                                PeerState::Dead => {
                                    peer.state.set(PeerState::Queued, &self.state.peers.stats)
                                }
                                other => bail!(
                                    "peer is in unexpected state: {}. Expected dead",
                                    other.name()
                                ),
                            };
                            Ok(())
                        })
                        .context("bug: peer disappeared")??;
                    self.state.peer_queue_tx.send(handle)?;
                    Ok::<_, anyhow::Error>(())
                },
            );
        } else {
            debug!("dropping peer, backoff exhausted");
            self.state.peers.drop_peer(handle);
        }
    }

    fn reserve_next_needed_piece(&self) -> Option<ValidPieceIndex> {
        // TODO: locking one inside the other in different order results in deadlocks.
        self.state
            .peers
            .with_live_mut(self.addr, "reserve_next_needed_piece", |live| {
                if self.locked.read().i_am_choked {
                    debug!("we are choked, can't reserve next piece");
                    return None;
                }
                let mut g = self.state.lock_write("reserve_next_needed_piece");

                let n = {
                    let mut n_opt = None;
                    let bf = &live.bitfield;
                    for n in g.chunks.iter_needed_pieces() {
                        if bf.get(n).map(|v| *v) == Some(true) {
                            n_opt = Some(n);
                            break;
                        }
                    }

                    self.state.lengths.validate_piece_index(n_opt? as u32)?
                };
                g.inflight_pieces.insert(
                    n,
                    InflightPiece {
                        peer: self.addr,
                        started: Instant::now(),
                    },
                );
                g.chunks.reserve_needed_piece(n);
                Some(n)
            })
            .flatten()
    }

    fn try_steal_old_slow_piece(&self, threshold: f64) -> Option<ValidPieceIndex> {
        let total = self
            .state
            .stats
            .downloaded_and_checked_pieces
            .load(Ordering::Acquire);

        // heuristic for not enough precision in average time
        if total < 20 {
            return None;
        }
        let avg_time = self.state.stats.average_piece_download_time()?;

        let mut g = self.state.lock_write("try_steal_old_slow_piece");
        let (idx, elapsed, piece_req) = g
            .inflight_pieces
            .iter_mut()
            // don't steal from myself
            .filter(|(_, r)| r.peer != self.addr)
            .map(|(p, r)| (p, r.started.elapsed(), r))
            .max_by_key(|(_, e, _)| *e)?;

        // heuristic for "too slow peer"
        if elapsed.as_secs_f64() > avg_time.as_secs_f64() * threshold {
            debug!(
                "will steal piece {} from {}: elapsed time {:?}, avg piece time: {:?}",
                idx, piece_req.peer, elapsed, avg_time
            );
            piece_req.peer = self.addr;
            piece_req.started = Instant::now();
            return Some(*idx);
        }
        None
    }

    fn on_download_request(&self, request: Request) -> anyhow::Result<()> {
        let piece_index = match self.state.lengths.validate_piece_index(request.index) {
            Some(p) => p,
            None => {
                anyhow::bail!(
                    "received {:?}, but it is not a valid chunk request (piece index is invalid). Ignoring.",
                    request
                );
            }
        };
        let chunk_info = match self.state.lengths.chunk_info_from_received_data(
            piece_index,
            request.begin,
            request.length,
        ) {
            Some(d) => d,
            None => {
                anyhow::bail!(
                    "received {:?}, but it is not a valid chunk request (chunk data is invalid). Ignoring.",
                    request
                );
            }
        };

        if !self
            .state
            .lock_read("is_chunk_ready_to_upload")
            .chunks
            .is_chunk_ready_to_upload(&chunk_info)
        {
            anyhow::bail!(
                "got request for a chunk that is not ready to upload. chunk {:?}",
                &chunk_info
            );
        }

        // TODO: this is not super efficient as it does copying multiple times.
        // Theoretically, this could be done in the sending code, so that it reads straight into
        // the send buffer.
        let request = WriterRequest::ReadChunkRequest(chunk_info);
        debug!("sending {:?}", &request);
        Ok::<_, anyhow::Error>(self.tx.send(request)?)
    }

    fn on_have(&self, have: u32) {
        self.state
            .peers
            .with_live_mut(self.addr, "on_have", |live| {
                live.bitfield.set(have as usize, true);
                debug!("updated bitfield with have={}", have);
            });
    }

    fn on_bitfield(&self, bitfield: ByteString) -> anyhow::Result<()> {
        if bitfield.len() != self.state.lengths.piece_bitfield_bytes() {
            anyhow::bail!(
                "dropping peer as its bitfield has unexpected size. Got {}, expected {}",
                bitfield.len(),
                self.state.lengths.piece_bitfield_bytes(),
            );
        }
        self.locked.write().previously_requested_pieces = BF::from_vec(vec![0; bitfield.len()]);
        self.state
            .peers
            .update_bitfield_from_vec(self.addr, bitfield.0);

        if !self.state.am_i_interested_in_peer(self.addr) {
            self.tx
                .send(WriterRequest::Message(MessageOwned::Unchoke))?;
            self.tx
                .send(WriterRequest::Message(MessageOwned::NotInterested))?;
            if self.state.is_finished() {
                self.tx.send(WriterRequest::Disconnect)?;
            }
            return Ok(());
        }

        self.on_bitfield_notify.notify_waiters();
        Ok(())
    }

    async fn task_peer_chunk_requester(&self, handle: PeerHandle) -> anyhow::Result<()> {
        self.on_bitfield_notify.notified().await;
        self.tx.send_many([
            WriterRequest::Message(MessageOwned::Unchoke),
            WriterRequest::Message(MessageOwned::Interested),
        ])?;

        #[allow(unused_must_use)]
        {
            timeout(Duration::from_secs(60), self.unchoke_notify.notified()).await;
        }

        loop {
            if self.locked.read().i_am_choked {
                debug!("we are choked, can't reserve next piece");
                #[allow(unused_must_use)]
                {
                    timeout(Duration::from_secs(60), self.unchoke_notify.notified()).await;
                }
                continue;
            }

            if self.state.is_finished() {
                debug!("nothing left to download, looping forever until manage_peer quits");
                loop {
                    tokio::time::sleep(Duration::from_secs(86400)).await;
                }
            }

            // Try steal a pice from a very slow peer first. Otherwise we might wait too long
            // to download early pieces.
            // Then try get the next one in queue.
            // Afterwards means we are close to completion, try stealing more aggressively.
            let next = match self
                .try_steal_old_slow_piece(10.)
                .or_else(|| self.reserve_next_needed_piece())
                .or_else(|| self.try_steal_old_slow_piece(2.))
            {
                Some(next) => next,
                None => {
                    debug!("no pieces to request");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
            };

            self.locked
                .write()
                .previously_requested_pieces
                .set(next.get() as usize, true);

            for chunk in self.state.lengths.iter_chunk_infos(next) {
                let request = Request {
                    index: next.get(),
                    begin: chunk.offset,
                    length: chunk.size,
                };

                match self
                    .state
                    .peers
                    .with_live_mut(handle, "add chunk request", |live| {
                        live.inflight_requests.insert(InflightRequest::from(&chunk))
                    }) {
                    Some(true) => {}
                    Some(false) => {
                        // This request was already in-flight for this peer for this chunk.
                        // This might happen in theory, but not very likely.
                        //
                        // Example:
                        // someone stole a piece from us, and then died, the piece became "needed" again, and we reserved it
                        // all before the piece request was processed by us.
                        warn!("we already requested {:?} previously", chunk);
                        continue;
                    }
                    // peer died
                    None => return Ok(()),
                };

                loop {
                    match timeout(Duration::from_secs(10), self.requests_sem.acquire()).await {
                        Ok(acq) => break acq?.forget(),
                        Err(_) => continue,
                    };
                }

                if self
                    .tx
                    .send(WriterRequest::Message(MessageOwned::Request(request)))
                    .is_err()
                {
                    return Ok(());
                }
            }
        }
    }

    fn on_i_am_choked(&self) {
        self.locked.write().i_am_choked = true;
    }

    fn on_peer_interested(&self) {
        debug!("peer is interested");
        self.state.peers.mark_peer_interested(self.addr, true);
    }

    fn reopen_read_only(&self) -> anyhow::Result<()> {
        fn dummy_file() -> anyhow::Result<std::fs::File> {
            #[cfg(target_os = "windows")]
            const DEVNULL: &str = "NUL";
            #[cfg(not(target_os = "windows"))]
            const DEVNULL: &str = "/dev/null";

            std::fs::OpenOptions::new()
                .read(true)
                .open(DEVNULL)
                .with_context(|| format!("error opening {}", DEVNULL))
        }

        // Lock exclusive just in case to ensure in-flight operations finish.??
        let _guard = self.state.lock_write("reopen_read_only");

        for (file, filename) in self.state.files.iter().zip(self.state.filenames.iter()) {
            let mut g = file.lock();
            // this should close the original file
            // putting in a block just in case to guarantee drop.
            {
                *g = dummy_file()?;
            }
            *g = std::fs::OpenOptions::new()
                .read(true)
                .open(filename)
                .with_context(|| format!("error re-opening {:?} readonly", filename))?;
            debug!("reopened {:?} read-only", filename);
        }
        info!("reopened all torrent files in read-only mode");
        Ok(())
    }

    fn on_i_am_unchoked(&self) {
        debug!("we are unchoked");
        self.locked.write().i_am_choked = false;
        self.unchoke_notify.notify_waiters();
        self.requests_sem.add_permits(16);
    }

    fn on_received_piece(&self, piece: Piece<ByteBuf>) -> anyhow::Result<()> {
        let chunk_info = match self.state.lengths.chunk_info_from_received_piece(
            piece.index,
            piece.begin,
            piece.block.len() as u32,
        ) {
            Some(i) => i,
            None => {
                anyhow::bail!("peer sent us an invalid piece {:?}", &piece,);
            }
        };

        self.requests_sem.add_permits(1);

        // Peer chunk/byte counters.
        self.counters
            .fetched_bytes
            .fetch_add(piece.block.len() as u64, Ordering::Relaxed);
        self.counters.fetched_chunks.fetch_add(1, Ordering::Relaxed);

        // Global chunk/byte counters.
        self.state
            .stats
            .fetched_bytes
            .fetch_add(piece.block.len() as u64, Ordering::Relaxed);

        self.state
            .peers
            .with_live_mut(self.addr, "inflight_requests.remove", |h| {
                if !h
                    .inflight_requests
                    .remove(&InflightRequest::from(&chunk_info))
                {
                    anyhow::bail!(
                        "peer sent us a piece we did not ask. Requested pieces: {:?}. Got: {:?}",
                        &h.inflight_requests,
                        &piece,
                    );
                }
                Ok(())
            })
            .context("peer not found")??;

        let full_piece_download_time = {
            let mut g = self.state.lock_write("mark_chunk_downloaded");

            match g.inflight_pieces.get(&chunk_info.piece_index) {
                Some(InflightPiece { peer, .. }) if *peer == self.addr => {}
                Some(InflightPiece { peer, .. }) => {
                    debug!(
                        "in-flight piece {} was stolen by {}, ignoring",
                        chunk_info.piece_index, peer
                    );
                    return Ok(());
                }
                None => {
                    debug!(
                        "in-flight piece {} not found. it was probably completed by someone else",
                        chunk_info.piece_index
                    );
                    return Ok(());
                }
            };

            match g.chunks.mark_chunk_downloaded(&piece) {
                Some(ChunkMarkingResult::Completed) => {
                    debug!("piece={} done, will write and checksum", piece.index,);
                    // This will prevent others from stealing it.
                    {
                        let piece = chunk_info.piece_index;
                        g.inflight_pieces.remove(&piece)
                    }
                    .map(|t| t.started.elapsed())
                }
                Some(ChunkMarkingResult::PreviouslyCompleted) => {
                    // TODO: we might need to send cancellations here.
                    debug!("piece={} was done by someone else, ignoring", piece.index,);
                    return Ok(());
                }
                Some(ChunkMarkingResult::NotCompleted) => None,
                None => {
                    anyhow::bail!(
                        "bogus data received: {:?}, cannot map this to a chunk, dropping peer",
                        piece
                    );
                }
            }
        };

        // By this time we reach here, no other peer can for this piece. All others, even if they steal pieces would
        // have fallen off above in one of the defensive checks.

        self.spawner
            .spawn_block_in_place(move || {
                let index = piece.index;

                // TODO: in theory we should unmark the piece as downloaded here. But if there was a disk error, what
                // should we really do? If we unmark it, it will get requested forever...
                //
                // So let's just unwrap and abort.
                match self
                    .state
                    .file_ops()
                    .write_chunk(self.addr, &piece, &chunk_info)
                {
                    Ok(()) => {}
                    Err(e) => {
                        error!("FATAL: error writing chunk to disk: {:?}", e);
                        panic!("{:?}", e);
                    }
                }

                let full_piece_download_time = match full_piece_download_time {
                    Some(t) => t,
                    None => return Ok(()),
                };

                match self
                    .state
                    .file_ops()
                    .check_piece(self.addr, chunk_info.piece_index, &chunk_info)
                    .with_context(|| format!("error checking piece={index}"))?
                {
                    true => {
                        {
                            let mut g = self.state.lock_write("mark_piece_downloaded");
                            g.chunks.mark_piece_downloaded(chunk_info.piece_index);
                        }

                        // Global piece counters.
                        let piece_len =
                            self.state.lengths.piece_length(chunk_info.piece_index) as u64;
                        self.state
                            .stats
                            .downloaded_and_checked_bytes
                            // This counter is used to compute "is_finished", so using
                            // stronger ordering.
                            .fetch_add(piece_len, Ordering::Release);
                        self.state
                            .stats
                            .downloaded_and_checked_pieces
                            // This counter is used to compute "is_finished", so using
                            // stronger ordering.
                            .fetch_add(1, Ordering::Release);
                        self.state
                            .stats
                            .have_bytes
                            .fetch_add(piece_len, Ordering::Relaxed);
                        self.state.stats.total_piece_download_ms.fetch_add(
                            full_piece_download_time.as_millis() as u64,
                            Ordering::Release,
                        );

                        // Per-peer piece counters.
                        self.counters
                            .downloaded_and_checked_pieces
                            .fetch_add(1, Ordering::Relaxed);
                        self.counters
                            .downloaded_and_checked_bytes
                            .fetch_add(piece_len, Ordering::Relaxed);

                        self.state.peers.reset_peer_backoff(self.addr);

                        debug!("piece={} successfully downloaded and verified", index);

                        if self.state.is_finished() {
                            info!("torrent finished downloading");
                            self.state.finished_notify.notify_waiters();
                            self.disconnect_all_peers_that_have_full_torrent();
                            self.reopen_read_only()?;
                        }

                        self.state.maybe_transmit_haves(chunk_info.piece_index);
                    }
                    false => {
                        warn!("checksum for piece={} did not validate", index,);
                        self.state
                            .lock_write("mark_piece_broken")
                            .chunks
                            .mark_piece_broken(chunk_info.piece_index);
                    }
                };
                Ok::<_, anyhow::Error>(())
            })
            .with_context(|| format!("error processing received chunk {chunk_info:?}"))?;
        Ok(())
    }

    fn disconnect_all_peers_that_have_full_torrent(&self) {
        for mut pe in self.state.peers.states.iter_mut() {
            if let PeerState::Live(l) = pe.value().state.get() {
                if l.has_full_torrent(self.state.lengths.total_pieces() as usize) {
                    let prev = pe.value_mut().state.to_not_needed(&self.state.peers.stats);
                    let _ = prev
                        .take_live_no_counters()
                        .unwrap()
                        .tx
                        .send(WriterRequest::Disconnect);
                }
            }
        }
    }
}
