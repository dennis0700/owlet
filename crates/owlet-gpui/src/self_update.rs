//! In-place replacement of the running `Owlet.app` (macOS).
//!
//! Modelled on FluxDown's macOS updater, minus the Developer ID team check
//! (Owlet is ad-hoc signed):
//! 1. [`preflight`]: the running bundle must sit in a writable directory
//!    (this also rules out App Translocation and running from the dmg, both
//!    read-only);
//! 2. [`apply`]: mount the verified dmg read-only, `ditto` the new app into a
//!    staging directory next to the old bundle (same volume, so the renames
//!    are atomic), `codesign --verify --deep --strict` it, then move the old
//!    bundle aside and the new one into place, restoring the old one if the
//!    second rename fails;
//! 3. [`relaunch`]: after the old process has stopped its server, start the
//!    new bundle through LaunchServices (`open -n`);
//! 4. [`cleanup_leftovers`]: on the next start, delete the moved-aside bundle
//!    and any staging directory.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

const HDIUTIL: &str = "/usr/bin/hdiutil";
const DITTO: &str = "/usr/bin/ditto";
const CODESIGN: &str = "/usr/bin/codesign";
const OPEN: &str = "/usr/bin/open";
/// Name of the app inside the release dmg (see `bundle-macos.sh`).
const DMG_APP: &str = "Owlet.app";
const STAGING_PREFIX: &str = ".owlet-staging-";

/// Returns the running app bundle when Owlet can replace it in place.
///
/// # Errors
///
/// When not running from an `.app`, or when its directory is not writable
/// (read-only volume, App Translocation, missing permissions).
///
/// # Examples
///
/// ```ignore
/// let bundle = self_update::preflight()?;
/// ```
pub(crate) fn preflight() -> anyhow::Result<PathBuf> {
    let bundle = current_bundle()?;
    let parent = parent_of(&bundle)?;
    if !dir_writable(parent) {
        bail!(
            "{} is not writable; move Owlet to /Applications to update in place",
            parent.display()
        );
    }
    Ok(bundle)
}

/// Replaces `bundle` with the app inside `dmg` (already checksum-verified).
///
/// On error the old bundle is left (or put back) in place.
///
/// # Examples
///
/// ```ignore
/// self_update::apply(&dmg_bytes, &bundle)?;
/// ```
pub(crate) fn apply(dmg: &[u8], bundle: &Path) -> anyhow::Result<()> {
    let parent = parent_of(bundle)?;
    let pid = std::process::id();
    let work = std::env::temp_dir().join(format!("owlet-update-{pid}"));
    let staging = parent.join(format!("{STAGING_PREFIX}{pid}"));
    let result = stage_and_swap(dmg, &work, &staging, bundle);
    for dir in [&work, &staging] {
        if let Err(e) = remove_dir_if_exists(dir) {
            eprintln!("owlet-gpui: cannot remove {}: {e}", dir.display());
        }
    }
    result
}

/// Starts a fresh instance of `bundle` through LaunchServices.
///
/// # Examples
///
/// ```ignore
/// self_update::relaunch(&bundle)?;
/// ```
pub(crate) fn relaunch(bundle: &Path) -> anyhow::Result<()> {
    Command::new(OPEN)
        .arg("-n")
        .arg(bundle)
        .spawn()
        .with_context(|| format!("open {}", bundle.display()))?;
    Ok(())
}

/// Deletes the bundle moved aside by a previous update and stale staging
/// directories. A no-op when not running from an `.app`.
///
/// # Examples
///
/// ```ignore
/// self_update::cleanup_leftovers();
/// ```
pub(crate) fn cleanup_leftovers() {
    let Ok(bundle) = current_bundle() else {
        return;
    };
    let (Some(parent), Some(name)) = (bundle.parent(), bundle.file_name()) else {
        return;
    };
    cleanup_dir(parent, &name.to_string_lossy());
}

fn current_bundle() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("locate the running executable")?;
    outer_bundle(&exe).context("Owlet is not running from an .app bundle")
}

/// The outermost `*.app` directory containing `exe`.
fn outer_bundle(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .filter(|p| p.extension() == Some(OsStr::new("app")))
        .last()
        .map(Path::to_path_buf)
}

fn parent_of(bundle: &Path) -> anyhow::Result<&Path> {
    bundle
        .parent()
        .with_context(|| format!("{} has no parent directory", bundle.display()))
}

