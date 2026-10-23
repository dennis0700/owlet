//! macOS application menu: **Check for Updates…**, version and **Quit**.

use gpui_kit::{Menu, MenuItem, actions};

use crate::tray::{Quit, ShowWindow};

actions!(owlet_menu, [CheckForUpdates]);

/// Menus for `cx.set_menus`. The app handles [`CheckForUpdates`] and
/// [`Quit`] with `cx.on_action`.
///
/// # Examples
///
/// ```ignore
/// cx.set_menus(owlet_gpui::app_menus());
/// ```
pub fn app_menus() -> Vec<Menu> {
    vec![Menu::new("Owlet").items([
        MenuItem::action("Check for Updates\u{2026}", CheckForUpdates),
        // Informational only; the action never fires on a disabled item.
        MenuItem::action(format!("Owlet {}", env!("CARGO_PKG_VERSION")), ShowWindow).disabled(true),
        MenuItem::separator(),
        MenuItem::action("Quit Owlet", Quit),
    ])]
}
