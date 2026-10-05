# 在 Linux 上用 systemd 运行 owlet

[English](systemd.md)

推荐用 systemd **用户服务**（`systemctl --user`）运行 owlet，而不是系统服务。owlet 启动的 MCP server（`uvx`、`npx`、`codegraph` 等）通常装在你自己的 home 目录下，要用你的缓存目录，`scope = "project"` 的 server 还要读写你的项目目录。用户服务以你本人的身份运行，这些都能直接用。

## 1. 安装二进制

```bash
VERSION=$(curl -fsSLI -o /dev/null -w "%{url_effective}" https://github.com/dennis0700/owlet/releases/latest | sed "s|.*/||")   # 最新 release 的 tag
TARGET=x86_64-unknown-linux-musl   # arm64 用 aarch64-unknown-linux-musl
curl -LO "https://github.com/dennis0700/owlet/releases/download/${VERSION}/owlet-${VERSION}-${TARGET}.tar.gz"
curl -LO "https://github.com/dennis0700/owlet/releases/download/${VERSION}/owlet-${VERSION}-${TARGET}.tar.gz.sha256"
sha256sum -c "owlet-${VERSION}-${TARGET}.tar.gz.sha256"
tar -xzf "owlet-${VERSION}-${TARGET}.tar.gz"
mkdir -p ~/.local/bin
install -m 755 "owlet-${VERSION}-${TARGET}/owlet" ~/.local/bin/owlet
```

## 2. 准备配置

```bash
mkdir -p ~/.config/owlet
curl -L -o ~/.config/owlet/config.toml \
  https://raw.githubusercontent.com/dennis0700/owlet/main/config.example.toml
chmod 600 ~/.config/owlet/config.toml
```

编辑 `~/.config/owlet/config.toml`，至少改掉 `token`，并删掉用不到的 server。可以用 `openssl rand -hex 32` 生成 token。

先在前台跑一次，确认配置能加载：

```bash
~/.local/bin/owlet
```

看到 `listening on http://127.0.0.1:8808` 后按 `Ctrl+C` 退出。

## 3. 创建 unit 文件

创建 `~/.config/systemd/user/owlet.service`：

```ini
[Unit]
Description=owlet - shared MCP server hub
Documentation=https://github.com/dennis0700/owlet

[Service]
Type=simple
ExecStart=%h/.local/bin/owlet -c %h/.config/owlet/config.toml serve
# 用户服务的 PATH 很短，要把 uvx、npx 等所在目录加进来（见下文）
Environment=PATH=%h/.local/bin:%h/.cargo/bin:/usr/local/bin:/usr/bin:/bin
Environment=RUST_LOG=owlet=info
# 可选：额外环境变量，例如 OWLET_TOKEN 或上游 server 需要的 API key
EnvironmentFile=-%h/.config/owlet/owlet.env
WorkingDirectory=%h

Restart=on-failure
RestartSec=3

# SIGTERM 只发给 owlet，由它按顺序停掉子进程；超时后 systemd 再强杀剩余进程
KillMode=mixed
TimeoutStopSec=20

NoNewPrivileges=true

[Install]
WantedBy=default.target
```

`%h` 由 systemd 展开为你的 home 目录。

### PATH

systemd 用户服务不会读取 `~/.bashrc`、`~/.zshrc`，所以 shell 里能找到的命令，owlet 不一定找得到。启动失败时日志里会出现 `spawn \`npx\`` 之类的错误。

用 `which uvx npx node codegraph` 查出这些命令所在目录，加到 `Environment=PATH=...` 里。常见位置：

| 工具 | 常见目录 |
|---|---|
| `uv` / `uvx` | `~/.local/bin` |
| `cargo install` 装的工具 | `~/.cargo/bin` |
| nvm 管理的 `node` / `npx` | `~/.nvm/versions/node/<version>/bin` |
| 系统包管理器安装 | `/usr/bin`、`/usr/local/bin` |

nvm 的路径带版本号，升级 Node 后要同步修改。也可以在 `config.toml` 里给 `command` 写绝对路径，不依赖 PATH。

### 用 EnvironmentFile 存放密钥（可选）

如果不想把 token 写进 `config.toml`，可以删掉其中的 `token` 一行，改为放进 `~/.config/owlet/owlet.env`：

```bash
OWLET_TOKEN=your-long-random-token
```

```bash
chmod 600 ~/.config/owlet/owlet.env
```

注意：`owlet status`、`enable`、`disable` 也会读取 token。如果 token 只放在 env 文件里，执行这些命令时也要带上它，例如 `OWLET_TOKEN=... owlet status`。

stdio server 会继承 owlet 的环境变量，所以上游 server 需要的 API key 也可以放在这个文件里。

## 4. 启动并设为开机自启

```bash
systemctl --user daemon-reload
systemctl --user enable --now owlet
systemctl --user status owlet
```

默认情况下，用户服务只在你登录时运行，退出登录后会被停掉。要让它开机就启动、退出登录后继续运行，需要开启 lingering：

```bash
sudo loginctl enable-linger "$USER"
```

## 5. 验证

```bash
owlet status
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8808/mcp/fetch
```

`owlet status` 能列出 server 即表示服务正常。`curl` 不带 token 时应返回 `401`。

## 日常操作

| 操作 | 命令 |
|---|---|
| 查看日志 | `journalctl --user -u owlet -f` |
| 修改配置后重启 | `systemctl --user restart owlet` |
| 停止 | `systemctl --user stop owlet` |
| 取消开机自启 | `systemctl --user disable owlet` |
| 临时启用/停用 server | `owlet enable <name>` / `owlet disable <name>` |

owlet 不支持热加载配置，修改 `config.toml` 后需要重启。重启后客户端的 session 会失效，MCP 客户端会自动重新 initialize。`owlet enable/disable --persist` 会直接写配置文件，无需重启。

排查 MCP server 启动问题时，把 `RUST_LOG` 改为 `owlet=debug`，日志中会包含各 stdio server 的 stderr 输出：

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

## 升级

```bash
install -m 755 "owlet-${VERSION}-${TARGET}/owlet" ~/.local/bin/owlet
systemctl --user restart owlet
```

## 卸载

```bash
systemctl --user disable --now owlet
rm ~/.config/systemd/user/owlet.service
systemctl --user daemon-reload
rm ~/.local/bin/owlet
# 如需同时删除配置：rm -r ~/.config/owlet
```

## 作为系统服务运行

只有在需要多用户共享一个 owlet，或机器上没有常驻登录用户时，才考虑系统服务。仍然要用普通用户运行，不要用 root：owlet 后面的 MCP server 往往能执行命令、读写文件。

创建 `/etc/systemd/system/owlet.service`，把 `alice` 换成实际用户：

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

## 安全提示

- `listen` 保持 `127.0.0.1`。如果要监听其他地址，必须设置 token（owlet 会拒绝无 token 在非 loopback 地址上启动），并且最好放在反向代理或防火墙后面。
- `config.toml` 和 `owlet.env` 中有 token，权限设为 `600`。
- 不要额外加 `ProtectHome=`、`ReadOnlyPaths=` 这类沙箱选项，除非你清楚每个 MCP server 需要访问哪些路径。它们会让 `scope = "project"` 的 server 以及依赖 home 目录缓存的 `uvx`/`npx` 无法正常工作。
