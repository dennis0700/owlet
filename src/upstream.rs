mod remote;
mod sse;
mod stdio;
mod streamable;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, bail};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::warn;

use crate::jsonrpc::{self, Kind};

const PROTOCOL_VERSION: &str = "2025-06-18";
const INIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Callback for server-initiated notifications (progress notifications are dropped).
pub type NotifyHandler = Arc<dyn Fn(Value) + Send + Sync>;

/// How owlet reaches an upstream MCP server.
#[derive(Debug, Clone)]
pub enum Transport {
    /// Launch a local process and speak newline-delimited JSON-RPC over stdio.
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<PathBuf>,
    },
    /// Connect to a remote server using Streamable HTTP (2025-03-26+).
    StreamableHttp {
        url: String,
        headers: HashMap<String, String>,
    },
    /// Connect to a remote server using the legacy HTTP+SSE transport (2024-11-05).
    Sse {
        url: String,
        headers: HashMap<String, String>,
    },
}

/// Everything needed to start one upstream connection.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub name: String,
    pub transport: Transport,
    /// Exposed to the server via `roots/list` when set.
    pub root: Option<PathBuf>,
}

/// What a transport needs to hand incoming messages to the shared core.
#[derive(Clone)]
struct Reader {
    inner: Arc<Inner>,
    on_notify: NotifyHandler,
    root: Option<PathBuf>,
}

/// Channels a transport uses to talk to the shared core.
struct Wiring {
    reader: Reader,
    /// Serialized JSON-RPC messages to deliver upstream.
    outgoing: mpsc::UnboundedReceiver<String>,
    /// Fires on shutdown or when the [`Upstream`] handle is dropped.
    stop: oneshot::Receiver<()>,
    /// Set to `true` once the transport has fully stopped.
    exited: watch::Sender<bool>,
}

impl Reader {
    fn dispatch_line(&self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(msg) => self
                .inner
                .dispatch(msg, &self.on_notify, self.root.as_deref()),
            Err(e) => warn!(server = %self.inner.name, "non-JSON message ({e}): {line}"),
        }
    }
}

/// State shared between the handle, the transport tasks and in-flight call guards.
struct Inner {
    name: String,
    tx: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    alive: AtomicBool,
}

impl Inner {
    fn send(&self, msg: &Value) -> bool {
        match serde_json::to_string(msg) {
            Ok(line) => self.tx.send(line).is_ok(),
            Err(_) => false,
        }
    }

    fn mark_dead(&self) {
        self.alive.store(false, Ordering::SeqCst);
        // Dropping the senders wakes every waiter with an error.
        self.pending.lock().clear();
    }

    fn take_pending(&self, id: u64) -> Option<oneshot::Sender<Value>> {
        self.pending.lock().remove(&id)
    }

    fn cancel(&self, id: u64, reason: &str) {
        if self.take_pending(id).is_some() && self.alive.load(Ordering::SeqCst) {
            self.send(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": { "requestId": id, "reason": reason },
            }));
        }
    }

    fn dispatch(&self, msg: Value, on_notify: &NotifyHandler, root: Option<&Path>) {
        match jsonrpc::classify(&msg) {
            Kind::Response => {
                let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                    warn!(server = %self.name, "response with foreign id: {msg}");
                    return;
                };
                if let Some(tx) = self.take_pending(id) {
                    // Receiver may be gone (cancelled); nothing to do then.
                    let _ = tx.send(msg);
                }
            }
            // No streaming channel to the client, so progress has nowhere to go.
            Kind::Notification if jsonrpc::method(&msg) == Some("notifications/progress") => {}
            Kind::Notification => on_notify(msg),
            Kind::Request => {
                let reply = answer_server_request(&msg, root);
                self.send(&reply);
            }
            Kind::Invalid => warn!(server = %self.name, "invalid JSON-RPC message: {msg}"),
        }
    }
}

