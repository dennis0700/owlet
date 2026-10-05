//! Client side of the legacy MCP HTTP+SSE transport (protocol 2024-11-05):
//! `GET <url>` opens an event stream whose first `endpoint` event names the URL
//! that client messages are POSTed to; every server message arrives as a
//! `message` event on that stream.

use std::{collections::HashMap, time::Duration};

use anyhow::{Context, bail};
use axum::body::Bytes;
use reqwest::{
    Client, Response, Url,
    header::{ACCEPT, CONTENT_TYPE},
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use super::{
    Reader, Wiring,
    remote::{self, SseParser},
};
const ENDPOINT_TIMEOUT: Duration = Duration::from_secs(30);
const POST_TIMEOUT: Duration = Duration::from_secs(30);

/// Opens the event stream, waits for the `endpoint` event and starts the IO tasks.
pub async fn start(
    name: &str,
    url: &str,
    headers: &HashMap<String, String>,
    wiring: Wiring,
) -> anyhow::Result<()> {
    let Wiring {
        reader,
        outgoing,
        stop,
        exited,
    } = wiring;
    let base = Url::parse(url).with_context(|| format!("invalid url `{url}`"))?;
    let client = remote::client(headers)?;

    let resp = client
        .get(base.clone())
        .header(ACCEPT, "text/event-stream")
        .send()
        .await
        .with_context(|| format!("connect {base}"))?
        .error_for_status()
        .with_context(|| format!("connect {base}"))?;

    let (endpoint_tx, endpoint_rx) = oneshot::channel();
    let mut read_task = tokio::spawn(read_loop(reader.clone(), resp, endpoint_tx));
    let endpoint = match tokio::time::timeout(ENDPOINT_TIMEOUT, endpoint_rx).await {
        Ok(Ok(raw)) => resolve_endpoint(&base, &raw),
        Ok(Err(_)) => Err(anyhow::anyhow!("stream closed before `endpoint` event")),
        Err(_) => Err(anyhow::anyhow!(
            "no `endpoint` event within {ENDPOINT_TIMEOUT:?}"
        )),
    };
    let endpoint = match endpoint {
        Ok(e) => e,
        Err(e) => {
            read_task.abort();
            return Err(e.context(format!("connect {base}")));
        }
    };
    info!(server = %name, "connected to {base}, posting to {endpoint}");

    let write_task = tokio::spawn(write_loop(client, endpoint, outgoing, reader.clone()));
    tokio::spawn(async move {
        tokio::select! {
            // Shutdown requested or handle dropped.
            _ = stop => {}
            // Server closed the stream; the slot reconnects on next use.
            _ = &mut read_task => {}
        }
        read_task.abort();
        write_task.abort();
        reader.inner.mark_dead();
        info!(server = %reader.inner.name, "disconnected");
        let _ = exited.send(true);
    });
    Ok(())
}

/// Resolves the `endpoint` event against the stream URL. Cross-origin
/// endpoints are rejected so configured credentials never leave the origin.
fn resolve_endpoint(base: &Url, raw: &str) -> anyhow::Result<Url> {
    let endpoint = base
        .join(raw.trim())
        .with_context(|| format!("invalid endpoint `{raw}`"))?;
    if endpoint.origin() != base.origin() {
        bail!("endpoint `{endpoint}` is not on the same origin as `{base}`");
    }
    Ok(endpoint)
}

async fn read_loop(reader: Reader, mut resp: Response, endpoint_tx: oneshot::Sender<String>) {
    let mut parser = SseParser::default();
    let mut endpoint_tx = Some(endpoint_tx);
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                for event in parser.feed(&chunk) {
                    match event.event.as_str() {
                        "endpoint" => {
                            if let Some(tx) = endpoint_tx.take() {
                                let _ = tx.send(event.data);
                            }
                        }
                        "message" => reader.dispatch_line(&event.data),
                        other => {
                            debug!(server = %reader.inner.name, "ignoring SSE event `{other}`")
                        }
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                warn!(server = %reader.inner.name, "event stream error: {e}");
                break;
            }
        }
    }
    reader.inner.mark_dead();
}

