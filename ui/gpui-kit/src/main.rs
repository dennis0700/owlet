//! Binary entry point: opens the Owlet desktop window.
//!
//! The window hosts an [`owlet_gpui::AppModel`] rooted view that reproduces
//! the former Tauri/React UI. Closing the window quits the whole application
//! (there is no tray-resident mode; see the crate docs and FEAT-002 findings).
//! Before quitting, the embedded server is shut down through its normal
//! graceful path so upstream MCP child processes are stopped explicitly
//! rather than left to OS teardown.

use std::path::PathBuf;

use gpui_kit::*;
use owlet::Config;
use owlet_gpui::AppModel;

/// Resolves the config file the UI edits: `$OWLET_CONFIG` if set, otherwise
/// the platform default `~/.config/owlet/config.toml`.
fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("OWLET_CONFIG") {
        return PathBuf::from(path);
    }
    Config::default_path().unwrap_or_else(|_| PathBuf::from("owlet.toml"))
}

fn main() {
    let path = config_path();
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        let path = path.clone();
        let (_window, model) = gpui_kit::open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Owlet".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            cx,
            move |window, cx| cx.new(|cx| AppModel::new(path.clone(), window, cx)),
        )
        .expect("failed to open window");

        // No tray: when the last window closes, gracefully stop the embedded
        // server (same path as the Stop button) and quit the application.
        cx.on_window_closed(move |cx, _window_id| {
            if cx.windows().is_empty() {
                model.read(cx).shutdown_server();
                cx.quit();
            }
        })
        .detach();
    });
}
