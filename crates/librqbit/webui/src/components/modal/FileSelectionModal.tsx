import { useContext, useEffect, useState, useRef } from "react";
import { AddTorrentResponse, AddTorrentOptions, DirPreview } from "../../api-types";
import { APIContext } from "../../context";
import { ErrorComponent } from "../ErrorComponent";
import { ErrorWithLabel } from "../../rqbit-web";
import { Spinner } from "../Spinner";
import { Modal } from "./Modal";
import { ModalBody } from "./ModalBody";
import { ModalFooter } from "./ModalFooter";
import { Button } from "../buttons/Button";
import { Fieldset } from "../forms/Fieldset";
import { FormInput } from "../forms/FormInput";
import { Form } from "../forms/Form";
import { FileListInput } from "../FileListInput";
import { useTorrentStore } from "../../stores/torrentStore";

function isPotentiallyValidUnixPath(s: string) {
  if (!s || !s.startsWith('/')) {
    return false;
  }
  // Regular expression to match valid Unix path characters
  const validPathRegex = /^[a-zA-Z0-9_\-./]+$/;

  // Check if the path contains only valid characters
  if (!validPathRegex.test(s)) {
    return false;
  }

  return true;
}

export const FileSelectionModal = (props: {
  onHide: () => void;
  listTorrentResponse: AddTorrentResponse | null;
  listTorrentError: ErrorWithLabel | null;
  listTorrentLoading: boolean;
  data: string | File;
}) => {
  let {
    onHide,
    listTorrentResponse,
    listTorrentError,
    listTorrentLoading,
    data,
  } = props;

  const [selectedFiles, setSelectedFiles] = useState<Set<number>>(new Set());
  const [uploading, setUploading] = useState(false);
  const [uploadError, setUploadError] = useState<ErrorWithLabel | null>(null);
  const [unpopularTorrent, setUnpopularTorrent] = useState(false);
  const [dirPreview, setDirPreview] = useState<DirPreview | null>(null);
  const [initialOutputFolder, setInitialOutputFolder] = useState<string | null>(null);
  const [inputDestination, setInputDestination] = useState<string>("");
  const [inputDestinationValid, setInputDestinationValid] = useState(false);
  const refreshTorrents = useTorrentStore((state) => state.refreshTorrents);
  const API = useContext(APIContext);

  useEffect(() => {
    setSelectedFiles(
      new Set(listTorrentResponse?.details.files.map((_, i) => i)),
    );
    setInputDestination(listTorrentResponse?.output_folder || "");
  }, [listTorrentResponse]);

// Modify the useEffect for inputDestination:
useEffect(() => {
  const isMinimallyValid = isPotentiallyValidUnixPath(inputDestination);
  setInputDestinationValid(isMinimallyValid);
  if (!isMinimallyValid) {
    setDirPreview(null);
    return;
  }

  const debounceTimer = setTimeout(() => {
    API.getDirPreview(inputDestination)
      .then((dirPreview) => {
        console.log("Got back dirPreview: ", dirPreview);
        setDirPreview(dirPreview);
      })
      .catch((e) => {
        console.error("Error when fetching dir preview: ", e);
        setDirPreview(null);
      });
  }, 300); // 300ms debounce

  return () => clearTimeout(debounceTimer);
}, [inputDestination, API]);

  const clear = () => {
    onHide();
    setSelectedFiles(new Set());
    setUploadError(null);
    setUploading(false);
  };

  const handleUpload = async () => {
    if (!listTorrentResponse) {
      return;
    }
    setUploading(true);
    let initialPeers = listTorrentResponse.seen_peers
      ? listTorrentResponse.seen_peers.slice(0, 32)
      : null;
    let opts: AddTorrentOptions = {
      overwrite: true,
      only_files: Array.from(selectedFiles),
      initial_peers: initialPeers,
      output_folder: inputDestination,
    };
    if (unpopularTorrent) {
      opts.peer_opts = {
        connect_timeout: 20,
        read_write_timeout: 60,
      };
    }
    API.uploadTorrent(data, opts)
      .then(
        () => {
          onHide();
          refreshTorrents();
        },
        (e) => {
          setUploadError({ text: "Error starting torrent", details: e });
        },
      )
      .finally(() => setUploading(false));
  };


  const getBody = () => {
    if (listTorrentLoading) {
      return <Spinner label="Loading torrent contents" />;
    } else if (listTorrentError) {
      return <ErrorComponent error={listTorrentError}></ErrorComponent>;
    } else if (listTorrentResponse) {
      return (
        <Form>
          <div className="mb-4">
            <label htmlFor="output_folder" className="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-1">
              Output folder
            </label>
            <DirectorySelector
              value={inputDestination}
              onChange={setInputDestination}
              dirPreview={dirPreview}
              isValid={inputDestinationValid}
            />
          </div>

          <Fieldset>
            <FileListInput
              selectedFiles={selectedFiles}
              setSelectedFiles={setSelectedFiles}
              torrentDetails={listTorrentResponse.details}
              torrentStats={null}
            />
          </Fieldset>

          {/* <Fieldset label="Options">
            <FormCheckbox
              label="Increase timeouts"
              checked={unpopularTorrent}
              onChange={() => setUnpopularTorrent(!unpopularTorrent)}
              help="This might be useful for unpopular torrents with few peers. It will slow down fast torrents though."
              name="increase_timeouts"
            />
          </Fieldset> */}
        </Form>
      );
    }
  };
  return (
    <Modal isOpen={true} onClose={clear} title="Add Torrent">
      <ModalBody>
        {getBody()}
        <ErrorComponent error={uploadError} />
      </ModalBody>
      <ModalFooter>
        {uploading && <Spinner />}
        <Button onClick={clear} variant="cancel">
          Cancel
        </Button>
        <Button
          onClick={handleUpload}
          variant="primary"
          disabled={listTorrentLoading || uploading || selectedFiles.size == 0 || !inputDestinationValid}
        >
          OK
        </Button>
      </ModalFooter>
    </Modal>
  );
};

