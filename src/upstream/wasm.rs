//! Host side of the WASM plugin transport.
//!
//! A plugin is a WebAssembly component that exports `handle: func(string) ->
//! string` and `on-shutdown: func()` (see `wit/mcp-plugin.wit`). owlet loads it, grants it a WASI
//! context, and for every outgoing MCP message calls `handle`, dispatching the
//! returned JSON messages back through the shared core.
//!
//! Capabilities are explicit: argv and environment come from the server config
//! (`args`, `env`, `inherit_env`), `dirs` are preopened read-only, and the only
//! network access is the `owlet:plugin/http` import implemented here with
//! reqwest. WASI sockets and IP name lookup stay disabled (the builder default).
//! The import itself has no destination policy: plugins are trusted code.
//!
//! A wasmtime `Store` is single-threaded and not `Send`, so each instance runs
//! on its own OS thread with a current-thread runtime and handles one message
//! at a time. The runtime is only entered to wait for the next message or to
//! perform an HTTP request from the import; `handle` itself is called outside
//! of it, so the synchronous import can `block_on` without nesting.
//!
//! Stop is honoured mid-call: an in-flight HTTP exchange is abandoned, and
//! running guest code traps at the next epoch tick (epoch interruption). The
//! epoch ticker thread starts with the first running plugin, not on
//! validation. A guest blocked inside a WASI host call (for example a
//! `wasi:clocks` sleep or poll through the sync linker) is not preempted: it
//! only traps once that call returns to guest code.
//!
//! When the instance stops while idle (or its input closes), the plugin's
//! `on-shutdown` export runs within [`SHUTDOWN_GRACE`]: `http.send` still works
//! until that deadline and running guest code traps once it has passed. An
//! instance stopped mid-call is not given `on-shutdown`.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{LazyLock, OnceLock},
    time::{Duration, SystemTime},
};

use anyhow::{Context, anyhow};
use parking_lot::Mutex;
use reqwest::{
    Client, Method,
    header::{HeaderName, HeaderValue},
};
use tokio::{
    runtime::Runtime,
    sync::oneshot::{self, error::TryRecvError},
    time::Instant,
};
use tracing::{info, warn};
use wasmtime::{
    Config, Engine, Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline,
    component::{Component, HasSelf, Linker},
};
use wasmtime_wasi::p2::add_to_linker_sync;
use wasmtime_wasi::{
    FsPerms, ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView,
};

use self::owlet::plugin::http;
use super::Wiring;

wasmtime::component::bindgen!({
    world: "mcp-plugin",
    path: "wit/mcp-plugin.wit",
});

/// Overall limit for one plugin HTTP exchange.
const HTTP_TIMEOUT: Duration = Duration::from_secs(100);
/// Largest response body handed to a plugin.
const MAX_BODY: usize = 16 * 1024 * 1024;
/// Engine epoch period: the longest a running guest ignores a stop request.
const EPOCH_TICK: Duration = Duration::from_millis(100);
/// How long the `on-shutdown` export may run, HTTP included.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Largest linear memory one plugin instance may grow to: a loose guard
/// against runaway plugins.
const MAX_MEMORY: usize = 1024 * 1024 * 1024;

/// Per-store state: the WASI context and HTTP client owlet grants the plugin.
struct Host {
    table: ResourceTable,
    wasi: WasiCtx,
    /// The plugin thread's runtime, used by the HTTP import.
    rt: Runtime,
    client: Client,
    /// Fires on shutdown; never polled again once `stopping` is set.
    stop: oneshot::Receiver<()>,
    stopping: bool,
    /// Set while the `on-shutdown` export runs: its deadline replaces `stop`.
    grace_until: Option<Instant>,
    limits: StoreLimits,
}

