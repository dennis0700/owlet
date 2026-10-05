use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::{
    config::{Config, RemoteTransport, Scope, ServerConfig},
    jsonrpc,
    upstream::{NotifyHandler, SpawnSpec, Transport, Upstream},
};

const LIST_METHODS: [&str; 4] = [
    "tools/list",
    "prompts/list",
    "resources/list",
    "resources/templates/list",
];

/// Errors the HTTP layer maps to status codes.
#[derive(Debug)]
pub enum HubError {
    UnknownServer(String),
    Disabled(String),
    BadProject(String),
    Upstream(anyhow::Error),
}

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownServer(name) => write!(f, "unknown server `{name}`"),
            Self::Disabled(name) => write!(f, "server `{name}` is disabled"),
            Self::BadProject(msg) => write!(f, "{msg}"),
            Self::Upstream(e) => write!(f, "{e:#}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SlotKey {
    server: String,
    instance: String,
}

/// One logical upstream instance (shared, per project, or per session).
/// The process behind it is started lazily and stopped when idle; caches survive restarts.
pub struct Slot {
    key: SlotKey,
    spec: SpawnSpec,
    idle: Duration,
    cache_lists: bool,
    live: tokio::sync::Mutex<Option<Arc<Upstream>>>,
    init: Mutex<Option<Value>>,
    lists: Mutex<HashMap<String, Value>>,
    last_used: Mutex<Instant>,
    inflight: AtomicUsize,
}

impl Slot {
    fn new(key: SlotKey, spec: SpawnSpec, idle: Duration, cache_lists: bool) -> Arc<Self> {
        Arc::new(Self {
            key,
            spec,
            idle,
            cache_lists,
            live: tokio::sync::Mutex::new(None),
            init: Mutex::new(None),
            lists: Mutex::new(HashMap::new()),
            last_used: Mutex::new(Instant::now()),
            inflight: AtomicUsize::new(0),
        })
    }

    /// Returns the running process, spawning it if needed.
    async fn upstream(self: &Arc<Self>) -> anyhow::Result<Arc<Upstream>> {
        let mut live = self.live.lock().await;
        if let Some(up) = live.as_ref().filter(|up| up.is_alive()) {
            return Ok(Arc::clone(up));
        }
        let weak = Arc::downgrade(self);
        let handler: NotifyHandler = Arc::new(move |msg| {
            if let Some(slot) = weak.upgrade() {
                slot.on_notify(msg);
            }
        });
        let (up, init) = Upstream::spawn(&self.spec, handler).await?;
        *self.init.lock() = Some(init);
        *live = Some(Arc::clone(&up));
        Ok(up)
    }

    fn running(&self) -> Option<Arc<Upstream>> {
        self.live
            .try_lock()
            .ok()
            .and_then(|live| live.as_ref().filter(|up| up.is_alive()).cloned())
    }

    fn on_notify(&self, msg: Value) {
        let invalidate: &[&str] = match jsonrpc::method(&msg) {
            Some("notifications/tools/list_changed") => &["tools/list"],
            Some("notifications/prompts/list_changed") => &["prompts/list"],
            Some("notifications/resources/list_changed") => {
                &["resources/list", "resources/templates/list"]
            }
            _ => &[],
        };
        if !invalidate.is_empty() {
            let mut lists = self.lists.lock();
            for m in invalidate {
                lists.remove(*m);
            }
        }
    }

    fn touch(&self) {
        *self.last_used.lock() = Instant::now();
    }

    fn begin(&self) -> UseGuard<'_> {
        self.inflight.fetch_add(1, Ordering::SeqCst);
        self.touch();
        UseGuard(self)
    }

    async fn stop(&self) {
        let up = self.live.lock().await.take();
        if let Some(up) = up {
            info!(server = %self.key.server, instance = %self.key.instance, "stopping");
            up.shutdown().await;
        }
    }

    async fn stop_if_idle(&self, now: Instant) {
        let idle_for = now.saturating_duration_since(*self.last_used.lock());
        if self.inflight.load(Ordering::SeqCst) > 0 || idle_for < self.idle {
            return;
        }
        // Skip if a spawn is in progress.
        let Ok(mut live) = self.live.try_lock() else {
            return;
        };
        if let Some(up) = live.take() {
            drop(live);
            info!(
                server = %self.key.server,
                instance = %self.key.instance,
                "idle for {}s, stopping",
                idle_for.as_secs()
            );
            up.shutdown().await;
        }
    }
}

