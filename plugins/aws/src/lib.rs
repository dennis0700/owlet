//! owlet WASM plugin: an MCP proxy for SigV4-protected AWS endpoints.
//!
//! The Rust counterpart of `uvx mcp-proxy-for-aws <endpoint> [--metadata K=V]`:
//! every MCP message from owlet is POSTed to the Streamable HTTP endpoint,
//! signed with SigV4, with metadata merged into `params._meta`. `--read-only`
//! hides and blocks tools without `readOnlyHint: true`; `--profile a b ...`
//! lets auth tools pick the signing profile per call (`aws_profile`), each
//! non-default profile on its own backend session. Configuration
//! comes from WASI argv (see [`cli`]); credentials come from the AWS SDK chain
//! over the WASI environment and the shared profile files under the preopened
//! `~/.aws`, including role and web identity profiles (see [`sigv4`]).
//! Outbound HTTP (to the endpoint, STS and ECS) goes through
//! owlet's `owlet:plugin/http` import. On shutdown every live backend session
//! is ended with a `DELETE`.

wit_bindgen::generate!({
    world: "mcp-plugin",
});

mod cli;
mod proxy;
mod sigv4;
mod sse;

use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock, PoisonError},
    time::SystemTime,
};

struct Plugin;

export!(Plugin);

/// Arguments, parsed on first use.
static OPTIONS: OnceLock<Result<cli::Options, String>> = OnceLock::new();
static SESSION: Mutex<proxy::Session> = Mutex::new(proxy::Session {
    id: None,
    version: None,
    init: None,
    expired: false,
    tools: BTreeMap::new(),
    profiles: BTreeMap::new(),
});
/// Temporary credentials of role profiles, shared by all sessions.
static CREDENTIALS: Mutex<sigv4::Cache> = Mutex::new(sigv4::Cache::new(owlet::plugin::http::send));

/// Reads a WASI environment variable; empty values count as unset.
fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

impl Guest for Plugin {
    fn handle(request: String) -> String {
        let options = OPTIONS.get_or_init(|| {
            let args: Vec<String> = std::env::args_os()
                .skip(1)
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            cli::parse(&args, &env)
        });
        let mut session = SESSION.lock().unwrap_or_else(PoisonError::into_inner);
        let mut credentials = CREDENTIALS.lock().unwrap_or_else(PoisonError::into_inner);
        proxy::handle(
            options.as_ref(),
            &mut session,
            &request,
            &env,
            &mut credentials,
            SystemTime::now(),
            owlet::plugin::http::send,
        )
    }

    /// DELETEs the live backend sessions, like `terminate_on_close` in
    /// mcp-proxy-for-aws.
    fn on_shutdown() {
        let Some(Ok(options)) = OPTIONS.get() else {
            return;
        };
        let session = SESSION.lock().unwrap_or_else(PoisonError::into_inner);
        let mut credentials = CREDENTIALS.lock().unwrap_or_else(PoisonError::into_inner);
        proxy::shutdown(
            options,
            &session,
            &env,
            &mut credentials,
            SystemTime::now(),
            owlet::plugin::http::send,
        );
    }
}