impl Host {
    /// Whether guest code must stop: shutdown was requested (or the
    /// `Upstream` was dropped), or the shutdown grace period has passed.
    fn stopped(&mut self) -> bool {
        if let Some(deadline) = self.grace_until {
            return Instant::now() >= deadline;
        }
        if !self.stopping {
            self.stopping = !matches!(self.stop.try_recv(), Err(TryRecvError::Empty));
        }
        self.stopping
    }
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl http::Host for Host {
    fn send(
        &mut self,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<http::Response, String> {
        let Self {
            rt,
            client,
            stop,
            stopping,
            grace_until,
            ..
        } = self;
        if let Some(deadline) = *grace_until {
            return rt.block_on(async {
                tokio::time::timeout_at(deadline, send(client, &method, &url, headers, body))
                    .await
                    .unwrap_or_else(|_| Err("plugin shutdown grace period expired".to_owned()))
            });
        }
        if *stopping {
            return Err("plugin is stopping".to_owned());
        }
        // Abandon the exchange on shutdown instead of waiting up to HTTP_TIMEOUT.
        rt.block_on(async {
            tokio::select! {
                biased;
                _ = stop => {
                    *stopping = true;
                    Err("plugin is stopping".to_owned())
                }
                resp = send(client, &method, &url, headers, body) => resp,
            }
        })
    }
}

/// Performs one plugin HTTP request; failures become the import's `Err`.
async fn send(
    client: &Client,
    method: &str,
    url: &str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Result<http::Response, String> {
    let method =
        Method::from_bytes(method.as_bytes()).map_err(|_| format!("invalid method `{method}`"))?;
    let mut req = client.request(method, url);
    for (k, v) in headers {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|_| format!("invalid header name `{k}`"))?;
        // Values may be credentials; never echo them.
        let mut value =
            HeaderValue::from_str(&v).map_err(|_| format!("invalid value for header `{k}`"))?;
        value.set_sensitive(true);
        req = req.header(name, value);
    }
    let mut resp = req
        .body(body)
        .send()
        .await
        .map_err(|e| format!("{:#}", anyhow::Error::from(e)))?;

    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_owned(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("read body: {:#}", anyhow::Error::from(e)))?
    {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(format!("response body exceeds {MAX_BODY} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(http::Response {
        status,
        headers,
        body,
    })
}

/// One shared engine for every plugin; building it is relatively expensive.
///
/// Epoch interruption is on; [`start_ticker`] bumps the epoch once a plugin
/// runs, and each store's deadline callback decides whether to trap.
fn engine() -> anyhow::Result<&'static Engine> {
    static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            Engine::new(Config::new().epoch_interruption(true)).map_err(|e| format!("{e:#}"))
        })
        .as_ref()
        .map_err(|e| anyhow!("create wasm engine: {e}"))
}

/// Starts (once per process) the background thread that bumps the engine's
/// epoch every [`EPOCH_TICK`]. Only [`start`] calls it, so validating or
/// installing a plugin spawns no thread.
fn start_ticker(engine: &Engine) {
    static TICKER: OnceLock<()> = OnceLock::new();
    TICKER.get_or_init(|| {
        let ticker = engine.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("owlet-wasm-epoch".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(EPOCH_TICK);
                    ticker.increment_epoch();
                }
            })
        {
            warn!("wasm epoch ticker not started; stop cannot preempt guests: {e}");
        }
    });
}

/// Checks that `bytes` is a WebAssembly component the shared engine can load.
/// Compiles the component, so call it off async worker threads.
///
/// # Examples
///
/// ```ignore
/// validate(&std::fs::read("aws-mcp.wasm")?)?;
/// ```
pub(crate) fn validate(bytes: &[u8]) -> anyhow::Result<()> {
    Component::from_binary(engine()?, bytes)
        .map(drop)
        .map_err(|e| e.context("not a loadable wasm component").into())
}

/// Compiled components by path, with the file's modification time.
static CACHE: LazyLock<Mutex<HashMap<PathBuf, (SystemTime, Component)>>> =
    LazyLock::new(Mutex::default);

