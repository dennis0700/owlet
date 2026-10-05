use anyhow::Context;
use tauri::{
    AppHandle, Manager, Runtime, Window, WindowEvent,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

const MAIN_WINDOW: &str = "main";

/// Brings the main window back from the tray (or minimized state) to the front.
///
/// # Examples
///
/// ```ignore
/// show_main_window(&app_handle);
/// ```
pub fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    #[cfg(target_os = "macos")]
    if let Err(e) = app.set_dock_visibility(true) {
        eprintln!("owlet-ui: cannot show dock icon: {e}");
    }
    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        return;
    };
    for result in [window.show(), window.unminimize(), window.set_focus()] {
        if let Err(e) = result {
            eprintln!("owlet-ui: cannot show main window: {e}");
        }
    }
}

/// Hides the main window instead of closing it, so the draft stays in memory
/// and the app keeps running in the tray.
///
/// # Examples
///
/// ```ignore
/// builder.on_window_event(|window, event| tray::hide_on_close(window, event));
/// ```
pub fn hide_on_close<R: Runtime>(window: &Window<R>, event: &WindowEvent) {
    if window.label() != MAIN_WINDOW {
        return;
    }
    if let WindowEvent::CloseRequested { api, .. } = event {
        api.prevent_close();
        if let Err(e) = window.hide() {
            eprintln!("owlet-ui: cannot hide main window: {e}");
        }
        #[cfg(target_os = "macos")]
        if let Err(e) = window.app_handle().set_dock_visibility(false) {
            eprintln!("owlet-ui: cannot hide dock icon: {e}");
        }
    }
}

/// Creates the tray icon: left click shows the window, the menu offers Show and Quit.
///
/// # Examples
///
/// ```ignore
/// tauri::Builder::default().setup(|app| Ok(tray::setup(app)?));
/// ```
pub fn setup<R: Runtime>(app: &tauri::App<R>) -> anyhow::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show Owlet", true, None::<&str>)
        .context("create tray menu item")?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)
        .context("create tray menu item")?;
    let menu = Menu::with_items(app, &[&show, &quit]).context("create tray menu")?;
    let icon = app
        .default_window_icon()
        .cloned()
        .context("missing default window icon")?;

    TrayIconBuilder::with_id("main")
        .icon(icon)
        .tooltip("Owlet")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)
        .context("create tray icon")?;
    Ok(())
}
