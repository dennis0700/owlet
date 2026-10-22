//! Parses a complete `text/event-stream` body into JSON-RPC messages.

use serde_json::Value;

/// Returns the JSON payload of every `message` event, re-serialized on one
/// line. Multi-line `data:` fields are joined with `\n` as the SSE spec says;
/// comments, other event types and non-JSON payloads are skipped.
pub fn messages(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut event = String::new();
    let mut data: Vec<&str> = Vec::new();
    // A trailing empty line dispatches the last event even without a final blank line.
    for line in body.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if !data.is_empty()
                && (event.is_empty() || event == "message")
                && let Ok(msg) = serde_json::from_str::<Value>(&data.join("\n"))
            {
                out.push(msg.to_string());
            }
            event.clear();
            data.clear();
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => value.clone_into(&mut event),
            "data" => data.push(value),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_to_lines() {
        let body = ": comment\r\n\r\nevent: message\r\ndata: {\"id\":1,\r\ndata:  \"result\":{}}\r\n\r\n\
                    event: ping\ndata: {\"x\":1}\n\nid: 7\ndata: [1, 2]\n\ndata: not json\n\ndata:{\"id\":2}";
        assert_eq!(
            messages(body),
            ["{\"id\":1,\"result\":{}}", "[1,2]", "{\"id\":2}"]
        );
        assert!(messages("").is_empty());
        assert!(messages("event: message\n\n").is_empty());
    }
}
