import { useCallback, useEffect, useState } from "react";
import * as api from "./api";
import ConfirmDialog from "./ConfirmDialog";
import ServerDialog from "./ServerDialog";
import UpdateDialog from "./UpdateDialog";
import type { Entry, ServerConfig, ServerStatus, Snapshot, UpdateInfo } from "./types";

type Dialog =
  | { kind: "add" }
  | { kind: "edit"; entry: Entry }
  | { kind: "delete"; name: string }
  | { kind: "discard" }
  | { kind: "update"; update: UpdateInfo }
  | null;

const MIN_SPIN_MS = 600;
const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

function Spinner() {
  return <span className="spinner" aria-hidden="true" />;
}

function summary(s: ServerConfig): string {
  if (s.url !== null) return `${s.transport ?? "http"} · ${s.url}`;
  return [s.command, ...s.args].join(" ");
}

export default function App() {
  const [snap, setSnap] = useState<Snapshot | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<{ id: number; text: string } | null>(null);
  const [dialog, setDialog] = useState<Dialog>(null);
  const [saving, setSaving] = useState(false);
  const [server, setServer] = useState<ServerStatus | null>(null);
  const [reloading, setReloading] = useState(false);
  const [restarting, setRestarting] = useState(false);
  const [update, setUpdate] = useState<UpdateInfo | null>(null);
  const [checking, setChecking] = useState(false);
  const [upToDate, setUpToDate] = useState(false);

  const run = useCallback(async (op: () => Promise<Snapshot>, done?: string) => {
    try {
      setSnap(await op());
      setError(null);
      setNotice(done ? { id: Date.now(), text: done } : null);
    } catch (e) {
      setError(api.errorMessage(e));
      setNotice(null);
    }
  }, []);

  useEffect(() => {
    void run(api.getState);
  }, [run]);

  useEffect(() => {
    if (!notice) return;
    const timer = setTimeout(() => setNotice(null), 3000);
    return () => clearTimeout(timer);
  }, [notice]);

  useEffect(() => {
    if (!upToDate) return;
    const timer = setTimeout(() => setUpToDate(false), 3000);
    return () => clearTimeout(timer);
  }, [upToDate]);

  useEffect(() => {
    let active = true;
    const unlisten = api.onServerStatus((status) => {
      if (!active) return;
      setServer(status);
    });
    api.serverStatus().then(
      (status) => active && setServer((cur) => cur ?? status),
      (e) => active && setError(api.errorMessage(e)),
    );
    return () => {
      active = false;
      void unlisten.then((off) => off());
    };
  }, []);

  useEffect(() => {
    let active = true;
    api.checkUpdate().then(
      (info) => active && setUpdate(info),
      () => {},
    );
    return () => {
      active = false;
    };
  }, []);

  useEffect(() => {
    if (!snap?.dirty) return;
    const warn = (e: BeforeUnloadEvent) => e.preventDefault();
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [snap?.dirty]);

  if (!snap) {
    return <main>{error ? <p className="banner error">{error}</p> : <p>Loading…</p>}</main>;
  }

  const locked = snap.load_error !== null;
  const changed = snap.entries.filter((e) => e.status !== "unchanged").length;

  async function onSave() {
    setSaving(true);
    await run(api.save, "Saved.");
    setSaving(false);
  }

  // The spinner stays until `op` has finished and the service left `starting`,
  // and for at least MIN_SPIN_MS so a fast operation is still visible.
  async function withSpinner(
    setBusy: (busy: boolean) => void,
    op: () => Promise<unknown>,
  ) {
    setBusy(true);
    const minimum = sleep(MIN_SPIN_MS);
    try {
      await op();
    } finally {
      await minimum;
      setBusy(false);
    }
  }

  async function onReload() {
    await withSpinner(setReloading, () => run(api.reload));
  }

  async function onUpdate() {
    if (update) {
      setDialog({ kind: "update", update });
      return;
    }
    setChecking(true);
    try {
      const info = await api.checkUpdate();
      setUpdate(info);
      setError(null);
      if (info) {
        setDialog({ kind: "update", update: info });
      } else {
        setUpToDate(true);
      }
    } catch (e) {
      setError(`Cannot check for updates: ${api.errorMessage(e)}`);
      setNotice(null);
    } finally {
      setChecking(false);
    }
  }

  async function onRestart() {
    await withSpinner(setRestarting, async () => {
      try {
        await api.restartServer();
        for (let i = 0; i < 100; i++) {
          if ((await api.serverStatus()).state !== "starting") break;
          await sleep(50);
        }
      } catch (e) {
        setError(api.errorMessage(e));
      }
    });
  }

  // Status comes from `server-status` events only: a command's return value can
  // be older than an event that already arrived and would overwrite it.
  async function onServer(op: () => Promise<ServerStatus>) {
    try {
      await op();
    } catch (e) {
      setError(api.errorMessage(e));
    }
  }

  const busy = server === null || server.state === "starting" || restarting;
  const running = server?.state === "running";

  return (
    <main>
      <header>
        <div>
          <h1>Owlet</h1>
          <p className="path" title={snap.path}>
            {snap.path}
          </p>
        </div>
        <div className="toolbar">
          {snap.dirty && <span className="dirty">Unsaved changes</span>}
          <button
            type="button"
            className={upToDate ? "ok" : ""}
            aria-busy={checking}
            disabled={checking || upToDate}
            onClick={() => void onUpdate()}
          >
            {checking && <Spinner />}
            {upToDate
              ? "✓ Up to date"
              : update
                ? `Update to ${update.version}`
                : "Check for updates"}
          </button>
          <button type="button" onClick={() => setDialog({ kind: "add" })} disabled={locked}>
            Add server
          </button>
          <button
            type="button"
            aria-busy={reloading}
            disabled={reloading}
            onClick={() => (snap.dirty ? setDialog({ kind: "discard" }) : void onReload())}
          >
            {reloading && <Spinner />}
            {snap.dirty ? "Discard" : "Reload"}
          </button>
          <button
            type="button"
            className="primary"
            onClick={() => void onSave()}
            disabled={locked || !snap.dirty || saving}
          >
            Save{changed > 0 ? ` (${changed})` : ""}
          </button>
        </div>
      </header>

      <section className="service" aria-label="Owlet service">
        <span className={`dot ${server?.state ?? "starting"}`} aria-hidden="true" />
        <span role="status">
          {server === null && "Checking service…"}
          {server?.state === "starting" && "Starting…"}
          {server?.state === "running" && (
            <>
              Listening on <code>http://{server.listen}</code>
            </>
          )}
          {server?.state === "stopped" && "Stopped"}
          {server?.state === "failed" && "Failed"}
        </span>
        <div className="controls">
          {running || server?.state === "starting" ? (
            <button type="button" disabled={busy} onClick={() => void onServer(api.stopServer)}>
              Stop
            </button>
          ) : (
            <button type="button" disabled={busy} onClick={() => void onServer(api.startServer)}>
              Start
            </button>
          )}
          <button
            type="button"
            disabled={busy || snap.dirty}
            title={snap.dirty ? "Save your changes first" : "Apply the saved config"}
            aria-busy={restarting}
            onClick={() => void onRestart()}
          >
            {restarting && <Spinner />}
            Restart
          </button>
        </div>
      </section>
      {server?.state === "failed" && (
        <div className="banner error" role="alert">
          <strong>Service failed.</strong>
          <pre>{server.error}</pre>
        </div>
      )}

      {snap.load_error && (
        <div className="banner error" role="alert">
          <strong>Cannot load config.</strong> Editing is disabled until the file is fixed.
          <pre>{snap.load_error}</pre>
          <button type="button" onClick={() => void onReload()}>
            Retry
          </button>
        </div>
      )}
      {error && (
        <p className="banner error" role="alert">
          {error}
        </p>
      )}
      {notice && !error && (
        <p key={notice.id} className="banner ok fade" role="status">
          {notice.text}
        </p>
      )}

      {!locked && snap.entries.length === 0 && (
        <p className="empty">No MCP servers configured yet.</p>
      )}

      <ul className="servers">
        {snap.entries.map((entry) => (
          <li key={entry.name} className={entry.server.enabled ? "" : "off"}>
            <div className="info">
              <div className="title">
                <strong>{entry.name}</strong>
                <span className="tag">{entry.server.scope}</span>
                {entry.status !== "unchanged" && (
                  <span className={`tag ${entry.status}`}>{entry.status}</span>
                )}
              </div>
              <code title={summary(entry.server)}>{summary(entry.server)}</code>
            </div>
            <div className="controls">
              <button type="button" onClick={() => setDialog({ kind: "edit", entry })} disabled={locked}>
                Edit
              </button>
              <button
                type="button"
                className="danger"
                onClick={() => setDialog({ kind: "delete", name: entry.name })}
                disabled={locked}
              >
                Delete
              </button>
              <button
                type="button"
                role="switch"
                aria-checked={entry.server.enabled}
                aria-label={`${entry.server.enabled ? "Disable" : "Enable"} ${entry.name}`}
                className="switch"
                disabled={locked}
                onClick={() =>
                  void run(() => api.setEnabled(entry.name, !entry.server.enabled))
                }
              />
            </div>
          </li>
        ))}
      </ul>

      {(dialog?.kind === "add" || dialog?.kind === "edit") && (
        <ServerDialog
          editing={dialog.kind === "edit" ? dialog.entry : null}
          onCancel={() => setDialog(null)}
          onSubmit={async (name, server) => {
            const next = await api.upsertServer(
              dialog.kind === "edit" ? dialog.entry.name : null,
              name,
              server,
            );
            setSnap(next);
            setError(null);
            setNotice(null);
            setDialog(null);
          }}
        />
      )}
      {dialog?.kind === "delete" && (
        <ConfirmDialog
          title={`Delete ${dialog.name}?`}
          message="The server is removed from the draft. The config file changes only when you press Save."
          confirmLabel="Delete"
          onCancel={() => setDialog(null)}
          onConfirm={() => {
            const name = dialog.name;
            setDialog(null);
            void run(() => api.deleteServer(name));
          }}
        />
      )}
      {dialog?.kind === "discard" && (
        <ConfirmDialog
          title="Discard unsaved changes?"
          message="The draft is replaced with the contents of the config file."
          confirmLabel="Discard"
          onCancel={() => setDialog(null)}
          onConfirm={() => {
            setDialog(null);
            void onReload();
          }}
        />
      )}
      {dialog?.kind === "update" && (
        <UpdateDialog update={dialog.update} onClose={() => setDialog(null)} />
      )}
    </main>
  );
}
