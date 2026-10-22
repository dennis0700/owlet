//! Forwards one MCP message to the SigV4-protected Streamable HTTP endpoint and
//! turns the HTTP response into the newline-separated reply owlet expects.

use std::{collections::BTreeMap, time::SystemTime};

use serde_json::{Map, Value, json};

use crate::{
    cli::{Options, Profile},
    owlet::plugin::http::Response,
    sigv4::{self, Env},
    sse,
};

const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";
const INTERNAL_ERROR: i64 = -32603;
/// Longest slice of an error body quoted in a JSON-RPC error.
const MAX_ERROR_BODY: usize = 512;
/// Request id of the plugin's own `tools/list` calls in read-only mode.
const LIST_ID: &str = "aws-mcp-tools-list";
/// Upper bound on `nextCursor` pages followed by one internal `tools/list`.
const MAX_TOOL_PAGES: usize = 100;
/// Tool argument selecting the profile a call is signed with.
const PROFILE_ARG: &str = "aws_profile";
/// Tools that accept `aws_profile` when profile switching is enabled.
const AUTH_REQUIRING_TOOLS: &[&str] = &[
    "aws___call_aws",
    "aws___run_script",
    "aws___get_presigned_url",
    "aws___get_tasks",
    "aws___suggest_aws_commands",
];

/// Streamable HTTP session state learned from the server.
#[derive(Debug, Default)]
pub struct Session {
    pub id: Option<String>,
    pub version: Option<String>,
    /// Body of the last successful `initialize` (without injected metadata),
    /// replayed when the session expires: owlet sends `initialize` upstream
    /// only once per instance.
    pub init: Option<String>,
    /// The session expired and replaying `init` failed; retried before the next request.
    pub expired: bool,
    /// Read-only mode: `readOnlyHint` of every tool listed so far, by name.
    pub tools: BTreeMap<String, bool>,
    /// Profile switching: the backend session of each non-default profile used
    /// so far, by profile name, created by replaying `init`.
    pub profiles: BTreeMap<String, Session>,
}

/// Credential sources: the environment and the temporary-credential cache.
struct Auth<'a> {
    env: Env<'a>,
    cache: &'a mut sigv4::Cache,
}

/// Handles one message from owlet. `send` performs `(method, url, headers,
/// body)`: POSTs to the endpoint; credentials are resolved through `cache`,
/// whose STS / ECS calls take its own HTTP route. `now` is the current
/// time.
///
/// On HTTP 404 to a request carrying a session id, the stored `initialize` and
/// `notifications/initialized` are replayed and the request is retried once.
/// If that replay fails, it is attempted again before the next request.
///
/// With `--read-only`, `tools/list` replies keep only tools annotated with
/// `readOnlyHint: true` and `tools/call` of any other tool is answered with a
/// tool error without reaching the server.
///
/// With two or more profiles, auth tools get an `aws_profile` parameter; a call
/// naming a non-default profile goes through that profile's own session.
///
/// Never panics: every failure of a request becomes a JSON-RPC error reply;
/// failures of notifications and responses are dropped (empty reply).
pub fn handle(
    options: Result<&Options, &String>,
    session: &mut Session,
    request: &str,
    env: Env,
    cache: &mut sigv4::Cache,
    now: SystemTime,
    mut send: impl FnMut(&str, &str, &[(String, String)], &[u8]) -> Result<Response, String>,
) -> String {
    let credentials = &mut Auth { env, cache };
    let Ok(mut msg) = serde_json::from_str::<Value>(request) else {
        return error_reply(&Value::Null, -32700, "aws-mcp: request is not valid JSON");
    };
    let method = msg.get("method").and_then(Value::as_str).map(str::to_owned);
    // Requests carry a method and a non-null id; only they get error replies.
    let id = msg
        .get("id")
        .filter(|id| !id.is_null() && method.is_some())
        .cloned();
    let fail = |message: String| match &id {
        Some(id) => error_reply(id, INTERNAL_ERROR, &format!("aws-mcp: {message}")),
        None => String::new(),
    };
    let options = match options {
        Ok(options) => options,
        Err(e) => return fail(format!("invalid plugin arguments: {e}")),
    };

    let is_init = method.as_deref() == Some("initialize");
    // A previous replay failed: the client still assumes a live session.
    if !is_init
        && session.expired
        && let Err(e) = recover(
            options,
            &options.profile,
            session,
            credentials,
            now,
            &mut send,
        )
    {
        return fail(e);
    }
    if options.read_only
        && method.as_deref() == Some("tools/call")
        && let Some(id) = &id
    {
        let name = msg
            .pointer("/params/name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !session.tools.contains_key(name) {
            refresh_tools(options, session, credentials, now, &mut send);
        }
        let denied = match session.tools.get(name) {
            Some(true) => None,
            Some(false) => Some(format!("Tool '{name}' is not available in read-only mode.")),
            None => Some(format!(
                "Tool '{name}' could not be resolved from the upstream server, \
                 so its read-only hint could not be verified."
            )),
        };
        if let Some(text) = denied {
            return tool_error(id, &text);
        }
    }
    let switching = !options.extra_profiles.is_empty();
    if switching
        && method.as_deref() == Some("tools/call")
        && let Some(id) = &id
    {
        let auth = msg
            .pointer("/params/name")
            .and_then(Value::as_str)
            .is_some_and(|name| AUTH_REQUIRING_TOOLS.contains(&name));
        let requested = msg
            .pointer_mut("/params/arguments")
            .and_then(Value::as_object_mut)
            .and_then(|args| args.remove(PROFILE_ARG));
        // On other tools the argument is dropped and the call proceeds normally.
        if let Some(requested) = requested.filter(|_| auth) {
            let requested = match requested {
                Value::String(name) => name,
                other => other.to_string(),
            };
            if options.profile.name.as_deref() != Some(requested.as_str()) {
                let Some(profile) = options
                    .extra_profiles
                    .iter()
                    .find(|p| p.name.as_deref() == Some(requested.as_str()))
                else {
                    return tool_error(
                        id,
                        &format!(
                            "Profile '{requested}' is not in the allowed list. \
                             Allowed profiles: {}",
                            allowed_profiles(options).join(", ")
                        ),
                    );
                };
                return call_with_profile(
                    options,
                    profile,
                    requested,
                    session,
                    msg,
                    id,
                    credentials,
                    now,
                    &mut send,
                );
            }
        }
    }

    let mut messages = match exchange(
        options,
        &options.profile,
        session,
        msg,
        id.as_ref(),
        is_init,
        credentials,
        now,
        &mut send,
    ) {
        Ok(messages) => messages,
        Err(e) => return fail(e),
    };
    if (options.read_only || switching)
        && method.as_deref() == Some("tools/list")
        && let Some(id) = &id
    {
        for message in messages.iter_mut().filter(|m| is_response_to(m, id)) {
            if let Ok(mut reply) = serde_json::from_str::<Value>(message)
                && let Some(result) = reply.get_mut("result")
            {
                if options.read_only {
                    keep_read_only(result, &mut session.tools);
                }
                if switching {
                    add_profile_param(result, &allowed_profiles(options));
                }
                *message = reply.to_string();
            }
        }
    }
    join_reply(&messages, id.as_ref())
}

/// Ends the default session and every profile session with a best-effort
/// SigV4-signed `DELETE`, so the server can free them. `send` performs
/// `(method, url, headers, body)`. Sessions without an id are skipped; signing
/// and HTTP failures and every response status are ignored.
///
/// # Examples
///
/// ```ignore
/// proxy::shutdown(&options, &session, &env, &mut cache, SystemTime::now(), owlet::plugin::http::send);
/// ```
pub fn shutdown(
    options: &Options,
    session: &Session,
    env: Env,
    cache: &mut sigv4::Cache,
    now: SystemTime,
    mut send: impl FnMut(&str, &str, &[(String, String)], &[u8]) -> Result<Response, String>,
) {
    let profiles = session.profiles.iter().filter_map(|(name, s)| {
        let profile = options
            .extra_profiles
            .iter()
            .find(|p| p.name.as_deref() == Some(name.as_str()))?;
        Some((profile, s))
    });
    for (profile, session) in std::iter::once((&options.profile, session)).chain(profiles) {
        delete_session(options, profile, session, env, cache, now, &mut send);
    }
}