/// Compiles the component at `path`, reusing the previous compilation while
/// the file's modification time is unchanged (e.g. `session`-scope spawns).
/// Each new compilation also drops entries whose file was removed or changed.
fn load(path: &Path) -> anyhow::Result<Component> {
    let modified = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .with_context(|| format!("stat {}", path.display()))?;
    if let Some((at, component)) = CACHE.lock().get(path)
        && *at == modified
    {
        return Ok(component.clone());
    }
    let component = Component::from_file(engine()?, path)
        .map_err(|e| e.context(format!("load wasm component {}", path.display())))?;
    let mut cache = CACHE.lock();
    cache.retain(|file, (at, _)| {
        std::fs::metadata(file)
            .and_then(|m| m.modified())
            .is_ok_and(|m| m == *at)
    });
    cache.insert(path.to_path_buf(), (modified, component.clone()));
    Ok(component)
}

/// Loads the component and starts the plugin's run loop on a dedicated thread.
///
/// `args` become the WASI argv after the server name; the environment is the
/// host variables matched by `inherit_env` overlaid with `env`; `dirs` are
/// preopened read-only at the same guest path. Compilation runs on the
/// blocking pool and is cached per path.
///
/// # Examples
///
/// ```ignore
/// wasm::start("aws-mcp", Path::new("aws-mcp.wasm"), &args, &env, &[], &[], wiring).await?;
/// ```
pub async fn start(
    name: &str,
    path: &Path,
    args: &[String],
    env: &HashMap<String, String>,
    inherit_env: &[String],
    dirs: &[PathBuf],
    wiring: Wiring,
) -> anyhow::Result<()> {
    let engine = engine()?;
    start_ticker(engine);
    let file = expand_home(path);
    let component = tokio::task::spawn_blocking(move || load(&file))
        .await
        .context("wasm compile task")??;

    let mut linker: Linker<Host> = Linker::new(engine);
    add_to_linker_sync(&mut linker).map_err(|e| e.context("add wasi to linker"))?;
    McpPlugin::add_to_linker::<Host, HasSelf<Host>>(&mut linker, |host| host)
        .map_err(|e| e.context("add owlet http to linker"))?;

    let wasi = wasi_ctx(name, args, env, inherit_env, dirs);
    let Wiring {
        reader,
        mut outgoing,
        stop,
        exited,
    } = wiring;
    let name = name.to_owned();

    std::thread::Builder::new()
        .name(format!("owlet-wasm-{name}"))
        .spawn(move || {
            let result = run(
                engine,
                &component,
                &linker,
                wasi,
                &name,
                &reader,
                &mut outgoing,
                stop,
            );
            if let Err(e) = result {
                warn!(server = %name, "wasm plugin stopped: {e:#}");
            }
            reader.inner.mark_dead();
            info!(server = %name, "wasm plugin exited");
            let _ = exited.send(true);
        })
        .context("spawn wasm plugin thread")?;
    Ok(())
}

/// Builds the WASI context from the server's explicit grants.
fn wasi_ctx(
    name: &str,
    args: &[String],
    env: &HashMap<String, String>,
    inherit_env: &[String],
    dirs: &[PathBuf],
) -> WasiCtx {
    let mut builder = WasiCtxBuilder::new();
    builder.arg(name).args(args);
    let vars = plugin_env(std::env::vars(), env, inherit_env);
    builder.envs(&vars);

    for dir in dirs {
        let host = expand_home(dir);
        let Some(guest) = host.to_str() else {
            warn!(server = %name, "skipping non-UTF-8 dir {}", host.display());
            continue;
        };
        if !host.is_dir() {
            warn!(server = %name, "skipping missing dir {}", host.display());
            continue;
        }
        if let Err(e) = builder.preopened_dir(&host, guest, FsPerms::ReadOnly) {
            warn!(server = %name, "skipping dir {}: {e:#}", host.display());
        }
    }
    builder.build()
}