struct UseGuard<'a>(&'a Slot);

impl Drop for UseGuard<'_> {
    fn drop(&mut self) {
        self.0.touch();
        self.0.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A client connection identified by `Mcp-Session-Id`.
pub struct Session {
    pub id: String,
    pub server: String,
    slot: Arc<Slot>,
    /// True when the slot belongs exclusively to this session (`scope = "session"`).
    owned: bool,
    last_seen: Mutex<Instant>,
    /// Client request id (serialized) -> (upstream, upstream request id).
    inflight: Mutex<HashMap<String, (Weak<Upstream>, u64)>>,
}

struct InflightEntry<'a> {
    session: &'a Session,
    key: String,
}

impl Drop for InflightEntry<'_> {
    fn drop(&mut self) {
        self.session.inflight.lock().remove(&self.key);
    }
}

struct ServerEntry {
    cfg: ServerConfig,
    enabled: AtomicBool,
}

/// Owns every upstream slot and client session.
pub struct Hub {
    servers: HashMap<String, ServerEntry>,
    idle_timeout: Duration,
    session_ttl: Duration,
    request_timeout: Duration,
    slots: Mutex<HashMap<SlotKey, Arc<Slot>>>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl Hub {
    /// Builds a hub from configuration; no process is started until first use.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let hub = Arc::new(Hub::new(&Config::load(path)?));
    /// ```
    pub fn new(cfg: &Config) -> Self {
        Self {
            servers: cfg
                .servers
                .iter()
                .map(|(name, sc)| {
                    let entry = ServerEntry {
                        cfg: sc.clone(),
                        enabled: AtomicBool::new(sc.enabled),
                    };
                    (name.clone(), entry)
                })
                .collect(),
            idle_timeout: cfg.idle_timeout(),
            session_ttl: cfg.session_ttl(),
            request_timeout: cfg.request_timeout(),
            slots: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn slot_for(
        &self,
        server: &str,
        project: Option<&str>,
        session_id: &str,
    ) -> Result<(Arc<Slot>, bool), HubError> {
        let entry = self.entry(server)?;
        let sc = &entry.cfg;
        let project = project.map(canonical_project).transpose()?;
        let instance = match sc.scope {
            Scope::Shared => String::new(),
            Scope::Project => {
                let p = project.as_deref().ok_or_else(|| {
                    HubError::BadProject(format!(
                        "server `{server}` has scope = \"project\"; pass ?project=<abs path>"
                    ))
                })?;
                p.to_string_lossy().into_owned()
            }
            Scope::Session => format!("session:{session_id}"),
        };
        let key = SlotKey {
            server: server.to_owned(),
            instance,
        };
        // Shared servers ignore `project`; they always use their configured cwd.
        let project = match sc.scope {
            Scope::Shared => None,
            Scope::Project | Scope::Session => project,
        };
        let idle = sc
            .idle_timeout
            .map_or(self.idle_timeout, Duration::from_secs);
        let mut slots = self.slots.lock();
        // Checked under the slots lock so a concurrent disable cannot miss this slot.
        if !entry.enabled.load(Ordering::SeqCst) {
            return Err(HubError::Disabled(server.to_owned()));
        }
        let slot = slots.entry(key.clone()).or_insert_with(|| {
            Slot::new(
                key,
                spawn_spec(server, sc, project.as_deref()),
                idle,
                sc.cache_lists,
            )
        });
        Ok((Arc::clone(slot), sc.scope == Scope::Session))
    }

    fn entry(&self, server: &str) -> Result<&ServerEntry, HubError> {
        self.servers
            .get(server)
            .ok_or_else(|| HubError::UnknownServer(server.to_owned()))
    }

    /// Names of enabled servers, sorted. Servers with `scope = "project"` are
    /// only included when the caller has a project path.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let names = hub.enabled_servers(false);
    /// ```
    pub fn enabled_servers(&self, with_project: bool) -> Vec<String> {
        let mut names: Vec<String> = self
            .servers
            .iter()
            .filter(|(_, e)| e.enabled.load(Ordering::SeqCst))
            .filter(|(_, e)| with_project || e.cfg.scope != Scope::Project)
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    /// Validates a `?project=` value and returns its canonical form.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let project = Hub::resolve_project("/Users/me/work/repo")?;
    /// ```
    pub fn resolve_project(raw: &str) -> Result<String, HubError> {
        canonical_project(raw).map(|p| p.to_string_lossy().into_owned())
    }

    /// How long an idle client session is kept.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let ttl = hub.session_ttl();
    /// ```
    pub fn session_ttl(&self) -> Duration {
        self.session_ttl
    }

    fn is_enabled(&self, server: &str) -> bool {
        self.servers
            .get(server)
            .is_some_and(|e| e.enabled.load(Ordering::SeqCst))
    }

    /// Enables or disables a server at runtime. Disabling closes its sessions
    /// and stops its processes; returns whether the state changed.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// hub.set_enabled("fetch", false).await?;
    /// ```
    pub async fn set_enabled(&self, server: &str, enabled: bool) -> Result<bool, HubError> {
        let entry = self.entry(server)?;
        let (changed, slots) = {
            let mut slots = self.slots.lock();
            let changed = entry.enabled.swap(enabled, Ordering::SeqCst) != enabled;
            let removed: Vec<Arc<Slot>> = if enabled {
                Vec::new()
            } else {
                let keys: Vec<SlotKey> = slots
                    .keys()
                    .filter(|k| k.server == server)
                    .cloned()
                    .collect();
                keys.iter().filter_map(|k| slots.remove(k)).collect()
            };
            (changed, removed)
        };
        if !enabled {
            let closed = {
                let mut sessions = self.sessions.lock();
                let before = sessions.len();
                sessions.retain(|_, s| s.server != server);
                before - sessions.len()
            };
            for slot in &slots {
                slot.stop().await;
            }
            info!(
                server,
                sessions = closed,
                instances = slots.len(),
                "disabled"
            );
        } else if changed {
            info!(server, "enabled");
        }
        Ok(changed)
    }

    /// Handles a client `initialize`: creates a session and answers from the
    /// cached handshake when possible, so connecting does not start a process.
    pub async fn initialize(
        &self,
        server: &str,
        project: Option<&str>,
        msg: &Value,
    ) -> Result<(Arc<Session>, Value), HubError> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let (slot, owned) = self.slot_for(server, project, &id)?;

        let cached = slot.init.lock().clone();
        let init = match cached {
            Some(init) => init,
            None => {
                let _guard = slot.begin();
                slot.upstream().await.map_err(HubError::Upstream)?;
                slot.init.lock().clone().unwrap_or_else(|| json!({}))
            }
        };

        if !self.is_enabled(server) {
            return Err(HubError::Disabled(server.to_owned()));
        }
        let session = Arc::new(Session {
            id: id.clone(),
            server: server.to_owned(),
            slot,
            owned,
            last_seen: Mutex::new(Instant::now()),
            inflight: Mutex::new(HashMap::new()),
        });
        self.sessions.lock().insert(id, Arc::clone(&session));
        info!(server, session = %session.id, "client initialized");

        let client_id = msg.get("id").cloned().unwrap_or(Value::Null);
        Ok((session, jsonrpc::result_response(client_id, init)))
    }

    /// Looks up a session and refreshes its TTL.
    pub fn session(&self, id: &str) -> Option<Arc<Session>> {
        let session = self.sessions.lock().get(id).cloned()?;
        *session.last_seen.lock() = Instant::now();
        Some(session)
    }

    /// Forwards a client request and returns the response with the client's id restored.
    pub async fn request(&self, session: &Session, msg: Value) -> Value {
        let client_id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = jsonrpc::method(&msg).unwrap_or_default().to_owned();
        let slot = &session.slot;

        match method.as_str() {
            "ping" => return jsonrpc::result_response(client_id, json!({})),
            "initialize" => {
                return jsonrpc::error_response(
                    client_id,
                    jsonrpc::INVALID_REQUEST,
                    "session already initialized",
                );
            }
            _ => {}
        }
        if !self.is_enabled(&session.server) {
            let msg = HubError::Disabled(session.server.clone()).to_string();
            return jsonrpc::error_response(client_id, jsonrpc::INTERNAL_ERROR, &msg);
        }

        let cacheable = slot.cache_lists
            && LIST_METHODS.contains(&method.as_str())
            && msg.pointer("/params/cursor").is_none();
        if cacheable {
            let hit = slot.lists.lock().get(&method).cloned();
            if let Some(result) = hit {
                slot.touch();
                return jsonrpc::result_response(client_id, result);
            }
        }

        let _guard = slot.begin();
        let fail = |e: anyhow::Error| {
            warn!(server = %session.server, %method, "{e:#}");
            jsonrpc::error_response(
                client_id.clone(),
                jsonrpc::INTERNAL_ERROR,
                &format!("{e:#}"),
            )
        };
        let up = match slot.upstream().await {
            Ok(up) => up,
            Err(e) => return fail(e),
        };
        let call = match up.request(msg) {
            Ok(call) => call,
            Err(e) => return fail(e),
        };

        let entry = InflightEntry {
            session,
            key: client_id.to_string(),
        };
        session
            .inflight
            .lock()
            .insert(entry.key.clone(), (Arc::downgrade(&up), call.upstream_id()));
        let result = call.wait(self.request_timeout).await;
        drop(entry);

        match result {
            Ok(mut resp) => {
                if cacheable && let Some(result) = resp.get("result") {
                    slot.lists.lock().insert(method, result.clone());
                }
                resp["id"] = client_id;
                resp
            }
            Err(e) => fail(e),
        }
    }

    /// Handles a client notification. Never starts a process.
    pub fn notify(&self, session: &Session, msg: &Value) {
        match jsonrpc::method(msg).unwrap_or_default() {
            "notifications/cancelled" => {
                let Some(request_id) = msg.pointer("/params/requestId") else {
                    return;
                };
                let target = session
                    .inflight
                    .lock()
                    .get(&request_id.to_string())
                    .cloned();
                if let Some((up, upstream_id)) = target
                    && let Some(up) = up.upgrade()
                {
                    let reason = msg
                        .pointer("/params/reason")
                        .and_then(Value::as_str)
                        .unwrap_or("cancelled by client");
                    up.cancel(upstream_id, reason);
                }
            }
            // owlet performed the handshake and answers roots itself.
            "notifications/initialized" | "notifications/roots/list_changed" => {}
            _ => {
                if let Some(up) = session.slot.running() {
                    up.notify(msg);
                }
            }
        }
    }

    /// Ends a session; session-scoped processes are stopped immediately.
    pub async fn close_session(&self, id: &str) -> bool {
        let Some(session) = self.sessions.lock().remove(id) else {
            return false;
        };
        info!(server = %session.server, session = %id, "session closed");
        if session.owned {
            self.slots.lock().remove(&session.slot.key);
            session.slot.stop().await;
        }
        true
    }

    /// Stops idle processes and forgets expired sessions.
    pub async fn reap(&self) {
        let now = Instant::now();
        let expired: Vec<String> = self
            .sessions
            .lock()
            .values()
            .filter(|s| now.saturating_duration_since(*s.last_seen.lock()) > self.session_ttl)
            .map(|s| s.id.clone())
            .collect();
        for id in expired {
            self.close_session(&id).await;
        }
        let slots: Vec<Arc<Slot>> = self.slots.lock().values().cloned().collect();
        for slot in slots {
            slot.stop_if_idle(now).await;
        }
    }

    /// Runs [`Hub::reap`] periodically until the task is dropped.
    pub async fn reap_loop(self: Arc<Self>, every: Duration) {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            self.reap().await;
        }
    }

    /// Stops every running process.
    pub async fn shutdown(&self) {
        let slots: Vec<Arc<Slot>> = self.slots.lock().drain().map(|(_, s)| s).collect();
        for slot in slots {
            slot.stop().await;
        }
    }

    /// Snapshot of configured servers, instances and sessions.
    pub fn status(&self) -> Value {
        let sessions = self.sessions.lock();
        let slots = self.slots.lock();
        let instances: Vec<Value> = slots
            .values()
            .map(|slot| {
                let up = slot.running();
                let clients = sessions
                    .values()
                    .filter(|s| Arc::ptr_eq(&s.slot, slot))
                    .count();
                json!({
                    "server": slot.key.server,
                    "instance": slot.key.instance,
                    "running": up.is_some(),
                    "pid": up.and_then(|u| u.pid()),
                    "inflight": slot.inflight.load(Ordering::SeqCst),
                    "idle_secs": slot.last_used.lock().elapsed().as_secs(),
                    "sessions": clients,
                    "cached": slot.lists.lock().keys().cloned().collect::<Vec<_>>(),
                })
            })
            .collect();
        let mut servers: Vec<Value> = self
            .servers
            .iter()
            .map(|(name, e)| {
                json!({
                    "name": name,
                    "enabled": e.enabled.load(Ordering::SeqCst),
                    "transport": transport_name(&e.cfg),
                    "scope": e.cfg.scope,
                })
            })
            .collect();
        servers.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        json!({ "servers": servers, "instances": instances, "sessions": sessions.len() })
    }
}

fn transport_name(sc: &ServerConfig) -> &'static str {
    match (&sc.url, sc.transport.unwrap_or_default()) {
        (None, _) => "stdio",
        (Some(_), RemoteTransport::Http) => "http",
        (Some(_), RemoteTransport::Sse) => "sse",
    }
}

fn canonical_project(raw: &str) -> Result<PathBuf, HubError> {
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(HubError::BadProject(format!(
            "project must be an absolute path: {raw}"
        )));
    }
    let canonical = path
        .canonicalize()
        .map_err(|e| HubError::BadProject(format!("project {raw}: {e}")))?;
    if !canonical.is_dir() {
        return Err(HubError::BadProject(format!(
            "project is not a directory: {raw}"
        )));
    }
    Ok(canonical)
}

