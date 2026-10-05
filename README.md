# owlet

[中文](README.zh-CN.md)

owlet runs your MCP servers once and shares them with every agent client over HTTP.

## The problem

MCP servers are usually configured per client, and most of them are local stdio processes. Once you use more than one agent, that gets wasteful and tedious:

- **Duplicated processes.** Each client (OpenCode, Claude Code, Cursor, …) spawns its own copy of every stdio server. Three agents with five servers each means fifteen Node/Python processes, most of them doing the same thing.
- **Servers run even when unused.** A client starts all its servers when it launches, and keeps them until it exits, whether or not you call them.
- **Configuration scattered across clients.** The command, arguments and environment of every server are repeated in each client's own config, and have to be kept in sync by hand.
- **Transport mismatches.** Some servers are remote, some use the legacy HTTP+SSE transport, and clients support different subsets.

## What owlet does

owlet replaces all of that with one small Rust process. You describe your servers once in a single config file, and every client just points at a URL. It:

- starts a server only when it is first used, and stops it after it has been idle for a while;
- shares one process between clients, or keeps one per project or per session when the server is stateful;
- answers `initialize` and `tools/list` from cache, so connecting a client does not start anything;
- talks to stdio servers, remote Streamable HTTP servers and legacy HTTP+SSE servers, while clients only ever see plain JSON over Streamable HTTP;
- lets you enable or disable servers at runtime from the CLI, or manage them in a desktop app;
- updates itself with `owlet update`.

## Install

### CLI

