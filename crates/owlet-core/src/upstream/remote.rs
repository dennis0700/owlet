//! Shared helpers for HTTP-based upstream transports.

use std::{borrow::Cow, collections::HashMap, time::Duration};

use anyhow::Context;
use reqwest::{
    Client,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::Value;

use super::Reader;
use crate::jsonrpc::{self, Kind};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Builds a client that sends the configured headers on every request.
/// Header values are marked sensitive so they never show up in debug output.
pub fn client(headers: &HashMap<String, String>) -> anyhow::Result<Client> {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .default_headers(header_map(headers)?)
        .build()
        .context("build HTTP client")
}

fn header_map(headers: &HashMap<String, String>) -> anyhow::Result<HeaderMap> {
    let mut map = HeaderMap::with_capacity(headers.len());
    for (k, v) in headers {
        let name = HeaderName::from_bytes(k.as_bytes())
            .with_context(|| format!("invalid header name `{k}`"))?;
        let mut value =
            HeaderValue::from_str(v).with_context(|| format!("invalid value for header `{k}`"))?;
        value.set_sensitive(true);
        map.insert(name, value);
    }
    Ok(map)
}

/// If `body` is a request, resolves it locally with a JSON-RPC error so the
/// caller does not wait for the full request timeout.
pub fn fail_request(reader: &Reader, body: &[u8], reason: &str) {
    let Ok(msg) = serde_json::from_slice::<Value>(body) else {
        return;
    };
    if jsonrpc::classify(&msg) != Kind::Request {
        return;
    }
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let resp = jsonrpc::error_response(id, jsonrpc::INTERNAL_ERROR, reason);
    reader
        .inner
        .dispatch(resp, &reader.on_notify, reader.root.as_deref());
}

/// One dispatched `text/event-stream` event.
#[derive(Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

#[derive(Default)]
struct PendingEvent {
    event: String,
    data: String,
    has_data: bool,
}

impl PendingEvent {
    fn line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if self.has_data {
                let event = if self.event.is_empty() {
                    "message".to_owned()
                } else {
                    std::mem::take(&mut self.event)
                };
                out.push(SseEvent {
                    event,
                    data: std::mem::take(&mut self.data),
                });
            }
            self.event.clear();
            self.has_data = false;
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => value.clone_into(&mut self.event),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            _ => {}
        }
    }
}

/// Incremental `text/event-stream` parser (LF and CRLF line endings).
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    pending: PendingEvent,
}

impl SseParser {
    /// Feeds a chunk and returns every event completed by it.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0;
        while let Some(pos) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let end = start + pos;
            let raw = &self.buf[start..end];
            let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
            let line: Cow<'_, str> = String::from_utf8_lossy(raw);
            self.pending.line(&line, &mut out);
            start = end + 1;
        }
        self.buf.drain(..start);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event: &str, data: &str) -> SseEvent {
        SseEvent {
            event: event.into(),
            data: data.into(),
        }
    }

    #[test]
    fn parses_events_across_chunks() {
        let mut p = SseParser::default();
        assert!(p.feed(b"event: endpoint\r\nda").is_empty());
        assert_eq!(
            p.feed(b"ta: /messages?sessionId=1\r\n\r\n: ping\n\n"),
            vec![ev("endpoint", "/messages?sessionId=1")]
        );
        assert_eq!(
            p.feed(b"data: {\"a\":\ndata:1}\n\nevent: message\ndata: x\n\n"),
            vec![ev("message", "{\"a\":\n1}"), ev("message", "x")]
        );
        // An event without data is dropped and does not leak its name.
        assert_eq!(
            p.feed(b"event: foo\n\ndata: y\n\n"),
            vec![ev("message", "y")]
        );
    }

    #[test]
    fn rejects_bad_headers() {
        let bad = HashMap::from([("bad header".to_owned(), "v".to_owned())]);
        assert!(header_map(&bad).is_err());
        let ok = HashMap::from([("Authorization".to_owned(), "Bearer x".to_owned())]);
        assert!(header_map(&ok).is_ok_and(|m| m.len() == 1));
    }
}
