//! Check-only updater for the gpui UI.
//!
//! gpui-kit has no signed auto-updater equivalent to Tauri's
//! `tauri-plugin-updater`, and there is no safe in-place `.app`/`.dmg`
//! self-replace that a running packaged bundle can perform without a helper.
//! So this module only *checks* GitHub Releases: it compares the running
//! version against the latest release tag and, when a newer one exists,
//! surfaces the version, release notes and a download URL. Installing is left
//! to the user, who opens the download in their browser.
//!
//! The logic mirrors the `owlet-core` crate's `update.rs` GitHub Releases
//! pattern (same repository, same release pipeline) but targets the UI's
//! release asset (`owlet-ui-<tag>-<target>.dmg`) and never downloads or swaps
//! files.

use std::time::Duration;

use anyhow::{Context, bail};
use semver::Version;
use serde::Deserialize;

/// GitHub API root of the repository that publishes releases.
const API_BASE: &str = "https://api.github.com/repos/dennis0700/owlet";

/// A newer release as shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub(crate) current: String,
    pub(crate) version: String,
    pub(crate) notes: Option<String>,
    /// Where the user downloads the update (the UI asset, or the release page).
    pub(crate) download_url: String,
}

impl UpdateInfo {
    /// Builds the summary of an available update; blank notes become `None`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let info = UpdateInfo::new("0.2.0", "0.3.0", Some("Fixes".into()), "https://…".into());
    /// ```
    pub fn new(current: &str, version: &str, notes: Option<String>, download_url: String) -> Self {
        Self {
            current: current.to_owned(),
            version: version.to_owned(),
            notes: notes.filter(|n| !n.trim().is_empty()),
            download_url,
        }
    }

    /// The running version.
    pub fn current(&self) -> &str {
        &self.current
    }

    /// The available version.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Release notes, if any.
    pub fn notes(&self) -> Option<&str> {
        self.notes.as_deref()
    }

    /// Browser URL where the update can be downloaded.
    pub fn download_url(&self) -> &str {
        &self.download_url
    }
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// Rust target triple of the UI release asset matching this build, if one is
/// published by the release workflow.
fn platform_target() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else {
        None
    }
}

/// Checks GitHub Releases for a version newer than the running one.
pub struct Updater {
    client: reqwest::Client,
    api: String,
    target: String,
}

impl Updater {
    /// Creates an updater that reads releases from `api` (a GitHub repository
    /// API root) and prefers the UI asset built for `target`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let updater = Updater::new("https://api.github.com/repos/o/r", "aarch64-apple-darwin")?;
    /// ```
    pub fn new(api: &str, target: &str) -> anyhow::Result<Self> {
        Ok(Self {
            client: client()?,
            api: api.trim_end_matches('/').to_owned(),
            target: target.to_owned(),
        })
    }

    /// Creates an updater for this build's platform and the public repository.
    ///
    /// Returns `None` when no UI release is published for this platform.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// if let Some(updater) = Updater::for_this_platform()? {
    ///     let info = updater.check(env!("CARGO_PKG_VERSION")).await?;
    /// }
    /// ```
    pub fn for_this_platform() -> anyhow::Result<Option<Self>> {
        match platform_target() {
            Some(target) => Ok(Some(Self::new(API_BASE, target)?)),
            None => Ok(None),
        }
    }

    /// Returns the available update when the latest release is newer than
    /// `current`, or `None` when `current` is already up to date.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let info = updater.check("0.2.0").await?;
    /// ```
    pub async fn check(&self, current: &str) -> anyhow::Result<Option<UpdateInfo>> {
        let current = parse_version(current).context("parse current version")?;
        let release = self.latest().await?;
        let latest = parse_tag(&release.tag_name)?;
        if latest <= current {
            return Ok(None);
        }

        let asset = format!("owlet-ui-{}-{}.dmg", release.tag_name, self.target);
        let download_url = release
            .assets
            .iter()
            .find(|a| a.name == asset)
            .map(|a| a.browser_download_url.clone())
            .or_else(|| release.html_url.clone())
            .with_context(|| {
                format!("release {} has no `{asset}` asset or page url", release.tag_name)
            })?;

        Ok(Some(UpdateInfo::new(
            &current.to_string(),
            &latest.to_string(),
            release.body,
            download_url,
        )))
    }

    async fn latest(&self) -> anyhow::Result<Release> {
        let url = format!("{}/releases/latest", self.api);
        let body = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("GET {url}"))?
            .bytes()
            .await
            .context("read response body")?;
        serde_json::from_slice(&body).context("parse latest release")
    }
}

/// HTTP client for the GitHub API.
fn client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("owlet-gpui/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
        .context("build http client")
}

fn parse_version(v: &str) -> anyhow::Result<Version> {
    Version::parse(v).with_context(|| format!("version {v:?} is not semver"))
}

fn parse_tag(tag: &str) -> anyhow::Result<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag))
        .with_context(|| format!("release tag {tag:?} is not a version"))
}