/// Ends `session` (if it has an id) with a best-effort DELETE signed for
/// `profile`; every failure is ignored.
fn delete_session(
    options: &Options,
    profile: &Profile,
    session: &Session,
    env: Env,
    cache: &mut sigv4::Cache,
    now: SystemTime,
    send: &mut impl FnMut(&str, &str, &[(String, String)], &[u8]) -> Result<Response, String>,
) {
    if session.id.is_none() {
        return;
    }
    let headers = sigv4::load_credentials(profile.name.as_deref(), env, cache, now)
        .and_then(|creds| signed_headers(options, session, "DELETE", &[], &creds, now));
    if let Ok(headers) = headers {
        let _ = send("DELETE", options.endpoint.as_str(), &headers, &[]);
    }
}

/// POSTs `msg` on `session`, signed for `profile` with its metadata merged
/// into `params._meta`, and returns the reply messages. On HTTP 404 to a
/// request carrying a session id, the handshake is replayed and the request
/// retried once. A successful `initialize` (`is_init`) is stored for replay.
#[allow(clippy::too_many_arguments)]
fn exchange(
    options: &Options,
    profile: &Profile,
    session: &mut Session,
    mut msg: Value,
    id: Option<&Value>,
    is_init: bool,
    auth: &mut Auth,
    now: SystemTime,
    send: &mut impl FnMut(&str, &str, &[(String, String)], &[u8]) -> Result<Response, String>,
) -> Result<Vec<String>, String> {
    let raw = is_init.then(|| msg.to_string());
    if msg.get("method").is_some() {
        inject_metadata(&mut msg, &profile.metadata);
    }
    let body = msg.to_string();
    let mut resp = post(options, profile, session, &body, auth, now, send)?;

    // Streamable HTTP: 404 to a request carrying a session id means the session
    // is gone. Nobody else will re-initialize, so replay the handshake and retry.
    if resp.status == 404 && session.id.is_some() {
        if is_init {
            session.init = None;
        }
        recover(options, profile, session, auth, now, send)?;
        resp = post(options, profile, session, &body, auth, now, send)?;
    }
    if !(200..300).contains(&resp.status) {
        let text = String::from_utf8_lossy(&resp.body);
        let text: String = text.chars().take(MAX_ERROR_BODY).collect();
        return Err(format!("HTTP {}: {}", resp.status, text.trim()));
    }
    if let Some(sid) = header(&resp, SESSION_HEADER) {
        session.id = Some(sid.to_owned());
    }
    if resp.status == 202 || resp.body.iter().all(u8::is_ascii_whitespace) {
        return match id {
            Some(_) => Err("server accepted the request without a reply".to_owned()),
            None => Ok(Vec::new()),
        };
    }

    let messages = parse_body(&resp)?;
    if is_init && let Some(version) = protocol_version(&messages) {
        session.version = Some(version);
        session.init = raw;
        session.expired = false;
    }
    Ok(messages)
}

/// Joins reply messages, adding an error when none answers request `id`.
fn join_reply(messages: &[String], id: Option<&Value>) -> String {
    let mut reply = messages.join("\n");
    if let Some(id) = id
        && !messages.iter().any(|m| is_response_to(m, id))
    {
        if !reply.is_empty() {
            reply.push('\n');
        }
        reply.push_str(&error_reply(
            id,
            INTERNAL_ERROR,
            "aws-mcp: server ended the response without a reply",
        ));
    }
    reply
}

/// Sorted names of every profile `aws_profile` may select.
fn allowed_profiles(options: &Options) -> Vec<&str> {
    let mut names: Vec<&str> = std::iter::once(&options.profile)
        .chain(&options.extra_profiles)
        .filter_map(|p| p.name.as_deref())
        .collect();
    names.sort_unstable();
    names
}

/// Adds the `aws_profile` parameter to the input schema of the auth tools in
/// `result.tools`.
fn add_profile_param(result: &mut Value, allowed: &[&str]) {
    let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools {
        let auth = tool
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| AUTH_REQUIRING_TOOLS.contains(&name));
        if !auth {
            continue;
        }
        let Some(schema) = tool.get_mut("inputSchema").and_then(Value::as_object_mut) else {
            continue;
        };
        let properties = schema
            .entry("properties")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(properties) = properties.as_object_mut() {
            properties.insert(
                PROFILE_ARG.to_owned(),
                json!({
                    "type": "string",
                    "description": format!(
                        "AWS CLI profile to sign this request with. Available profiles: {}.",
                        allowed.join(", ")
                    ),
                    "enum": allowed,
                }),
            );
        }
    }
}

/// Sends a `tools/call` (`aws_profile` already removed) through the session
/// of the non-default `profile` named `name`, creating it on first use.
/// Failures are tool errors; a failed call ends and drops the profile's session.
#[allow(clippy::too_many_arguments)]
fn call_with_profile(
    options: &Options,
    profile: &Profile,
    name: String,
    session: &mut Session,
    msg: Value,
    id: &Value,
    auth: &mut Auth,
    now: SystemTime,
    send: &mut impl FnMut(&str, &str, &[(String, String)], &[u8]) -> Result<Response, String>,
) -> String {
    let mut profile_session = match session.profiles.remove(&name) {
        Some(existing) => existing,
        None => {
            let mut created = Session::default();
            let result = match &session.init {
                Some(init) => reinitialize(options, profile, &mut created, init, auth, now, send),
                None => Err("the client has not initialized the MCP session".to_owned()),
            };
            if let Err(e) = result {
                return tool_error(
                    id,
                    &format!(
                        "Failed to create connection for profile '{name}'. \
                         Check that the profile is configured and credentials are valid. {e}"
                    ),
                );
            }
            created.init.clone_from(&session.init);
            created
        }
    };
    match exchange(
        options,
        profile,
        &mut profile_session,
        msg,
        Some(id),
        false,
        auth,
        now,
        send,
    ) {
        Ok(messages) => {
            session.profiles.insert(name, profile_session);
            join_reply(&messages, Some(id))
        }
        Err(e) => {
            // Like Python's client disconnect: end the dropped session upstream.
            delete_session(
                options,
                profile,
                &profile_session,
                auth.env,
                auth.cache,
                now,
                send,
            );
            tool_error(
                id,
                &format!(
                    "Tool call failed using profile '{name}'. \
                     The request could not be completed with the specified profile. {e}"
                ),
            )
        }
    }
}

/// Merges `metadata` into `params._meta`; keys already present win.
fn inject_metadata(msg: &mut Value, metadata: &Map<String, Value>) {
    if metadata.is_empty() {
        return;
    }
    let Some(obj) = msg.as_object_mut() else {
        return;
    };
    let params = obj
        .entry("params")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(params) = params.as_object_mut() else {
        return;
    };
    let meta = params
        .entry("_meta")
        .or_insert_with(|| Value::Object(Map::new()));
    match meta.as_object_mut() {
        Some(existing) => {
            for (k, v) in metadata {
                existing.entry(k.as_str()).or_insert_with(|| v.clone());
            }
        }
        None => *meta = Value::Object(metadata.clone()),
    }
}

/// Signs `body` for `profile` with the current session headers and POSTs it.
fn post(
    options: &Options,
    profile: &Profile,
    session: &Session,
    body: &str,
    auth: &mut Auth,
    now: SystemTime,
    send: &mut impl FnMut(&str, &str, &[(String, String)], &[u8]) -> Result<Response, String>,
) -> Result<Response, String> {
    let body = body.as_bytes();
    // Resolved for every request so rotated static keys are picked up.
    let headers = sigv4::load_credentials(profile.name.as_deref(), auth.env, auth.cache, now)
        .and_then(|creds| signed_headers(options, session, "POST", body, &creds, now))
        .map_err(|e| format!("{e:#}"))?;
    send("POST", options.endpoint.as_str(), &headers, body)
}

