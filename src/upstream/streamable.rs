//! Client side of the MCP Streamable HTTP transport (2025-03-26 and later):
//! every message is POSTed to a single endpoint; the reply arrives either as a
//! JSON body or as a `text/event-stream` body carrying it.
//!
//! The optional server-to-client GET stream is not opened.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Context;
use parking_lot::Mutex;
use reqwest::{
    Client, Response, StatusCode, Url,
    header::{ACCEPT, CONTENT_TYPE, HeaderValue},
};
use serde_json::Value;
use tokio::{
    sync::watch,
    task::{AbortHandle, JoinSet},
};
use tracing::{info, warn};

use super::{
    Reader, Wiring,
    remote::{self, SseParser},
};
use crate::jsonrpc::{self, Kind};

const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";
const DELETE_TIMEOUT: Duration = Duration::from_secs(3);

/// State shared by all in-flight POSTs of one connection.
struct Conn {
    client: Client,
    url: Url,
    reader: Reader,
    session: Mutex<Option<HeaderValue>>,
    version: Mutex<Option<HeaderValue>>,
    /// Set when the server ends the session (404) or the connection breaks.
    closed: watch::Sender<bool>,
}

/// Starts the dispatcher; nothing is sent until the handshake request arrives.
pub fn start(url: &str, headers: &HashMap<String, String>, wiring: Wiring) -> anyhow::Result<()> {
    let Wiring {
        reader,
        mut outgoing,
        mut stop,
        exited,
    } = wiring;
    let url = Url::parse(url).with_context(|| format!("invalid url `{url}`"))?;
    let conn = Arc::new(Conn {
        client: remote::client(headers)?,
        url,
        reader,
        session: Mutex::new(None),
        version: Mutex::new(None),
        closed: watch::channel(false).0,
    });

    tokio::spawn(async move {
        let mut tasks = JoinSet::new();
        // Upstream request id -> POST task, so cancellation can abort it.
        let inflight: Arc<Mutex<HashMap<u64, AbortHandle>>> = Arc::default();
        let mut closed = conn.closed.subscribe();
        loop {
            tokio::select! {
                _ = &mut stop => break,
                _ = closed.wait_for(|c| *c) => break,
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                line = outgoing.recv() => {
                    let Some(line) = line else { break };
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                    if jsonrpc::method(&msg) == Some("notifications/cancelled")
                        && let Some(id) = msg.pointer("/params/requestId").and_then(Value::as_u64)
                        && let Some(task) = inflight.lock().remove(&id)
                    {
                        task.abort();
                    }
                    let request_id = (jsonrpc::classify(&msg) == Kind::Request)
                        .then(|| msg.get("id").and_then(Value::as_u64))
                        .flatten();
                    let conn = Arc::clone(&conn);
                    let map = Arc::clone(&inflight);
                    // Hold the lock across spawn so the task cannot remove its entry first.
                    let mut guard = inflight.lock();
                    let handle = tasks.spawn(async move {
                        conn.post(line, &msg).await;
                        if let Some(id) = request_id {
                            map.lock().remove(&id);
                        }
                    });
                    if let Some(id) = request_id {
                        guard.insert(id, handle);
                    }
                }
            }
        }
        tasks.shutdown().await;
        conn.reader.inner.mark_dead();
        conn.end_session().await;
        info!(server = %conn.reader.inner.name, "disconnected");
        let _ = exited.send(true);
    });
    Ok(())
}

