//! Menu bar (tray) icon that keeps the app reachable while its window is
//! hidden. Built on `gpui-tray`, whose menu items dispatch gpui actions:
//! **Show Owlet** dispatches [`ShowWindow`], **Quit** dispatches [`Quit`].
//! The application registers handlers for both.

use anyhow::Context;
use gpui_kit::{App, Global, MenuItem, actions};
use gpui_tray::{Icon, Tray};

actions!(owlet_tray, [ShowWindow, Quit]);

/// Tray icon image (the app icon, scaled to 18pt at 2x for the menu bar).
const ICON_PNG: &[u8] = include_bytes!("../assets/icons/128x128.png");
const ICON_PX: u32 = 36;

/// Keeps the tray alive for the app's lifetime; dropping it removes the icon.
struct TrayHandle {
    _tray: Tray,
}

impl Global for TrayHandle {}

/// Creates the tray icon and keeps it alive in a gpui global.
///
/// Must be called after the application has launched. Register handlers for
/// [`ShowWindow`] and [`Quit`] with `cx.on_action`.
///
/// # Examples
///
/// ```ignore
/// cx.on_action(|_: &ShowWindow, cx| show_window(cx));
/// cx.on_action(|_: &Quit, cx| cx.quit());
/// owlet_gpui::install_tray(cx)?;
/// ```
pub fn install_tray(cx: &mut App) -> anyhow::Result<()> {
    let tray = Tray::builder()
        .icon(icon()?)
        .tooltip("Owlet")
        .menu(|_| {
            vec![
                MenuItem::action("Show Owlet", ShowWindow),
                MenuItem::separator(),
                MenuItem::action("Quit", Quit),
            ]
        })
        .build(cx)
        .context("create tray icon")?;
    cx.set_global(TrayHandle { _tray: tray });
    Ok(())
}

/// Decodes the embedded PNG into a 2x menu bar icon.
fn icon() -> anyhow::Result<Icon> {
    let image = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png)
        .context("decode tray icon")?
        .resize_exact(ICON_PX, ICON_PX, image::imageops::FilterType::Lanczos3)
        .into_rgba8();
    Icon::from_rgba(image.into_raw(), ICON_PX, ICON_PX)
        .scale_factor(2.0)
        .build()
        .context("build tray icon")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_icon_decodes() {
        icon().expect("embedded tray icon must decode");
    }
}