/// Host variables matched by `inherit` (`NAME` or `PREFIX*`), overlaid with
/// `explicit`, sorted by name.
fn plugin_env(
    host: impl Iterator<Item = (String, String)>,
    explicit: &HashMap<String, String>,
    inherit: &[String],
) -> Vec<(String, String)> {
    let wanted = |key: &str| {
        inherit.iter().any(|pat| match pat.strip_suffix('*') {
            Some(prefix) => key.starts_with(prefix),
            None => key == pat,
        })
    };
    let mut vars: std::collections::BTreeMap<String, String> =
        host.filter(|(k, _)| wanted(k)).collect();
    vars.extend(explicit.iter().map(|(k, v)| (k.clone(), v.clone())));
    vars.into_iter().collect()
}

/// Expands a leading `~/` to `$HOME`.
pub(crate) fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    }
}

/// Instantiates the component and pumps messages until shutdown or EOF.
#[allow(clippy::too_many_arguments)]
fn run(
    engine: &Engine,
    component: &Component,
    linker: &Linker<Host>,
    wasi: WasiCtx,
    name: &str,
    reader: &super::Reader,
    outgoing: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    stop: oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    // A current-thread runtime lets this OS thread await the channels and the
    // HTTP import while the non-`Send` `Store` stays confined to it.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build plugin runtime")?;
    let client = Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build plugin HTTP client")?;
    let host = Host {
        table: ResourceTable::new(),
        wasi,
        rt,
        client,
        stop,
        stopping: false,
        grace_until: None,
        limits: StoreLimitsBuilder::new().memory_size(MAX_MEMORY).build(),
    };
    let mut store = Store::new(engine, host);
    store.limiter(|host| &mut host.limits);
    // Running guest code traps at the next epoch tick once stop is requested
    // (or the shutdown grace period has passed).
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(|mut ctx| {
        if ctx.data_mut().stopped() {
            Err(wasmtime::Error::msg("plugin stopped"))
        } else {
            Ok(UpdateDeadline::Continue(1))
        }
    });
    let plugin = McpPlugin::instantiate(&mut store, component, linker)
        .map_err(|e| e.context("instantiate wasm component"))?;
    info!(server = %name, "wasm plugin loaded");

    // `true` when the loop ended between calls: stop while idle, or EOF.
    let idle = loop {
        let Host {
            rt, stop, stopping, ..
        } = store.data_mut();
        if *stopping {
            // Stop arrived during the last `handle`.
            break false;
        }
        // `biased`: a pending stop wins over queued messages.
        let next = rt.block_on(async {
            tokio::select! {
                biased;
                _ = stop => {
                    *stopping = true;
                    None
                }
                line = outgoing.recv() => line,
            }
        });
        let Some(line) = next else { break true };
        // Called outside `block_on` so the HTTP import can enter the runtime.
        let reply = match plugin.call_handle(&mut store, &line) {
            Ok(reply) => reply,
            Err(_) if store.data().stopping => break false,
            Err(e) => return Err(e.context(format!("plugin `{name}` handle call failed")).into()),
        };
        for msg in reply.lines() {
            reader.dispatch_line(msg);
        }
    };
    if idle {
        store.data_mut().grace_until = Some(Instant::now() + SHUTDOWN_GRACE);
        if let Err(e) = plugin.call_on_shutdown(&mut store) {
            warn!(server = %name, "wasm plugin shutdown failed: {e:#}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc, time::Duration};

    use axum::{
        Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::post,
    };
    use serde_json::{Value, json};

    use super::super::{SpawnSpec, Transport, Upstream};
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn wasm_spec(name: &str, args: Vec<String>, env: HashMap<String, String>) -> SpawnSpec {
        SpawnSpec {
            name: name.into(),
            transport: Transport::Wasm {
                path: fixture(&format!("{name}.wasm")),
                args,
                env,
                inherit_env: Vec::new(),
                dirs: Vec::new(),
            },
            root: None,
        }
    }

    #[tokio::test]
    async fn loads_component_and_speaks_mcp() {
        let spec = wasm_spec("echo", Vec::new(), HashMap::new());
        let (up, init) = Upstream::spawn(&spec, Arc::new(|_| {}))
            .await
            .expect("spawn wasm plugin");
        assert_eq!(init["serverInfo"]["name"], "owlet-plugin-echo");

        let resp = up
            .request(json!({"jsonrpc": "2.0", "id": "c", "method": "tools/call",
                "params": {"name": "echo", "arguments": {"message": "hi"}}}))
            .expect("request")
            .wait(Duration::from_secs(10))
            .await
            .expect("response");
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("text content");
        assert!(text.contains("\"message\":\"hi\""));

        up.shutdown().await;
        assert!(!up.is_alive());
    }

    /// What the mock AWS MCP endpoint saw: (method, authorization, x-amz-date, body).
    type Seen = Arc<parking_lot::Mutex<Vec<(String, String, Option<String>, Value)>>>;
    /// The mock's live session id; set to `None` to expire it.
    type Live = Arc<parking_lot::Mutex<Option<String>>>;

    /// Streamable HTTP endpoint standing in for aws-mcp: JSON for `initialize`
    /// (issuing session `s1`, `s2`, ...), SSE for `tools/list`, 202 for everything
    /// else. A stale session id gets 404, a missing one 400.
    async fn mock(
        State((seen, live)): State<(Seen, Live)>,
        h: HeaderMap,
        body: String,
    ) -> axum::response::Response {
        let header = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let msg: Value = serde_json::from_str(&body).unwrap_or_default();
        let method = msg["method"].as_str().unwrap_or("<response>").to_owned();
        let initializes = {
            let mut seen = seen.lock();
            seen.push((
                method.clone(),
                header("authorization").unwrap_or_default(),
                header("x-amz-date"),
                msg.clone(),
            ));
            seen.iter().filter(|(m, ..)| m == "initialize").count()
        };
        if method == "initialize" {
            let sid = format!("s{initializes}");
            *live.lock() = Some(sid.clone());
            let r = json!({"jsonrpc": "2.0", "id": msg["id"], "result": {
                "protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock-aws-mcp", "version": "1"}}});
            return ([("mcp-session-id", sid)], axum::Json(r)).into_response();
        }
        let current = live.lock().clone();
        match header("mcp-session-id") {
            None => return (StatusCode::BAD_REQUEST, "missing session").into_response(),
            Some(sid) if current.as_ref() != Some(&sid) => {
                return (StatusCode::NOT_FOUND, "unknown session").into_response();
            }
            Some(_) => {}
        }
        if method == "tools/list" {
            let r = json!({"jsonrpc": "2.0", "id": msg["id"], "result": {
                "tools": [{"name": "aws___search_documentation", "inputSchema": {"type": "object"}}]}});
            let body = format!("event: message\ndata: {r}\n\n");
            return ([("content-type", "text/event-stream")], body).into_response();
        }
        if method == "tools/call" {
            // Never answers: stands in for a slow tool.
            std::future::pending::<()>().await;
        }
        StatusCode::ACCEPTED.into_response()
    }

    /// Records a session `DELETE` (method `DELETE`, the session id as body).
    /// Never answers once the mock's session was expired (`Live` is `None`).
    async fn end(State((seen, live)): State<(Seen, Live)>, h: HeaderMap) -> StatusCode {
        let header = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_owned);
        seen.lock().push((
            "DELETE".to_owned(),
            header("authorization").unwrap_or_default(),
            header("x-amz-date"),
            json!(header("mcp-session-id")),
        ));
        if live.lock().is_none() {
            std::future::pending::<()>().await;
        }
        StatusCode::NO_CONTENT
    }

    async fn serve_mock() -> (String, Seen, Live) {
        let seen = Seen::default();
        let live = Live::default();
        let app = Router::new()
            .route("/mcp", post(mock).delete(end))
            .with_state((Arc::clone(&seen), Arc::clone(&live)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/mcp"), seen, live)
    }

    fn aws_args(url: &str) -> Vec<String> {
        [
            url,
            "--service",
            "aws-mcp",
            "--region",
            "us-east-1",
            "--metadata",
            "AWS_REGION=us-west-2",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    #[tokio::test]
    async fn aws_plugin_signs_and_proxies() {
        let (url, seen, _) = serve_mock().await;
        let env = HashMap::from([
            ("AWS_ACCESS_KEY_ID".to_owned(), "AKIDEXAMPLE".to_owned()),
            ("AWS_SECRET_ACCESS_KEY".to_owned(), "fake-secret".to_owned()),
        ]);
        let spec = wasm_spec("aws-mcp", aws_args(&url), env);
        let (up, init) = Upstream::spawn(&spec, Arc::new(|_| {}))
            .await
            .expect("spawn aws-mcp plugin");
        assert_eq!(init["serverInfo"]["name"], "mock-aws-mcp");

        let resp = up
            .request(json!({"jsonrpc": "2.0", "id": "c", "method": "tools/list"}))
            .expect("request")
            .wait(Duration::from_secs(10))
            .await
            .expect("response");
        assert_eq!(
            resp["result"]["tools"][0]["name"],
            "aws___search_documentation"
        );
        up.shutdown().await;
        assert!(!up.is_alive());

        let seen = seen.lock();
        let methods: Vec<&str> = seen.iter().map(|(m, ..)| m.as_str()).collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "DELETE"
            ]
        );
        for (method, auth, date, body) in seen.iter() {
            assert!(
                auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"),
                "{method}: {auth}"
            );
            assert!(auth.contains("/us-east-1/aws-mcp/aws4_request"), "{auth}");
            assert!(date.is_some(), "{method} has no x-amz-date");
            if method == "DELETE" {
                assert_eq!(body, "s1");
            } else {
                assert_eq!(body["params"]["_meta"]["AWS_REGION"], "us-west-2");
            }
        }
    }

    #[tokio::test]
    async fn aws_plugin_read_only_hides_and_blocks_write_tools() {
        let (url, seen, _) = serve_mock().await;
        let env = HashMap::from([
            ("AWS_ACCESS_KEY_ID".to_owned(), "AKIDEXAMPLE".to_owned()),
            ("AWS_SECRET_ACCESS_KEY".to_owned(), "fake-secret".to_owned()),
        ]);
        let mut args = aws_args(&url);
        args.push("--read-only".to_owned());
        let spec = wasm_spec("aws-mcp", args, env);
        let (up, _) = Upstream::spawn(&spec, Arc::new(|_| {}))
            .await
            .expect("spawn aws-mcp plugin");

        // The mock's only tool has no readOnlyHint.
        let resp = up
            .request(json!({"jsonrpc": "2.0", "id": "l", "method": "tools/list"}))
            .expect("request")
            .wait(Duration::from_secs(10))
            .await
            .expect("response");
        assert_eq!(resp["result"]["tools"], json!([]), "{resp}");

        // Denied in the plugin: the mock never answers tools/call.
        let call = json!({"jsonrpc": "2.0", "id": "c", "method": "tools/call",
            "params": {"name": "aws___search_documentation", "arguments": {}}});
        let resp = up
            .request(call)
            .expect("request")
            .wait(Duration::from_secs(10))
            .await
            .expect("response");
        assert_eq!(resp["result"]["isError"], true, "{resp}");
        up.shutdown().await;

        let seen = seen.lock();
        assert!(seen.iter().all(|(m, ..)| m != "tools/call"));
    }

    #[tokio::test]
    async fn aws_plugin_recovers_from_session_expiry() {
        let (url, seen, live) = serve_mock().await;
        let env = HashMap::from([
            ("AWS_ACCESS_KEY_ID".to_owned(), "AKIDEXAMPLE".to_owned()),
            ("AWS_SECRET_ACCESS_KEY".to_owned(), "fake-secret".to_owned()),
        ]);
        let spec = wasm_spec("aws-mcp", aws_args(&url), env);
        let (up, _) = Upstream::spawn(&spec, Arc::new(|_| {}))
            .await
            .expect("spawn aws-mcp plugin");
        let list = |id: &str| json!({"jsonrpc": "2.0", "id": id, "method": "tools/list"});

        for id in ["a", "b"] {
            if id == "b" {
                // The server forgets s1; the next request must still succeed.
                *live.lock() = None;
            }
            let resp = up
                .request(list(id))
                .expect("request")
                .wait(Duration::from_secs(10))
                .await
                .expect("response");
            assert_eq!(
                resp["result"]["tools"][0]["name"], "aws___search_documentation",
                "{resp}"
            );
        }
        assert_eq!(live.lock().as_deref(), Some("s2"));
        assert!(up.is_alive());
        up.shutdown().await;

        let seen = seen.lock();
        let methods: Vec<&str> = seen.iter().map(|(m, ..)| m.as_str()).collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/list",
                "initialize",
                "notifications/initialized",
                "tools/list",
                "DELETE"
            ]
        );
        assert_eq!(seen[4].3, seen[0].3, "replays the original initialize");
        assert_eq!(seen[7].3, "s2", "ends the replayed session");
    }

    #[tokio::test]
    async fn aws_plugin_reads_credentials_from_granted_dir() {
        let (url, seen, _) = serve_mock().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("credentials");
        std::fs::write(
            &file,
            "[default]\naws_access_key_id = AKIDFROMFILE\naws_secret_access_key = fake\n",
        )
        .expect("write credentials");
        let env = HashMap::from([(
            "AWS_SHARED_CREDENTIALS_FILE".to_owned(),
            file.to_str().expect("utf-8").to_owned(),
        )]);
        let mut spec = wasm_spec("aws-mcp", aws_args(&url), env);
        if let Transport::Wasm { dirs, .. } = &mut spec.transport {
            dirs.push(dir.path().to_path_buf());
        }
        let (up, _) = Upstream::spawn(&spec, Arc::new(|_| {}))
            .await
            .expect("spawn aws-mcp plugin");
        up.shutdown().await;
        assert!(seen.lock()[0].1.contains("Credential=AKIDFROMFILE/"));

        // Without the directory grant the file is invisible to the plugin.
        let spec = wasm_spec("aws-mcp", aws_args(&url), spec_env(&spec));
        let err = Upstream::spawn(&spec, Arc::new(|_| {}))
            .await
            .err()
            .expect("initialize must fail");
        assert!(format!("{err:#}").contains("no credentials"), "{err:#}");

        // Bad arguments surface as an initialize error, not a crash.
        let spec = wasm_spec("aws-mcp", Vec::new(), HashMap::new());
        let err = Upstream::spawn(&spec, Arc::new(|_| {}))
            .await
            .err()
            .expect("initialize must fail");
        assert!(format!("{err:#}").contains("missing <endpoint>"), "{err:#}");
    }

    #[tokio::test]
    async fn shutdown_abandons_an_in_flight_http_exchange() {
        let (url, seen, _) = serve_mock().await;
        let env = HashMap::from([
            ("AWS_ACCESS_KEY_ID".to_owned(), "AKIDEXAMPLE".to_owned()),
            ("AWS_SECRET_ACCESS_KEY".to_owned(), "fake-secret".to_owned()),
        ]);
        let spec = wasm_spec("aws-mcp", aws_args(&url), env);
        let (up, _) = Upstream::spawn(&spec, Arc::new(|_| {}))
            .await
            .expect("spawn aws-mcp plugin");
        let _call = up
            .request(json!({"jsonrpc": "2.0", "id": "c", "method": "tools/call",
                "params": {"name": "slow", "arguments": {}}}))
            .expect("request");
        // Wait until the plugin is blocked inside http.send.
        while !seen.lock().iter().any(|(m, ..)| m == "tools/call") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::timeout(Duration::from_secs(5), up.shutdown())
            .await
            .expect("shutdown must not wait for the HTTP timeout");
        assert!(!up.is_alive());
        // Interrupted mid-call: `shutdown` is not called.
        assert!(seen.lock().iter().all(|(m, ..)| m != "DELETE"));
    }

    #[tokio::test]
    async fn idle_shutdown_deletes_the_session_within_the_grace_period() {
        let (url, seen, live) = serve_mock().await;
        let env = HashMap::from([
            ("AWS_ACCESS_KEY_ID".to_owned(), "AKIDEXAMPLE".to_owned()),
            ("AWS_SECRET_ACCESS_KEY".to_owned(), "fake-secret".to_owned()),
        ]);
        let spec = wasm_spec("aws-mcp", aws_args(&url), env);
        let (up, _) = Upstream::spawn(&spec, Arc::new(|_| {}))
            .await
            .expect("spawn aws-mcp plugin");
        // A round trip: the queued notifications/initialized has been handled.
        up.request(json!({"jsonrpc": "2.0", "id": "l", "method": "tools/list"}))
            .expect("request")
            .wait(Duration::from_secs(10))
            .await
            .expect("response");
        // The mock now never answers the DELETE: the grace period must end it.
        *live.lock() = None;
        let started = std::time::Instant::now();
        up.shutdown().await;
        let took = started.elapsed();
        assert!(!up.is_alive());
        assert!(
            took >= SHUTDOWN_GRACE - EPOCH_TICK && took < SHUTDOWN_GRACE + Duration::from_secs(1),
            "{took:?}"
        );

        let seen = seen.lock();
        let (method, auth, _, sid) = seen.last().expect("requests");
        assert_eq!(method, "DELETE");
        assert_eq!(sid, "s1");
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"));
    }

    fn spec_env(spec: &SpawnSpec) -> HashMap<String, String> {
        match &spec.transport {
            Transport::Wasm { env, .. } => env.clone(),
            _ => HashMap::new(),
        }
    }

    #[test]
    fn env_inherits_patterns_and_explicit_wins() {
        let host = [
            ("AWS_REGION", "us-east-1"),
            ("AWS_PROFILE", "host"),
            ("HOME", "/h"),
            ("OWLET_TOKEN", "t"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        let explicit = HashMap::from([("AWS_PROFILE".to_owned(), "dev".to_owned())]);
        let vars = plugin_env(
            host.into_iter(),
            &explicit,
            &["AWS_*".to_owned(), "HOME".to_owned()],
        );
        let get = |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("AWS_REGION"), Some("us-east-1"));
        assert_eq!(get("AWS_PROFILE"), Some("dev"));
        assert_eq!(get("HOME"), Some("/h"));
        assert_eq!(get("OWLET_TOKEN"), None);
        assert!(plugin_env(std::iter::empty(), &HashMap::new(), &[]).is_empty());
    }

    #[test]
    fn load_drops_cache_entries_of_removed_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (gone, kept) = (dir.path().join("gone.wasm"), dir.path().join("kept.wasm"));
        std::fs::copy(fixture("echo.wasm"), &gone).expect("copy");
        load(&gone).expect("load");
        assert!(CACHE.lock().contains_key(&gone));

        std::fs::remove_file(&gone).expect("remove");
        std::fs::copy(fixture("echo.wasm"), &kept).expect("copy");
        load(&kept).expect("load");
        let cache = CACHE.lock();
        assert!(!cache.contains_key(&gone));
        assert!(cache.contains_key(&kept));
    }

    #[test]
    fn expands_home_prefix_only() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
        assert_eq!(expand_home(Path::new("~/.aws")), home.join(".aws"));
        assert_eq!(expand_home(Path::new("/etc/x")), PathBuf::from("/etc/x"));
        assert_eq!(expand_home(Path::new("~user/x")), PathBuf::from("~user/x"));
    }
}
