//! Self-update from GitHub Releases: download the archive for this platform,
//! verify its sha256, and swap the running executable.

use std::{
    ffi::OsStr,
    fmt::Write as _,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, bail};
use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// GitHub API root of the repository that publishes releases.
const API_BASE: &str = "https://api.github.com/repos/dennis0700/owlet";

/// Upper bound for the unpacked binary, so a corrupt archive cannot fill the disk.
const MAX_BINARY: u64 = 256 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

impl Release {
    fn asset(&self, name: &str) -> anyhow::Result<&Asset> {
        self.assets
            .iter()
            .find(|a| a.name == name)
            .with_context(|| format!("release {} has no asset {name}", self.tag_name))
    }
}

/// What [`Updater::update`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The running version is already the latest release.
    UpToDate,
    /// A newer release exists; nothing was installed (`check_only`).
    Available(Version),
    /// The executable was replaced with this version.
    Updated(Version),
}

/// Rust target triple of the release archive matching this build, if one is published.
///
/// # Examples
///
/// ```ignore
/// let target = platform_target().context("unsupported platform")?;
/// ```
pub fn platform_target() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-musl")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("aarch64-unknown-linux-musl")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else {
        None
    }
}

/// Downloads and installs releases of owlet.
pub struct Updater {
    client: reqwest::Client,
    api: String,
    target: String,
}

impl Updater {
    /// Creates an updater that reads releases from `api` (a GitHub repository
    /// API root) and picks the archive built for `target`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let updater = Updater::new("https://api.github.com/repos/o/r", "aarch64-apple-darwin")?;
    /// ```
    pub fn new(api: &str, target: &str) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("owlet/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(300))
            .build()
            .context("build http client")?;
        Ok(Self {
            client,
            api: api.trim_end_matches('/').to_owned(),
            target: target.to_owned(),
        })
    }

    /// Compares `current` with the latest release and, unless `check_only`,
    /// replaces the executable at `exe` with the verified download.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let outcome = updater.update(&current, &exe, false).await?;
    /// ```
    pub async fn update(
        &self,
        current: &Version,
        exe: &Path,
        check_only: bool,
    ) -> anyhow::Result<Outcome> {
        let release = self.latest().await?;
        let latest = parse_tag(&release.tag_name)?;
        if latest <= *current {
            return Ok(Outcome::UpToDate);
        }
        if check_only {
            return Ok(Outcome::Available(latest));
        }

        let archive = format!("owlet-{}-{}.tar.gz", release.tag_name, self.target);
        let tarball = self
            .download(&release.asset(&archive)?.browser_download_url)
            .await?;
        let sums = self
            .download(
                &release
                    .asset(&format!("{archive}.sha256"))?
                    .browser_download_url,
            )
            .await?;
        verify_sha256(&tarball, &sums).with_context(|| format!("verify {archive}"))?;

        let binary = extract_binary(&tarball).with_context(|| format!("unpack {archive}"))?;
        install(exe, &binary)?;
        Ok(Outcome::Updated(latest))
    }

    async fn latest(&self) -> anyhow::Result<Release> {
        let body = self
            .download(&format!("{}/releases/latest", self.api))
            .await
            .context("fetch latest release")?;
        serde_json::from_slice(&body).context("parse latest release")
    }

    async fn download(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("GET {url}"))?;
        Ok(resp.bytes().await.context("read response body")?.to_vec())
    }
}

fn parse_tag(tag: &str) -> anyhow::Result<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag))
        .with_context(|| format!("release tag {tag:?} is not a version"))
}