/// Server-to-client requests are answered by owlet itself; they cannot be
/// routed to a particular client when the connection is shared.
fn answer_server_request(msg: &Value, root: Option<&Path>) -> Value {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    match jsonrpc::method(msg).unwrap_or_default() {
        "ping" => jsonrpc::result_response(id, json!({})),
        "roots/list" => {
            let roots: Vec<Value> = root
                .map(|p| {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
                    json!({ "uri": file_uri(p), "name": name })
                })
                .into_iter()
                .collect();
            jsonrpc::result_response(id, json!({ "roots": roots }))
        }
        other => jsonrpc::error_response(
            id,
            jsonrpc::METHOD_NOT_FOUND,
            &format!("owlet does not forward `{other}` to clients"),
        ),
    }
}

fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            uri.push(char::from(b));
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

/// A live upstream connection that has completed the `initialize` handshake.
pub struct Upstream {
    inner: Arc<Inner>,
    pid: Option<u32>,
    next_id: AtomicU64,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    exited: watch::Receiver<bool>,
}

impl Upstream {
    /// Starts the transport and performs the MCP handshake, returning the
    /// server's `initialize` result.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let (up, init) = Upstream::spawn(&spec, Arc::new(|_| {})).await?;
    /// ```
    pub async fn spawn(
        spec: &SpawnSpec,
        on_notify: NotifyHandler,
    ) -> anyhow::Result<(Arc<Self>, Value)> {
        let (tx, outgoing) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            name: spec.name.clone(),
            tx,
            pending: Mutex::new(HashMap::new()),
            alive: AtomicBool::new(true),
        });
        let (stop_tx, stop) = oneshot::channel();
        let (exited_tx, exited) = watch::channel(false);
        let wiring = Wiring {
            reader: Reader {
                inner: Arc::clone(&inner),
                on_notify,
                root: spec.root.clone(),
            },
            outgoing,
            stop,
            exited: exited_tx,
        };

        let pid = match &spec.transport {
            Transport::Stdio {
                command,
                args,
                env,
                cwd,
            } => stdio::start(&spec.name, command, args, env, cwd.as_deref(), wiring)?,
            Transport::StreamableHttp { url, headers } => {
                streamable::start(url, headers, wiring)?;
                None
            }
            Transport::Sse { url, headers } => {
                sse::start(&spec.name, url, headers, wiring).await?;
                None
            }
        };

        let up = Arc::new(Self {
            inner,
            pid,
            next_id: AtomicU64::new(1),
            stop: Mutex::new(Some(stop_tx)),
            exited,
        });
        match up.handshake(spec.root.is_some()).await {
            Ok(init) => Ok((up, init)),
            Err(e) => {
                up.shutdown().await;
                Err(e)
            }
        }
    }

    async fn handshake(&self, with_roots: bool) -> anyhow::Result<Value> {
        let capabilities = if with_roots {
            json!({ "roots": { "listChanged": false } })
        } else {
            json!({})
        };
        let req = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": capabilities,
                "clientInfo": { "name": "owlet", "version": env!("CARGO_PKG_VERSION") },
            },
        });
        let mut resp = self
            .request(req)?
            .wait(INIT_TIMEOUT)
            .await
            .context("initialize")?;
        if let Some(err) = resp.get("error") {
            bail!("initialize rejected: {err}");
        }
        let result = resp
            .get_mut("result")
            .map(Value::take)
            .context("initialize response has no result")?;
        self.notify(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        Ok(result)
    }

    /// Sends a request under a fresh upstream id. Any `progressToken` is
    /// stripped because progress cannot be delivered without a stream.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let resp = up.request(json!({"jsonrpc":"2.0","id":"x","method":"tools/list"}))?
    ///     .wait(Duration::from_secs(30)).await?;
    /// ```
    pub fn request(&self, mut msg: Value) -> anyhow::Result<PendingCall> {
        if !self.is_alive() {
            bail!("upstream `{}` is not running", self.inner.name);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let obj = msg
            .as_object_mut()
            .context("request must be a JSON object")?;
        obj.insert("id".to_owned(), id.into());
        if let Some(meta) = msg
            .pointer_mut("/params/_meta")
            .and_then(Value::as_object_mut)
        {
            meta.remove("progressToken");
        }

        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().insert(id, tx);
        let call = PendingCall {
            inner: Arc::clone(&self.inner),
            id,
            rx,
            done: false,
        };
        if !self.inner.send(&msg) {
            bail!("upstream `{}` connection closed", self.inner.name);
        }
        Ok(call)
    }

    /// Sends a notification to the server.
    pub fn notify(&self, msg: &Value) {
        self.inner.send(msg);
    }

    /// Cancels an in-flight request by its upstream id.
    pub fn cancel(&self, upstream_id: u64, reason: &str) {
        self.inner.cancel(upstream_id, reason);
    }

    pub fn is_alive(&self) -> bool {
        self.inner.alive.load(Ordering::SeqCst)
    }

    /// Process id for stdio upstreams; `None` for remote ones.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Stops the transport (kills the process group / closes the stream) and waits.
    pub async fn shutdown(&self) {
        let stop = self.stop.lock().take();
        if let Some(stop) = stop {
            let _ = stop.send(());
        }
        let mut exited = self.exited.clone();
        let _ = exited.wait_for(|done| *done).await;
    }
}

