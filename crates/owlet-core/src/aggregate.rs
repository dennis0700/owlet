use std::{collections::HashMap, sync::Arc, time::Duration, time::Instant};

use futures_util::future::join_all;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tracing::warn;

use crate::{
    hub::{Hub, HubError, Session},
    jsonrpc,
};

/// Joins a server name and a tool name: `<server>__<tool>`.
const SEP: &str = "__";
const PROTOCOL_VERSION: &str = "2025-06-18";
/// Upper bound for listing one server, so a slow server cannot stall the whole list.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PAGES: usize = 20;

type SubSlot = Arc<tokio::sync::Mutex<Option<Arc<Session>>>>;

/// A client connection to the aggregate endpoint. It lazily opens one hub
/// session per upstream server the client actually touches.
pub struct AggSession {
    pub id: String,
    project: Option<String>,
    last_seen: Mutex<Instant>,
    subs: Mutex<HashMap<String, SubSlot>>,
}

impl AggSession {
    fn live_subs(&self) -> Vec<Arc<Session>> {
        self.subs
            .lock()
            .values()
            .filter_map(|cell| cell.try_lock().ok().and_then(|g| (*g).clone()))
            .collect()
    }
}

/// Presents every enabled server behind one MCP endpoint. Tools are exposed as
/// `<server>__<tool>`; calls are routed back to the owning server.
pub struct Aggregator {
    hub: Arc<Hub>,
    sessions: Mutex<HashMap<String, Arc<AggSession>>>,
}

