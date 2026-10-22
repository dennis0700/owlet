# 编写 owlet WASM 插件

本文档面向插件开发,而非仅仅使用插件。如果只是安装和配置一个已有插件,请看
README 的 [WASM plugins](../README.zh-CN.md#wasm-plugins) 章节。

owlet 插件是一个
[WebAssembly Component](https://component-model.bytecodealliance.org/),由 owlet
通过 [wasmtime](https://wasmtime.dev/) 在进程内运行,没有外部运行时、容器或语言
依赖。它本质上是一个自包含的 MCP server:owlet 每次把一条 MCP JSON-RPC 消息交给它,
再转发它的回复。

## 契约

插件实现 [`wit/mcp-plugin.wit`](../wit/mcp-plugin.wit) 里的 `mcp-plugin` world,
接口很小:

- `export handle(request: string) -> string` —— 处理一条 MCP JSON-RPC 消息。
  返回零条或多条以 `\n` 分隔的 JSON-RPC 消息;返回空串表示"无响应"(通知类消息,
  或已接受但无需回复)。
- `export on-shutdown()` —— 实例停止时(server 停止、会话结束、owlet 退出)在最后
  一次 `handle` 返回后调用一次。best-effort,有一小段宽限期,期间出站 HTTP 仍可用。
- `import http.send(...)` —— 插件**唯一**的网络访问方式。WASI 套接字和 DNS 均被
  禁用,请求由 owlet 代为发出。

每个实例的调用是串行的:只有上一条 `handle` 返回后才会投递下一条消息,所以一次慢的
`http.send` 会阻塞该实例的其他请求。若希望每个客户端独占一个实例,在配置里设置
`scope = "session"`。

## 插件获得的能力

owlet 只授予 server 配置里声明的能力:

- `args` —— 插件的 WASI argv(排在 server 名之后)。
- `env` + `inherit_env` —— 插件的 WASI 环境变量。`inherit_env` 从宿主转发变量
  (以 `*` 结尾表示前缀匹配,如 `AWS_*`);冲突时 `env` 优先。
- `dirs` —— 以**只读**方式按相同 guest 路径预打开的宿主目录(`~/` 展开为 `$HOME`)。
- `http.send` —— 可访问任意 http(s) URL(含 loopback 和 link-local),没有目的地
  白名单,因此**插件等同于受信任代码**。

`cwd` 和 `scope = "project"` 对 `wasm` server 不适用。

## 工具链

插件以 `wasm32-wasip2` 为目标,构建为 cdylib 组件。先安装一次 target:

```sh
rustup target add wasm32-wasip2
```

绑定由 [`wit-bindgen`](https://crates.io/crates/wit-bindgen) 从 WIT 生成。不需要
`cargo-component` 或 `wasm-tools` 做后处理:`wasm32-wasip2` 直接产出组件。

## 一个最小插件

仓库在 [`plugins/echo`](../plugins/echo) 提供了一个完整的最小示例:一个自包含的
MCP server,实现了 `initialize`、`tools/list`(一个 `echo` 工具)和 `tools/call`。
可以直接拿它做模板。

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

把 [`wit/mcp-plugin.wit`](../wit/mcp-plugin.wit) 复制到插件自己的 `wit/` 目录,
然后在 `src/lib.rs` 中:

```rust
wit_bindgen::generate!({ world: "mcp-plugin" });

struct Plugin;
export!(Plugin);

impl Guest for Plugin {
    fn handle(request: String) -> String {
        // 解析 JSON-RPC `request`,构造回复,以字符串返回。
        // 对通知类消息(没有 `id`)返回 ""。
        reply(&request).unwrap_or_default()
    }

    fn on_shutdown() {
        // 在这里结束上游会话。best-effort;`http.send` 仍可短暂使用。
        // 若 `handle` 发生 trap 则不会被调用。
    }
}
```

要调用上游 HTTP,使用生成的 `http::send` 导入(aws-mcp 插件就是这样访问 AWS 端点
的)。真实示例见 [`plugins/aws`](../plugins/aws):它代理到一个远程 MCP server 并
对请求签名。

## 构建

```sh
cd plugins/echo
cargo build --release --target wasm32-wasip2
# -> target/wasm32-wasip2/release/owlet_plugin_echo.wasm
```

## 本地测试

让某个 `[servers.*]` 条目指向构建出的 `.wasm`,再启动 owlet:

```toml
[servers.echo]
wasm = "/absolute/path/to/owlet_plugin_echo.wasm"
scope = "session"
```

```sh
owlet --config ./config.toml serve
```

然后通过 MCP 端点驱动它(把 `$TOKEN` 换成配置里的 `token`):

```sh
curl -s -X POST http://127.0.0.1:8808/mcp/echo \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}'
```

`owlet validate`/`install` 也会编译组件,因此格式不对的组件会尽早失败。在
`on-shutdown` 出现之前构建的组件能安装但无法启动;重新构建即可。

## 打包进 catalog

`owlet plugin install <name>` 会下载发布资产、校验其 sha256、写到配置文件旁边,
并添加一个 `[servers.<name>]` 条目。要通过 catalog 分发插件,构建 release 版
`.wasm`,挂到与 owlet 版本匹配的 GitHub release 上,并把条目加入 catalog。开发
阶段用 `owlet plugin install <name> --file your.wasm` 安装本地构建。

## 签名注意事项(macOS 上尤其重要)

插件由 wasmtime 在运行时 JIT 编译。在 macOS 上,运行它们的进程必须被允许执行 JIT
内存,否则插件一旦运行,进程就会被内核杀掉。

- **CLI / 服务器二进制(`owlet serve`)** —— 纯 CLI 二进制没有用 Hardened Runtime
  签名,JIT 内存被接受,插件无需额外设置即可运行。这是 Linux 以及直接运行服务器
  的常见场景。

- **桌面应用（`ui/gpui-kit`）** —— gpui-kit 应用目前没有 `.app`/`.dmg` 打包，也
  没有 Hardened Runtime 签名（见[桌面 UI](../README.zh-CN.md#桌面-ui)），所以这
  个问题目前不适用于它；它的运行方式和上面的纯 CLI 二进制一样。下面的
  entitlement 是以前 Tauri 打包时用的，留在这里仅作参考，以防将来给这个应用做
  签名打包时需要：

  ```xml
  <key>com.apple.security.cs.allow-jit</key>
  <true/>
  <key>com.apple.security.cs.allow-unsigned-executable-memory</key>
  <true/>
  ```

  缺少 `allow-unsigned-executable-memory` 是最隐蔽的一种:应用能正常启动,只有在
  **插件首次运行**时才崩溃,表现为 `EXC_BAD_ACCESS / SIGKILL (Code Signature Invalid)`,
  并在某个 wasmtime 线程上以 `CODESIGNING / Invalid Page` 终止。如果你在别处(公证、
  自定义签名步骤)加了 Hardened Runtime 签名,务必带上这两个 entitlement。

  用以下命令验证已构建的应用:

  ```sh
  codesign -d --entitlements - path/to/owlet.app | grep allow
  ```