/// Creates and deletes a probe file: honest about ACLs and read-only mounts,
/// unlike permission bits.
fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".owlet-write-probe-{}", std::process::id()));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(file) => {
            drop(file);
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn stage_and_swap(dmg: &[u8], work: &Path, staging: &Path, bundle: &Path) -> anyhow::Result<()> {
    remove_dir_if_exists(work).with_context(|| format!("clear {}", work.display()))?;
    let mount = work.join("mnt");
    fs::create_dir_all(&mount).with_context(|| format!("create {}", mount.display()))?;
    let image = work.join("Owlet.dmg");
    fs::write(&image, dmg).with_context(|| format!("write {}", image.display()))?;

    remove_dir_if_exists(staging).with_context(|| format!("clear {}", staging.display()))?;
    fs::create_dir(staging).with_context(|| format!("create {}", staging.display()))?;
    let new_app = staging.join(DMG_APP);
    copy_from_dmg(&image, &mount, &new_app)?;

    run(
        CODESIGN,
        [
            OsStr::new("--verify"),
            OsStr::new("--deep"),
            OsStr::new("--strict"),
            new_app.as_os_str(),
        ],
    )
    .context("the new app failed code signature verification")?;
    swap(bundle, &new_app)
}

/// Mounts `image` read-only at `mount` and copies its `Owlet.app` to `dest`.
fn copy_from_dmg(image: &Path, mount: &Path, dest: &Path) -> anyhow::Result<()> {
    run(
        HDIUTIL,
        [
            OsStr::new("attach"),
            OsStr::new("-nobrowse"),
            OsStr::new("-noautoopen"),
            OsStr::new("-readonly"),
            OsStr::new("-mountpoint"),
            mount.as_os_str(),
            image.as_os_str(),
        ],
    )?;
    let source = mount.join(DMG_APP);
    let copied = if source.is_dir() {
        run(DITTO, [source.as_os_str(), dest.as_os_str()])
    } else {
        Err(anyhow::anyhow!("the disk image does not contain {DMG_APP}"))
    };
    detach(mount);
    copied
}

/// Unmounts the dmg, forcing if busy; failure only leaves a hidden read-only
/// mount behind, so it is logged rather than failing the update.
fn detach(mount: &Path) {
    for force in [false, true] {
        let mut args = vec![OsStr::new("detach")];
        if force {
            args.push(OsStr::new("-force"));
        }
        args.push(mount.as_os_str());
        match run(HDIUTIL, args) {
            Ok(()) => return,
            Err(e) => eprintln!("owlet-gpui: {e:#}"),
        }
    }
}

/// Moves `bundle` aside and `new_app` into its place; puts `bundle` back if
/// the second rename fails. The old bundle stays until [`cleanup_leftovers`]
/// because the running process still executes from it.
fn swap(bundle: &Path, new_app: &Path) -> anyhow::Result<()> {
    let old = aside_path(bundle)?;
    remove_dir_if_exists(&old).with_context(|| format!("clear {}", old.display()))?;
    fs::rename(bundle, &old).with_context(|| format!("move {} aside", bundle.display()))?;
    if let Err(e) = fs::rename(new_app, bundle) {
        return Err(match fs::rename(&old, bundle) {
            Ok(()) => {
                anyhow::Error::new(e).context(format!("move the new app to {}", bundle.display()))
            }
            Err(restore) => anyhow::Error::new(e).context(format!(
                "move the new app to {}; restoring the old app from {} also failed: {restore}",
                bundle.display(),
                old.display()
            )),
        });
    }
    Ok(())
}

/// `<parent>/.<name>.old-<pid>`.
fn aside_path(bundle: &Path) -> anyhow::Result<PathBuf> {
    let name = bundle
        .file_name()
        .with_context(|| format!("{} has no file name", bundle.display()))?;
    Ok(parent_of(bundle)?.join(format!(
        ".{}.old-{}",
        name.to_string_lossy(),
        std::process::id()
    )))
}

/// Removes `.<name>.old-*` and `.owlet-staging-*` entries in `dir`.
fn cleanup_dir(dir: &Path, name: &str) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let aside = format!(".{name}.old-");
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !(file_name.starts_with(&aside) || file_name.starts_with(STAGING_PREFIX)) {
            continue;
        }
        let path = entry.path();
        if let Err(e) = fs::remove_dir_all(&path) {
            eprintln!("owlet-gpui: cannot remove {}: {e}", path.display());
        }
    }
}

