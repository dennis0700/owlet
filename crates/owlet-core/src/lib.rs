mod admin;
mod aggregate;
mod config;
mod http;
mod hub;
mod jsonrpc;
mod plugin;
mod update;
mod upstream;

pub use self::admin::AdminClient;
pub use self::config::{Config, RemoteTransport, Scope, ServerConfig};
pub use self::http::router;
pub use self::hub::Hub;
pub use self::plugin::Plugin;
pub use self::update::run as self_update;
