import { useEffect, useRef, useState } from "react";
import * as api from "./api";
import type { UpdateInfo } from "./types";

interface Props {
  update: UpdateInfo;
  onClose: () => void;
}

export default function UpdateDialog({ update, onClose }: Props) {
  const ref = useRef<HTMLDialogElement>(null);
  const [installing, setInstalling] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const dialog = ref.current;
    if (dialog && !dialog.open) dialog.showModal();
  }, []);

  async function onInstall() {
    setInstalling(true);
    setError(null);
    try {
      // The app relaunches when the install succeeds, so there is no success path here.
      await api.installUpdate();
    } catch (e) {
      setError(api.errorMessage(e));
      setInstalling(false);
    }
  }

  return (
    <dialog
      ref={ref}
      onCancel={(e) => {
        if (installing) e.preventDefault();
        else onClose();
      }}
      aria-labelledby="update-title"
    >
      <h2 id="update-title">Update available</h2>
      <p>
        Owlet <strong>{update.version}</strong> is available (you have {update.current}).
        The app restarts after installing and the MCP server is stopped for a moment.
      </p>
      {update.notes && <pre className="notes">{update.notes}</pre>}
      {error && (
        <p className="banner error" role="alert">
          {error}
        </p>
      )}
      <div className="actions">
        <button type="button" onClick={onClose} disabled={installing}>
          Later
        </button>
        <button
          type="button"
          className="primary"
          aria-busy={installing}
          disabled={installing}
          onClick={() => void onInstall()}
          autoFocus
        >
          {installing && <span className="spinner" aria-hidden="true" />}
          {installing ? "Installing…" : "Install and restart"}
        </button>
      </div>
    </dialog>
  );
}
