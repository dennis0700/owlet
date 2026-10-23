//! Library crate backing the gpui-kit desktop UI for owlet.
//!
//! This crate hosts the application state ([`Store`]), the MCP server
//! lifecycle manager ([`ServerState`]), server-form parsing ([`from_form`]),
//! update checking ([`Updater`]) and the menu bar icon ([`install_tray`]). The
//! gpui-kit window/view code lives in `main.rs` and `app.rs`.

mod app;
mod form;
mod server;
mod store;
mod tray;
mod updater;

pub use self::app::AppModel;
pub use self::form::{FormValues, Kind, from_form, to_form};
pub use self::server::{ServerState, ServerStatus};
pub use self::store::{Entry, PluginInfo, Snapshot, Status, Store};
pub use self::tray::{Quit, ShowWindow, install_tray};
pub use self::updater::{UpdateInfo, Updater, open_download};