Download the archive for your platform from [Releases](https://github.com/dennis0700/owlet/releases). Linux builds are statically linked (musl) and run on any distribution.

| Platform | Archive |
|---|---|
| Linux x86_64 | `owlet-<version>-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 | `owlet-<version>-aarch64-unknown-linux-musl.tar.gz` |
| macOS Apple Silicon | `owlet-<version>-aarch64-apple-darwin.tar.gz` |

There are no prebuilt binaries for macOS Intel or Windows; build from source instead.

```bash
VERSION=$(curl -fsSLI -o /dev/null -w "%{url_effective}" https://github.com/dennis0700/owlet/releases/latest | sed "s|.*/||")   # latest release tag
TARGET=aarch64-apple-darwin
curl -LO "https://github.com/dennis0700/owlet/releases/download/${VERSION}/owlet-${VERSION}-${TARGET}.tar.gz"
curl -LO "https://github.com/dennis0700/owlet/releases/download/${VERSION}/owlet-${VERSION}-${TARGET}.tar.gz.sha256"
shasum -a 256 -c "owlet-${VERSION}-${TARGET}.tar.gz.sha256"
tar -xzf "owlet-${VERSION}-${TARGET}.tar.gz"
install -m 755 "owlet-${VERSION}-${TARGET}/owlet" ~/.local/bin/owlet
```

On macOS, a binary downloaded with a browser may be quarantined. Remove the flag with `xattr -d com.apple.quarantine ~/.local/bin/owlet`. Downloads made with `curl` are not affected.

To build from source (Rust 1.89 or newer):

```bash
cargo install --locked --git https://github.com/dennis0700/owlet
```

To run owlet as a background service on Linux, see [docs/systemd.md](docs/systemd.md). To run it with Docker, see [docs/docker.md](docs/docker.md).

### Desktop UI (macOS)

Download `owlet-ui-<version>-aarch64-apple-darwin.dmg` from Releases (Apple Silicon only) and drag `owlet.app` to Applications. The app runs owlet inside itself, so you do not need the CLI to use it. See [Desktop UI](#desktop-ui) for what it does.

The app is signed ad hoc and not notarized by Apple, so on first launch macOS may say it cannot verify the developer. Right-click the app and choose **Open**, or allow it under System Settings → Privacy & Security. If macOS still refuses to open it, remove the quarantine flag with `xattr -cr /Applications/owlet.app`.

On other platforms, build the UI from source (see [Desktop UI](#desktop-ui)).

## Update

- CLI: run `owlet update`. It downloads the archive for your platform from GitHub Releases, verifies its sha256 and atomically replaces the running executable. Use `owlet update --check` to only see whether a newer version exists. Restart a running owlet afterwards. If the executable's directory is not writable (for example `/usr/local/bin`), run it with `sudo`.
- Desktop UI: it checks once at startup. Press **Check for updates** (or **Update to x.y.z** when one is found) to download, verify the signature, install and relaunch.

## Quick start

1. Create `~/.config/owlet/config.toml`:

   ```toml
   listen = "127.0.0.1:8808"
   token = "pick-a-long-random-string"

   [servers.fetch]
   command = "uvx"
   args = ["mcp-server-fetch"]
   ```

2. Start owlet:

   ```bash
   owlet
   ```

3. Point your client at the aggregate endpoint, which exposes every enabled server's tools in one place and lets the client discover them on its own. For OpenCode (`opencode.json`):

   ```json
   {
     "mcp": {
       "owlet": {
         "type": "remote",
         "url": "http://127.0.0.1:8808/mcp",
         "headers": { "Authorization": "Bearer pick-a-long-random-string" }
       }
     }
   }
   ```

   For Claude Code:

   ```bash
   claude mcp add --transport http owlet http://127.0.0.1:8808/mcp \
     --header "Authorization: Bearer pick-a-long-random-string"
   ```

   Any client that supports the Streamable HTTP transport works the same way. See [Aggregate endpoint](#aggregate-endpoint) for how tool names are namespaced and its limitations.

   Prefer one URL per server instead? Point the client at `http://127.0.0.1:8808/mcp/<name>`, for example `http://127.0.0.1:8808/mcp/fetch`.

4. Check what is running:

   ```bash
   owlet status
   ```

## Aggregate endpoint

The aggregate endpoint shown above lists the tools of every enabled server, renamed as `<server>__<tool>` (for example `fetch__fetch`) so names never collide. `tools/call` strips the prefix and routes the call back to the owning server, starting it on first use just like the per-server endpoint.

Notes:

- Only `tools/list` and `tools/call` are supported; `prompts/*` and `resources/*` are not exposed here.
- The tool list is read once at `initialize` time by most clients. After adding or enabling a server, reconnect the client to see its tools.
- Servers with `scope = "project"` only show up when the URL carries the project, e.g. `http://127.0.0.1:8808/mcp?project=/Users/me/work/my-repo`. Without it, project-scoped servers are omitted from the list.
- Each upstream server is still started lazily and shut down after being idle, same as when accessed directly at `/mcp/<name>`.

## Configuration

The default config path is `~/.config/owlet/config.toml`. Use `-c <path>` to choose another one. A complete example is in [config.example.toml](config.example.toml).

### Global settings

| Key | Default | Meaning |
|---|---|---|
| `listen` | `"127.0.0.1:8808"` | Address to listen on. |
| `token` | none | Bearer token required on every request. If unset, `$OWLET_TOKEN` is used. owlet refuses to start on a non-loopback address without a token. |
| `idle_timeout` | `600` | Seconds a server may stay idle before owlet stops it. |
| `session_ttl` | `86400` | Seconds a client session may stay idle before owlet forgets it. |
| `request_timeout` | `300` | Seconds to wait for one upstream response. |

### Server settings: `[servers.<name>]`

`<name>` becomes the URL path `/mcp/<name>`. Every server needs either `command` (a local stdio process) or `url` (a remote server), not both.

| Key | Applies to | Default | Meaning |
|---|---|---|---|
| `enabled` | all | `true` | Set to `false` to keep the entry without serving it. |
| `command` | stdio | | Executable to run. |
| `args` | stdio | `[]` | Arguments. `{project}` is replaced with the project path when `scope = "project"`. |
| `env` | stdio | `{}` | Extra environment variables. |
| `cwd` | stdio | owlet's cwd | Working directory (ignored when `scope = "project"`). |
| `url` | remote | | Server endpoint. For `transport = "sse"`, this is the SSE stream URL. |
| `transport` | remote | `"http"` | `"http"` for Streamable HTTP, `"sse"` for the legacy 2024-11-05 HTTP+SSE transport. |
| `headers` | remote | `{}` | HTTP headers sent to the server, for example `Authorization`. |
| `scope` | all | `"shared"` | How the server is shared between clients (see below). |
| `idle_timeout` | all | global value | Per-server override. |
| `cache_lists` | all | `true` | Cache `tools/list`, `prompts/list`, `resources/list` and `resources/templates/list`. |

### Choosing a scope

| Scope | Instances | Use for |
|---|---|---|
| `shared` | One for all clients. | Stateless servers such as web fetch, search and docs lookup. |
| `project` | One per project directory. The process starts with that directory as its cwd. | Servers that read the current project, such as filesystem, git and code indexers. stdio only. |
| `session` | One per client session, stopped when the client disconnects. | Servers with strong per-client state, such as browsers or database transactions. |

With `scope = "project"`, add the project path to the URL:

```
http://127.0.0.1:8808/mcp/git?project=/Users/me/work/my-repo
```

The path must be absolute and must exist. Clients do not add it for you, so configure one URL per project.

```toml
[servers.git]
command = "uvx"
args = ["mcp-server-git", "--repository", "{project}"]
scope = "project"
```

### Remote servers

```toml
# Streamable HTTP (current MCP standard)
[servers.remote]
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer upstream-token" }

# Legacy HTTP+SSE
[servers.old-remote]
url = "http://127.0.0.1:9000/sse"
transport = "sse"
```

A remote server can run multiple instances, but `project` scope is not supported because there is no local process to set the cwd for. If the remote tool list changes often, set `cache_lists = false`, because owlet does not receive `list_changed` notifications from remote servers.

## CLI

```
owlet [-c <config>] [serve]                       run the server (default)
owlet [-c <config>] status [--json]               show servers and running instances
owlet [-c <config>] enable  <name>... [--persist] enable servers on the running owlet
owlet [-c <config>] disable <name>... [--persist] disable servers on the running owlet
owlet update [--check]                            update to the latest GitHub release
```

`status`, `enable` and `disable` read `listen` and `token` from the same config file and talk to the running owlet.

```
$ owlet status
NAME        STATE     TRANSPORT  SCOPE    INSTANCES
fetch       enabled   stdio      shared   1/1 running (pid 4242)
git         enabled   stdio      project  2/3 running (pid 4250,4261)
playwright  disabled  stdio      session  -
sessions: 4
```

- `disable` immediately stops all processes or connections of that server and drops its sessions. Clients get `503` until it is enabled again.
- `enable` makes the server available again. Its process starts on the next request.
- Without `--persist`, the change lasts until owlet restarts. With `--persist`, owlet also writes `enabled = false` to the config file, or removes that line when enabling. Comments and formatting are preserved.

`owlet update` is described under [Update](#update).

## Desktop UI

`ui/` contains a Tauri 2 app (React + TypeScript) for managing the `[servers]` table of the config file and running owlet in the tray. On macOS, install it from the `.dmg` (see [Install](#desktop-ui-macos)). It is a separate Cargo workspace, so `cargo build` at the repository root does not need the Tauri toolchain.

To build it yourself:

```sh
cd ui
npm install
npm run tauri dev      # or: npm run tauri build
```

- It edits the file given by `$OWLET_CONFIG`, or `~/.config/owlet/config.toml` by default. The path is shown in the window.
- The switch, add, edit and delete actions only change an in-memory draft. Nothing is written until you press **Save**; **Discard** drops the draft.
- Saving rewrites only `[servers]`. Comments, other keys and untouched entries are preserved.
- Closing the window hides it in the tray (menu bar on macOS) and keeps the draft. Click the tray icon or the Dock icon to show it again; use the tray menu's **Quit** to exit. Quitting discards an unsaved draft.
- It starts its own owlet on the configured `listen` address and shows its status (**Start**, **Stop**, **Restart**). Press **Restart** after saving to apply changes. It does not control a separately running `owlet` process; stop that one first, or the port will be taken.
- If the config file cannot be parsed, the UI shows the error and disables editing so the file is never overwritten.

## Security

The servers behind owlet can often run commands and read files, so treat owlet's port like a shell.

- Keep `listen` on `127.0.0.1` unless you have a reason not to.
- Always set `token`. Without it, any local process can call your MCP servers and enable or disable them.
- owlet rejects browser requests whose `Origin` is not localhost, which protects against DNS rebinding.
- For remote servers, owlet only sends your configured `headers` to the configured origin.

## Limitations

- Clients receive plain JSON responses only. Progress notifications and `list_changed` pushes are not forwarded. Tool calls are not affected.
- Requests that a server sends to a client (`sampling`, `elicitation`) are rejected. `roots/list` is answered by owlet with the project path, or an empty list.
- Sessions live in memory. After owlet restarts, clients get `404` and initialize again, which MCP clients do automatically.
- Windows is not supported yet.

## Logs

owlet logs to stderr. Use `RUST_LOG` to control the level, for example `RUST_LOG=owlet=debug owlet`. At debug level, the stderr output of each stdio server is included in the logs.

## How it works

```mermaid
flowchart LR
    A["Agent client A"] -->|"HTTP"| O
    B["Agent client B"] -->|"HTTP"| O
    O["owlet"] -->|"shared: one process for all"| P1["fetch"]
    O -->|"project: one per directory"| P2["filesystem (project X)"]
    O -->|"project: one per directory"| P3["filesystem (project Y)"]
    O -->|"session: one per client"| P4["playwright"]
    O -->|"Streamable HTTP or SSE"| R["remote MCP"]
```

See [docs/architecture.md](docs/architecture.md) for the details.
