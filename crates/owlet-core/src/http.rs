use std::{path::PathBuf, sync::Arc};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use crate::{
    aggregate::Aggregator,
    config::Config,
    hub::{Hub, HubError, Session},
    jsonrpc::{self, Kind},
};

const SESSION_HEADER: &str = "mcp-session-id";

#[derive(Clone)]
struct AppState {
    hub: Arc<Hub>,
    agg: Arc<Aggregator>,
    token: Option<Arc<str>>,
    /// Config file that `?persist=true` admin calls write back to.
    config_path: Arc<PathBuf>,
    /// Serializes config file edits.
    config_lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Debug, Deserialize)]
struct AdminQuery {
    #[serde(default)]
    persist: bool,
}

#[derive(Debug, Deserialize)]
struct McpQuery {
    project: Option<String>,
}

/// Builds the HTTP router: `/mcp` (all servers' tools behind one endpoint),
/// `/mcp/{server}` (Streamable HTTP, JSON responses only), `/status`, and `/admin/servers/{server}/{enable|disable}`.
///
/// # Examples
///
/// ```ignore
/// let app = router(hub, Some("secret".into()), config_path);
/// axum::serve(listener, app).await?;
/// ```
pub fn router(hub: Arc<Hub>, token: Option<String>, config_path: PathBuf) -> Router {
    let state = AppState {
        agg: Arc::new(Aggregator::new(Arc::clone(&hub))),
        hub,
        token: token.map(Arc::from),
        config_path: Arc::new(config_path),
        config_lock: Arc::default(),
    };
    Router::new()
        .route("/mcp", post(post_agg).get(get_mcp).delete(delete_agg))
        .route(
            "/mcp/{server}",
            post(post_mcp).get(get_mcp).delete(delete_mcp),
        )
        .route("/status", get(status))
        .route("/admin/servers/{server}/{action}", post(admin_toggle))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Bearer-token check plus Origin validation (DNS-rebinding protection).
async fn guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if let Some(origin) = req.headers().get(header::ORIGIN)
        && !origin_allowed(origin)
    {
        return text(StatusCode::FORBIDDEN, "origin not allowed");
    }
    if let Some(expected) = &state.token {
        let provided = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default();
        if !bool::from(provided.as_bytes().ct_eq(expected.as_bytes())) {
            return text(StatusCode::UNAUTHORIZED, "invalid or missing bearer token");
        }
    }
    next.run(req).await
}

