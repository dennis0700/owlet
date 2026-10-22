//! Installable WASM plugins: a built-in catalog, install from the GitHub
//! release matching this owlet version (or from a local file), and the
//! default `[servers.<name>]` entry for each plugin.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};

use crate::{
    config::{Scope, ServerConfig},
    update::{self, Release},
    upstream::{expand_home, validate_wasm},
};

/// A plugin owlet knows how to install.
#[derive(Debug)]
pub struct Plugin {
    /// Plugin and default server name.
    pub name: &'static str,
    pub description: &'static str,
    /// Release asset file name; `<asset>.sha256` holds its checksum.
    pub asset: &'static str,
    args: &'static [&'static str],
    inherit_env: &'static [&'static str],
    dirs: &'static [&'static str],
}

const CATALOG: &[Plugin] = &[Plugin {
    name: "aws-mcp",
    description: "AWS MCP Server through a SigV4-signing proxy (replaces uvx mcp-proxy-for-aws)",
    asset: "owlet-plugin-aws-mcp.wasm",
    args: &[
        "https://aws-mcp.us-east-1.api.aws/mcp",
        "--metadata",
        "AWS_REGION=us-west-2",
    ],
    inherit_env: &[
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_PROFILE",
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
        "AWS_CONFIG_FILE",
        "AWS_SHARED_CREDENTIALS_FILE",
        "AWS_MCP_PROXY_PROFILES",
        "HOME",
    ],
    dirs: &["~/.aws"],
}];

