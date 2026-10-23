//! Binary entry point: opens the Owlet desktop window and its tray icon.
//!
//! The window hosts an [`owlet_gpui::AppModel`] rooted view that reproduces
//! the former Tauri/React UI. Closing the window only hides the app (and its
//! Dock icon) so the embedded server keeps running; the tray icon, a Dock
//! click or relaunching the app brings it back. Quitting (tray **Quit**, Dock
//! **Quit**, logout) shuts the embedded server down through its normal
//! graceful path so upstream MCP child processes are stopped explicitly
//! rather than left to OS teardown.

use std::path::PathBuf;

use gpui_kit::*;
use owlet::Config;
use owlet_gpui::{AppModel, Quit, ShowWindow};

/// Resolves the config file the UI edits: `$OWLET_CONFIG` if set, otherwise
/// the platform default `~/.config/owlet/config.toml`.
fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("OWLET_CONFIG") {
        return PathBuf::from(path);
    }
    Config::default_path().unwrap_or_else(|_| PathBuf::from("owlet.toml"))
}

/// Shows the Dock icon again and brings the main window to the front.
fn show_window(cx: &mut App) {
    cx.set_activation_policy(ActivationPolicy::Regular);
    cx.activate(true);
    let Some(window) = cx.windows().first().copied() else {
        return;
    };
    if let Err(e) = window.update(cx, |_, window, _| window.activate_window()) {
        eprintln!("owlet-gpui: cannot show main window: {e:#}");
    }
}

/// Hides the app and its Dock icon, leaving only the tray icon.
fn hide_window(cx: &mut App) {
    cx.hide();
    cx.set_activation_policy(ActivationPolicy::Accessory);
}

/// Creates the tray icon and handles its menu actions.
fn start_tray(cx: &mut App) {
    cx.on_action(|_: &ShowWindow, cx| show_window(cx));
    cx.on_action(|_: &Quit, cx| cx.quit());
    if let Err(e) = owlet_gpui::install_tray(cx) {
        // The window can still be brought back by relaunching the app.
        eprintln!("owlet-gpui: cannot create tray icon: {e:#}");
    }
}

fn main() {
    let path = config_path();
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    // Dock click or relaunch while the window is hidden.
    app.on_reopen(show_window);
    app.run(move |cx| {
        gpui_kit::init(cx);
        let path = path.clone();
        let (_window, model) = gpui_kit::open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Owlet".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(760.), px(820.)),
                    cx,
                ))),
                ..Default::default()
            },
            cx,
            move |window, cx| {
                window.on_window_should_close(cx, |_, cx| {
                    hide_window(cx);
                    false
                });
                cx.new(|cx| AppModel::new(path.clone(), window, cx))
            },
        )
        .expect("failed to open window");

        // Every quit path gracefully stops the embedded server first (same
        // path as the Stop button).
        cx.on_app_quit(move |cx| {
            model.read(cx).shutdown_server();
            async {}
        })
        .detach();

        start_tray(cx);
    });
}