fn origin_allowed(origin: &HeaderValue) -> bool {
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    if origin == "null" {
        return false;
    }
    let host = origin
        .split_once("://")
        .map_or(origin, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default();
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

async fn status(State(state): State<AppState>) -> Json<Value> {
    Json(state.hub.status())
}

async fn admin_toggle(
    State(state): State<AppState>,
    Path((server, action)): Path<(String, String)>,
    Query(query): Query<AdminQuery>,
) -> Response {
    let enabled = match action.as_str() {
        "enable" => true,
        "disable" => false,
        _ => {
            return text(
                StatusCode::NOT_FOUND,
                "action must be `enable` or `disable`",
            );
        }
    };
    let changed = match state.hub.set_enabled(&server, enabled).await {
        Ok(changed) => changed,
        Err(e @ HubError::UnknownServer(_)) => return text(StatusCode::NOT_FOUND, &e.to_string()),
        Err(e) => return text(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    if query.persist {
        let _guard = state.config_lock.lock().await;
        let path = Arc::clone(&state.config_path);
        let name = server.clone();
        let result =
            tokio::task::spawn_blocking(move || Config::persist_enabled(&path, &name, enabled))
                .await;
        let err = match result {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(format!("{e:#}")),
            Err(e) => Some(e.to_string()),
        };
        if let Some(err) = err {
            // Runtime state already changed; report that the file did not.
            let body = json!({
                "server": server, "enabled": enabled, "changed": changed,
                "persisted": false, "error": err,
            });
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response();
        }
    }
    Json(json!({
        "server": server, "enabled": enabled, "changed": changed, "persisted": query.persist,
    }))
    .into_response()
}

async fn post_mcp(
    State(state): State<AppState>,
    Path(server): Path<String>,
    Query(query): Query<McpQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (batch, messages) = match parse_messages(&body) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    match check_initialize(&messages) {
        Ok(true) => {
            return initialize(&state, &server, query.project.as_deref(), &messages[0]).await;
        }
        Ok(false) => {}
        Err(resp) => return *resp,
    }

    let session = match session_from(&state, &server, &headers) {
        Ok(s) => s,
        Err(resp) => return *resp,
    };

    let mut requests = Vec::new();
    for msg in messages {
        match jsonrpc::classify(&msg) {
            Kind::Request => requests.push(msg),
            Kind::Notification => state.hub.notify(&session, &msg),
            // owlet answers server-initiated requests itself, so client responses are stray.
            Kind::Response | Kind::Invalid => {}
        }
    }
    if requests.is_empty() {
        return StatusCode::ACCEPTED.into_response();
    }

    let hub = &state.hub;
    let responses =
        futures_util::future::join_all(requests.into_iter().map(|m| hub.request(&session, m)))
            .await;
    let body = match (batch, responses.len()) {
        (false, 1) => responses.into_iter().next().unwrap_or(Value::Null),
        _ => Value::Array(responses),
    };
    Json(body).into_response()
}

/// Parses a POST body into JSON-RPC messages; the flag tells whether it was a batch.
fn parse_messages(body: &[u8]) -> Result<(bool, Vec<Value>), Box<Response>> {
    let parsed: Value = serde_json::from_slice(body).map_err(|e| {
        let err = jsonrpc::error_response(Value::Null, jsonrpc::PARSE_ERROR, &e.to_string());
        Box::new((StatusCode::BAD_REQUEST, Json(err)).into_response())
    })?;
    let batch = parsed.is_array();
    let messages = match parsed {
        Value::Array(items) if items.is_empty() => {
            return Err(Box::new(rpc_error(
                StatusCode::BAD_REQUEST,
                jsonrpc::INVALID_REQUEST,
                "empty batch",
            )));
        }
        Value::Array(items) => items,
        single => vec![single],
    };
    if messages
        .iter()
        .any(|m| jsonrpc::classify(m) == Kind::Invalid)
    {
        return Err(Box::new(rpc_error(
            StatusCode::BAD_REQUEST,
            jsonrpc::INVALID_REQUEST,
            "invalid JSON-RPC message",
        )));
    }
    Ok((batch, messages))
}

/// Returns whether the POST is an `initialize`, which must be sent alone.
fn check_initialize(messages: &[Value]) -> Result<bool, Box<Response>> {
    if !messages
        .iter()
        .any(|m| jsonrpc::method(m) == Some("initialize"))
    {
        return Ok(false);
    }
    if messages.len() != 1 || jsonrpc::classify(&messages[0]) != Kind::Request {
        return Err(Box::new(rpc_error(
            StatusCode::BAD_REQUEST,
            jsonrpc::INVALID_REQUEST,
            "initialize must be sent alone",
        )));
    }
    Ok(true)
}

/// `POST /mcp`: one endpoint exposing every enabled server's tools.
async fn post_agg(
    State(state): State<AppState>,
    Query(query): Query<McpQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (batch, messages) = match parse_messages(&body) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    match check_initialize(&messages) {
        Ok(true) => {
            let msg = &messages[0];
            return match state.agg.initialize(query.project.as_deref(), msg) {
                Ok((session, resp)) => {
                    let mut response = Json(resp).into_response();
                    if let Ok(v) = HeaderValue::from_str(&session.id) {
                        response.headers_mut().insert(SESSION_HEADER, v);
                    }
                    response
                }
                Err(e) => hub_error(e, msg.get("id").cloned()),
            };
        }
        Ok(false) => {}
        Err(resp) => return *resp,
    }

    let Some(session) = headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|id| state.agg.session(id))
    else {
        return rpc_error(
            StatusCode::NOT_FOUND,
            jsonrpc::INVALID_REQUEST,
            "unknown or missing session; send initialize first",
        );
    };

    let mut requests = Vec::new();
    for msg in messages {
        match jsonrpc::classify(&msg) {
            Kind::Request => requests.push(msg),
            Kind::Notification => state.agg.notify(&session, &msg),
            Kind::Response | Kind::Invalid => {}
        }
    }
    if requests.is_empty() {
        return StatusCode::ACCEPTED.into_response();
    }
    let agg = &state.agg;
    let responses =
        futures_util::future::join_all(requests.into_iter().map(|m| agg.request(&session, m)))
            .await;
    let body = match (batch, responses.len()) {
        (false, 1) => responses.into_iter().next().unwrap_or(Value::Null),
        _ => Value::Array(responses),
    };
    Json(body).into_response()
}

async fn delete_agg(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(id) = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) else {
        return rpc_error(
            StatusCode::BAD_REQUEST,
            jsonrpc::INVALID_REQUEST,
            "missing Mcp-Session-Id",
        );
    };
    if state.agg.close_session(id).await {
        StatusCode::NO_CONTENT.into_response()
    } else {
        rpc_error(
            StatusCode::NOT_FOUND,
            jsonrpc::INVALID_REQUEST,
            "unknown session",
        )
    }
}

async fn initialize(
    state: &AppState,
    server: &str,
    project: Option<&str>,
    msg: &Value,
) -> Response {
    match state.hub.initialize(server, project, msg).await {
        Ok((session, resp)) => {
            let mut response = Json(resp).into_response();
            if let Ok(v) = HeaderValue::from_str(&session.id) {
                response.headers_mut().insert(SESSION_HEADER, v);
            }
            response
        }
        Err(e) => hub_error(e, msg.get("id").cloned()),
    }
}

/// No server-initiated stream is offered (spec: GET may return 405).
async fn get_mcp() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST, DELETE")],
    )
        .into_response()
}

