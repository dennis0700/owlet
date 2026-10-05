use std::{
    collections::{BTreeMap, HashMap},
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

/// How instances of an upstream server are shared between client sessions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// One process shared by every session.
    #[default]
    Shared,
    /// One process per `?project=<path>`; the process runs with that path as cwd.
    Project,
    /// One process per client session (strongly stateful servers such as browsers).
    Session,
}

/// Wire protocol used for `url` servers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTransport {
    /// Streamable HTTP (2025-03-26 and later).
    #[default]
    Http,
    /// Legacy HTTP+SSE (2024-11-05).
    Sse,
}

/// Top-level configuration file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Bearer token required on every request. Falls back to `$OWLET_TOKEN`.
    #[serde(default)]
    pub token: Option<String>,
    /// Seconds an upstream process may stay idle before being stopped.
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: u64,
    /// Seconds a client session may stay idle before being forgotten.
    #[serde(default = "default_session_ttl")]
    pub session_ttl: u64,
    /// Seconds to wait for a single upstream response.
    #[serde(default = "default_request_timeout")]
    pub request_timeout: u64,
    #[serde(default)]
    pub servers: HashMap<String, ServerConfig>,
}

/// One upstream MCP server: either a local stdio process (`command`) or a
/// remote endpoint (`url`).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Disabled servers keep their configuration but cannot be used until
    /// enabled again (at runtime via `owlet enable`, or here).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Executable for a stdio server.
    #[serde(default)]
    pub command: Option<String>,
    /// Remote server URL: the MCP endpoint for `http`, the `GET /sse` stream for `sse`.
    #[serde(default)]
    pub url: Option<String>,
    /// Protocol for `url`; defaults to Streamable HTTP.
    #[serde(default)]
    pub transport: Option<RemoteTransport>,
    /// Extra HTTP headers sent to `url` (e.g. `Authorization`).
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Arguments; `{project}` is replaced by the project path for `scope = "project"`.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Working directory for `shared`/`session` scope.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub scope: Scope,
    /// Overrides the global `idle_timeout` for this server.
    #[serde(default)]
    pub idle_timeout: Option<u64>,
    /// Cache `*/list` results so clients can list tools without spawning the process.
    #[serde(default = "default_true")]
    pub cache_lists: bool,
}

impl ServerConfig {
    /// Checks that the entry is a coherent `command` or `url` server.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// server.check()?;
    /// ```
    pub fn check(&self) -> anyhow::Result<()> {
        match (&self.command, &self.url) {
            (Some(_), Some(_)) => bail!("set either `command` or `url`, not both"),
            (None, None) => bail!("missing `command` or `url`"),
            (Some(cmd), None) => {
                if cmd.trim().is_empty() {
                    bail!("empty command");
                }
                if !self.headers.is_empty() || self.transport.is_some() {
                    bail!("`headers` and `transport` only apply to `url` servers");
                }
            }
            (None, Some(url)) => {
                let parsed =
                    reqwest::Url::parse(url).with_context(|| format!("invalid url `{url}`"))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    bail!("url must be http(s): `{url}`");
                }
                if !self.args.is_empty() || !self.env.is_empty() || self.cwd.is_some() {
                    bail!("`args`, `env` and `cwd` only apply to `command` servers");
                }
                if self.scope == Scope::Project {
                    bail!("scope = \"project\" needs a local process; use `command`");
                }
            }
        }
        Ok(())
    }
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8808))
}
fn default_idle_timeout() -> u64 {
    600
}
fn default_session_ttl() -> u64 {
    86_400
}
fn default_request_timeout() -> u64 {
    300
}
fn default_true() -> bool {
    true
}