impl Aggregator {
    /// Creates an aggregator on top of `hub`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let agg = Aggregator::new(Arc::clone(&hub));
    /// ```
    pub fn new(hub: Arc<Hub>) -> Self {
        Self {
            hub,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Handles a client `initialize`. No upstream process is started here.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let (session, resp) = agg.initialize(None, &msg)?;
    /// ```
    pub fn initialize(
        &self,
        project: Option<&str>,
        msg: &Value,
    ) -> Result<(Arc<AggSession>, Value), HubError> {
        let project = project.map(Hub::resolve_project).transpose()?;
        let ttl = self.hub.session_ttl();
        let session = Arc::new(AggSession {
            id: uuid::Uuid::new_v4().simple().to_string(),
            project,
            last_seen: Mutex::new(Instant::now()),
            subs: Mutex::new(HashMap::new()),
        });
        {
            let mut sessions = self.sessions.lock();
            // Hub sessions behind an expired entry age out on their own TTL.
            sessions.retain(|_, s| s.last_seen.lock().elapsed() <= ttl);
            sessions.insert(session.id.clone(), Arc::clone(&session));
        }
        let client_id = msg.get("id").cloned().unwrap_or(Value::Null);
        let result = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "owlet", "version": env!("CARGO_PKG_VERSION") },
        });
        Ok((session, jsonrpc::result_response(client_id, result)))
    }

    /// Looks up a session and refreshes its TTL.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let session = agg.session(id);
    /// ```
    pub fn session(&self, id: &str) -> Option<Arc<AggSession>> {
        let session = self.sessions.lock().get(id).cloned()?;
        *session.last_seen.lock() = Instant::now();
        Some(session)
    }

    /// Ends a session and the hub sessions opened on its behalf.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// agg.close_session(id).await;
    /// ```
    pub async fn close_session(&self, id: &str) -> bool {
        let Some(session) = self.sessions.lock().remove(id) else {
            return false;
        };
        for sub in session.live_subs() {
            self.hub.close_session(&sub.id).await;
        }
        true
    }

    /// Answers one client request.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let resp = agg.request(&session, json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).await;
    /// ```
    pub async fn request(&self, session: &AggSession, msg: Value) -> Value {
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = jsonrpc::method(&msg).unwrap_or_default().to_owned();
        match method.as_str() {
            "ping" => jsonrpc::result_response(id, json!({})),
            "initialize" => {
                jsonrpc::error_response(id, jsonrpc::INVALID_REQUEST, "session already initialized")
            }
            "tools/list" => {
                let tools = self.list_tools(session).await;
                jsonrpc::result_response(id, json!({ "tools": tools }))
            }
            "tools/call" => self.call_tool(session, msg).await,
            other => jsonrpc::error_response(
                id,
                jsonrpc::METHOD_NOT_FOUND,
                &format!("method `{other}` is not supported by the aggregate endpoint"),
            ),
        }
    }

    /// Handles a client notification; only cancellation is forwarded.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// agg.notify(&session, &msg);
    /// ```
    pub fn notify(&self, session: &AggSession, msg: &Value) {
        if jsonrpc::method(msg) == Some("notifications/cancelled") {
            for sub in session.live_subs() {
                self.hub.notify(&sub, msg);
            }
        }
    }

    /// Returns the hub session for `server`, opening it on first use or after expiry.
    async fn sub(&self, session: &AggSession, server: &str) -> Result<Arc<Session>, HubError> {
        let cell = Arc::clone(session.subs.lock().entry(server.to_owned()).or_default());
        // Held across initialize so concurrent callers share one hub session.
        let mut slot = cell.lock().await;
        if let Some(existing) = slot.as_ref()
            && let Some(live) = self.hub.session(&existing.id)
        {
            return Ok(live);
        }
        let init = json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize" });
        let (sub, _) = self
            .hub
            .initialize(server, session.project.as_deref(), &init)
            .await?;
        *slot = Some(Arc::clone(&sub));
        Ok(sub)
    }

    async fn list_tools(&self, session: &AggSession) -> Vec<Value> {
        let servers = self.hub.enabled_servers(session.project.is_some());
        let lists = join_all(servers.iter().map(|server| async move {
            match tokio::time::timeout(LIST_TIMEOUT, self.fetch_tools(session, server)).await {
                Ok(Ok(tools)) => tools,
                Ok(Err(e)) => {
                    warn!(server = %server, "tools/list failed: {e}");
                    Vec::new()
                }
                Err(_) => {
                    warn!(server = %server, "tools/list timed out");
                    Vec::new()
                }
            }
        }))
        .await;
        lists.into_iter().flatten().collect()
    }

    async fn fetch_tools(&self, session: &AggSession, server: &str) -> Result<Vec<Value>, String> {
        let sub = self.sub(session, server).await.map_err(|e| e.to_string())?;
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = cursor
                .as_deref()
                .map_or_else(|| json!({}), |c| json!({ "cursor": c }));
            let req = json!({
                "jsonrpc": "2.0", "id": "owlet-list", "method": "tools/list", "params": params,
            });
            let resp = self.hub.request(&sub, req).await;
            if let Some(err) = resp.get("error") {
                return Err(err.to_string());
            }
            let result = resp.get("result").ok_or("response has no result")?;
            if let Some(page) = result.get("tools").and_then(Value::as_array) {
                tools.extend(page.iter().filter_map(|t| prefix_tool(server, t)));
            }
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    async fn call_tool(&self, session: &AggSession, mut msg: Value) -> Value {
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let Some(full) = msg.pointer("/params/name").and_then(Value::as_str) else {
            return jsonrpc::error_response(id, jsonrpc::INVALID_PARAMS, "missing tool name");
        };
        let servers = self.hub.enabled_servers(session.project.is_some());
        let Some((server, tool)) = split_tool(&servers, full) else {
            let text = format!("unknown tool `{full}`; expected `<server>{SEP}<tool>`");
            return jsonrpc::error_response(id, jsonrpc::INVALID_PARAMS, &text);
        };
        let (server, tool) = (server.to_owned(), tool.to_owned());
        let sub = match self.sub(session, &server).await {
            Ok(sub) => sub,
            Err(e) => return jsonrpc::error_response(id, jsonrpc::INTERNAL_ERROR, &e.to_string()),
        };
        msg["params"]["name"] = Value::String(tool);
        self.hub.request(&sub, msg).await
    }
}