/// Checks `data` against the first field of a `shasum` output line.
fn verify_sha256(data: &[u8], sums: &[u8]) -> anyhow::Result<()> {
    let sums = std::str::from_utf8(sums).context("checksum file is not UTF-8")?;
    let expected = sums
        .split_whitespace()
        .next()
        .context("checksum file is empty")?;
    let digest = Sha256::digest(data);
    let actual = digest.iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    });
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("sha256 mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

/// Returns the contents of the `owlet` file inside a `.tar.gz` release archive.
fn extract_binary(tarball: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut archive = tar::Archive::new(GzDecoder::new(tarball));
    for entry in archive.entries().context("read archive")? {
        let entry = entry.context("read archive entry")?;
        let is_binary = entry.header().entry_type().is_file()
            && entry.path().context("read entry path")?.file_name() == Some(OsStr::new("owlet"));
        if !is_binary {
            continue;
        }
        let mut binary = Vec::new();
        entry
            .take(MAX_BINARY + 1)
            .read_to_end(&mut binary)
            .context("unpack binary")?;
        if binary.len() as u64 > MAX_BINARY {
            bail!("binary exceeds {MAX_BINARY} bytes");
        }
        return Ok(binary);
    }
    bail!("archive does not contain an owlet binary")
}

/// Writes `binary` next to `exe` and renames it over `exe`, so the swap is
/// atomic and a running process keeps its old inode.
fn install(exe: &Path, binary: &[u8]) -> anyhow::Result<()> {
    let dir = exe
        .parent()
        .context("executable path has no parent directory")?;
    let staged: PathBuf = dir.join(format!(".owlet-update-{}", std::process::id()));
    let result = stage_and_swap(&staged, exe, binary);
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

fn stage_and_swap(staged: &Path, exe: &Path, binary: &[u8]) -> anyhow::Result<()> {
    fs::write(staged, binary).with_context(|| {
        format!(
            "write {} (is the directory writable? try sudo)",
            staged.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(staged, fs::Permissions::from_mode(0o755))
            .context("mark the new binary executable")?;
    }
    fs::rename(staged, exe).with_context(|| format!("replace {}", exe.display()))
}

/// Entry point of `owlet update`: prints progress and replaces the running executable.
///
/// # Examples
///
/// ```ignore
/// update::run(false).await?;
/// ```
pub async fn run(check_only: bool) -> anyhow::Result<()> {
    let target = platform_target().context("no prebuilt release for this platform")?;
    let current = Version::parse(env!("CARGO_PKG_VERSION")).context("parse current version")?;
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("locate the running executable")?;

    match Updater::new(API_BASE, target)?
        .update(&current, &exe, check_only)
        .await?
    {
        Outcome::UpToDate => println!("owlet {current} is up to date"),
        Outcome::Available(latest) => {
            println!(
                "owlet {latest} is available (current {current}); run `owlet update` to install"
            )
        }
        Outcome::Updated(latest) => {
            println!("updated owlet {current} -> {latest}");
            println!("restart any running owlet (e.g. `systemctl restart owlet`) to use it");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use axum::{Json, Router, extract::Path as UrlPath, http::StatusCode, routing::get};
    use flate2::{Compression, write::GzEncoder};
    use tokio::net::TcpListener;

    use super::*;

    const TARGET: &str = "test-target";

    fn tarball(binary: &[u8]) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        let mut header = tar::Header::new_gnu();
        header.set_size(binary.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "owlet-v9.9.9-test-target/owlet", binary)
            .expect("append binary");
        builder
            .into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish gzip")
    }

    fn sha256_hex(data: &[u8]) -> String {
        let mut line = String::new();
        for b in Sha256::digest(data) {
            write!(line, "{b:02x}").expect("write hex");
        }
        line
    }

    /// Serves a fake GitHub API whose latest release is `tag`; returns its base URL.
    async fn serve(tag: &str, tarball: Vec<u8>, sha_file: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let archive = format!("owlet-{tag}-{TARGET}.tar.gz");
        let release = serde_json::json!({
            "tag_name": tag,
            "assets": [
                {"name": archive, "browser_download_url": format!("{base}/dl/{archive}")},
                {
                    "name": format!("{archive}.sha256"),
                    "browser_download_url": format!("{base}/dl/{archive}.sha256"),
                },
            ],
        });
        let files = Arc::new(HashMap::from([
            (archive.clone(), tarball),
            (format!("{archive}.sha256"), sha_file.into_bytes()),
        ]));
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
        base
    }

    fn fake_exe() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("owlet");
        fs::write(&exe, b"old binary").expect("write exe");
        (dir, exe)
    }

    fn v(s: &str) -> Version {
        Version::parse(s).expect("version")
    }

    #[test]
    fn extracts_the_binary() {
        let binary = extract_binary(&tarball(b"hello")).expect("extract");
        assert_eq!(binary, b"hello");
    }

    #[test]
    fn rejects_archive_without_binary() {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_cksum();
        builder
            .append_data(&mut header, "pkg/README.md", &b"x"[..])
            .expect("append");
        let data = builder.into_inner().expect("tar").finish().expect("gzip");
        assert!(extract_binary(&data).is_err());
    }

    #[test]
    fn verifies_shasum_output() {
        let line = format!("{}  owlet.tar.gz\n", sha256_hex(b"data"));
        verify_sha256(b"data", line.as_bytes()).expect("match");
        assert!(verify_sha256(b"other", line.as_bytes()).is_err());
        assert!(verify_sha256(b"data", b"").is_err());
    }

    #[test]
    fn parses_tags_with_and_without_prefix() {
        assert_eq!(parse_tag("v1.2.3").expect("tag"), v("1.2.3"));
        assert_eq!(parse_tag("1.2.3").expect("tag"), v("1.2.3"));
        assert!(parse_tag("nightly").is_err());
    }

    #[tokio::test]
    async fn replaces_the_executable() {
        let tgz = tarball(b"new binary");
        let base = serve("v9.9.9", tgz.clone(), sha256_hex(&tgz)).await;
        let (_dir, exe) = fake_exe();

        let outcome = Updater::new(&base, TARGET)
            .expect("updater")
            .update(&v("0.2.0"), &exe, false)
            .await
            .expect("update");

        assert_eq!(outcome, Outcome::Updated(v("9.9.9")));
        assert_eq!(fs::read(&exe).expect("read"), b"new binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&exe).expect("metadata").permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "binary must stay executable");
        }
        assert_eq!(
            fs::read_dir(exe.parent().expect("dir"))
                .expect("ls")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn check_only_leaves_the_executable_alone() {
        let tgz = tarball(b"new binary");
        let base = serve("v9.9.9", tgz.clone(), sha256_hex(&tgz)).await;
        let (_dir, exe) = fake_exe();

        let outcome = Updater::new(&base, TARGET)
            .expect("updater")
            .update(&v("0.2.0"), &exe, true)
            .await
            .expect("check");

        assert_eq!(outcome, Outcome::Available(v("9.9.9")));
        assert_eq!(fs::read(&exe).expect("read"), b"old binary");
    }

    #[tokio::test]
    async fn skips_when_already_latest() {
        let tgz = tarball(b"new binary");
        let base = serve("v0.2.0", tgz.clone(), sha256_hex(&tgz)).await;
        let (_dir, exe) = fake_exe();

        let outcome = Updater::new(&base, TARGET)
            .expect("updater")
            .update(&v("0.2.0"), &exe, false)
            .await
            .expect("update");

        assert_eq!(outcome, Outcome::UpToDate);
        assert_eq!(fs::read(&exe).expect("read"), b"old binary");
    }

    #[tokio::test]
    async fn refuses_a_checksum_mismatch() {
        let tgz = tarball(b"new binary");
        let base = serve("v9.9.9", tgz, sha256_hex(b"something else")).await;
        let (_dir, exe) = fake_exe();

        let err = Updater::new(&base, TARGET)
            .expect("updater")
            .update(&v("0.2.0"), &exe, false)
            .await
            .expect_err("must fail");

        assert!(format!("{err:#}").contains("sha256 mismatch"), "{err:#}");
        assert_eq!(fs::read(&exe).expect("read"), b"old binary");
        assert_eq!(
            fs::read_dir(exe.parent().expect("dir"))
                .expect("ls")
                .count(),
            1
        );
    }
}