impl Config {
    /// Loads and validates a TOML config file; `$OWLET_TOKEN` is used when `token` is unset.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let cfg = Config::load(Path::new("owlet.toml"))?;
    /// ```
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let mut cfg = Self::parse(&text).with_context(|| format!("parse {}", path.display()))?;
        if cfg.token.is_none() {
            cfg.token = std::env::var("OWLET_TOKEN").ok().filter(|t| !t.is_empty());
        }
        cfg.validate()?;
        Ok(cfg)
    }

    /// Parses config text without consulting the environment.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let cfg = Config::parse("[servers.fetch]\ncommand = \"uvx\"\nargs = [\"mcp-server-fetch\"]")?;
    /// assert_eq!(cfg.servers.len(), 1);
    /// ```
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let cfg: Self = toml::from_str(text)?;
        for (name, server) in &cfg.servers {
            Self::check_server(name, server)?;
        }
        Ok(cfg)
    }

    /// Validates a server name together with its definition.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// Config::check_server("fetch", &server)?;
    /// ```
    pub fn check_server(name: &str, server: &ServerConfig) -> anyhow::Result<()> {
        if name.is_empty() || name.contains('/') {
            bail!("invalid server name `{name}`");
        }
        server.check().with_context(|| format!("server `{name}`"))
    }

    /// `~/.config/owlet/config.toml`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let path = Config::default_path()?;
    /// ```
    pub fn default_path() -> anyhow::Result<PathBuf> {
        let home = std::env::var_os("HOME").context("HOME not set; pass --config")?;
        Ok(PathBuf::from(home).join(".config/owlet/config.toml"))
    }

    /// Reads the `[servers]` table only; a missing file yields no servers.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let servers = Config::load_servers(Path::new("owlet.toml"))?;
    /// ```
    pub fn load_servers(path: &Path) -> anyhow::Result<BTreeMap<String, ServerConfig>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(e).with_context(|| format!("read config {}", path.display())),
        };
        let cfg = Self::parse(&text).with_context(|| format!("parse {}", path.display()))?;
        Ok(cfg.servers.into_iter().collect())
    }

    /// Makes the `[servers]` table of the config file equal to `servers`,
    /// preserving comments, other keys and untouched entries. The file is
    /// created when missing.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// Config::persist_servers(Path::new("owlet.toml"), &servers)?;
    /// ```
    pub fn persist_servers(
        path: &Path,
        servers: &BTreeMap<String, ServerConfig>,
    ) -> anyhow::Result<()> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("read config {}", path.display())),
        };
        let updated = apply_servers(&text, servers)?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        write_atomic(path, &updated)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if !self.listen.ip().is_loopback() && self.token.is_none() {
            bail!(
                "refusing to listen on non-loopback {} without a token",
                self.listen
            );
        }
        Ok(())
    }

    /// Sets `servers.<name>.enabled` in the config file, preserving comments
    /// and formatting. `enabled = true` is written as removal of the key.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// Config::persist_enabled(Path::new("owlet.toml"), "fetch", false)?;
    /// ```
    pub fn persist_enabled(path: &Path, name: &str, enabled: bool) -> anyhow::Result<()> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let updated = set_enabled(&text, name, enabled)?;
        write_atomic(path, &updated)
    }

    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout)
    }
    pub fn session_ttl(&self) -> Duration {
        Duration::from_secs(self.session_ttl)
    }
    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout)
    }
}