/// POSTs messages in order. A failed POST of a request is turned into a
/// JSON-RPC error so the caller does not wait for the full request timeout.
async fn write_loop(
    client: Client,
    endpoint: Url,
    mut rx: mpsc::UnboundedReceiver<String>,
    reader: Reader,
) {
    while let Some(body) = rx.recv().await {
        let body = Bytes::from(body);
        let result = client
            .post(endpoint.clone())
            .header(CONTENT_TYPE, "application/json")
            .timeout(POST_TIMEOUT)
            .body(body.clone())
            .send()
            .await
            .and_then(Response::error_for_status);
        if let Err(e) = result {
            warn!(server = %reader.inner.name, "POST {endpoint} failed: {e}");
            remote::fail_request(&reader, &body, &e.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc;

    #[test]
    fn endpoint_must_stay_on_origin() {
        let base = Url::parse("http://127.0.0.1:9000/sse").expect("url");
        assert_eq!(
            resolve_endpoint(&base, "/messages?sessionId=a")
                .expect("relative")
                .as_str(),
            "http://127.0.0.1:9000/messages?sessionId=a"
        );
        assert!(resolve_endpoint(&base, "http://127.0.0.1:9000/m").is_ok());
        assert!(resolve_endpoint(&base, "http://evil.example/m").is_err());
        assert!(resolve_endpoint(&base, "http://127.0.0.1:9001/m").is_err());
    }

    /// Minimal legacy HTTP+SSE MCP server: `GET /sse` + `POST /messages`.
    mod fake_server {
        use std::{convert::Infallible, sync::Arc};

        use axum::{
            Json, Router,
            extract::State,
            http::{HeaderMap, StatusCode},
            response::sse::{Event, Sse},
            routing::{get, post},
        };
        use futures_util::stream;
        use parking_lot::Mutex;
        use serde_json::{Value, json};
        use tokio::sync::mpsc;

        #[derive(Clone, Default)]
        pub struct Fake {
            tx: Arc<Mutex<Option<mpsc::UnboundedSender<Value>>>>,
            pub auth: Arc<Mutex<Vec<String>>>,
        }

        async fn sse(
            State(f): State<Fake>,
            headers: HeaderMap,
        ) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            f.auth.lock().push(auth.to_owned());
            let (tx, rx) = mpsc::unbounded_channel();
            *f.tx.lock() = Some(tx);
            let first = stream::once(async {
                Ok(Event::default()
                    .event("endpoint")
                    .data("/messages?sessionId=abc"))
            });
            let rest = stream::unfold(rx, |mut rx| async move {
                let v = rx.recv().await?;
                Some((
                    Ok(Event::default().event("message").data(v.to_string())),
                    rx,
                ))
            });
            Sse::new(futures_util::StreamExt::chain(first, rest))
        }

        async fn messages(State(f): State<Fake>, Json(m): Json<Value>) -> StatusCode {
            let Some(tx) = f.tx.lock().clone() else {
                return StatusCode::NOT_FOUND;
            };
            let id = m.get("id").cloned();
            let reply = match (m.get("method").and_then(Value::as_str), id) {
                (Some("initialize"), Some(id)) => Some(json!({"jsonrpc": "2.0", "id": id,
                    "result": {"protocolVersion": "2024-11-05", "capabilities": {},
                               "serverInfo": {"name": "fake-sse", "version": "1"}}})),
                (Some("tools/call"), Some(id)) => {
                    // Server-initiated request first; owlet must answer it itself.
                    let _ = tx.send(json!({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"}));
                    Some(json!({"jsonrpc": "2.0", "id": id, "result": m["params"].clone()}))
                }
                (Some("boom"), Some(_)) => return StatusCode::INTERNAL_SERVER_ERROR,
                _ => None,
            };
            if let Some(r) = reply {
                let _ = tx.send(r);
            }
            StatusCode::ACCEPTED
        }

        pub async fn serve() -> (String, Fake) {
            let fake = Fake::default();
            let app = Router::new()
                .route("/sse", get(sse))
                .route("/messages", post(messages))
                .with_state(fake.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            (format!("http://{addr}/sse"), fake)
        }
    }

    #[tokio::test]
    async fn talks_to_legacy_sse_server() {
        use super::super::{SpawnSpec, Transport, Upstream};
        use serde_json::json;
        use std::sync::Arc;

        let (url, fake) = fake_server::serve().await;
        let spec = SpawnSpec {
            name: "remote".into(),
            transport: Transport::Sse {
                url,
                headers: HashMap::from([("Authorization".into(), "Bearer up".into())]),
            },
            root: None,
        };
        let (up, init) = Upstream::spawn(&spec, Arc::new(|_| {}))
            .await
            .expect("connect");
        assert_eq!(init["serverInfo"]["name"], "fake-sse");
        assert_eq!(fake.auth.lock().as_slice(), ["Bearer up"]);
        assert!(up.pid().is_none());

        let req = json!({"jsonrpc": "2.0", "id": "c1", "method": "tools/call",
            "params": {"name": "echo", "arguments": {"x": 1}}});
        let resp = up
            .request(req)
            .expect("request")
            .wait(Duration::from_secs(5))
            .await
            .expect("response");
        assert_eq!(resp["result"]["arguments"]["x"], 1);

        // A failed POST surfaces as a JSON-RPC error right away.
        let resp = up
            .request(json!({"jsonrpc": "2.0", "id": 9, "method": "boom"}))
            .expect("request")
            .wait(Duration::from_secs(5))
            .await
            .expect("error response");
        assert_eq!(resp["error"]["code"], jsonrpc::INTERNAL_ERROR);

        up.shutdown().await;
        assert!(!up.is_alive());
    }
}