const DirectorySelector: React.FC<{
  value: string;
  onChange: (value: string) => void;
  dirPreview: DirPreview | null;
  isValid: boolean;
}> = ({ value, onChange, dirPreview, isValid }) => {
  const [showDropdown, setShowDropdown] = useState(false);
  const [selectedIndex, setSelectedIndex] = useState(-1);
  const inputRef = useRef<HTMLInputElement>(null);

  const handleKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setSelectedIndex((prev) => 
        Math.min(prev + 1, ((dirPreview?.matching_dirs.length || 0) + 1) - 1)
      );
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setSelectedIndex((prev) => Math.max(prev - 1, -1));
    } else if (e.key === "Enter" && selectedIndex !== -1) {
      e.preventDefault();
      if (selectedIndex === 0) {
        // Current value selected
        setShowDropdown(false);
      } else {
        onChange(dirPreview!.matching_dirs[selectedIndex - 1]);
        setShowDropdown(false);
      }
    }
  };

  useEffect(() => {
    setSelectedIndex(-1);
    setShowDropdown(dirPreview !== null && dirPreview.matching_dirs.length > 0);
  }, [dirPreview]);

  const handleSelectOption = (option: string) => {
    onChange(option);
    setShowDropdown(false);
    inputRef.current?.focus();
  };

  return (
    <div className="relative mb-16">
      <input
        ref={inputRef}
        type="text"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={handleKeyDown}
        onFocus={() => setShowDropdown(true)}
        className={`w-full p-2 border rounded ${
          isValid ? "border-gray-300" : "border-red-500"
        }`}
      />
      {showDropdown && (
        <ul className="absolute z-10 w-full bg-white border border-gray-300 rounded mt-1 max-h-60 overflow-y-auto">
          <li
            className={`p-2 cursor-pointer hover:bg-gray-100 ${
              selectedIndex === 0 ? "bg-blue-100" : ""
            }`}
            onMouseDown={() => handleSelectOption(value)}
          >
            {value} (current)
          </li>
          {dirPreview?.matching_dirs.map((dir, index) => (
            <li
              key={dir}
              className={`p-2 cursor-pointer hover:bg-gray-100 ${
                index + 1 === selectedIndex ? "bg-blue-100" : ""
              }`}
              onMouseDown={() => handleSelectOption(dir)}
            >
              {dir}
            </li>
          ))}
        </ul>
      )}
      {!isValid && <p className="text-red-500 text-sm mt-1">Invalid path</p>}
      {dirPreview && dirPreview.suggestion_full_path && (
        <p className="text-gray-500 text-sm mt-1">
          Suggestion: {dirPreview.suggestion_full_path}
        </p>
      )}
      {dirPreview && (
        <p className="text-gray-500 text-sm mt-1">
          {dirPreview.full_path} {dirPreview.exists ? "exists" : "will be created"}
        </p>
      )}
    </div>
  );
};

