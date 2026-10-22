use std::path::PathBuf;

use owlet::{Plugin, ServerConfig};
use parking_lot::Mutex;
use tauri::State;

use crate::{
    server::{ServerState, ServerStatus},
    store::{Snapshot, Store},
};

/// Shared state behind every Tauri command.
pub struct AppState(Mutex<Store>);

impl AppState {
    /// Wraps a store for use as Tauri managed state.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let state = AppState::new(Store::open(path));
    /// ```
    pub fn new(store: Store) -> Self {
        Self(Mutex::new(store))
    }

    /// Path of the config file the store edits.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let path = state.config_path();
    /// ```
    pub fn config_path(&self) -> PathBuf {
        self.0.lock().path().to_path_buf()
    }

    /// Runs `f` on the store and returns the resulting snapshot.
    fn apply(&self, f: impl FnOnce(&mut Store) -> anyhow::Result<()>) -> Result<Snapshot, String> {
        let mut store = self.0.lock();
        f(&mut store).map_err(|e| format!("{e:#}"))?;
        Ok(store.snapshot())
    }
}

#[tauri::command]
pub fn get_state(state: State<'_, AppState>) -> Snapshot {
    state.0.lock().snapshot()
}

#[tauri::command]
pub fn set_enabled(
    state: State<'_, AppState>,
    name: String,
    enabled: bool,
) -> Result<Snapshot, String> {
    state.apply(|s| s.set_enabled(&name, enabled))
}

#[tauri::command]
pub fn upsert_server(
    state: State<'_, AppState>,
    old_name: Option<String>,
    name: String,
    server: ServerConfig,
) -> Result<Snapshot, String> {
    state.apply(|s| s.upsert(old_name.as_deref(), &name, server))
}

#[tauri::command]
pub fn delete_server(state: State<'_, AppState>, name: String) -> Result<Snapshot, String> {
    state.apply(|s| s.delete(&name))
}

/// Downloads the plugin from the matching GitHub release into the plugins
/// directory and adds its server entry to the draft.
#[tauri::command]
pub async fn install_plugin(state: State<'_, AppState>, name: String) -> Result<Snapshot, String> {
    let plugin = Plugin::find(&name).map_err(|e| format!("{e:#}"))?;
    let dir = state.0.lock().plugins_dir().map_err(|e| format!("{e:#}"))?;
    plugin.install(&dir).await.map_err(|e| format!("{e:#}"))?;
    state.apply(|s| s.install_plugin(plugin))
}

/// Removes the plugin's server entries from the draft; Save deletes the file.
#[tauri::command]
pub fn uninstall_plugin(state: State<'_, AppState>, name: String) -> Result<Snapshot, String> {
    state.apply(|s| s.uninstall_plugin(Plugin::find(&name)?))
}

#[tauri::command]
pub fn save(state: State<'_, AppState>) -> Result<Snapshot, String> {
    state.apply(Store::save)
}

/// Drops the draft and re-reads the config file.
#[tauri::command]
pub fn reload(state: State<'_, AppState>) -> Snapshot {
    let mut store = state.0.lock();
    store.reload();
    store.snapshot()
}

#[tauri::command]
pub fn server_status(server: State<'_, ServerState>) -> ServerStatus {
    server.status()
}

#[tauri::command]
pub fn start_server(server: State<'_, ServerState>, state: State<'_, AppState>) -> ServerStatus {
    server.start(state.config_path())
}

#[tauri::command]
pub async fn stop_server(server: State<'_, ServerState>) -> Result<ServerStatus, String> {
    server.stop().await;
    Ok(server.status())
}

/// Stops the server, then starts it again so it picks up the saved config.
#[tauri::command]
pub async fn restart_server(
    server: State<'_, ServerState>,
    state: State<'_, AppState>,
) -> Result<ServerStatus, String> {
    server.stop().await;
    Ok(server.start(state.config_path()))
}