async fn delete_mcp(
    State(state): State<AppState>,
    Path(server): Path<String>,
    headers: HeaderMap,
) -> Response {
    let session = match session_from(&state, &server, &headers) {
        Ok(s) => s,
        Err(resp) => return *resp,
    };
    state.hub.close_session(&session.id).await;
    StatusCode::NO_CONTENT.into_response()
}

fn session_from(
    state: &AppState,
    server: &str,
    headers: &HeaderMap,
) -> Result<Arc<Session>, Box<Response>> {
    let Some(id) = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) else {
        return Err(Box::new(rpc_error(
            StatusCode::BAD_REQUEST,
            jsonrpc::INVALID_REQUEST,
            "missing Mcp-Session-Id; send initialize first",
        )));
    };
    match state.hub.session(id) {
        Some(s) if s.server == server => Ok(s),
        // 404 tells the client to re-initialize.
        _ => Err(Box::new(rpc_error(
            StatusCode::NOT_FOUND,
            jsonrpc::INVALID_REQUEST,
            "unknown session",
        ))),
    }
}

fn hub_error(e: HubError, id: Option<Value>) -> Response {
    let (status, code) = match &e {
        HubError::UnknownServer(_) => (StatusCode::NOT_FOUND, jsonrpc::INVALID_REQUEST),
        HubError::Disabled(_) => (StatusCode::SERVICE_UNAVAILABLE, jsonrpc::INVALID_REQUEST),
        HubError::BadProject(_) => (StatusCode::BAD_REQUEST, jsonrpc::INVALID_REQUEST),
        HubError::Upstream(_) => (StatusCode::BAD_GATEWAY, jsonrpc::INTERNAL_ERROR),
    };
    let body = jsonrpc::error_response(id.unwrap_or(Value::Null), code, &e.to_string());
    (status, Json(body)).into_response()
}

fn rpc_error(status: StatusCode, code: i64, message: &str) -> Response {
    (
        status,
        Json(jsonrpc::error_response(Value::Null, code, message)),
    )
        .into_response()
}

fn text(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_rules() {
        let ok = |s: &str| origin_allowed(&HeaderValue::from_str(s).expect("header"));
        assert!(ok("http://localhost:3000"));
        assert!(ok("http://127.0.0.1"));
        assert!(ok("http://[::1]:8808"));
        assert!(!ok("https://evil.com"));
        assert!(!ok("http://localhost.evil.com"));
        assert!(!ok("null"));
    }
}
