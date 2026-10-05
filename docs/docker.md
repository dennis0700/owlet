# Running owlet with Docker

[中文](docker.zh-CN.md)

The `Dockerfile` in the repository root is for building your own image; no official image is published. The image is based on `node:22-bookworm-slim` and includes `node`/`npx` and `uv`/`uvx`, so common `npx` and `uvx` MCP servers work without extra setup.

## Build

```bash
git clone https://github.com/dennis0700/owlet
cd owlet
docker build -t owlet .
```

Versions can be changed with build args: `--build-arg NODE_VERSION=24`, `--build-arg UV_VERSION=0.8`.

## Configure

Inside the container, owlet must listen on `0.0.0.0`, otherwise the published port is unreachable. owlet requires a token on non-loopback addresses.

```bash
mkdir -p ~/.config/owlet-docker
```

`~/.config/owlet-docker/config.toml`:

```toml
listen = "0.0.0.0:8808"
token = "pick-a-long-random-string"

[servers.fetch]
command = "uvx"
args = ["mcp-server-fetch"]
```

## Run

```bash
docker run -d --name owlet --restart unless-stopped \
  -p 127.0.0.1:8808:8808 \
  -v ~/.config/owlet-docker:/home/node/.config/owlet \
  -v owlet-cache:/home/node/.cache \
  owlet
```

- `-p 127.0.0.1:8808:8808` exposes the port to this machine only. Do not use `-p 8808:8808`, which opens it to your network.
- Mount the config directory, not the file. `owlet enable/disable --persist` writes a temporary file and renames it, which fails on a single-file bind mount.
- The `owlet-cache` volume keeps the `uv` and `npm` download caches, so MCP servers are not downloaded again after the container is recreated.
- The container runs as the `node` user (uid 1000). If the config directory on the host is not owned by uid 1000, run `sudo chown -R 1000:1000 ~/.config/owlet-docker` first.

Client configuration is the same as without Docker: `http://127.0.0.1:8808/mcp/<name>`.

## Manage

```bash
docker exec owlet owlet status
docker exec owlet owlet disable fetch --persist
docker logs -f owlet
docker restart owlet      # after editing config.toml
```

To debug a server that fails to start, recreate the container with `-e RUST_LOG=owlet=debug`. The logs then include the stderr of each stdio server.

## Servers with `scope = "project"`

The path in `?project=<path>` must exist inside the container. Mount your project directories at the same path as on the host, so clients can keep using host paths:

```bash
-v /home/me/work:/home/me/work
```

If the MCP server writes to these directories, the container user's uid must match the owner of the files on the host.

## Extending the image

If you need other runtimes (Python packages, `codegraph`, Playwright browsers, …), build on top of this image:

```dockerfile
FROM owlet
USER root
RUN apt-get update && apt-get install -y --no-install-recommends git \
    && rm -rf /var/lib/apt/lists/*
USER node
```

## Limitations

- MCP servers in the container only see what is mounted into it, not other directories or tools on the host. If they need broad access to your environment, run the binary directly instead (on Linux, see [systemd.md](systemd.md)).
- The image is not built or tested in CI.
