# Writing owlet WASM plugins

This guide is for building a plugin, not just using one. For installing and
configuring a plugin that already exists, see the [WASM plugins](../README.md#wasm-plugins)
section of the README.

An owlet plugin is a [WebAssembly Component](https://component-model.bytecodealliance.org/)
that owlet runs in-process with [wasmtime](https://wasmtime.dev/); there is no
external runtime, container, or language dependency. It is a self-contained MCP
server: owlet hands it one MCP JSON-RPC message at a time and forwards whatever
it replies.

## The contract

A plugin implements the `mcp-plugin` world in [`wit/mcp-plugin.wit`](../wit/mcp-plugin.wit).
It is small:

- `export handle(request: string) -> string` — handle one MCP JSON-RPC message.
  Return zero or more JSON-RPC messages separated by `\n`, or an empty string
  for "no response" (notifications, or accepted-without-reply).
- `export on-shutdown()` — called once when the instance stops (server stop,
  session end, owlet exit), after the last `handle` returned. Best effort, with
  a short grace period during which outbound HTTP still works.
- `import http.send(...)` — the plugin's **only** network access. WASI sockets
  and DNS stay disabled. owlet performs the request on the plugin's behalf.

Calls are serial per instance: the next message is delivered only after `handle`
returns, so a slow `http.send` blocks every other request to that instance. Use
`scope = "session"` in the config to give each client its own instance.

## Capabilities a plugin gets

owlet grants only what the server config declares:

- `args` — the plugin's WASI argv (after the server name).
- `env` + `inherit_env` — its WASI environment. `inherit_env` forwards host vars
  (an entry ending in `*` is a prefix match, e.g. `AWS_*`); `env` wins on conflict.
- `dirs` — host directories preopened **read-only** at the same guest path
  (`~/` expands to `$HOME`).
- `http.send` — reaches any http(s) URL as given, loopback and link-local
  included. There is no destination allowlist, so **a plugin is trusted code**.

`cwd` and `scope = "project"` do not apply to `wasm` servers.

## Toolchain

Plugins target `wasm32-wasip2` and are built as a cdylib component. Install the
target once:

```sh
rustup target add wasm32-wasip2
```

Bindings are generated from the WIT with [`wit-bindgen`](https://crates.io/crates/wit-bindgen).
No `cargo-component` or `wasm-tools` post-processing is needed: `wasm32-wasip2`
emits a component directly.

## A minimal plugin

The repository ships a complete minimal example at [`plugins/echo`](../plugins/echo):
a self-contained MCP server that implements `initialize`, `tools/list` (one
`echo` tool) and `tools/call`. Use it as a starting template.

`Cargo.toml`:

```toml
[package]
name = "owlet-plugin-echo"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen = "0.46.0"
serde_json = "1.0"

[profile.release]
opt-level = "s"
strip = true
```

Copy [`wit/mcp-plugin.wit`](../wit/mcp-plugin.wit) into the plugin's own `wit/`
directory, then in `src/lib.rs`:

```rust
wit_bindgen::generate!({ world: "mcp-plugin" });

struct Plugin;
export!(Plugin);

impl Guest for Plugin {
    fn handle(request: String) -> String {
        // Parse the JSON-RPC `request`, build a reply, return it as a string.
        // Return "" for notifications (messages without an `id`).
        reply(&request).unwrap_or_default()
    }

    fn on_shutdown() {
        // End any upstream sessions here. Best effort; `http.send` still works
        // briefly. Not called if `handle` trapped.
    }
}
```

To call upstream HTTP, use the generated `http::send` import (that is how the
aws-mcp plugin reaches the AWS endpoint). See [`plugins/aws`](../plugins/aws)
for a real example that proxies to a remote MCP server and signs requests.

## Building

```sh
cd plugins/echo
cargo build --release --target wasm32-wasip2
# -> target/wasm32-wasip2/release/owlet_plugin_echo.wasm
```

## Testing locally

Point a `[servers.*]` entry at the built `.wasm` and start owlet:

```toml
[servers.echo]
wasm = "/absolute/path/to/owlet_plugin_echo.wasm"
scope = "session"
```

```sh
owlet --config ./config.toml serve
```

Then drive it over the MCP endpoint (replace `$TOKEN` with your config `token`):

```sh
curl -s -X POST http://127.0.0.1:8808/mcp/echo \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}'
```

`owlet validate`/`install` also compiles the component, so a malformed component
fails fast. A component built before `on-shutdown` existed installs but fails to
start; rebuild it.

## Packaging for the catalog

`owlet plugin install <name>` downloads a release asset, verifies its sha256,
writes it next to the config, and adds a `[servers.<name>]` entry. To ship a
plugin through the catalog, build the release `.wasm`, attach it to the GitHub
release that matches the owlet version, and add the entry to the catalog. During
development, `owlet plugin install <name> --file your.wasm` installs a local
build instead.

## Signing notes (important on macOS)

Plugins are JIT-compiled by wasmtime at runtime. On macOS, the process that runs
them must be allowed to execute JIT memory, or the kernel kills it the moment a
plugin runs.

- **CLI / server binary (`owlet serve`)** — the plain CLI binary is not signed
  with Hardened Runtime, so JIT memory is accepted and plugins run with no extra
  setup. This is the common case for Linux and for running the server directly.

- **Desktop app (`ui/gpui-kit`)** — the gpui-kit app has no `.app`/`.dmg`
  bundle or Hardened Runtime signing today (see [Desktop UI](../README.md#desktop-ui)),
  so this concern does not currently apply to it; it runs the same as the
  plain CLI binary above. The entitlements below were specific to the former
  Tauri bundle and are kept here only as a reference in case a signed bundle
  is built for this app in the future:

  ```xml
  <key>com.apple.security.cs.allow-jit</key>
  <true/>
  <key>com.apple.security.cs.allow-unsigned-executable-memory</key>
  <true/>
  ```

  Missing `allow-unsigned-executable-memory` is the subtle one: the app launches
  fine and only crashes **when a plugin first runs**, with
  `EXC_BAD_ACCESS / SIGKILL (Code Signature Invalid)` and a `CODESIGNING / Invalid Page`
  termination on a wasmtime thread. If you add Hardened Runtime signing anywhere
  else (notarization, a custom signing step), carry both entitlements.

  Verify a built app with:

  ```sh
  codesign -d --entitlements - path/to/owlet.app | grep allow
  ```