/// Drops the expired session and replays the stored `initialize`. On failure
/// `init` is kept and `session.expired` set, so the next request retries.
fn recover(
    options: &Options,
    profile: &Profile,
    session: &mut Session,
    auth: &mut Auth,
    now: SystemTime,
    send: &mut impl FnMut(&str, &str, &[(String, String)], &[u8]) -> Result<Response, String>,
) -> Result<(), String> {
    session.id = None;
    session.version = None;
    let Some(init) = session.init.take() else {
        session.expired = false;
        return Err("MCP session expired (HTTP 404)".to_owned());
    };
    let result = reinitialize(options, profile, session, &init, auth, now, send);
    session.init = Some(init);
    session.expired = result.is_err();
    result.map_err(|e| format!("MCP session expired (HTTP 404) and re-initialize failed: {e}"))
}

/// Replays `initialize` (body `init`) and `notifications/initialized` with
/// `profile`'s credentials and metadata, filling `session` with the new
/// session id and protocol version.
///
/// Like a client `initialize`, the replay only counts when the reply carries
/// `result.protocolVersion`.
fn reinitialize(
    options: &Options,
    profile: &Profile,
    session: &mut Session,
    init: &str,
    auth: &mut Auth,
    now: SystemTime,
    send: &mut impl FnMut(&str, &str, &[(String, String)], &[u8]) -> Result<Response, String>,
) -> Result<(), String> {
    let mut init: Value =
        serde_json::from_str(init).map_err(|e| format!("stored initialize: {e}"))?;
    inject_metadata(&mut init, &profile.metadata);
    let resp = post(
        options,
        profile,
        session,
        &init.to_string(),
        auth,
        now,
        send,
    )?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("HTTP {}", resp.status));
    }
    let messages = parse_body(&resp)?;
    let Some(version) = protocol_version(&messages) else {
        let text: String = messages.join("\n").chars().take(MAX_ERROR_BODY).collect();
        return Err(format!("reply has no result.protocolVersion: {text}"));
    };
    session.id = header(&resp, SESSION_HEADER).map(str::to_owned);
    session.version = Some(version);
    let mut note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    inject_metadata(&mut note, &profile.metadata);
    let result = post(
        options,
        profile,
        session,
        &note.to_string(),
        auth,
        now,
        send,
    )
    .and_then(|resp| match resp.status {
        200..300 => Ok(()),
        status => Err(format!("notifications/initialized: HTTP {status}")),
    });
    if result.is_err() {
        // Half-open: end the new session so a later replay does not leak it.
        delete_session(options, profile, session, auth.env, auth.cache, now, send);
        session.id = None;
    }
    result
}

/// `result.protocolVersion` of the first `initialize` reply among `messages`.
fn protocol_version(messages: &[String]) -> Option<String> {
    messages.iter().find_map(|m| {
        serde_json::from_str::<Value>(m)
            .ok()?
            .pointer("/result/protocolVersion")?
            .as_str()
            .map(str::to_owned)
    })
}

/// Read-only mode: lists the upstream tools (following `nextCursor`) into
/// `session.tools`. A failed page ends the walk; unresolved tools stay unknown.
fn refresh_tools(
    options: &Options,
    session: &mut Session,
    auth: &mut Auth,
    now: SystemTime,
    send: &mut impl FnMut(&str, &str, &[(String, String)], &[u8]) -> Result<Response, String>,
) {
    let id = json!(LIST_ID);
    let mut cursor = None;
    for _ in 0..MAX_TOOL_PAGES {
        let mut req = json!({"jsonrpc": "2.0", "id": id, "method": "tools/list"});
        if let Some(cursor) = cursor {
            req["params"] = json!({"cursor": cursor});
        }
        inject_metadata(&mut req, &options.profile.metadata);
        let Ok(resp) = post(
            options,
            &options.profile,
            session,
            &req.to_string(),
            auth,
            now,
            send,
        ) else {
            return;
        };
        if !(200..300).contains(&resp.status) {
            return;
        }
        let Ok(messages) = parse_body(&resp) else {
            return;
        };
        let Some(mut result) = messages
            .iter()
            .filter(|m| is_response_to(m, &id))
            .find_map(|m| {
                serde_json::from_str::<Value>(m)
                    .ok()?
                    .get_mut("result")
                    .map(Value::take)
            })
        else {
            return;
        };
        keep_read_only(&mut result, &mut session.tools);
        cursor = match result.get_mut("nextCursor").map(Value::take) {
            Some(Value::String(next)) => Some(next),
            _ => return,
        };
    }
}

/// Records the `annotations.readOnlyHint` of every tool in `result.tools` and
/// drops the tools that are not read-only (a missing hint means not read-only).
fn keep_read_only(result: &mut Value, tools: &mut BTreeMap<String, bool>) {
    let Some(list) = result.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    list.retain(|tool| {
        let read_only = tool.pointer("/annotations/readOnlyHint") == Some(&Value::Bool(true));
        if let Some(name) = tool.get("name").and_then(Value::as_str) {
            tools.insert(name.to_owned(), read_only);
        }
        read_only
    });
}

/// A `tools/call` result reporting `text` as a tool error.
fn tool_error(id: &Value, text: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id,
        "result": {"content": [{"type": "text", "text": text}], "isError": true}})
    .to_string()
}

/// Request headers including a fresh SigV4 signature for `method` with `creds`.
fn signed_headers(
    options: &Options,
    session: &Session,
    method: &str,
    body: &[u8],
    creds: &sigv4::Credentials,
    now: SystemTime,
) -> anyhow::Result<Vec<(String, String)>> {
    let mut headers = vec![
        ("content-type".to_owned(), "application/json".to_owned()),
        (
            "accept".to_owned(),
            "application/json, text/event-stream".to_owned(),
        ),
        ("content-length".to_owned(), body.len().to_string()),
    ];
    if let Some(sid) = &session.id {
        headers.push((SESSION_HEADER.to_owned(), sid.clone()));
    }
    if let Some(version) = &session.version {
        headers.push((VERSION_HEADER.to_owned(), version.clone()));
    }
    let signed = sigv4::sign(
        creds,
        method,
        &options.endpoint,
        &headers,
        body,
        &options.region,
        &options.service,
        now,
    )?;
    headers.extend(signed);
    Ok(headers)
}

/// Splits a JSON or SSE body into single-line JSON-RPC messages.
fn parse_body(resp: &Response) -> Result<Vec<String>, String> {
    let text = std::str::from_utf8(&resp.body).map_err(|_| "response is not UTF-8".to_owned())?;
    let sse = header(resp, "content-type").is_some_and(|v| v.starts_with("text/event-stream"));
    if sse {
        return Ok(sse::messages(text));
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(items)) => Ok(items.iter().map(Value::to_string).collect()),
        Ok(single) => Ok(vec![single.to_string()]),
        Err(e) => Err(format!("response is not JSON: {e}")),
    }
}

fn header<'a>(resp: &'a Response, name: &str) -> Option<&'a str> {
    resp.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn is_response_to(msg: &str, id: &Value) -> bool {
    serde_json::from_str::<Value>(msg)
        .is_ok_and(|m| m.get("id") == Some(id) && m.get("method").is_none())
}