/// An outstanding upstream request. Dropping it before completion sends
/// `notifications/cancelled` upstream (client disconnect, timeout).
pub struct PendingCall {
    inner: Arc<Inner>,
    id: u64,
    rx: oneshot::Receiver<Value>,
    done: bool,
}

impl PendingCall {
    pub fn upstream_id(&self) -> u64 {
        self.id
    }

    /// Waits for the response (with the upstream id still in place).
    pub async fn wait(mut self, timeout: Duration) -> anyhow::Result<Value> {
        match tokio::time::timeout(timeout, &mut self.rx).await {
            Ok(Ok(resp)) => {
                self.done = true;
                Ok(resp)
            }
            Ok(Err(_)) => {
                self.done = true;
                bail!("upstream `{}` closed before responding", self.inner.name)
            }
            Err(_) => bail!("upstream `{}` timed out after {timeout:?}", self.inner.name),
        }
    }
}

impl Drop for PendingCall {
    fn drop(&mut self) {
        if !self.done {
            self.inner.cancel(self.id, "cancelled by owlet");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_server_requests_locally() {
        let root = Path::new("/tmp/my proj");
        let r = answer_server_request(&json!({"id": 3, "method": "roots/list"}), Some(root));
        assert_eq!(r["result"]["roots"][0]["uri"], "file:///tmp/my%20proj");
        assert_eq!(r["result"]["roots"][0]["name"], "my proj");

        let r = answer_server_request(&json!({"id": 4, "method": "roots/list"}), None);
        assert_eq!(r["result"]["roots"], json!([]));

        let r = answer_server_request(&json!({"id": 5, "method": "sampling/createMessage"}), None);
        assert_eq!(r["error"]["code"], jsonrpc::METHOD_NOT_FOUND);
        assert_eq!(r["id"], 5);
    }

    #[tokio::test]
    async fn rewrites_id_and_strips_progress_token() {
        // Echoes each request's params back as the result.
        let script = r#"
import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if "id" in m and "method" in m:
        r = {"protocolVersion": "x"} if m["method"] == "initialize" else m.get("params")
        print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#;
        let spec = SpawnSpec {
            name: "echo".into(),
            transport: Transport::Stdio {
                command: "python3".into(),
                args: vec!["-u".into(), "-c".into(), script.into()],
                env: HashMap::new(),
                cwd: None,
            },
            root: None,
        };
        let (up, init) = Upstream::spawn(&spec, Arc::new(|_| {}))
            .await
            .expect("spawn python3");
        assert_eq!(init["protocolVersion"], "x");

        let req = json!({"jsonrpc": "2.0", "id": "client-7", "method": "tools/call",
            "params": {"name": "t", "_meta": {"progressToken": "p", "keep": 1}}});
        let call = up.request(req).expect("request");
        let upstream_id = call.upstream_id();
        let resp = call.wait(Duration::from_secs(10)).await.expect("response");
        assert_eq!(resp["id"], upstream_id);
        assert_eq!(resp["result"]["_meta"], json!({"keep": 1}));

        up.shutdown().await;
        assert!(!up.is_alive());
        assert!(up.request(json!({"method": "ping"})).is_err());
    }
}
