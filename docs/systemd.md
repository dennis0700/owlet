# Running owlet with systemd on Linux

[中文](systemd.zh-CN.md)

Run owlet as a systemd **user service** (`systemctl --user`), not a system service. The MCP servers owlet starts (`uvx`, `npx`, `codegraph`, …) are usually installed in your home directory and use your caches, and servers with `scope = "project"` read and write your project directories. A user service runs as you, so all of this works without extra setup.

## 1. Install the binary

```bash
VERSION=$(curl -fsSLI -o /dev/null -w "%{url_effective}" https://github.com/dennis0700/owlet/releases/latest | sed "s|.*/||")   # latest release tag
TARGET=x86_64-unknown-linux-musl   # use aarch64-unknown-linux-musl on arm64
curl -LO "https://github.com/dennis0700/owlet/releases/download/${VERSION}/owlet-${VERSION}-${TARGET}.tar.gz"
curl -LO "https://github.com/dennis0700/owlet/releases/download/${VERSION}/owlet-${VERSION}-${TARGET}.tar.gz.sha256"
sha256sum -c "owlet-${VERSION}-${TARGET}.tar.gz.sha256"
tar -xzf "owlet-${VERSION}-${TARGET}.tar.gz"
mkdir -p ~/.local/bin
install -m 755 "owlet-${VERSION}-${TARGET}/owlet" ~/.local/bin/owlet
```

## 2. Create the config

```bash
mkdir -p ~/.config/owlet
curl -L -o ~/.config/owlet/config.toml \
  https://raw.githubusercontent.com/dennis0700/owlet/main/config.example.toml
chmod 600 ~/.config/owlet/config.toml
```

Edit `~/.config/owlet/config.toml`. At minimum, change `token` and remove the servers you do not need. `openssl rand -hex 32` generates a good token.

Run owlet in the foreground once to check that the config loads:

```bash
~/.local/bin/owlet
```

When you see `listening on http://127.0.0.1:8808`, press `Ctrl+C`.

## 3. Create the unit file

Create `~/.config/systemd/user/owlet.service`:

```ini
[Unit]
Description=owlet - shared MCP server hub
Documentation=https://github.com/dennis0700/owlet

[Service]
Type=simple
ExecStart=%h/.local/bin/owlet -c %h/.config/owlet/config.toml serve
# User services get a minimal PATH; add the directories of uvx, npx, etc. (see below)
Environment=PATH=%h/.local/bin:%h/.cargo/bin:/usr/local/bin:/usr/bin:/bin
Environment=RUST_LOG=owlet=info
# Optional: extra variables such as OWLET_TOKEN or API keys for upstream servers
EnvironmentFile=-%h/.config/owlet/owlet.env
WorkingDirectory=%h

Restart=on-failure
RestartSec=3

# SIGTERM goes to owlet only, which stops its children in order; systemd kills leftovers after the timeout
KillMode=mixed
TimeoutStopSec=20

NoNewPrivileges=true

[Install]
WantedBy=default.target
```

systemd expands `%h` to your home directory.

### PATH

systemd user services do not read `~/.bashrc` or `~/.zshrc`, so a command your shell finds may not be found by owlet. When this happens, the log shows an error such as ``spawn `npx` ``.

Run `which uvx npx node codegraph` and add those directories to `Environment=PATH=...`. Common locations:

| Tool | Usual directory |
|---|---|
| `uv` / `uvx` | `~/.local/bin` |
| Tools installed with `cargo install` | `~/.cargo/bin` |
| `node` / `npx` managed by nvm | `~/.nvm/versions/node/<version>/bin` |
| System package manager | `/usr/bin`, `/usr/local/bin` |

The nvm path contains the Node version, so update it after upgrading Node. Alternatively, use absolute paths in `command` in `config.toml` and do not rely on PATH.

### Keeping secrets in an EnvironmentFile (optional)

To keep the token out of `config.toml`, remove the `token` line and put it in `~/.config/owlet/owlet.env` instead:

```bash
OWLET_TOKEN=your-long-random-token
```

```bash
chmod 600 ~/.config/owlet/owlet.env
```

`owlet status`, `enable` and `disable` also need the token. If it only lives in the env file, pass it to those commands as well, for example `OWLET_TOKEN=... owlet status`.

stdio servers inherit owlet's environment, so API keys for upstream servers can go in this file too.

## 4. Start and enable on boot

```bash
systemctl --user daemon-reload
systemctl --user enable --now owlet
systemctl --user status owlet
```

By default, user services run only while you are logged in and stop when you log out. To start owlet at boot and keep it running after logout, enable lingering:

```bash
sudo loginctl enable-linger "$USER"
```

## 5. Verify

```bash
owlet status
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8808/mcp/fetch
```

If `owlet status` lists your servers, the service is running. `curl` without a token should print `401`.

## Day-to-day operations

| Task | Command |
|---|---|
| Follow logs | `journalctl --user -u owlet -f` |
| Restart after editing the config | `systemctl --user restart owlet` |
| Stop | `systemctl --user stop owlet` |
| Disable start on boot | `systemctl --user disable owlet` |
| Enable or disable a server at runtime | `owlet enable <name>` / `owlet disable <name>` |

owlet does not reload its config, so restart it after editing `config.toml`. Client sessions are lost on restart, and MCP clients initialize again automatically. `owlet enable/disable --persist` writes the config file directly and needs no restart.

To debug a server that fails to start, set `RUST_LOG` to `owlet=debug`. The logs then include the stderr of each stdio server:

```bash
systemctl --user edit owlet
```

```ini
[Service]
Environment=RUST_LOG=owlet=debug
```

```bash
systemctl --user restart owlet
journalctl --user -u owlet -f
```

## Upgrade

```bash
install -m 755 "owlet-${VERSION}-${TARGET}/owlet" ~/.local/bin/owlet
systemctl --user restart owlet
```

## Uninstall

```bash
systemctl --user disable --now owlet
rm ~/.config/systemd/user/owlet.service
systemctl --user daemon-reload
rm ~/.local/bin/owlet
# To remove the config as well: rm -r ~/.config/owlet
```

## Running as a system service

Use a system service only if several users share one owlet or no user stays logged in. Still run it as a regular user, never as root: the MCP servers behind owlet can often run commands and read files.

Create `/etc/systemd/system/owlet.service` and replace `alice` with the actual user:

```ini
[Unit]
Description=owlet - shared MCP server hub

[Service]
Type=simple
User=alice
Group=alice
ExecStart=/home/alice/.local/bin/owlet -c /home/alice/.config/owlet/config.toml serve
Environment=HOME=/home/alice
Environment=PATH=/home/alice/.local/bin:/home/alice/.cargo/bin:/usr/local/bin:/usr/bin:/bin
Environment=RUST_LOG=owlet=info
EnvironmentFile=-/home/alice/.config/owlet/owlet.env
WorkingDirectory=/home/alice
Restart=on-failure
RestartSec=3
KillMode=mixed
TimeoutStopSec=20
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now owlet
journalctl -u owlet -f
```

## Security notes

- Keep `listen` on `127.0.0.1`. To listen elsewhere you must set a token (owlet refuses to start on a non-loopback address without one), and you should put owlet behind a reverse proxy or firewall.
- `config.toml` and `owlet.env` contain the token; keep them at mode `600`.
- Do not add sandboxing options such as `ProtectHome=` or `ReadOnlyPaths=` unless you know which paths each MCP server needs. They break servers with `scope = "project"` and `uvx`/`npx`, which cache in your home directory.
