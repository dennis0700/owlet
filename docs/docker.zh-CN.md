# 用 Docker 运行 owlet

[English](docker.md)

仓库根目录的 `Dockerfile` 供你自行构建镜像，项目不发布官方镜像。镜像基于 `node:22-bookworm-slim`，内置 `node`/`npx` 和 `uv`/`uvx`，常见的 `npx`、`uvx` 类 MCP server 无需额外安装。

## 构建

```bash
git clone https://github.com/dennis0700/owlet
cd owlet
docker build -t owlet .
```

可以用 build arg 调整版本：`--build-arg NODE_VERSION=24`、`--build-arg UV_VERSION=0.8`。

## 配置

容器里的 owlet 必须监听 `0.0.0.0`，否则端口映射无法访问。owlet 在非 loopback 地址上要求必须设置 token。

```bash
mkdir -p ~/.config/owlet-docker
```

`~/.config/owlet-docker/config.toml`：

```toml
listen = "0.0.0.0:8808"
token = "pick-a-long-random-string"

[servers.fetch]
command = "uvx"
args = ["mcp-server-fetch"]
```

## 运行

```bash
docker run -d --name owlet --restart unless-stopped \
  -p 127.0.0.1:8808:8808 \
  -v ~/.config/owlet-docker:/home/node/.config/owlet \
  -v owlet-cache:/home/node/.cache \
  owlet
```

- `-p 127.0.0.1:8808:8808` 只把端口暴露给本机。不要写成 `-p 8808:8808`，那样会对局域网开放。
- 挂载的是配置目录而不是单个文件：`owlet enable/disable --persist` 会先写临时文件再 rename，单文件挂载下会失败。
- `owlet-cache` 卷保存 `uv` 和 `npm` 的下载缓存，避免容器重建后重新下载 MCP server。
- 容器内以 `node` 用户（uid 1000）运行。如果宿主机上配置目录的属主不是 uid 1000，先执行 `sudo chown -R 1000:1000 ~/.config/owlet-docker`。

客户端配置与直接运行时相同，URL 为 `http://127.0.0.1:8808/mcp/<name>`。

## 管理

```bash
docker exec owlet owlet status
docker exec owlet owlet disable fetch --persist
docker logs -f owlet
docker restart owlet      # 修改 config.toml 后
```

调试 MCP server 启动问题时，加 `-e RUST_LOG=owlet=debug` 重新创建容器，日志中会包含各 stdio server 的 stderr。

## `scope = "project"` 的 server

`?project=<路径>` 中的路径必须在容器内存在。把项目目录按宿主机上的相同路径挂载进去，客户端就能直接用宿主机路径：

```bash
-v /home/me/work:/home/me/work
```

如果 MCP server 需要写这些目录，注意容器用户与宿主机文件属主的 uid 要一致。

## 自定义镜像

需要其他运行时（如 Python 包、`codegraph`、Playwright 浏览器）时，以此镜像为基础再扩展：

```dockerfile
FROM owlet
USER root
RUN apt-get update && apt-get install -y --no-install-recommends git \
    && rm -rf /var/lib/apt/lists/*
USER node
```

## 限制

- 容器内的 MCP server 只能看到挂载进去的文件，无法访问宿主机上其他目录和工具。需要深度访问宿主机环境时，直接运行二进制（Linux 见 [systemd.zh-CN.md](systemd.zh-CN.md)）更合适。
- 镜像未在官方 CI 中构建和测试。
