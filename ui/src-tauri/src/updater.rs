use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, State};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::server::ServerState;

/// The update found by the last [`check_update`], kept for [`install_update`].
#[derive(Default)]
pub struct PendingUpdate(Mutex<Option<Update>>);

/// A newer release as shown to the user.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct UpdateInfo {
    current: String,
    version: String,
    notes: Option<String>,
}

impl UpdateInfo {
    /// Builds the summary of an available update; blank notes become `None`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let info = UpdateInfo::new("0.2.0", "0.3.0", Some("Fixes".into()));
    /// ```
    pub fn new(current: &str, version: &str, notes: Option<String>) -> Self {
        Self {
            current: current.to_owned(),
            version: version.to_owned(),
            notes: notes.filter(|n| !n.trim().is_empty()),
        }
    }
}

fn message(e: impl std::fmt::Display) -> String {
    format!("{e:#}")
}

/// Asks the update endpoint whether a newer signed release exists.
#[tauri::command]
pub async fn check_update(
    app: AppHandle,
    pending: State<'_, PendingUpdate>,
) -> Result<Option<UpdateInfo>, String> {
    let update = app
        .updater()
        .map_err(message)?
        .check()
        .await
        .map_err(message)?;
    let info = update
        .as_ref()
        .map(|u| UpdateInfo::new(&u.current_version, &u.version, u.body.clone()));
    *pending.0.lock() = update;
    Ok(info)
}

/// Downloads and installs the update found by [`check_update`], then relaunches.
#[tauri::command]
pub async fn install_update(
    app: AppHandle,
    pending: State<'_, PendingUpdate>,
    server: State<'_, ServerState>,
) -> Result<(), String> {
    let update = pending.0.lock().clone();
    let Some(update) = update else {
        return Err("no update has been checked for".to_owned());
    };
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(message)?;
    // `restart` does not fire `RunEvent::Exit`, so stop upstream processes here.
    server.stop().await;
    app.restart()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_real_notes() {
        let info = UpdateInfo::new("0.2.0", "0.3.0", Some("Fixes".to_owned()));
        assert_eq!(info.notes.as_deref(), Some("Fixes"));
        assert_eq!(info.version, "0.3.0");
        assert_eq!(info.current, "0.2.0");
    }

    #[test]
    fn drops_blank_notes() {
        assert_eq!(
            UpdateInfo::new("0.2.0", "0.3.0", Some("  \n".to_owned())).notes,
            None
        );
        assert_eq!(UpdateInfo::new("0.2.0", "0.3.0", None).notes, None);
    }
}