fn remove_dir_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Runs a system tool; a non-zero exit is an error carrying its stderr.
fn run<I, S>(program: &str, args: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("run {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(dir: &Path, name: &str, version: &str) -> PathBuf {
        let bundle = dir.join(name);
        fs::create_dir_all(bundle.join("Contents")).expect("create bundle");
        fs::write(bundle.join("Contents").join("version"), version).expect("write version");
        bundle
    }

    fn version(bundle: &Path) -> String {
        fs::read_to_string(bundle.join("Contents").join("version")).expect("read version")
    }

    #[test]
    fn finds_the_outermost_bundle() {
        assert_eq!(
            outer_bundle(Path::new(
                "/Applications/Owlet.app/Contents/Helpers/X.app/Contents/MacOS/x"
            )),
            Some(PathBuf::from("/Applications/Owlet.app"))
        );
        assert_eq!(outer_bundle(Path::new("/usr/local/bin/owlet-gpui")), None);
    }

    #[test]
    fn probes_writability() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(dir_writable(dir.path()));
        assert!(!dir_writable(&dir.path().join("missing")));
    }

    #[test]
    fn swap_replaces_and_keeps_the_old_bundle_aside() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = app(dir.path(), "Owlet.app", "old");
        let new_app = app(&dir.path().join(".staging"), "Owlet.app", "new");

        swap(&bundle, &new_app).expect("swap");

        assert_eq!(version(&bundle), "new");
        assert_eq!(version(&aside_path(&bundle).expect("aside")), "old");
    }

    #[test]
    fn swap_restores_the_old_bundle_on_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = app(dir.path(), "Owlet.app", "old");

        let missing = dir.path().join(".staging").join("Owlet.app");
        assert!(swap(&bundle, &missing).is_err());

        assert_eq!(version(&bundle), "old");
        assert!(!aside_path(&bundle).expect("aside").exists());
    }

    #[test]
    fn cleanup_removes_only_own_leftovers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let keep = [
            app(dir.path(), "Owlet.app", "current"),
            app(dir.path(), ".Other.app.old-1", "x"),
        ];
        let gone = [
            app(dir.path(), ".Owlet.app.old-42", "x"),
            app(dir.path(), ".owlet-staging-42", "x"),
        ];

        cleanup_dir(dir.path(), "Owlet.app");

        assert!(keep.iter().all(|p| p.exists()));
        assert!(gone.iter().all(|p| !p.exists()));
    }

    /// Builds a real ad-hoc signed app inside a dmg and swaps it in.
    #[test]
    #[ignore = "needs macOS hdiutil/codesign; run with --ignored"]
    fn applies_a_real_dmg() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src");
        let new_app = src.join(DMG_APP);
        fs::create_dir_all(new_app.join("Contents/MacOS")).expect("create app");
        fs::copy("/usr/bin/true", new_app.join("Contents/MacOS/owlet-gpui")).expect("copy exe");
        fs::write(
            new_app.join("Contents/Info.plist"),
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\
             <key>CFBundleExecutable</key><string>owlet-gpui</string>\
             <key>CFBundleIdentifier</key><string>dev.owlet.test</string>\
             </dict></plist>",
        )
        .expect("write plist");
        run(
            CODESIGN,
            [
                OsStr::new("--force"),
                OsStr::new("--sign"),
                OsStr::new("-"),
                new_app.as_os_str(),
            ],
        )
        .expect("sign");
        let image = dir.path().join("t.dmg");
        run(
            HDIUTIL,
            [
                OsStr::new("create"),
                OsStr::new("-volname"),
                OsStr::new("Owlet"),
                OsStr::new("-srcfolder"),
                src.as_os_str(),
                OsStr::new("-format"),
                OsStr::new("UDZO"),
                image.as_os_str(),
            ],
        )
        .expect("create dmg");

        let installed = dir.path().join("apps");
        let bundle = app(&installed, "Owlet.app", "old");
        apply(&fs::read(&image).expect("read dmg"), &bundle).expect("apply");

        assert!(bundle.join("Contents/MacOS/owlet-gpui").is_file());
        assert_eq!(version(&aside_path(&bundle).expect("aside")), "old");
        assert!(
            !installed
                .join(format!("{STAGING_PREFIX}{}", std::process::id()))
                .exists()
        );
    }
}