/// Opens `url` in the user's default browser.
///
/// # Examples
///
/// ```ignore
/// open_download(info.download_url())?;
/// ```
pub fn open_download(url: &str) -> anyhow::Result<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("refusing to open non-http url {url:?}");
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .with_context(|| format!("open {url}"))?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        bail!("opening a browser is only implemented on macOS");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use axum::{Json, Router, extract::Path as UrlPath, http::StatusCode, routing::get};
    use tokio::net::TcpListener;

    use super::*;

    const TARGET: &str = "aarch64-apple-darwin";

    #[test]
    fn keeps_real_notes() {
        let info = UpdateInfo::new("0.2.0", "0.3.0", Some("Fixes".to_owned()), "u".to_owned());
        assert_eq!(info.notes.as_deref(), Some("Fixes"));
        assert_eq!(info.version, "0.3.0");
        assert_eq!(info.current, "0.2.0");
        assert_eq!(info.download_url, "u");
    }

    #[test]
    fn drops_blank_notes() {
        assert_eq!(
            UpdateInfo::new("0.2.0", "0.3.0", Some("  \n".to_owned()), "u".to_owned()).notes,
            None
        );
        assert_eq!(
            UpdateInfo::new("0.2.0", "0.3.0", None, "u".to_owned()).notes,
            None
        );
    }

    #[test]
    fn parses_tags_with_and_without_prefix() {
        assert_eq!(parse_tag("v1.2.3").expect("tag"), Version::new(1, 2, 3));
        assert_eq!(parse_tag("1.2.3").expect("tag"), Version::new(1, 2, 3));
        assert!(parse_tag("nightly").is_err());
    }

    #[test]
    fn refuses_to_open_non_http() {
        assert!(open_download("file:///etc/passwd").is_err());
        assert!(open_download("javascript:alert(1)").is_err());
    }

    /// Serves a fake GitHub API whose latest release is `tag` with the given
    /// assets (name -> download url); returns its base URL.
    async fn serve(release: serde_json::Value) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let app = Router::new().route(
            "/releases/latest",
            get(move || {
                let release = release.clone();
                async move { Json(release) }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await });
        base
    }

    fn release_with_asset(tag: &str, base: &str, body: &str) -> serde_json::Value {
        let asset = format!("owlet-ui-{tag}-{TARGET}.dmg");
        serde_json::json!({
            "tag_name": tag,
            "body": body,
            "html_url": format!("{base}/releases/tag/{tag}"),
            "assets": [
                {"name": asset, "browser_download_url": format!("{base}/dl/{asset}")},
            ],
        })
    }

    #[tokio::test]
    async fn reports_a_newer_release_with_the_ui_asset() {
        // `base` is known before the server starts because bind picks the port
        // up front; embed it in the release JSON, then serve that JSON.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let release = release_with_asset("v9.9.9", &base, "New things");
        let files = Arc::new(HashMap::from([(
            format!("owlet-ui-v9.9.9-{TARGET}.dmg"),
            b"dmg".to_vec(),
        )]));
        let app = Router::new()
            .route(
                "/releases/latest",
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

        let info = Updater::new(&base, TARGET)
            .expect("updater")
            .check("0.2.0")
            .await
            .expect("check")
            .expect("update available");

        assert_eq!(info.version(), "9.9.9");
        assert_eq!(info.current(), "0.2.0");
        assert_eq!(info.notes(), Some("New things"));
        assert_eq!(info.download_url(), format!("{base}/dl/owlet-ui-v9.9.9-{TARGET}.dmg"));
    }

    #[tokio::test]
    async fn reports_none_when_up_to_date() {
        let base = serve(serde_json::json!({
            "tag_name": "v0.2.0",
            "assets": [],
        }))
        .await;

        let info = Updater::new(&base, TARGET)
            .expect("updater")
            .check("0.2.0")
            .await
            .expect("check");
        assert_eq!(info, None);
    }

    #[tokio::test]
    async fn reports_none_when_older() {
        let base = serve(serde_json::json!({
            "tag_name": "v0.1.0",
            "assets": [],
        }))
        .await;

        let info = Updater::new(&base, TARGET)
            .expect("updater")
            .check("0.2.0")
            .await
            .expect("check");
        assert_eq!(info, None);
    }

    #[tokio::test]
    async fn falls_back_to_the_release_page_without_the_ui_asset() {
        let base = serve(serde_json::json!({
            "tag_name": "v9.9.9",
            "body": "",
            "html_url": "https://example.com/releases/v9.9.9",
            "assets": [
                {"name": "something-else.tar.gz", "browser_download_url": "https://x/other"},
            ],
        }))
        .await;

        let info = Updater::new(&base, TARGET)
            .expect("updater")
            .check("0.2.0")
            .await
            .expect("check")
            .expect("update available");
        assert_eq!(info.download_url(), "https://example.com/releases/v9.9.9");
        assert_eq!(info.notes(), None);
    }

    #[tokio::test]
    async fn errors_without_asset_or_page() {
        let base = serve(serde_json::json!({
            "tag_name": "v9.9.9",
            "assets": [],
        }))
        .await;

        let err = Updater::new(&base, TARGET)
            .expect("updater")
            .check("0.2.0")
            .await
            .expect_err("must fail");
        assert!(format!("{err:#}").contains("no `owlet-ui"), "{err:#}");
    }
}
