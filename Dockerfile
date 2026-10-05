# syntax=docker/dockerfile:1

ARG RUST_VERSION=1
ARG NODE_VERSION=22
ARG UV_VERSION=0.8

FROM rust:${RUST_VERSION}-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/owlet /usr/local/bin/owlet

FROM ghcr.io/astral-sh/uv:${UV_VERSION} AS uv

# Node and uv are included so the common `npx` / `uvx` MCP servers work out of the box.
FROM node:${NODE_VERSION}-bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/*
COPY --from=uv /uv /uvx /usr/local/bin/
COPY --from=build /usr/local/bin/owlet /usr/local/bin/owlet

USER node
ENV HOME=/home/node \
    RUST_LOG=owlet=info
RUN mkdir -p /home/node/.config/owlet /home/node/.cache
WORKDIR /home/node
VOLUME ["/home/node/.config/owlet", "/home/node/.cache"]
EXPOSE 8808

# tini reaps orphaned grandchildren of MCP servers; owlet itself handles SIGTERM.
ENTRYPOINT ["/usr/bin/tini", "--", "owlet"]
CMD ["serve"]
