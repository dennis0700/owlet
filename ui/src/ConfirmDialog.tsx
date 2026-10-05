import { useEffect, useRef } from "react";

interface Props {
  title: string;
  message: string;
  confirmLabel: string;
  onConfirm: () => void;
  onCancel: () => void;
}

export default function ConfirmDialog({
  title,
  message,
  confirmLabel,
  onConfirm,
  onCancel,
}: Props) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const dialog = ref.current;
    if (dialog && !dialog.open) dialog.showModal();
  }, []);

  return (
    <dialog ref={ref} onCancel={onCancel} aria-labelledby="confirm-title">
      <h2 id="confirm-title">{title}</h2>
      <p>{message}</p>
      <div className="actions">
        <button type="button" onClick={onCancel}>
          Cancel
        </button>
        <button type="button" className="danger" onClick={onConfirm} autoFocus>
          {confirmLabel}
        </button>
      </div>
    </dialog>
  );
}