/// Writes via a temp file so a crash cannot leave a truncated config.
fn write_atomic(path: &Path, text: &str) -> anyhow::Result<()> {
    let tmp = path.with_extension("toml.owlet-tmp");
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn apply_servers(text: &str, servers: &BTreeMap<String, ServerConfig>) -> anyhow::Result<String> {
    for (name, server) in servers {
        Config::check_server(name, server)?;
    }
    let current = Config::parse(text).context("parse config")?.servers;
    let mut doc: toml_edit::DocumentMut = text.parse().context("parse config")?;
    let root = doc.as_table_mut();
    if !root.contains_key("servers") {
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        root.insert("servers", toml_edit::Item::Table(table));
    }
    let table = root
        .get_mut("servers")
        .and_then(toml_edit::Item::as_table_like_mut)
        .context("`servers` is not a table")?;

    let stale: Vec<String> = table
        .iter()
        .map(|(key, _)| key.to_owned())
        .filter(|key| !servers.contains_key(key))
        .collect();
    for key in stale {
        table.remove(&key);
    }
    for (name, server) in servers {
        if current.get(name) == Some(server) {
            continue;
        }
        if !table.contains_key(name) {
            table.insert(name, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let entry = table
            .get_mut(name)
            .and_then(toml_edit::Item::as_table_like_mut)
            .with_context(|| format!("server `{name}` is not a table"))?;
        write_server(entry, server).with_context(|| format!("server `{name}`"))?;
    }

    let out = doc.to_string();
    Config::parse(&out).context("edited config is invalid")?;
    Ok(out)
}

/// Writes every field, omitting those equal to their default.
fn write_server(t: &mut dyn toml_edit::TableLike, s: &ServerConfig) -> anyhow::Result<()> {
    use toml_edit::Value;

    fn put(t: &mut dyn toml_edit::TableLike, key: &str, v: Option<Value>) {
        match v {
            Some(v) => {
                t.insert(key, toml_edit::value(v));
            }
            None => {
                t.remove(key);
            }
        }
    }
    fn inline(map: &HashMap<String, String>) -> Option<Value> {
        if map.is_empty() {
            return None;
        }
        let mut pairs: Vec<_> = map.iter().collect();
        pairs.sort();
        let mut table = toml_edit::InlineTable::new();
        for (k, v) in pairs {
            table.insert(k.as_str(), Value::from(v.as_str()));
        }
        table.fmt();
        Some(Value::InlineTable(table))
    }

    put(t, "enabled", (!s.enabled).then_some(Value::from(false)));
    put(t, "command", s.command.as_deref().map(Value::from));
    put(t, "url", s.url.as_deref().map(Value::from));
    let transport = s.transport.map(|tr| match tr {
        RemoteTransport::Http => "http",
        RemoteTransport::Sse => "sse",
    });
    put(t, "transport", transport.map(Value::from));
    put(t, "headers", inline(&s.headers));
    let args = (!s.args.is_empty()).then(|| {
        Value::Array(
            s.args
                .iter()
                .map(String::as_str)
                .collect::<toml_edit::Array>(),
        )
    });
    put(t, "args", args);
    put(t, "env", inline(&s.env));
    let cwd = s
        .cwd
        .as_deref()
        .map(|p| p.to_str().context("cwd is not valid UTF-8"))
        .transpose()?;
    put(t, "cwd", cwd.map(Value::from));
    let scope = match s.scope {
        Scope::Shared => None,
        Scope::Project => Some("project"),
        Scope::Session => Some("session"),
    };
    put(t, "scope", scope.map(Value::from));
    let idle = s
        .idle_timeout
        .map(|v| i64::try_from(v).context("idle_timeout too large"))
        .transpose()?;
    put(t, "idle_timeout", idle.map(Value::from));
    put(
        t,
        "cache_lists",
        (!s.cache_lists).then_some(Value::from(false)),
    );
    Ok(())
}

fn set_enabled(text: &str, name: &str, enabled: bool) -> anyhow::Result<String> {
    let mut doc: toml_edit::DocumentMut = text.parse().context("parse config")?;
    let server = doc
        .get_mut("servers")
        .and_then(|s| s.get_mut(name))
        .and_then(toml_edit::Item::as_table_like_mut)
        .with_context(|| format!("server `{name}` not found in config"))?;
    if enabled {
        server.remove("enabled");
    } else {
        server.insert("enabled", toml_edit::value(false));
    }
    let out = doc.to_string();
    // Never write a file that owlet itself would refuse to load.
    Config::parse(&out).context("edited config is invalid")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_enabled_preserving_comments() {
        let text = "# top\n[servers.a]\ncommand = \"x\" # keep\n\n[servers.b]\ncommand = \"y\"\n";
        let off = set_enabled(text, "a", false).expect("disable");
        assert!(off.contains("# top") && off.contains("# keep"));
        let cfg = Config::parse(&off).expect("parse");
        assert!(!cfg.servers["a"].enabled);
        assert!(cfg.servers["b"].enabled);

        let on = set_enabled(&off, "a", true).expect("enable");
        assert!(!on.contains("enabled"));
        assert!(set_enabled(text, "missing", false).is_err());
    }

    #[test]
    fn applies_server_edits_preserving_the_rest() {
        let text = "# top\nlisten = \"127.0.0.1:9\"\n\n[servers.a]\ncommand = \"x\" # keep\n\n\
                    [servers.b]\ncommand = \"y\"\nargs = [\"1\"]\n\n[servers.gone]\ncommand = \"z\"\n";
        let mut servers = Config::parse(text)
            .expect("parse")
            .servers
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        servers.remove("gone");
        servers.get_mut("b").expect("b").enabled = false;
        servers.get_mut("b").expect("b").args.clear();
        let mut added = servers["a"].clone();
        added.command = None;
        added.url = Some("https://h/mcp".into());
        added.headers.insert("X-Key".into(), "v".into());
        added.scope = Scope::Session;
        added.idle_timeout = Some(5);
        added.cache_lists = false;
        servers.insert("new".into(), added.clone());

        let out = apply_servers(text, &servers).expect("apply");
        assert!(out.contains("# top") && out.contains("# keep"));
        assert!(out.contains("listen = \"127.0.0.1:9\""));
        assert!(!out.contains("gone"));
        let back = Config::parse(&out).expect("reparse");
        assert_eq!(back.servers.len(), 3);
        assert!(!back.servers["b"].enabled && back.servers["b"].args.is_empty());
        assert_eq!(back.servers["new"], added);
    }

    #[test]
    fn applies_servers_to_empty_text() {
        let server = Config::parse("[servers.a]\ncommand = \"x\"\nenv = { K = \"v\" }\n")
            .expect("parse")
            .servers
            .remove("a")
            .expect("a");
        let servers = BTreeMap::from([("a".to_owned(), server)]);
        let out = apply_servers("", &servers).expect("apply");
        assert!(out.contains("[servers.a]") && !out.contains("[servers]"));
        assert_eq!(
            Config::parse(&out).expect("reparse").servers["a"],
            servers["a"]
        );

        let none = apply_servers(&out, &BTreeMap::new()).expect("apply");
        assert!(Config::parse(&none).expect("reparse").servers.is_empty());

        let bad = ServerConfig {
            command: None,
            ..servers["a"].clone()
        };
        assert!(apply_servers("", &BTreeMap::from([("a".to_owned(), bad)])).is_err());
    }

    #[test]
    fn parses_defaults_and_servers() {
        let cfg = Config::parse(
            r#"
            [servers.fetch]
            command = "uvx"
            args = ["mcp-server-fetch"]

            [servers.cg]
            command = "codegraph"
            scope = "project"
            idle_timeout = 30
            "#,
        )
        .expect("valid config");
        assert_eq!(cfg.listen, default_listen());
        assert_eq!(cfg.servers["fetch"].scope, Scope::Shared);
        assert!(cfg.servers["fetch"].cache_lists);
        assert_eq!(cfg.servers["cg"].scope, Scope::Project);
        assert_eq!(cfg.servers["cg"].idle_timeout, Some(30));
        assert!(cfg.servers["cg"].enabled);
        let off = Config::parse("[servers.x]\ncommand = \"a\"\nenabled = false").expect("parse");
        assert!(!off.servers["x"].enabled);
    }

    #[test]
    fn parses_sse_servers() {
        let cfg = Config::parse(
            r#"
            [servers.remote]
            url = "http://127.0.0.1:9000/sse"
            transport = "sse"
            headers = { Authorization = "Bearer x" }
            scope = "session"

            [servers.modern]
            url = "https://example.com/mcp"
            "#,
        )
        .expect("valid config");
        let r = &cfg.servers["remote"];
        assert_eq!(r.url.as_deref(), Some("http://127.0.0.1:9000/sse"));
        assert_eq!(r.headers["Authorization"], "Bearer x");
        assert!(r.command.is_none());
        assert_eq!(r.transport, Some(RemoteTransport::Sse));
        assert_eq!(cfg.servers["modern"].transport, None);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Config::parse("[servers.x]\ncommand = \"\"").is_err());
        assert!(Config::parse("[servers.x]\ncommand = \"a\"\nbogus = 1").is_err());
        assert!(Config::parse("[servers.x]\nscope = \"shared\"").is_err());
        assert!(Config::parse("[servers.x]\ncommand = \"a\"\nurl = \"http://h/sse\"").is_err());
        assert!(Config::parse("[servers.x]\nurl = \"ftp://h/sse\"").is_err());
        assert!(Config::parse("[servers.x]\nurl = \"http://h\"\ntransport = \"ws\"").is_err());
        assert!(Config::parse("[servers.x]\ncommand = \"a\"\ntransport = \"sse\"").is_err());
        assert!(Config::parse("[servers.x]\nurl = \"http://h/sse\"\nscope = \"project\"").is_err());
        assert!(Config::parse("[servers.x]\nurl = \"http://h/sse\"\nargs = [\"a\"]").is_err());
        assert!(Config::parse("[servers.x]\ncommand = \"a\"\nheaders = { A = \"b\" }").is_err());
        let cfg = Config::parse("listen = \"0.0.0.0:1\"").expect("parses");
        assert!(cfg.validate().is_err());
    }
}