/// Renames a tool to `<server>__<name>` and tags its description.
fn prefix_tool(server: &str, tool: &Value) -> Option<Value> {
    let name = tool.get("name")?.as_str()?;
    let mut tool = tool.clone();
    tool["name"] = Value::String(format!("{server}{SEP}{name}"));
    let description = match tool.get("description").and_then(Value::as_str) {
        Some(d) => format!("[{server}] {d}"),
        None => format!("[{server}]"),
    };
    tool["description"] = Value::String(description);
    Some(tool)
}

/// Finds the owning server; the longest matching server name wins so that
/// names like `a` and `a__b` stay unambiguous.
fn split_tool<'a>(servers: &'a [String], full: &'a str) -> Option<(&'a str, &'a str)> {
    servers
        .iter()
        .filter_map(|server| {
            let tool = full.strip_prefix(server.as_str())?.strip_prefix(SEP)?;
            (!tool.is_empty()).then_some((server.as_str(), tool))
        })
        .max_by_key(|(server, _)| server.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn prefixes_tools() {
        let tool = json!({"name": "echo", "description": "d", "inputSchema": {}});
        let out = prefix_tool("srv", &tool).expect("tool");
        assert_eq!(out["name"], "srv__echo");
        assert_eq!(out["description"], "[srv] d");
        assert_eq!(out["inputSchema"], json!({}));
        assert_eq!(
            prefix_tool("srv", &json!({"name": "x"})).expect("tool")["description"],
            "[srv]"
        );
        assert!(prefix_tool("srv", &json!({"description": "no name"})).is_none());
    }

    #[test]
    fn splits_by_longest_server_name() {
        let servers = vec!["a".to_owned(), "a__b".to_owned(), "aws-mcp".to_owned()];
        assert_eq!(split_tool(&servers, "a__tool"), Some(("a", "tool")));
        assert_eq!(split_tool(&servers, "a__b__tool"), Some(("a__b", "tool")));
        assert_eq!(
            split_tool(&servers, "aws-mcp__aws___list"),
            Some(("aws-mcp", "aws___list"))
        );
        assert_eq!(split_tool(&servers, "a__"), None);
        assert_eq!(split_tool(&servers, "tool"), None);
        assert_eq!(split_tool(&servers, "b__tool"), None);
    }

    #[tokio::test]
    async fn lists_and_routes_tools_across_servers() {
        let script = r#"
import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m or "method" not in m:
        continue
    method = m["method"]
    if method == "initialize":
        r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "t", "version": "0"}}
    elif method == "tools/list":
        r = {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        r = {"content": [{"type": "text", "text": m["params"]["name"]}]}
    else:
        r = {}
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#;
        let cfg = Config::parse(&format!(
            "[servers.one]\ncommand = \"python3\"\nargs = [\"-u\", \"-c\", '''{script}''']\n\
             [servers.two]\ncommand = \"python3\"\nargs = [\"-u\", \"-c\", '''{script}''']\n\
             [servers.off]\nenabled = false\ncommand = \"python3\"\n"
        ))
        .expect("config");
        let hub = Arc::new(Hub::new(&cfg));
        let agg = Aggregator::new(Arc::clone(&hub));

        let (session, init) = agg
            .initialize(
                None,
                &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}),
            )
            .expect("initialize");
        assert_eq!(init["result"]["serverInfo"]["name"], "owlet");
        assert!(agg.session(&session.id).is_some());

        let list = agg
            .request(
                &session,
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            )
            .await;
        let mut names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["one__echo", "two__echo"]);

        let call = agg
            .request(
                &session,
                json!({"jsonrpc": "2.0", "id": "c1", "method": "tools/call",
                    "params": {"name": "two__echo", "arguments": {}}}),
            )
            .await;
        assert_eq!(call["id"], "c1");
        assert_eq!(call["result"]["content"][0]["text"], "echo");

        let unknown = agg
            .request(
                &session,
                json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                    "params": {"name": "off__echo"}}),
            )
            .await;
        assert_eq!(unknown["error"]["code"], jsonrpc::INVALID_PARAMS);

        assert!(agg.close_session(&session.id).await);
        assert!(agg.session(&session.id).is_none());
        hub.shutdown().await;
    }
}
