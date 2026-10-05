import { useEffect, useRef, useState, type FormEvent } from "react";
import { emptyForm, fromForm, toForm, type FormValues } from "./form";
import type { Scope, ServerConfig, Transport } from "./types";

interface Props {
  /** Existing entry being edited; null when adding. */
  editing: { name: string; server: ServerConfig } | null;
  onSubmit: (name: string, server: ServerConfig) => Promise<void>;
  onCancel: () => void;
}

export default function ServerDialog({ editing, onSubmit, onCancel }: Props) {
  const ref = useRef<HTMLDialogElement>(null);
  const [form, setForm] = useState<FormValues>(() =>
    editing ? toForm(editing.name, editing.server) : emptyForm(),
  );
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const dialog = ref.current;
    if (dialog && !dialog.open) dialog.showModal();
  }, []);

  const set = <K extends keyof FormValues>(key: K, value: FormValues[K]) =>
    setForm((f) => ({ ...f, [key]: value }));

  async function submit(e: FormEvent) {
    e.preventDefault();
    setError(null);
    let parsed;
    try {
      parsed = fromForm(form);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      return;
    }
    setBusy(true);
    try {
      await onSubmit(parsed.name, parsed.server);
    } catch (err) {
      setError(typeof err === "string" ? err : String(err));
      setBusy(false);
    }
  }

  const isCommand = form.kind === "command";

  return (
    <dialog ref={ref} onCancel={onCancel} aria-labelledby="server-title">
      <form onSubmit={submit}>
        <h2 id="server-title">{editing ? `Edit ${editing.name}` : "Add server"}</h2>

        <label>
          Name
          <input
            value={form.name}
            onChange={(e) => set("name", e.target.value)}
            autoFocus
            required
          />
        </label>

        <fieldset>
          <legend>Type</legend>
          <label className="inline">
            <input
              type="radio"
              name="kind"
              checked={isCommand}
              onChange={() => set("kind", "command")}
            />
            Local command (stdio)
          </label>
          <label className="inline">
            <input
              type="radio"
              name="kind"
              checked={!isCommand}
              onChange={() => set("kind", "url")}
            />
            Remote URL
          </label>
        </fieldset>

        {isCommand ? (
          <>
            <label>
              Command
              <input
                value={form.command}
                onChange={(e) => set("command", e.target.value)}
                placeholder="uvx"
              />
            </label>
            <label>
              Arguments <small>one per line</small>
              <textarea
                rows={3}
                value={form.args}
                onChange={(e) => set("args", e.target.value)}
                placeholder="mcp-server-fetch"
              />
            </label>
            <label>
              Environment <small>KEY=VALUE per line</small>
              <textarea
                rows={2}
                value={form.env}
                onChange={(e) => set("env", e.target.value)}
              />
            </label>
            <label>
              Working directory
              <input value={form.cwd} onChange={(e) => set("cwd", e.target.value)} />
            </label>
          </>
        ) : (
          <>
            <label>
              URL
              <input
                value={form.url}
                onChange={(e) => set("url", e.target.value)}
                placeholder="https://example.com/mcp"
              />
            </label>
            <label>
              Transport
              <select
                value={form.transport}
                onChange={(e) => set("transport", e.target.value as "" | Transport)}
              >
                <option value="">Streamable HTTP (default)</option>
                <option value="http">http</option>
                <option value="sse">sse (legacy)</option>
              </select>
            </label>
            <label>
              Headers <small>Key: Value per line</small>
              <textarea
                rows={2}
                value={form.headers}
                onChange={(e) => set("headers", e.target.value)}
              />
            </label>
          </>
        )}

        <div className="row">
          <label>
            Scope
            <select
              value={form.scope}
              onChange={(e) => set("scope", e.target.value as Scope)}
            >
              <option value="shared">shared</option>
              <option value="project" disabled={!isCommand}>
                project
              </option>
              <option value="session">session</option>
            </select>
          </label>
          <label>
            Idle timeout <small>seconds, blank = global</small>
            <input
              inputMode="numeric"
              value={form.idleTimeout}
              onChange={(e) => set("idleTimeout", e.target.value)}
            />
          </label>
        </div>

        <label className="inline">
          <input
            type="checkbox"
            checked={form.cacheLists}
            onChange={(e) => set("cacheLists", e.target.checked)}
          />
          Cache */list results
        </label>
        <label className="inline">
          <input
            type="checkbox"
            checked={form.enabled}
            onChange={(e) => set("enabled", e.target.checked)}
          />
          Enabled
        </label>

        {error && (
          <p className="error" role="alert">
            {error}
          </p>
        )}

        <div className="actions">
          <button type="button" onClick={onCancel} disabled={busy}>
            Cancel
          </button>
          <button type="submit" className="primary" disabled={busy}>
            {editing ? "Apply" : "Add"}
          </button>
        </div>
      </form>
    </dialog>
  );
}