fn spawn_spec(name: &str, sc: &ServerConfig, project: Option<&Path>) -> SpawnSpec {
    let project_str = project.map(|p| p.to_string_lossy());
    let args = sc
        .args
        .iter()
        .map(|a| match &project_str {
            Some(p) => a.replace("{project}", p),
            None => a.clone(),
        })
        .collect();
    let cwd = project.map(Path::to_path_buf).or_else(|| sc.cwd.clone());
    // Config validation guarantees exactly one of `command` / `url`.
    let transport = match (&sc.command, &sc.url) {
        (_, Some(url)) => match sc.transport.unwrap_or_default() {
            RemoteTransport::Http => Transport::StreamableHttp {
                url: url.clone(),
                headers: sc.headers.clone(),
            },
            RemoteTransport::Sse => Transport::Sse {
                url: url.clone(),
                headers: sc.headers.clone(),
            },
        },
        (command, None) => Transport::Stdio {
            command: command.clone().unwrap_or_default(),
            args,
            env: sc.env.clone(),
            cwd: cwd.clone(),
        },
    };
    SpawnSpec {
        name: name.to_owned(),
        transport,
        root: cwd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub(toml: &str) -> Hub {
        Hub::new(&Config::parse(toml).expect("valid config"))
    }

    #[test]
    fn slots_follow_scope() {
        let hub = hub(r#"
            [servers.a]
            command = "x"
            [servers.p]
            command = "x"
            args = ["--root", "{project}"]
            scope = "project"
            [servers.s]
            command = "x"
            scope = "session"
            "#);
        let tmp = std::env::temp_dir();
        let tmp = tmp.to_str().expect("utf-8 temp dir");

        let (a1, owned) = hub.slot_for("a", Some(tmp), "s1").expect("slot");
        let (a2, _) = hub.slot_for("a", None, "s2").expect("slot");
        assert!(Arc::ptr_eq(&a1, &a2));
        assert!(!owned);
        assert!(a1.spec.root.is_none());

        let (p1, _) = hub.slot_for("p", Some(tmp), "s1").expect("slot");
        let (p2, _) = hub.slot_for("p", Some(tmp), "s2").expect("slot");
        assert!(Arc::ptr_eq(&p1, &p2));
        let canon = Path::new(tmp).canonicalize().expect("canonical");
        assert_eq!(p1.spec.root.as_deref(), Some(canon.as_path()));
        let Transport::Stdio { args, cwd, .. } = &p1.spec.transport else {
            panic!("expected stdio transport");
        };
        assert_eq!(cwd.as_deref(), Some(canon.as_path()));
        assert_eq!(args[1], canon.to_string_lossy());

        assert!(matches!(
            hub.slot_for("p", None, "s1"),
            Err(HubError::BadProject(_))
        ));
        assert!(matches!(
            hub.slot_for("p", Some("rel"), "s1"),
            Err(HubError::BadProject(_))
        ));
        assert!(matches!(
            hub.slot_for("nope", None, "s1"),
            Err(HubError::UnknownServer(_))
        ));

        let (s1, owned) = hub.slot_for("s", None, "s1").expect("slot");
        let (s2, _) = hub.slot_for("s", None, "s2").expect("slot");
        assert!(owned);
        assert!(!Arc::ptr_eq(&s1, &s2));
    }

    #[tokio::test]
    async fn disable_and_enable_at_runtime() {
        let hub =
            hub("[servers.a]\ncommand = \"x\"\n[servers.off]\ncommand = \"x\"\nenabled = false");
        assert!(matches!(
            hub.slot_for("off", None, "s"),
            Err(HubError::Disabled(_))
        ));

        let (slot, _) = hub.slot_for("a", None, "s1").expect("slot");
        hub.sessions.lock().insert(
            "s1".into(),
            Arc::new(Session {
                id: "s1".into(),
                server: "a".into(),
                slot,
                owned: false,
                last_seen: Mutex::new(Instant::now()),
                inflight: Mutex::new(HashMap::new()),
            }),
        );
        assert_eq!(hub.set_enabled("a", false).await.ok(), Some(true));
        assert!(hub.sessions.lock().is_empty());
        assert!(hub.slots.lock().is_empty());
        assert!(matches!(
            hub.slot_for("a", None, "s2"),
            Err(HubError::Disabled(_))
        ));
        assert_eq!(hub.set_enabled("a", false).await.ok(), Some(false));

        assert_eq!(hub.set_enabled("off", true).await.ok(), Some(true));
        assert!(hub.slot_for("off", None, "s3").is_ok());
        assert!(matches!(
            hub.set_enabled("nope", true).await,
            Err(HubError::UnknownServer(_))
        ));

        let status = hub.status();
        assert_eq!(status["servers"][0]["name"], "a");
        assert_eq!(status["servers"][0]["enabled"], false);
        assert_eq!(status["servers"][1]["transport"], "stdio");
    }

    #[test]
    fn list_changed_invalidates_cache() {
        let hub = hub("[servers.a]\ncommand = \"x\"");
        let (slot, _) = hub.slot_for("a", None, "s").expect("slot");
        slot.lists
            .lock()
            .insert("tools/list".into(), json!({"tools": []}));
        slot.lists
            .lock()
            .insert("prompts/list".into(), json!({"prompts": []}));
        slot.on_notify(json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}));
        assert!(!slot.lists.lock().contains_key("tools/list"));
        assert!(slot.lists.lock().contains_key("prompts/list"));
    }
}
