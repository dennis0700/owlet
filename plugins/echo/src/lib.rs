//! Minimal owlet WASM plugin: a self-contained MCP server that echoes.
//!
//! It implements just enough of MCP to prove the owlet WASM host pipeline:
//! `initialize` handshake, `tools/list` (one `echo` tool) and `tools/call`
//! (returns the arguments it was given). Everything is plain JSON string in,
//! JSON string out, matching the `handle` function in `wit/mcp-plugin.wit`.

wit_bindgen::generate!({
    world: "mcp-plugin",
});

struct Plugin;

export!(Plugin);

impl Guest for Plugin {
    fn handle(request: String) -> String {
        reply(&request).unwrap_or_default()
    }

    /// Nothing to clean up.
    fn on_shutdown() {}
}

/// Parses `request` and builds the JSON-RPC reply, or `None` for notifications.
fn reply(request: &str) -> Option<String> {
    let msg: serde_json::Value = serde_json::from_str(request).ok()?;
    let method = msg.get("method")?.as_str()?;
    // Notifications carry no id and expect no response.
    let id = msg.get("id").cloned();
    let id = match id {
        Some(id) if !id.is_null() => id,
        _ => return None,
    };

    let result = match method {
        "initialize" => serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "owlet-plugin-echo", "version": "0.1.0" },
        }),
        "tools/list" => serde_json::json!({
            "tools": [{
                "name": "echo",
                "description": "Returns the arguments it is given.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "message": { "type": "string" } },
                },
            }],
        }),
        "tools/call" => {
            let args = msg
                .get("params")
                .and_then(|p| p.get("arguments"))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let text = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_owned());
            serde_json::json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            })
        }
        _ => {
            return Some(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("unknown method `{method}`") },
                })
                .to_string(),
            );
        }
    };

    Some(
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string(),
    )
}