impl Conn {
    async fn post(&self, body: String, msg: &Value) {
        let is_request = jsonrpc::classify(msg) == Kind::Request;
        let is_init = jsonrpc::method(msg) == Some("initialize");
        let name = &self.reader.inner.name;

        let mut req = self
            .client
            .post(self.url.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream");
        if let Some(session) = self.session.lock().clone() {
            req = req.header(SESSION_HEADER, session);
        }
        if let Some(version) = self.version.lock().clone() {
            req = req.header(VERSION_HEADER, version);
        }

        let resp = match req.body(body).send().await {
            Ok(resp) => resp,
            Err(e) => {
                warn!(server = %name, "POST {} failed: {e}", self.url);
                self.fail(msg, &e.to_string());
                return;
            }
        };
        let status = resp.status();
        if status == StatusCode::NOT_FOUND && self.session.lock().is_some() {
            warn!(server = %name, "session expired; will reconnect on next use");
            self.fail(msg, "upstream session expired");
            self.closed.send_replace(true);
            return;
        }
        if !status.is_success() {
            let reason = format!("upstream returned HTTP {status}");
            warn!(server = %name, "{reason}");
            self.fail(msg, &reason);
            if is_init {
                self.closed.send_replace(true);
            }
            return;
        }
        if is_init && let Some(session) = resp.headers().get(SESSION_HEADER) {
            *self.session.lock() = Some(session.clone());
        }
        if status == StatusCode::ACCEPTED || !is_request {
            return;
        }

        let sse = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        let result = if sse {
            self.read_events(resp, is_init).await
        } else {
            self.read_json(resp, is_init).await
        };
        if let Err(e) = result {
            warn!(server = %name, "reading response failed: {e:#}");
        }
        // No-op if the response was already delivered.
        self.fail(msg, "upstream ended the response without a reply");
    }

    async fn read_json(&self, resp: Response, is_init: bool) -> anyhow::Result<()> {
        let bytes = resp.bytes().await.context("read body")?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let value: Value = serde_json::from_slice(&bytes).context("parse JSON body")?;
        let messages = match value {
            Value::Array(items) => items,
            single => vec![single],
        };
        for msg in messages {
            self.deliver(msg, is_init);
        }
        Ok(())
    }

    async fn read_events(&self, mut resp: Response, is_init: bool) -> anyhow::Result<()> {
        let mut parser = SseParser::default();
        while let Some(chunk) = resp.chunk().await.context("read event stream")? {
            for event in parser.feed(&chunk) {
                if event.event != "message" {
                    continue;
                }
                match serde_json::from_str::<Value>(&event.data) {
                    Ok(msg) => self.deliver(msg, is_init),
                    Err(e) => warn!(server = %self.reader.inner.name, "bad event data ({e})"),
                }
            }
        }
        Ok(())
    }

    fn deliver(&self, msg: Value, is_init: bool) {
        if is_init
            && let Some(v) = msg
                .pointer("/result/protocolVersion")
                .and_then(Value::as_str)
            && let Ok(v) = HeaderValue::from_str(v)
        {
            *self.version.lock() = Some(v);
        }
        let r = &self.reader;
        r.inner.dispatch(msg, &r.on_notify, r.root.as_deref());
    }

    fn fail(&self, msg: &Value, reason: &str) {
        if let Ok(body) = serde_json::to_vec(msg) {
            remote::fail_request(&self.reader, &body, reason);
        }
    }

    /// Best-effort `DELETE` so the server can free the session.
    async fn end_session(&self) {
        let Some(session) = self.session.lock().take() else {
            return;
        };
        if *self.closed.borrow() {
            // Server already forgot it.
            return;
        }
        let result = self
            .client
            .delete(self.url.clone())
            .header(SESSION_HEADER, session)
            .timeout(DELETE_TIMEOUT)
            .send()
            .await;
        if let Err(e) = result {
            warn!(server = %self.reader.inner.name, "DELETE session failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        routing::post,
    };
    use parking_lot::Mutex;
    use serde_json::{Value, json};

    use super::super::{SpawnSpec, Transport, Upstream};
    use crate::jsonrpc;

    /// (method, session header, protocol-version header) per POST.
    type Seen = Vec<(String, Option<String>, Option<String>)>;

    /// Minimal Streamable HTTP server. `tools/call` answers via SSE (after a
    /// server-initiated ping), everything else via JSON. `expire` forgets sessions.
    #[derive(Clone, Default)]
    struct Fake {
        sessions: Arc<Mutex<Vec<String>>>,
        counter: Arc<AtomicUsize>,
        seen: Arc<Mutex<Seen>>,
        deleted: Arc<Mutex<Vec<String>>>,
    }

    fn header(h: &HeaderMap, k: &str) -> Option<String> {
        h.get(k).and_then(|v| v.to_str().ok()).map(str::to_owned)
    }

    async fn mcp(State(f): State<Fake>, h: HeaderMap, body: String) -> Response {
        let m: Value = serde_json::from_str(&body).unwrap_or_default();
        let method = jsonrpc::method(&m).unwrap_or("<response>").to_owned();
        let session = header(&h, "mcp-session-id");
        f.seen.lock().push((
            method.clone(),
            session.clone(),
            header(&h, "mcp-protocol-version"),
        ));

        if method == "initialize" {
            let id = format!("s{}", f.counter.fetch_add(1, Ordering::SeqCst));
            f.sessions.lock().push(id.clone());
            let r = json!({"jsonrpc": "2.0", "id": m["id"], "result": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "serverInfo": {"name": "fake-http", "version": "1"}}});
            return ([("mcp-session-id", id)], axum::Json(r)).into_response();
        }
        if !session.is_some_and(|s| f.sessions.lock().contains(&s)) {
            return StatusCode::NOT_FOUND.into_response();
        }
        match (method.as_str(), m.get("id")) {
            ("tools/call", Some(id)) => {
                let ping = json!({"jsonrpc": "2.0", "id": "srv", "method": "ping"});
                let r = json!({"jsonrpc": "2.0", "id": id, "result": m["params"]});
                let body = format!(": hi\n\nevent: message\ndata: {ping}\n\ndata: {r}\n\n");
                ([("content-type", "text/event-stream")], body).into_response()
            }
            ("expire", Some(_)) => {
                f.sessions.lock().clear();
                StatusCode::NOT_FOUND.into_response()
            }
            ("tools/list", Some(id)) => {
                axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": {"tools": []}}))
                    .into_response()
            }
            _ => StatusCode::ACCEPTED.into_response(),
        }
    }

    async fn delete(State(f): State<Fake>, h: HeaderMap) -> StatusCode {
        if let Some(s) = header(&h, "mcp-session-id") {
            f.deleted.lock().push(s);
        }
        StatusCode::NO_CONTENT
    }

    async fn serve() -> (String, Fake) {
        let fake = Fake::default();
        let app = Router::new()
            .route("/mcp", post(mcp).delete(delete))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/mcp"), fake)
    }

    fn spec(url: String) -> SpawnSpec {
        SpawnSpec {
            name: "remote".into(),
            transport: Transport::StreamableHttp {
                url,
                headers: Default::default(),
            },
            root: None,
        }
    }

    async fn call(up: &Upstream, method: &str) -> Value {
        up.request(json!({"jsonrpc": "2.0", "id": "c", "method": method,
            "params": {"name": "echo", "arguments": {"x": 1}}}))
            .expect("request")
            .wait(std::time::Duration::from_secs(5))
            .await
            .expect("response")
    }

    #[tokio::test]
    async fn json_and_sse_responses_with_session() {
        let (url, fake) = serve().await;
        let (up, init) = Upstream::spawn(&spec(url), Arc::new(|_| {}))
            .await
            .expect("connect");
        assert_eq!(init["serverInfo"]["name"], "fake-http");

        assert_eq!(call(&up, "tools/list").await["result"]["tools"], json!([]));
        assert_eq!(call(&up, "tools/call").await["result"]["arguments"]["x"], 1);

        // owlet answers the server's ping itself, on a separate POST.
        let answered = || fake.seen.lock().iter().any(|(m, _, _)| m == "<response>");
        for _ in 0..50 {
            if answered() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(answered());

        {
            let seen = fake.seen.lock();
            // initialize carries no session or version; everything after carries both.
            assert_eq!(seen[0], ("initialize".into(), None, None));
            assert!(
                seen[1..]
                    .iter()
                    .all(|(_, s, v)| s.as_deref() == Some("s0")
                        && v.as_deref() == Some("2025-06-18"))
            );
            assert!(
                seen.iter()
                    .any(|(m, _, _)| m == "notifications/initialized")
            );
        }

        up.shutdown().await;
        assert!(!up.is_alive());
        assert_eq!(fake.deleted.lock().as_slice(), ["s0"]);
    }

    #[tokio::test]
    async fn expired_session_marks_connection_dead() {
        let (url, fake) = serve().await;
        let (up, _) = Upstream::spawn(&spec(url.clone()), Arc::new(|_| {}))
            .await
            .expect("connect");
        let resp = call(&up, "expire").await;
        assert_eq!(resp["error"]["code"], jsonrpc::INTERNAL_ERROR);
        up.shutdown().await;
        assert!(!up.is_alive());
        // The server already dropped the session, so no DELETE is sent.
        assert!(fake.deleted.lock().is_empty());

        // A fresh connection gets a new session.
        let (up, _) = Upstream::spawn(&spec(url), Arc::new(|_| {}))
            .await
            .expect("reconnect");
        assert_eq!(call(&up, "tools/list").await["result"]["tools"], json!([]));
        up.shutdown().await;
        assert_eq!(fake.deleted.lock().as_slice(), ["s1"]);
    }
}