fn error_reply(id: &Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        sync::OnceLock,
        time::{Duration, UNIX_EPOCH},
    };

    use super::*;
    use crate::cli;

    fn options() -> Options {
        let args = [
            "https://aws-mcp.us-east-1.api.aws/mcp",
            "--metadata",
            "AWS_REGION=us-west-2",
        ]
        .map(str::to_owned);
        cli::parse(&args, &|_| None).expect("options")
    }

    /// Env whose shared credentials file has a `default` profile with
    /// AKIDEXAMPLE; the file is written once per test run. `AWS_PROFILE` names
    /// it so the SDK skips the real process environment's keys.
    fn creds(k: &str) -> Option<String> {
        static FILE: OnceLock<String> = OnceLock::new();
        if k == "AWS_PROFILE" {
            return Some("default".to_owned());
        }
        (k == "AWS_SHARED_CREDENTIALS_FILE").then(|| {
            FILE.get_or_init(|| {
                let path = std::env::temp_dir().join("owlet-aws-proxy-test-credentials");
                std::fs::write(
                    &path,
                    "[default]\naws_access_key_id = AKIDEXAMPLE\naws_secret_access_key = secret\n",
                )
                .expect("write");
                path.to_str().expect("utf-8").to_owned()
            })
            .clone()
        })
    }

    /// 2024-01-01T00:00:00Z.
    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_704_067_200)
    }

    /// The credential cache's HTTP route; static credentials never use it.
    fn offline(_: &str, url: &str, _: &[(String, String)], _: &[u8]) -> Result<Response, String> {
        Err(format!("unexpected request to {url}"))
    }

    fn response(status: u16, headers: &[(&str, &str)], body: &str) -> Response {
        Response {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn metadata_merge_keeps_existing_keys() {
        let meta = Map::from_iter([
            ("AWS_REGION".to_owned(), json!("us-west-2")),
            ("K".to_owned(), json!("v")),
        ]);
        let mut msg = json!({"method": "tools/call", "params": {"_meta": {"K": "mine"}}});
        inject_metadata(&mut msg, &meta);
        assert_eq!(
            msg["params"]["_meta"],
            json!({"K": "mine", "AWS_REGION": "us-west-2"})
        );

        let mut msg = json!({"method": "tools/list"});
        inject_metadata(&mut msg, &meta);
        assert_eq!(msg["params"]["_meta"]["AWS_REGION"], "us-west-2");

        let mut msg = json!({"method": "x", "params": {"_meta": 5}});
        inject_metadata(&mut msg, &meta);
        assert_eq!(msg["params"]["_meta"]["K"], "v");

        let mut msg = json!({"method": "x", "params": [1]});
        inject_metadata(&mut msg, &meta);
        assert_eq!(msg["params"], json!([1]));
    }

    #[test]
    fn proxies_json_and_sse_with_session() {
        let opts = options();
        let mut session = Session::default();
        let seen = RefCell::new(Vec::new());

        let init = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#;
        let reply = handle(
            Ok(&opts),
            &mut session,
            init,
            &creds,
            &mut sigv4::Cache::new(offline),
            now(),
            |_, url, h, b| {
                seen.borrow_mut()
                    .push((url.to_owned(), h.to_vec(), b.to_vec()));
                Ok(response(
                    200,
                    &[
                        ("Content-Type", "application/json"),
                        ("Mcp-Session-Id", "s1"),
                    ],
                    "{\"jsonrpc\":\"2.0\",\"id\":0,\n\"result\":{\"protocolVersion\":\"2025-06-18\"}}",
                ))
            },
        );
        assert_eq!(
            reply,
            r#"{"id":0,"jsonrpc":"2.0","result":{"protocolVersion":"2025-06-18"}}"#
        );
        assert_eq!(session.id.as_deref(), Some("s1"));
        assert_eq!(session.version.as_deref(), Some("2025-06-18"));
        {
            let seen = seen.borrow();
            let (url, headers, body) = &seen[0];
            assert_eq!(url, "https://aws-mcp.us-east-1.api.aws/mcp");
            let auth = &headers
                .iter()
                .find(|(k, _)| k == "authorization")
                .expect("auth")
                .1;
            assert!(auth.starts_with(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20240101/us-east-1/aws-mcp/"
            ));
            assert!(!headers.iter().any(|(k, _)| k == SESSION_HEADER));
            let body: Value = serde_json::from_slice(body).expect("json");
            assert_eq!(body["params"]["_meta"]["AWS_REGION"], "us-west-2");
        }

        let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let reply = handle(
            Ok(&opts),
            &mut session,
            list,
            &creds,
            &mut sigv4::Cache::new(offline),
            now(),
            |_, _, h, _| {
                assert!(h.contains(&(SESSION_HEADER.to_owned(), "s1".to_owned())));
                assert!(h.contains(&(VERSION_HEADER.to_owned(), "2025-06-18".to_owned())));
                Ok(response(
                    200,
                    &[("content-type", "text/event-stream")],
                    "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/x\"}\n\n\
                 event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n",
                ))
            },
        );
        let lines: Vec<&str> = reply.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("\"tools\":[]"));

        let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let reply = handle(
            Ok(&opts),
            &mut session,
            note,
            &creds,
            &mut sigv4::Cache::new(offline),
            now(),
            |_, _, _, _| Ok(response(202, &[], "")),
        );
        assert_eq!(reply, "");
    }

    #[test]
    fn failures_become_json_rpc_errors() {
        let opts = options();
        let mut session = Session::default();
        let req = r#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#;
        let date = now();

        let reply = handle(
            Ok(&opts),
            &mut session,
            req,
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            |_, _, _, _| Ok(response(403, &[], "denied")),
        );
        let err: Value = serde_json::from_str(&reply).expect("json");
        assert_eq!(err["id"], "a");
        assert_eq!(err["error"]["code"], INTERNAL_ERROR);
        assert_eq!(err["error"]["message"], "aws-mcp: HTTP 403: denied");

        let reply = handle(
            Ok(&opts),
            &mut session,
            req,
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            |_, _, _, _| Err("dns error".to_owned()),
        );
        assert!(reply.contains("aws-mcp: dns error"));

        // No credentials anywhere: clear error, no HTTP call.
        let reply = handle(
            Ok(&opts),
            &mut session,
            req,
            &|k| (k == "AWS_PROFILE").then(|| "default".to_owned()),
            &mut sigv4::Cache::new(offline),
            date,
            |_, _, _, _| Err("must not be called".to_owned()),
        );
        assert!(reply.contains("no credentials"), "{reply}");

        let bad = "bad args".to_owned();
        let reply = handle(
            Err(&bad),
            &mut session,
            req,
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            |_, _, _, _| Err("must not be called".to_owned()),
        );
        assert!(reply.contains("invalid plugin arguments: bad args"));

        let reply = handle(
            Ok(&opts),
            &mut session,
            "{",
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            |_, _, _, _| Err("must not be called".to_owned()),
        );
        assert!(reply.contains("-32700"));

        // Notifications and responses to server requests never get error replies.
        let note = r#"{"jsonrpc":"2.0","method":"notifications/cancelled"}"#;
        let resp = r#"{"jsonrpc":"2.0","id":"srv","result":{}}"#;
        for msg in [note, resp] {
            let reply = handle(
                Ok(&opts),
                &mut session,
                msg,
                &creds,
                &mut sigv4::Cache::new(offline),
                date,
                |_, _, _, _| Ok(response(500, &[], "boom")),
            );
            assert_eq!(reply, "");
        }

        // A reply stream without our response gets a synthesized error.
        let reply = handle(
            Ok(&opts),
            &mut session,
            req,
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            |_, _, _, _| {
                Ok(response(
                    200,
                    &[("content-type", "text/event-stream")],
                    ": nothing\n\n",
                ))
            },
        );
        assert!(reply.contains("without a reply"));
    }

    #[test]
    fn expired_session_is_cleared_on_404() {
        let opts = options();
        let date = now();
        let req = r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#;

        // Without a session a 404 is an ordinary HTTP error.
        let mut session = Session::default();
        let reply = handle(
            Ok(&opts),
            &mut session,
            req,
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            |_, _, _, _| Ok(response(404, &[], "nope")),
        );
        assert!(reply.contains("HTTP 404: nope"), "{reply}");

        let mut session = Session {
            id: Some("s1".to_owned()),
            version: Some("2025-06-18".to_owned()),
            ..Session::default()
        };
        let reply = handle(
            Ok(&opts),
            &mut session,
            req,
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            |_, _, h, _| {
                assert!(h.contains(&(SESSION_HEADER.to_owned(), "s1".to_owned())));
                Ok(response(404, &[], "unknown session"))
            },
        );
        let err: Value = serde_json::from_str(&reply).expect("json");
        assert_eq!(err["id"], 7);
        assert!(
            err["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("session expired")),
            "{reply}"
        );
        assert!(session.id.is_none() && session.version.is_none());
    }

    /// Mock server: `initialize` issues `s{n}`; only the latest session is live.
    fn server(live: &RefCell<u32>, h: &[(String, String)], b: &[u8]) -> Response {
        let msg: Value = serde_json::from_slice(b).expect("json");
        let sid = h
            .iter()
            .find(|(k, _)| k == SESSION_HEADER)
            .map(|(_, v)| v.as_str());
        match msg["method"].as_str() {
            Some("initialize") => {
                *live.borrow_mut() += 1;
                let sid = format!("s{}", live.borrow());
                let body = json!({"jsonrpc": "2.0", "id": msg["id"],
                    "result": {"protocolVersion": "2025-06-18"}});
                Response {
                    status: 200,
                    headers: vec![(SESSION_HEADER.to_owned(), sid)],
                    body: body.to_string().into_bytes(),
                }
            }
            _ if sid != Some(format!("s{}", live.borrow()).as_str()) => {
                response(404, &[], "unknown session")
            }
            Some("notifications/initialized") => response(202, &[], ""),
            _ => response(
                200,
                &[],
                &json!({"jsonrpc": "2.0", "id": msg["id"], "result": {}}).to_string(),
            ),
        }
    }

    #[test]
    fn expired_session_replays_initialize_and_retries() {
        let opts = options();
        let date = now();
        let live = RefCell::new(0);
        let calls = RefCell::new(Vec::new());
        let mut send = |_: &str, _: &str, h: &[(String, String)], b: &[u8]| {
            let msg: Value = serde_json::from_slice(b).expect("json");
            calls.borrow_mut().push(msg);
            Ok(server(&live, h, b))
        };

        let mut session = Session::default();
        let init = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"clientInfo":{"name":"owlet"}}}"#;
        handle(
            Ok(&opts),
            &mut session,
            init,
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            &mut send,
        );
        assert_eq!(session.id.as_deref(), Some("s1"));

        // The server forgets s1: the plugin re-initializes (s2) and retries once.
        *live.borrow_mut() += 1;
        let req = r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#;
        let reply = handle(
            Ok(&opts),
            &mut session,
            req,
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            &mut send,
        );
        assert_eq!(reply, r#"{"id":7,"jsonrpc":"2.0","result":{}}"#);
        assert_eq!(session.id.as_deref(), Some("s3"));
        let calls = calls.borrow();
        let methods: Vec<&str> = calls
            .iter()
            .map(|m| m["method"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "tools/list",
                "initialize",
                "notifications/initialized",
                "tools/list"
            ]
        );
        assert_eq!(calls[2], calls[0], "replays the original initialize");
        assert_eq!(calls[3]["params"]["_meta"]["AWS_REGION"], "us-west-2");
    }

    #[test]
    fn failed_replay_becomes_an_error() {
        let opts = options();
        let date = now();
        let mut session = Session {
            id: Some("s1".to_owned()),
            init: Some(r#"{"jsonrpc":"2.0","id":0,"method":"initialize"}"#.to_owned()),
            ..Session::default()
        };
        let req = r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#;
        let reply = handle(
            Ok(&opts),
            &mut session,
            req,
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            |_, _, h, _| {
                Ok(match h.iter().any(|(k, _)| k == SESSION_HEADER) {
                    true => response(404, &[], ""),
                    false => response(503, &[], "down"),
                })
            },
        );
        assert!(reply.contains("re-initialize failed: HTTP 503"), "{reply}");
        assert!(session.id.is_none() && session.expired);
        assert!(session.init.is_some(), "init is kept for the next attempt");

        // The server is back: the next request re-initializes first and succeeds.
        let live = RefCell::new(1);
        let calls = RefCell::new(Vec::new());
        let reply = handle(
            Ok(&opts),
            &mut session,
            req,
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            |_, _, h, b| {
                let msg: Value = serde_json::from_slice(b).expect("json");
                let sid = h
                    .iter()
                    .find(|(k, _)| k == SESSION_HEADER)
                    .map(|(_, v)| v.clone());
                calls.borrow_mut().push((msg["method"].clone(), sid));
                Ok(server(&live, h, b))
            },
        );
        assert_eq!(reply, r#"{"id":7,"jsonrpc":"2.0","result":{}}"#);
        assert_eq!(
            *calls.borrow(),
            [
                (json!("initialize"), None),
                (json!("notifications/initialized"), Some("s2".to_owned())),
                (json!("tools/list"), Some("s2".to_owned())),
            ]
        );
        assert_eq!(session.id.as_deref(), Some("s2"));
        assert!(!session.expired);
    }

    #[test]
    fn replay_without_protocol_version_fails() {
        let opts = options();
        let date = now();
        let mut session = Session {
            id: Some("s1".to_owned()),
            init: Some(r#"{"jsonrpc":"2.0","id":0,"method":"initialize"}"#.to_owned()),
            ..Session::default()
        };
        let req = r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#;
        let mut sent = Vec::new();
        let reply = handle(
            Ok(&opts),
            &mut session,
            req,
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            |_, _, h, b| {
                let msg: Value = serde_json::from_slice(b).expect("json");
                sent.push(msg["method"].clone());
                Ok(match h.iter().any(|(k, _)| k == SESSION_HEADER) {
                    true => response(404, &[], ""),
                    false => response(
                        200,
                        &[(SESSION_HEADER, "s2")],
                        r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32600,"message":"bad"}}"#,
                    ),
                })
            },
        );
        assert!(
            reply.contains("re-initialize failed: reply has no result.protocolVersion"),
            "{reply}"
        );
        assert_eq!(sent, [json!("tools/list"), json!("initialize")]);
        assert!(session.id.is_none() && session.expired && session.init.is_some());
    }

    fn read_only_options() -> Options {
        Options {
            read_only: true,
            ..options()
        }
    }

    fn tools_page(id: &Value, cursor: Option<&str>) -> Value {
        let tools = match cursor {
            None => json!([
                {"name": "read", "annotations": {"readOnlyHint": true}},
                {"name": "write", "annotations": {"readOnlyHint": false}},
            ]),
            Some(_) => json!([{"name": "bare"}, {"name": "hinted", "annotations": {"title": "t"}}]),
        };
        let mut result = json!({"tools": tools});
        if cursor.is_none() {
            result["nextCursor"] = json!("p2");
        }
        json!({"jsonrpc": "2.0", "id": id, "result": result})
    }

    /// Mock server with two `tools/list` pages; `tools/call` echoes the name.
    fn tool_server(calls: &RefCell<Vec<Value>>, b: &[u8]) -> Response {
        let msg: Value = serde_json::from_slice(b).expect("json");
        calls.borrow_mut().push(msg.clone());
        let body = match msg["method"].as_str() {
            Some("tools/list") => tools_page(&msg["id"], msg["params"]["cursor"].as_str()),
            _ => {
                json!({"jsonrpc": "2.0", "id": msg["id"], "result": {"called": msg["params"]["name"]}})
            }
        };
        response(200, &[], &body.to_string())
    }

    #[test]
    fn read_only_filters_tools_list() {
        let date = now();
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let calls = RefCell::new(Vec::new());

        // Without --read-only the reply is untouched.
        let mut session = Session::default();
        let reply = handle(
            Ok(&options()),
            &mut session,
            req,
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            |_, _, _, b| Ok(tool_server(&calls, b)),
        );
        let reply: Value = serde_json::from_str(&reply).expect("json");
        assert_eq!(reply, tools_page(&json!(1), None));
        assert!(session.tools.is_empty());

        let opts = read_only_options();
        let reply = handle(
            Ok(&opts),
            &mut session,
            req,
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            |_, _, _, b| Ok(tool_server(&calls, b)),
        );
        let reply: Value = serde_json::from_str(&reply).expect("json");
        assert_eq!(
            reply["result"],
            json!({"tools": [{"name": "read", "annotations": {"readOnlyHint": true}}],
                "nextCursor": "p2"})
        );
        let req = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"cursor":"p2"}}"#;
        let reply = handle(
            Ok(&opts),
            &mut session,
            req,
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            |_, _, _, b| Ok(tool_server(&calls, b)),
        );
        let reply: Value = serde_json::from_str(&reply).expect("json");
        assert_eq!(reply["result"], json!({"tools": []}));
        assert_eq!(
            session.tools,
            BTreeMap::from([
                ("bare".to_owned(), false),
                ("hinted".to_owned(), false),
                ("read".to_owned(), true),
                ("write".to_owned(), false),
            ])
        );
    }

    #[test]
    fn read_only_gates_tools_call() {
        let opts = read_only_options();
        let date = now();
        let call = |name: &str| {
            json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
                "params": {"name": name, "arguments": {}}})
            .to_string()
        };
        let tool_error_text = |reply: &str| {
            let reply: Value = serde_json::from_str(reply).expect("json");
            assert_eq!(reply["id"], 5);
            assert_eq!(reply["result"]["isError"], true);
            reply["result"]["content"][0]["text"]
                .as_str()
                .map(str::to_owned)
        };
        let calls = RefCell::new(Vec::new());
        let mut session = Session::default();
        let mut send =
            |_: &str, _: &str, _: &[(String, String)], b: &[u8]| Ok(tool_server(&calls, b));

        // Unknown tool: both upstream pages are listed internally, then denied.
        let reply = handle(
            Ok(&opts),
            &mut session,
            &call("write"),
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            &mut send,
        );
        assert_eq!(
            tool_error_text(&reply).as_deref(),
            Some("Tool 'write' is not available in read-only mode.")
        );
        {
            let calls = calls.borrow();
            assert_eq!(calls.len(), 2, "the call itself is not forwarded");
            assert!(
                calls
                    .iter()
                    .all(|m| m["method"] == "tools/list" && m["id"] == LIST_ID)
            );
            assert_eq!(calls[1]["params"]["cursor"], "p2");
            assert_eq!(calls[1]["params"]["_meta"]["AWS_REGION"], "us-west-2");
        }

        // Known read-only tool: forwarded without another listing.
        let reply = handle(
            Ok(&opts),
            &mut session,
            &call("read"),
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            &mut send,
        );
        assert_eq!(
            reply,
            r#"{"id":5,"jsonrpc":"2.0","result":{"called":"read"}}"#
        );
        assert_eq!(calls.borrow().len(), 3);

        // Tool without annotations is denied from the cache.
        let reply = handle(
            Ok(&opts),
            &mut session,
            &call("bare"),
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            &mut send,
        );
        assert!(tool_error_text(&reply).is_some_and(|t| t.contains("not available")));
        assert_eq!(calls.borrow().len(), 3);

        // Still unknown after listing: cannot be verified.
        let reply = handle(
            Ok(&opts),
            &mut session,
            &call("ghost"),
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            &mut send,
        );
        assert_eq!(
            tool_error_text(&reply).as_deref(),
            Some(
                "Tool 'ghost' could not be resolved from the upstream server, \
                 so its read-only hint could not be verified."
            )
        );
        assert_eq!(calls.borrow().len(), 5);

        // Listing fails upstream: unresolved, not a read-only denial.
        let mut session = Session::default();
        let reply = handle(
            Ok(&opts),
            &mut session,
            &call("read"),
            &creds,
            &mut sigv4::Cache::new(offline),
            date,
            |_, _, _, _| Ok(response(429, &[], "slow down")),
        );
        assert!(tool_error_text(&reply).is_some_and(|t| t.contains("could not be resolved")));
    }

    /// One request seen by `profile_server`.
    #[derive(Debug)]
    struct Seen {
        method: String,
        session: Option<String>,
        access_key: String,
        body: Value,
    }

    /// Mock server: `initialize` issues `s-<access key>`; `tools/list` lists two
    /// auth tools and one other; `tools/call` echoes name and arguments.
    fn profile_server(seen: &RefCell<Vec<Seen>>, h: &[(String, String)], b: &[u8]) -> Response {
        let body: Value = serde_json::from_slice(b).expect("json");
        let get = |name: &str| h.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        let auth = get("authorization").expect("signed");
        let access_key = auth
            .split("Credential=")
            .nth(1)
            .and_then(|c| c.split('/').next())
            .expect("credential")
            .to_owned();
        let method = body["method"].as_str().unwrap_or_default().to_owned();
        let result = match method.as_str() {
            "initialize" => Some(json!({"protocolVersion": "2025-06-18"})),
            "notifications/initialized" => None,
            "tools/list" => Some(json!({"tools": [
                {"name": "aws___call_aws", "inputSchema": {"type": "object",
                    "properties": {"cli_command": {"type": "string"}}}},
                {"name": "aws___get_presigned_url", "inputSchema": {"type": "object"}},
                {"name": "aws___search_documentation", "inputSchema": {"type": "object"}},
            ]})),
            _ => {
                Some(json!({"called": body["params"]["name"], "args": body["params"]["arguments"]}))
            }
        };
        let sid = format!("s-{access_key}");
        let reply =
            result.map(|result| json!({"jsonrpc": "2.0", "id": body["id"], "result": result}));
        let init = method == "initialize";
        seen.borrow_mut().push(Seen {
            method,
            session: get(SESSION_HEADER),
            access_key,
            body,
        });
        match reply {
            None => response(202, &[], ""),
            Some(reply) if init => response(200, &[(SESSION_HEADER, &sid)], &reply.to_string()),
            Some(reply) => response(200, &[], &reply.to_string()),
        }
    }

    #[test]
    fn profile_switching_routes_calls_per_profile() {
        let dir = std::env::temp_dir().join(format!("owlet-aws-switch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let (credentials, config) = (dir.join("credentials"), dir.join("config"));
        std::fs::write(
            &credentials,
            "[a]\naws_access_key_id = AKIDA\naws_secret_access_key = x\n\
             [b]\naws_access_key_id = AKIDB\naws_secret_access_key = y\n",
        )
        .expect("write");
        std::fs::write(&config, "[profile b]\nregion = eu-central-1\n").expect("write");
        let files = [credentials, config].map(|p| p.to_str().expect("utf-8").to_owned());
        let env = |k: &str| match k {
            "AWS_SHARED_CREDENTIALS_FILE" => Some(files[0].clone()),
            "AWS_CONFIG_FILE" => Some(files[1].clone()),
            _ => None,
        };
        let parse = |profiles: &[&str]| {
            let mut args = vec!["https://aws-mcp.us-east-1.api.aws/mcp", "--profile"];
            args.extend(profiles);
            let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
            cli::parse(&args, &env).expect("options")
        };
        let opts = parse(&["a", "b", "c"]);
        let date = now();
        let seen = RefCell::new(Vec::new());
        let mut send =
            |_: &str, _: &str, h: &[(String, String)], b: &[u8]| Ok(profile_server(&seen, h, b));
        let mut session = Session::default();
        let init = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"clientInfo":{"name":"owlet"}}}"#;
        handle(
            Ok(&opts),
            &mut session,
            init,
            &env,
            &mut sigv4::Cache::new(offline),
            date,
            &mut send,
        );
        assert_eq!(session.id.as_deref(), Some("s-AKIDA"));
        assert!(
            !session
                .init
                .as_deref()
                .unwrap_or_default()
                .contains("_meta")
        );

        // tools/list: only auth tools get the parameter.
        let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let reply = handle(
            Ok(&opts),
            &mut session,
            list,
            &env,
            &mut sigv4::Cache::new(offline),
            date,
            &mut send,
        );
        let reply: Value = serde_json::from_str(&reply).expect("json");
        let tools = &reply["result"]["tools"];
        let param = json!({"type": "string",
            "description": "AWS CLI profile to sign this request with. Available profiles: a, b, c.",
            "enum": ["a", "b", "c"]});
        assert_eq!(tools[0]["inputSchema"]["properties"]["aws_profile"], param);
        assert_eq!(
            tools[0]["inputSchema"]["properties"]["cli_command"]["type"],
            "string"
        );
        assert_eq!(
            tools[1]["inputSchema"]["properties"],
            json!({"aws_profile": param})
        );
        assert_eq!(tools[2]["inputSchema"], json!({"type": "object"}));

        let call = |tool: &str, profile: &str| {
            json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                "params": {"name": tool, "arguments": {"cli_command": "aws s3 ls", "aws_profile": profile}}})
            .to_string()
        };
        let echoed = |tool: &str| {
            json!({"jsonrpc": "2.0", "id": 9,
                "result": {"called": tool, "args": {"cli_command": "aws s3 ls"}}})
        };
        let parse_reply = |reply: &str| serde_json::from_str::<Value>(reply).expect("json");
        seen.borrow_mut().clear();

        // Profile b: a new session, initialized and called with b's identity.
        let reply = handle(
            Ok(&opts),
            &mut session,
            &call("aws___call_aws", "b"),
            &env,
            &mut sigv4::Cache::new(offline),
            date,
            &mut send,
        );
        assert_eq!(parse_reply(&reply), echoed("aws___call_aws"));
        {
            let seen = seen.borrow();
            let methods: Vec<&str> = seen.iter().map(|s| s.method.as_str()).collect();
            assert_eq!(
                methods,
                ["initialize", "notifications/initialized", "tools/call"]
            );
            assert!(seen.iter().all(|s| s.access_key == "AKIDB"));
            assert!(
                seen.iter()
                    .all(|s| s.body["params"]["_meta"]["AWS_REGION"] == "eu-central-1")
            );
            assert_eq!(seen[0].session, None);
            assert_eq!(seen[0].body["params"]["clientInfo"]["name"], "owlet");
            assert_eq!(
                seen[2].session.as_deref(),
                Some("s-AKIDB"),
                "b's own session id"
            );
        }
        assert_eq!(session.id.as_deref(), Some("s-AKIDA"));
        assert_eq!(session.profiles["b"].id.as_deref(), Some("s-AKIDB"));
        seen.borrow_mut().clear();

        // Reused without another handshake.
        handle(
            Ok(&opts),
            &mut session,
            &call("aws___call_aws", "b"),
            &env,
            &mut sigv4::Cache::new(offline),
            date,
            &mut send,
        );
        assert_eq!(seen.borrow().len(), 1);
        seen.borrow_mut().clear();

        // The default profile and non-auth tools use the main session, argument stripped.
        for (tool, profile) in [("aws___call_aws", "a"), ("aws___search_documentation", "b")] {
            let reply = handle(
                Ok(&opts),
                &mut session,
                &call(tool, profile),
                &env,
                &mut sigv4::Cache::new(offline),
                date,
                &mut send,
            );
            assert_eq!(parse_reply(&reply), echoed(tool));
            let seen = seen.borrow();
            let last = seen.last().expect("sent");
            assert_eq!(
                (last.access_key.as_str(), last.session.as_deref()),
                ("AKIDA", Some("s-AKIDA"))
            );
            assert_eq!(last.body["params"]["_meta"]["AWS_REGION"], "us-east-1");
        }
        assert_eq!(seen.borrow().len(), 2);
        assert_eq!(session.profiles.len(), 1);

        let tool_text = |reply: &str| {
            let reply = parse_reply(reply);
            assert_eq!(reply["result"]["isError"], true);
            reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        };
        let reply = handle(
            Ok(&opts),
            &mut session,
            &call("aws___call_aws", "zzz"),
            &env,
            &mut sigv4::Cache::new(offline),
            date,
            &mut send,
        );
        assert_eq!(
            tool_text(&reply),
            "Profile 'zzz' is not in the allowed list. Allowed profiles: a, b, c"
        );
        // c has no credentials: the connection cannot be created.
        let reply = handle(
            Ok(&opts),
            &mut session,
            &call("aws___call_aws", "c"),
            &env,
            &mut sigv4::Cache::new(offline),
            date,
            &mut send,
        );
        assert!(
            tool_text(&reply).starts_with("Failed to create connection for profile 'c'."),
            "{reply}"
        );
        assert_eq!(seen.borrow().len(), 2, "nothing sent");
        assert!(!session.profiles.contains_key("c"));

        // A failed call drops the profile's session.
        let reply = handle(
            Ok(&opts),
            &mut session,
            &call("aws___call_aws", "b"),
            &env,
            &mut sigv4::Cache::new(offline),
            date,
            |_, _, _, _| Err("connection reset".to_owned()),
        );
        assert!(
            tool_text(&reply).starts_with("Tool call failed using profile 'b'."),
            "{reply}"
        );
        assert!(session.profiles.is_empty());

        // A single profile: no parameter, the argument is forwarded untouched.
        let single = parse(&["a"]);
        let reply = handle(
            Ok(&single),
            &mut session,
            list,
            &env,
            &mut sigv4::Cache::new(offline),
            date,
            &mut send,
        );
        assert!(!reply.contains("aws_profile"), "{reply}");
        handle(
            Ok(&single),
            &mut session,
            &call("aws___call_aws", "b"),
            &env,
            &mut sigv4::Cache::new(offline),
            date,
            &mut send,
        );
        let seen = seen.borrow();
        let last = seen.last().expect("sent");
        assert_eq!(last.access_key, "AKIDA");
        assert_eq!(last.body["params"]["arguments"]["aws_profile"], "b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Temp credentials (`a`: AKIDA, `b`: AKIDB) and config (`b` in
    /// eu-central-1), and an env pointing at them.
    fn profile_files(name: &str) -> (std::path::PathBuf, [String; 2]) {
        let dir = std::env::temp_dir().join(format!("owlet-aws-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let (credentials, config) = (dir.join("credentials"), dir.join("config"));
        std::fs::write(
            &credentials,
            "[a]\naws_access_key_id = AKIDA\naws_secret_access_key = x\n\
             [b]\naws_access_key_id = AKIDB\naws_secret_access_key = y\n",
        )
        .expect("write");
        std::fs::write(&config, "[profile b]\nregion = eu-central-1\n").expect("write");
        let files = [credentials, config].map(|p| p.to_str().expect("utf-8").to_owned());
        (dir, files)
    }

    fn profile_call(tool: &str, profile: &str) -> String {
        json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
            "params": {"name": tool, "arguments": {"cli_command": "aws s3 ls", "aws_profile": profile}}})
        .to_string()
    }

    #[test]
    fn profile_session_replays_on_404_and_is_ended_on_failure() {
        let (dir, files) = profile_files("profile-404");
        let env = |k: &str| match k {
            "AWS_SHARED_CREDENTIALS_FILE" => Some(files[0].clone()),
            "AWS_CONFIG_FILE" => Some(files[1].clone()),
            _ => None,
        };
        let args = [
            "https://aws-mcp.us-east-1.api.aws/mcp",
            "--profile",
            "a",
            "b",
        ];
        let opts = cli::parse(&args.map(str::to_owned), &env).expect("options");
        let date = now();
        let seen = RefCell::new(Vec::new());
        // Status the next request on b's session gets instead of a reply.
        let fail = RefCell::new(None::<u16>);
        let deletes = RefCell::new(Vec::new());
        let mut send = |method: &str, _: &str, h: &[(String, String)], b: &[u8]| {
            let get = |name: &str| h.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
            if method == "DELETE" {
                deletes.borrow_mut().push((
                    get(SESSION_HEADER),
                    get("authorization").unwrap_or_default(),
                ));
                return Ok(response(204, &[], ""));
            }
            if get(SESSION_HEADER).as_deref() == Some("s-AKIDB")
                && let Some(status) = fail.borrow_mut().take()
            {
                return Ok(response(status, &[], "gone"));
            }
            Ok(profile_server(&seen, h, b))
        };
        let mut session = Session::default();
        let init = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"clientInfo":{"name":"owlet"}}}"#;
        for msg in [init, &profile_call("aws___call_aws", "b")] {
            handle(
                Ok(&opts),
                &mut session,
                msg,
                &env,
                &mut sigv4::Cache::new(offline),
                date,
                &mut send,
            );
        }
        assert_eq!(session.profiles["b"].id.as_deref(), Some("s-AKIDB"));
        seen.borrow_mut().clear();

        // b's session expires: its copied initialize is replayed with b's identity.
        *fail.borrow_mut() = Some(404);
        let reply = handle(
            Ok(&opts),
            &mut session,
            &profile_call("aws___call_aws", "b"),
            &env,
            &mut sigv4::Cache::new(offline),
            date,
            &mut send,
        );
        let reply: Value = serde_json::from_str(&reply).expect("json");
        assert_eq!(reply["result"]["called"], "aws___call_aws", "{reply}");
        {
            let seen = seen.borrow();
            let methods: Vec<&str> = seen.iter().map(|s| s.method.as_str()).collect();
            assert_eq!(
                methods,
                ["initialize", "notifications/initialized", "tools/call"]
            );
            assert!(seen.iter().all(|s| s.access_key == "AKIDB"));
            assert_eq!(seen[0].session, None);
            assert_eq!(seen[0].body["params"]["clientInfo"]["name"], "owlet");
            assert_eq!(
                seen[0].body["params"]["_meta"]["AWS_REGION"],
                "eu-central-1"
            );
        }
        assert_eq!(session.id.as_deref(), Some("s-AKIDA"), "default untouched");
        assert!(deletes.borrow().is_empty());

        // A failed call ends b's session upstream before dropping it.
        *fail.borrow_mut() = Some(500);
        let reply = handle(
            Ok(&opts),
            &mut session,
            &profile_call("aws___call_aws", "b"),
            &env,
            &mut sigv4::Cache::new(offline),
            date,
            &mut send,
        );
        assert!(
            reply.contains("Tool call failed using profile 'b'."),
            "{reply}"
        );
        assert!(session.profiles.is_empty());
        let deletes = deletes.borrow();
        assert_eq!(deletes.len(), 1);
        assert_eq!(deletes[0].0.as_deref(), Some("s-AKIDB"));
        assert!(
            deletes[0].1.contains("Credential=AKIDB/"),
            "{}",
            deletes[0].1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn half_open_replay_is_ended() {
        let opts = options();
        let mut session = Session {
            id: Some("s1".to_owned()),
            init: Some(r#"{"jsonrpc":"2.0","id":0,"method":"initialize"}"#.to_owned()),
            ..Session::default()
        };
        let req = r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#;
        let mut sent = Vec::new();
        let reply = handle(
            Ok(&opts),
            &mut session,
            req,
            &creds,
            &mut sigv4::Cache::new(offline),
            now(),
            |method, _, h, b| {
                let sid = h
                    .iter()
                    .find(|(k, _)| k == SESSION_HEADER)
                    .map(|(_, v)| v.clone());
                let msg: Value = serde_json::from_slice(b).unwrap_or_default();
                sent.push((method.to_owned(), msg["method"].clone(), sid.clone()));
                Ok(match (msg["method"].as_str(), sid.as_deref()) {
                    (Some("initialize"), _) => response(
                        200,
                        &[(SESSION_HEADER, "s2")],
                        r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2025-06-18"}}"#,
                    ),
                    (Some("notifications/initialized"), _) => response(503, &[], ""),
                    (None, _) => response(204, &[], ""),
                    (_, _) => response(404, &[], ""),
                })
            },
        );
        assert!(
            reply.contains("notifications/initialized: HTTP 503"),
            "{reply}"
        );
        let s2 = Some("s2".to_owned());
        assert_eq!(
            sent,
            [
                (
                    "POST".to_owned(),
                    json!("tools/list"),
                    Some("s1".to_owned())
                ),
                ("POST".to_owned(), json!("initialize"), None),
                (
                    "POST".to_owned(),
                    json!("notifications/initialized"),
                    s2.clone()
                ),
                ("DELETE".to_owned(), Value::Null, s2),
            ]
        );
        assert!(session.id.is_none() && session.expired && session.init.is_some());
    }

    #[test]
    fn read_only_and_profile_switching_combine() {
        let (dir, files) = profile_files("profile-ro");
        let env = |k: &str| match k {
            "AWS_SHARED_CREDENTIALS_FILE" => Some(files[0].clone()),
            "AWS_CONFIG_FILE" => Some(files[1].clone()),
            _ => None,
        };
        let args = [
            "https://aws-mcp.us-east-1.api.aws/mcp",
            "--read-only",
            "--profile",
            "a",
            "b",
        ];
        let opts = cli::parse(&args.map(str::to_owned), &env).expect("options");
        let date = now();
        let seen = RefCell::new(Vec::new());
        let mut send = |_: &str, _: &str, h: &[(String, String)], b: &[u8]| {
            let msg: Value = serde_json::from_slice(b).expect("json");
            if msg["method"] == "tools/list" {
                let tools = json!([
                    {"name": "aws___call_aws", "annotations": {"readOnlyHint": true},
                        "inputSchema": {"type": "object"}},
                    {"name": "aws___run_script", "inputSchema": {"type": "object"}},
                ]);
                let reply = json!({"jsonrpc": "2.0", "id": msg["id"], "result": {"tools": tools}});
                return Ok(response(200, &[], &reply.to_string()));
            }
            Ok(profile_server(&seen, h, b))
        };
        let mut session = Session::default();
        let init = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#;
        let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let mut run = |msg: &str| {
            let reply = handle(
                Ok(&opts),
                &mut session,
                msg,
                &env,
                &mut sigv4::Cache::new(offline),
                date,
                &mut send,
            );
            serde_json::from_str::<Value>(&reply).expect("json")
        };
        run(init);

        // Both transforms: write tools hidden, auth tools gain aws_profile.
        let reply = run(list);
        let tools = reply["result"]["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 1, "{reply}");
        assert_eq!(tools[0]["name"], "aws___call_aws");
        assert_eq!(
            tools[0]["inputSchema"]["properties"]["aws_profile"]["enum"],
            json!(["a", "b"])
        );
        seen.borrow_mut().clear();

        // A write tool is denied before profile routing: nothing reaches b.
        let reply = run(&profile_call("aws___run_script", "b"));
        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(
            reply["result"]["content"][0]["text"],
            "Tool 'aws___run_script' is not available in read-only mode."
        );
        assert!(seen.borrow().is_empty());

        // A read-only tool is routed to b.
        let reply = run(&profile_call("aws___call_aws", "b"));
        assert_eq!(reply["result"]["called"], "aws___call_aws", "{reply}");
        {
            let seen = seen.borrow();
            assert!(!seen.is_empty() && seen.iter().all(|s| s.access_key == "AKIDB"));
        }
        assert!(session.profiles.contains_key("b"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shutdown_deletes_every_live_session() {
        let dir = std::env::temp_dir().join(format!("owlet-aws-shutdown-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let credentials = dir.join("credentials");
        std::fs::write(
            &credentials,
            "[a]\naws_access_key_id = AKIDA\naws_secret_access_key = x\n\
             [b]\naws_access_key_id = AKIDB\naws_secret_access_key = y\n",
        )
        .expect("write");
        let file = credentials.to_str().expect("utf-8").to_owned();
        let env = |k: &str| (k == "AWS_SHARED_CREDENTIALS_FILE").then(|| file.clone());
        let args = [
            "https://aws-mcp.us-east-1.api.aws/mcp",
            "--profile",
            "a",
            "b",
            "c",
        ]
        .map(str::to_owned);
        let opts = cli::parse(&args, &env).expect("options");
        let live = |id: Option<&str>| Session {
            id: id.map(str::to_owned),
            version: Some("2025-06-18".to_owned()),
            ..Session::default()
        };
        let mut session = live(Some("s-a"));
        session.profiles.insert("b".to_owned(), live(Some("s-b")));
        // Never initialized: nothing to end.
        session.profiles.insert("c".to_owned(), live(None));

        let seen = RefCell::new(Vec::new());
        shutdown(
            &opts,
            &session,
            &env,
            &mut sigv4::Cache::new(offline),
            now(),
            |method, url, h: &[(String, String)], body| {
                let get = |name: &str| h.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
                assert_eq!(url, "https://aws-mcp.us-east-1.api.aws/mcp");
                assert!(body.is_empty());
                assert_eq!(get(VERSION_HEADER).as_deref(), Some("2025-06-18"));
                seen.borrow_mut().push((
                    method.to_owned(),
                    get(SESSION_HEADER),
                    get("authorization").unwrap_or_default(),
                ));
                // Failures are ignored: the next session is still ended.
                Err("connection reset".to_owned())
            },
        );
        let seen = seen.borrow();
        assert_eq!(seen.len(), 2, "{seen:?}");
        for ((method, sid, auth), (want_sid, key)) in
            seen.iter().zip([("s-a", "AKIDA"), ("s-b", "AKIDB")])
        {
            assert_eq!(method, "DELETE");
            assert_eq!(sid.as_deref(), Some(want_sid));
            assert!(auth.contains(&format!("Credential={key}/")), "{auth}");
        }

        // No session at all: no request.
        shutdown(
            &opts,
            &Session::default(),
            &env,
            &mut sigv4::Cache::new(offline),
            now(),
            |_, _, _, _| panic!("must not be called"),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