impl Plugin {
    /// Every plugin owlet can install.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// for p in Plugin::catalog() { println!("{}", p.name); }
    /// ```
    pub fn catalog() -> &'static [Plugin] {
        CATALOG
    }

    /// Looks up a catalog plugin by name.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let aws = Plugin::find("aws-mcp")?;
    /// ```
    pub fn find(name: &str) -> anyhow::Result<&'static Plugin> {
        CATALOG.iter().find(|p| p.name == name).with_context(|| {
            let known: Vec<_> = CATALOG.iter().map(|p| p.name).collect();
            format!("unknown plugin `{name}` (available: {})", known.join(", "))
        })
    }

    /// Absolute `plugins/` directory next to the config file.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let dir = Plugin::dir(Path::new("/home/u/.config/owlet/config.toml"))?;
    /// ```
    pub fn dir(config: &Path) -> anyhow::Result<PathBuf> {
        let parent = config.parent().unwrap_or(Path::new(""));
        std::path::absolute(parent.join("plugins"))
            .with_context(|| format!("resolve plugins dir of {}", config.display()))
    }

    /// Where the plugin's component is installed inside `dir`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let wasm = plugin.file(&dir);
    /// ```
    pub fn file(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.wasm", self.name))
    }

    /// Default server entry running the plugin installed in `dir`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// servers.insert(plugin.name.into(), plugin.server_config(&dir));
    /// ```
    pub fn server_config(&self, dir: &Path) -> ServerConfig {
        let strings = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect();
        ServerConfig {
            enabled: true,
            command: None,
            wasm: Some(self.file(dir)),
            url: None,
            transport: None,
            headers: HashMap::new(),
            args: strings(self.args),
            env: HashMap::new(),
            inherit_env: strings(self.inherit_env),
            dirs: self.dirs.iter().map(PathBuf::from).collect(),
            cwd: None,
            scope: Scope::Session,
            idle_timeout: None,
            cache_lists: true,
        }
    }

    /// Name of the first server in `servers` that runs this plugin's file.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let name = plugin.server_name(&dir, &servers);
    /// ```
    pub fn server_name<'a>(
        &self,
        dir: &Path,
        servers: &'a BTreeMap<String, ServerConfig>,
    ) -> Option<&'a str> {
        let file = self.file(dir);
        servers
            .iter()
            .find(|(_, s)| s.wasm.as_deref().map(expand_home).as_ref() == Some(&file))
            .map(|(name, _)| name.as_str())
    }

    /// Makes `servers` run this plugin: keeps an entry that already uses its
    /// file, points an existing `wasm` entry of the same name at the file
    /// (keeping its other settings), or adds the default entry. An existing
    /// non-wasm entry with the plugin's name is an error.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// plugin.add_server(&dir, &mut servers)?;
    /// ```
    pub fn add_server(
        &self,
        dir: &Path,
        servers: &mut BTreeMap<String, ServerConfig>,
    ) -> anyhow::Result<()> {
        if self.server_name(dir, servers).is_some() {
            return Ok(());
        }
        match servers.get_mut(self.name) {
            Some(s) if s.wasm.is_some() => s.wasm = Some(self.file(dir)),
            Some(_) => bail!(
                "server `{}` already exists and is not a wasm plugin; rename or delete it first",
                self.name
            ),
            None => {
                servers.insert(self.name.to_owned(), self.server_config(dir));
            }
        }
        Ok(())
    }

    /// Removes every server that runs this plugin's file; returns their names.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let removed = plugin.remove_servers(&dir, &mut servers);
    /// ```
    pub fn remove_servers(
        &self,
        dir: &Path,
        servers: &mut BTreeMap<String, ServerConfig>,
    ) -> Vec<String> {
        let mut removed = Vec::new();
        while let Some(name) = self.server_name(dir, servers).map(str::to_owned) {
            servers.remove(&name);
            removed.push(name);
        }
        removed
    }

    /// Downloads the plugin from the GitHub release of this owlet version,
    /// verifies its sha256 and installs it into `dir`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let path = plugin.install(&dir).await?;
    /// ```
    pub async fn install(&self, dir: &Path) -> anyhow::Result<PathBuf> {
        self.install_release(update::API_BASE, env!("CARGO_PKG_VERSION"), dir)
            .await
    }

    async fn install_release(
        &self,
        api: &str,
        version: &str,
        dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        let client = update::client()?;
        let tag = format!("v{version}");
        let url = format!("{}/releases/tags/{tag}", api.trim_end_matches('/'));
        let body = update::download(&client, &url)
            .await
            .with_context(|| format!("fetch release {tag}"))?;
        let release: Release =
            serde_json::from_slice(&body).with_context(|| format!("parse release {tag}"))?;
        let wasm_url = &release.asset(self.asset)?.browser_download_url;
        let sums_url = &release
            .asset(&format!("{}.sha256", self.asset))?
            .browser_download_url;
        let wasm = update::download(&client, wasm_url).await?;
        let sums = update::download(&client, sums_url).await?;
        update::verify_sha256(&wasm, &sums).with_context(|| format!("verify {}", self.asset))?;
        // Validation compiles the component; keep it off the async workers.
        let wasm = tokio::task::spawn_blocking(move || validate_wasm(&wasm).map(|()| wasm))
            .await
            .context("validate task")?
            .with_context(|| format!("plugin `{}`", self.name))?;
        self.write(dir, &wasm)
    }

    /// Installs a local `.wasm` build of the plugin into `dir` (offline or development use).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let path = plugin.install_file(&dir, Path::new("aws-mcp.wasm"))?;
    /// ```
    pub fn install_file(&self, dir: &Path, src: &Path) -> anyhow::Result<PathBuf> {
        let wasm = std::fs::read(src).with_context(|| format!("read {}", src.display()))?;
        validate_wasm(&wasm).with_context(|| format!("plugin `{}`", self.name))?;
        self.write(dir, &wasm)
    }

    /// Deletes the installed file; a missing file is not an error.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// plugin.uninstall(&dir)?;
    /// ```
    pub fn uninstall(&self, dir: &Path) -> anyhow::Result<()> {
        let file = self.file(dir);
        match std::fs::remove_file(&file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("remove {}", file.display()))
            }
            _ => Ok(()),
        }
    }

    /// Writes already validated `wasm` into `dir` atomically.
    fn write(&self, dir: &Path, wasm: &[u8]) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let file = self.file(dir);
        let tmp = file.with_extension("wasm.owlet-tmp");
        let result = std::fs::write(&tmp, wasm)
            .with_context(|| format!("write {}", tmp.display()))
            .and_then(|()| {
                std::fs::rename(&tmp, &file).with_context(|| format!("replace {}", file.display()))
            });
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.map(|()| file)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{Json, Router, extract::Path as UrlPath, http::StatusCode, routing::get};
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::Config;

    fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/aws-mcp.wasm")
    }

    fn aws() -> &'static Plugin {
        Plugin::find("aws-mcp").expect("aws-mcp in catalog")
    }

    /// Serves a fake GitHub release `v9.9.9` with the aws-mcp asset and `sums`.
    async fn serve(wasm: Vec<u8>, sums: String) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let asset = aws().asset;
        let release = serde_json::json!({
            "tag_name": "v9.9.9",
            "assets": [
                {"name": asset, "browser_download_url": format!("{base}/dl/{asset}")},
                {"name": format!("{asset}.sha256"),
                 "browser_download_url": format!("{base}/dl/{asset}.sha256")},
            ],
        });
        let files = Arc::new(HashMap::from([
            (asset.to_owned(), wasm),
            (format!("{asset}.sha256"), sums.into_bytes()),
        ]));
        let app = Router::new()
            .route(
                "/releases/tags/v9.9.9",
                get(move || {
                    let release = release.clone();
                    async move { Json(release) }
                }),
            )
            .route(
                "/dl/{name}",
                get(move |UrlPath(name): UrlPath<String>| {
                    let files = Arc::clone(&files);
                    async move { files.get(&name).cloned().ok_or(StatusCode::NOT_FOUND) }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await });
        base
    }

    fn sha256_line(data: &[u8]) -> String {
        let hex: String = Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("{hex}  owlet-plugin-aws-mcp.wasm\n")
    }

    #[test]
    fn installs_and_uninstalls_a_local_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("plugins");
        let path = aws().install_file(&dir, &fixture()).expect("install");
        assert_eq!(path, dir.join("aws-mcp.wasm"));
        assert_eq!(
            std::fs::read(&path).expect("read"),
            std::fs::read(fixture()).expect("fixture")
        );
        assert_eq!(std::fs::read_dir(&dir).expect("ls").count(), 1);

        aws().uninstall(&dir).expect("uninstall");
        assert!(!path.exists());
        aws().uninstall(&dir).expect("uninstall is idempotent");
    }

    #[test]
    fn rejects_a_non_component_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("bogus.wasm");
        std::fs::write(&src, b"not wasm").expect("write");
        let dir = tmp.path().join("plugins");
        let err = aws().install_file(&dir, &src).expect_err("must fail");
        assert!(format!("{err:#}").contains("wasm component"), "{err:#}");
        assert!(!aws().file(&dir).exists());
    }

    #[tokio::test]
    async fn installs_from_a_release() {
        let wasm = std::fs::read(fixture()).expect("fixture");
        let base = serve(wasm.clone(), sha256_line(&wasm)).await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = aws()
            .install_release(&base, "9.9.9", tmp.path())
            .await
            .expect("install");
        assert_eq!(std::fs::read(path).expect("read"), wasm);
    }

    #[tokio::test]
    async fn refuses_a_release_checksum_mismatch() {
        let wasm = std::fs::read(fixture()).expect("fixture");
        let base = serve(wasm, sha256_line(b"something else")).await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = aws()
            .install_release(&base, "9.9.9", tmp.path())
            .await
            .expect_err("must fail");
        assert!(format!("{err:#}").contains("sha256 mismatch"), "{err:#}");
        assert_eq!(std::fs::read_dir(tmp.path()).expect("ls").count(), 0);

        let err = aws()
            .install_release(&base, "1.0.0", tmp.path())
            .await
            .expect_err("missing release");
        assert!(
            format!("{err:#}").contains("fetch release v1.0.0"),
            "{err:#}"
        );
    }

    #[test]
    fn default_server_config_is_valid() {
        let dir = Plugin::dir(Path::new("cfg/config.toml")).expect("dir");
        assert!(dir.is_absolute() && dir.ends_with("cfg/plugins"));
        let server = aws().server_config(&dir);
        Config::check_server(aws().name, &server).expect("valid");
        assert_eq!(server.wasm, Some(dir.join("aws-mcp.wasm")));
        assert!(server.inherit_env.iter().all(|k| !k.ends_with('*')));
        assert!(server.inherit_env.iter().any(|k| k == "AWS_PROFILE"));
        assert!(!server.inherit_env.iter().any(|k| k == "AWS_BEARER_TOKEN"));
        assert_eq!(server.scope, Scope::Session);
        assert!(Plugin::find("nope").is_err());
    }

    #[test]
    fn adds_and_removes_servers() {
        let dir = PathBuf::from("/cfg/plugins");
        let mut servers = BTreeMap::new();
        aws().add_server(&dir, &mut servers).expect("add");
        assert_eq!(aws().server_name(&dir, &servers), Some("aws-mcp"));

        // An existing wasm entry keeps its settings and is pointed at the file.
        let mut custom = aws().server_config(&dir);
        custom.wasm = Some("/elsewhere.wasm".into());
        custom.enabled = false;
        servers.insert("aws-mcp".into(), custom);
        aws().add_server(&dir, &mut servers).expect("refresh");
        assert!(!servers["aws-mcp"].enabled);
        assert_eq!(servers["aws-mcp"].wasm, Some(aws().file(&dir)));

        let mut other = aws().server_config(&dir);
        other.wasm = None;
        other.command = Some("uvx".into());
        other.inherit_env.clear();
        other.dirs.clear();
        let mut conflict = BTreeMap::from([("aws-mcp".to_owned(), other)]);
        assert!(aws().add_server(&dir, &mut conflict).is_err());

        servers.insert("copy".into(), aws().server_config(&dir));
        assert_eq!(
            aws().remove_servers(&dir, &mut servers),
            ["aws-mcp", "copy"]
        );
        assert!(servers.is_empty());
    }
}
