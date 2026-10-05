//! Desktop UI for editing the `[servers]` table of an owlet config file.

mod commands;
mod server;
mod store;
mod tray;
mod updater;

use std::path::PathBuf;

use anyhow::Context;
use owlet::Config;
use tauri::{Emitter, Manager};

pub use commands::AppState;
pub use server::{ServerState, ServerStatus};
pub use store::{Entry, Snapshot, Status, Store};
pub use updater::{PendingUpdate, UpdateInfo};

/// Environment variable overriding the config file path.
pub const CONFIG_ENV: &str = "OWLET_CONFIG";

/// Event carrying every [`ServerStatus`] change to the frontend.
pub const SERVER_STATUS_EVENT: &str = "server-status";

/// Starts the UI and the embedded owlet server on the config file from
/// `$OWLET_CONFIG` or the default path.
///
/// # Examples
///
/// ```ignore
/// owlet_ui::run()?;
/// ```
pub fn run() -> anyhow::Result<()> {
    let path = match std::env::var_os(CONFIG_ENV).filter(|p| !p.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => Config::default_path()?,
    };
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(AppState::new(Store::open(path)))
        .manage(PendingUpdate::default())
        .setup(|app| {
            tray::setup(app)?;
            let handle = app.handle().clone();
            let server = ServerState::new(move |status| {
                if let Err(e) = handle.emit(SERVER_STATUS_EVENT, status) {
                    eprintln!("owlet-ui: cannot emit server status: {e}");
                }
            });
            server.start(app.state::<AppState>().config_path());
            app.manage(server);
            Ok(())
        })
        .on_window_event(tray::hide_on_close)
        .invoke_handler(tauri::generate_handler![
            commands::get_state,
            commands::set_enabled,
            commands::upsert_server,
            commands::delete_server,
            commands::save,
            commands::reload,
            commands::server_status,
            commands::start_server,
            commands::stop_server,
            commands::restart_server,
            updater::check_update,
            updater::install_update,
        ])
        .build(tauri::generate_context!())
        .context("build tauri application")?
        .run(|app, event| match event {
            // Stop upstream processes before the UI exits.
            tauri::RunEvent::Exit => {
                tauri::async_runtime::block_on(app.state::<ServerState>().stop());
            }
            // Dock icon click on macOS while the window is hidden in the tray.
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen {
                has_visible_windows: false,
                ..
            } => tray::show_main_window(app),
            _ => {}
        });
    Ok(())
}
