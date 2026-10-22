//! Binary entry point: opens the Owlet desktop window.
//!
//! The window hosts an [`owlet_gpui::AppModel`] rooted view that reproduces
//! the former Tauri/React UI. Closing the window quits the whole application
//! (there is no tray-resident mode; see the crate docs and FEAT-002 findings).

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
        gpui_kit::open_window(
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

        // No tray: when the last window closes, quit the application (which
        // stops the embedded owlet server with it).
        cx.on_window_closed(|cx, _window_id| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
    });
}
