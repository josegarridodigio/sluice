# rmcp (modelcontextprotocol/rust-sdk) — reference notes for Sluice

Source tree studied: `<repo>/refs/rust-sdk`
(commit `b11a40d`, 2026-10-22). All paths below are relative to that root unless
absolute. Nothing in that repo was modified.

## 1. What it is

- The official Rust SDK for MCP, tokio-based. Workspace with two crates:
  `rmcp` (protocol, transports, handler traits) and `rmcp-macros`
  (`#[tool]`, `#[tool_router]`, `#[tool_handler]`, prompt equivalents)
  (`README.md:11-14`).
- License: Apache-2.0 for new code; the project is mid-transition from MIT,
  so some older contributions remain MIT (`LICENSE:1-5`,
  `Cargo.toml:17` declares `license = "Apache-2.0"`).
- Crate version: **3.6.0** (workspace `version` at `Cargo.toml:14`, inherited
  by `crates/rmcp/Cargo.toml:5`; changelog entry dated 2026-10-19 at
  `crates/rmcp/CHANGELOG.md:10`).
- MSRV: `rust-version = "1.88"` (`Cargo.toml:13`); the repo pins toolchain
  channel `1.96` for development (`rust-toolchain.toml:2`). Edition 2024
  (`Cargo.toml:12`), so you need a recent stable.
- Protocol versions: implements the stable `2026-07-28` spec and stays
  compatible with `2025-11-25` and earlier (`README.md:16-22`). Constants in
  `crates/rmcp/src/model.rs:167-174`: `V_2026_07_28`, `V_2025_11_25`,
  `V_2025_06_18`, `V_2025_03_26`, `V_2024_11_05`; `LATEST = V_2026_07_28`
  (`:174`), `LATEST_WITH_INITIALIZE = V_2025_11_25` (`:199`),
  `KNOWN_VERSIONS` lists all five (`:202-208`). Negotiation is automatic on
  connect (`README.md:1787-1801`); a server can pin an older version with
  `ServerConfig::with_protocol_version(..)` (`examples/servers/src/common/counter.rs:218`).

## 2. Cargo features relevant to Sluice

All defined in `crates/rmcp/Cargo.toml`:

| feature | line | what it pulls in |
|---|---|---|
| `default = ["base64", "macros", "server"]` | `:130` | defaults already cover server + macros |
| `server` | `:133` | `transport-async-rw`, `schemars`, `pastey`, `uuid`; gates `ServerHandler`, `Json`, `task_manager` (`crates/rmcp/src/lib.rs:14-17, 29-30`) |
| `macros` | `:134` | `rmcp-macros` + `pastey`; re-exports `rmcp_macros::*`, `serde`, `serde_json` (`lib.rs:38-44`) |
| `transport-io` | `:181` | `transport-async-rw` + `tokio/io-std` + `tokio/net`; provides `rmcp::transport::stdio()` |
| `schemars` | `:206` | `dep:schemars` (schemars 1.x, `:67`); already implied by `server` |
| `transport-async-rw` | `:180` | `tokio/io-util`, `tokio-util/codec` (line-delimited JSON codec) |

Minimum line for Sluice (stdio server, tool macros, structured output):

```toml
rmcp = { version = "3.6", features = ["server", "macros", "transport-io", "schemars"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "io-std"] }
schemars = "1"          # derive JsonSchema on param/result types
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt", "std"] }
```

Compare the example crate manifest at `examples/servers/Cargo.toml:8-18`
(it also enables `client`, `auth`, `elicitation`, HTTP transports — not needed).
Features we do NOT need: `client`, `transport-child-process`,
`transport-streamable-http-*`, `auth*`, `elicitation`, `request-state`.
Note `local` (`:131`) relaxes the `Send` bound on handlers; do not enable it,
the default `ServerHandler: Sized + Send + Sync + 'static`
(`crates/rmcp/src/handler/server.rs:642`) is what we want with a multi-thread
runtime.

## 3. Minimal stdio server skeleton

Adapted from `examples/servers/src/calculator_stdio.rs` (main) +
`examples/servers/src/common/calculator.rs` (router) +
`examples/servers/src/common/counter.rs:206-220` (explicit `get_info`) +
`examples/servers/src/structured_output.rs` (`Json<T>` output).

```rust
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::{Json, Parameters}},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use tracing_subscriber::EnvFilter;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct VerifyParams {
    /// Gate to run (as listed by `gate_list`).
    pub gate: String,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct VerifyReport {
    pub gate: String,
    pub passed: bool,
    pub duration_ms: u64,
}

#[derive(Clone)]
pub struct Sluice {
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl Sluice {
    pub fn new() -> Self {
        Self { tool_router: Self::tool_router() }
    }

    /// Plain-text result: `String` implements `IntoContents`.
    #[tool(description = "Show queue status")]
    async fn status(&self) -> String {
        "idle".to_string()
    }

    /// Structured result: `Json<T>` goes to `structuredContent` and the
    /// macro derives `outputSchema` from `T`.
    #[tool(description = "Run a gate and block until it finishes")]
    async fn verify(
        &self,
        Parameters(VerifyParams { gate }): Parameters<VerifyParams>,
    ) -> Result<Json<VerifyReport>, McpError> {
        Ok(Json(VerifyReport { gate, passed: true, duration_ms: 0 }))
    }

    /// Full control over the result.
    #[tool(description = "Tail logs of the last run")]
    async fn logs(&self) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![ContentBlock::text("...")]))
    }
}

#[tool_handler]
impl ServerHandler for Sluice {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions("Sluice: queued verification gates.")
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let service = Sluice::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
```

Names and where they are defined:

- `#[tool]` attributes: `name`, `description`, `input_schema`, `annotations`
  (`crates/rmcp-macros/src/lib.rs:21-26`); `output_schema` is also parsed
  (`crates/rmcp-macros/src/tool.rs:72`) and otherwise derived from a
  `Json<T>` / `Result<Json<T>, E>` return type
  (`crates/rmcp-macros/src/tool.rs:22-57, 249-253`). Doc comment on the fn
  becomes the description when `description` is absent (`lib.rs:24`).
- `#[tool_router]` options `router`, `vis`, `server_handler`, `allow_empty`
  (`crates/rmcp-macros/src/lib.rs:54-59`). `#[tool_router(server_handler)]`
  emits the `impl ServerHandler` for you (tools-only servers,
  `README.md:211`), but then you cannot customise `get_info`; use the
  two-block form above when you need `instructions`.
- `#[tool_handler]` options `router`, `meta`, `name`, `version`,
  `instructions`; it generates `call_tool`, `list_tools`, `get_tool` and
  `get_info` unless you write `get_info` yourself (`crates/rmcp-macros/src/lib.rs:164-226`).
- `Parameters<P>` is `#[serde(transparent)]` and delegates `JsonSchema` to
  `P` (`crates/rmcp/src/handler/server/wrapper/parameters.rs:43-56`); only
  field names/types/docs of `P` reach the schema (`README.md:156`).
- `Json<T>` puts the value in `structured_content` via
  `CallToolResult::structured` (`crates/rmcp/src/handler/server/wrapper/json.rs:32-43`).
- `ServerConfig` is a type alias of `InitializeResult`
  (`crates/rmcp/src/model.rs:1341`); builders `with_instructions`,
  `with_server_info`, `with_protocol_version` at `model.rs:1307-1321`.
  `Implementation::from_build_env()` at `model.rs:1719` reads crate
  name/version.
- Other injectable handler arguments (any order, via `FromContextPart`,
  `crates/rmcp/src/handler/server/common.rs:147-235`):
  `RequestContext<RoleServer>`, `tokio_util::sync::CancellationToken`,
  `Peer<RoleServer>`, `RequestMetaObject`, `Extensions`, `Extension<T>`,
  `RequestId`; plus `ToolName`, `JsonObject` (raw args) from
  `crates/rmcp/src/handler/server/tool.rs:208-260`.
- `stdio()` returns `(tokio::io::Stdin, tokio::io::Stdout)`
  (`crates/rmcp/src/transport/io.rs:16-18`). A non-blocking Linux variant
  `stdio_pipes()` (`io.rs:39`) was added in 3.6.0 (`CHANGELOG.md:16`); it
  falls back to the blocking handles when stdin/stdout are not FIFOs, so it
  is safe to use unconditionally. Either works for Sluice.

## 4. Long-running tools

**Can a handler `.await` for 20 minutes?** Yes. Each incoming request is
dispatched with `spawn_service_task` onto its own tokio task
(`crates/rmcp/src/service.rs:1589-1604`); the serve loop keeps reading other
messages meanwhile (`service.rs:1394-1447`). There is no server-side timeout
on incoming requests: the only timeouts in `service.rs` belong to
`RequestHandle` / `PeerRequestOptions` (`service.rs:539-650, 760-795`), which
apply to requests the *server sends to the client* (sampling, elicitation,
roots), not to handlers. The shipped `long_task` example simply sleeps 10 s
inside the handler (`examples/servers/src/common/counter.rs:107-113`).

What limits us is therefore the client: Claude Code applies its own tool-call
timeout to MCP tools (an env var on its side, outside this repo; verify the
current value in Claude Code's docs). If the client gives up it sends
`notifications/cancelled`, see below.

**Progress notifications.** Supported; the client opts in by sending a
`progressToken` in `_meta`. From a handler:

1. Read the token: `ctx.meta.get_progress_token()` on `RequestMetaObject`
   (`crates/rmcp/src/model/meta.rs:436`), or manually from
   `ctx.meta.get_key_value("progressToken")` as the demo does
   (`examples/servers/src/common/progress_demo.rs:77-87`).
2. Send: `ctx.peer.notify_progress(ProgressNotificationParam::new(ProgressToken(token), progress_f64).with_total(..).with_message(..)).await?`
   (`progress_demo.rs:100-107`; README snippet `README.md:1110-1125`;
   `notify_progress` generated by `method!` at
   `crates/rmcp/src/service/server.rs:940`).
3. If the client sent no token, skip progress (the demo returns an error, but
   silently skipping is friendlier). Claude Code does send progress tokens
   for MCP tool calls in recent versions — verify, since it matters for
   keeping its timeout alive.

The test `crates/rmcp/tests/test_request_timeout_progress.rs` shows the
client-side `reset_timeout_on_progress` option (`service.rs:762-763, 788`):
a well-behaved client resets its idle timeout on each progress notification.
Whether Claude Code does this is not something this repo can tell us.

**Cancellation.** `notifications/cancelled` is wired to a
`CancellationToken`: the serve loop keeps one token per in-flight request in
`local_ct_pool` (`service.rs:1358, 1571-1573`); `RequestContext.ct` is a
child of it (`service.rs:1580-1586`, field doc at `service.rs:1194-1196`).
When a cancelled notification with that `requestId` arrives the loop removes
the token and calls `ct.cancel()` (`service.rs:1634-1639`). So inside a tool:

```rust
async fn verify(&self, ct: tokio_util::sync::CancellationToken, ...) -> Result<CallToolResult, McpError> {
    tokio::select! {
        _ = ct.cancelled() => Ok(CallToolResult::error(vec![ContentBlock::text("cancelled by client")])),
        r = self.run_gate(..) => r,
    }
}
```

(`CancellationToken` extractor at `common.rs:161-167`; the test
`crates/rmcp/tests/test_abandoned_request_cancel.rs:46-51` awaits
`context.ct.cancelled()` the same way.) Two caveats:
- The token is also cancelled when the transport closes (3.6.0 changelog
  "cancel pending request tokens after transport EOF", `CHANGELOG.md:34`),
  so a Claude Code exit cancels the running gate — good for us, as long as
  the gate runner honours the token.
- After cancellation the request id is gone from the pool, so a late response
  is dropped ("dropping response for cancelled request",
  `service.rs:1489-1491`). Finishing anyway is harmless but wasted.
- `ServerHandler::on_cancelled` (`handler/server.rs:566-572`) is an extra
  hook, not required; the token is enough.

**Transport limits.** The line codec caps an incoming message at
`DEFAULT_MAX_LINE_LENGTH = 16 MiB` (`crates/rmcp/src/transport/async_rw.rs:58`).
Outgoing results are not capped by rmcp, but a `logs` tool should still
truncate.

## 5. MCP Tasks extension (SEP-2663)

**Implemented**, not behind an extra feature flag: `rmcp::task_manager` is
compiled with the `server` feature (`crates/rmcp/src/lib.rs:29-30`). It
landed as a breaking change in 3.0.0-beta.1 (2026-07-23,
`crates/rmcp/CHANGELOG.md:266`), replacing an earlier SEP-1686 draft
(`CHANGELOG.md:639`). README section at `README.md:1409-1442`. The extension
id is `io.modelcontextprotocol/tasks` (`crates/rmcp/src/model/task.rs:14`)
and is declared through the capabilities `extensions` map on both sides
(`ClientCapabilities::builder().enable_tasks()`,
`ServerCapabilities::builder().enable_tasks()`,
`crates/rmcp/src/model/capabilities.rs:429-432, 460-463`;
`supports_tasks()` at `:200, :248`).

The spec itself still calls Tasks an *extension*; rmcp's implementation is
not marked experimental, but the client must opt in per session and the
server MUST NOT return a task to a client that did not declare it
(`examples/servers/src/common/task_demo.rs:85-90`).

Types and traits:
- `rmcp::task_manager::{TaskManager, TaskOptions, TaskContext, TaskExit, TaskFuture}`
  (`crates/rmcp/src/task_manager.rs:53, 155, 171, 229, 300`).
- Model: `Task`, `TaskStatus` (`working | input_required | completed | failed | cancelled`),
  `DetailedTask`, `TaskPayload`, `CreateTaskResult`, `GetTaskResult`,
  `TaskAckResult`, `GetTaskParams`, `UpdateTaskParams`, `CancelTaskParams`
  (`crates/rmcp/src/model/task.rs:21-48, 118, 165, 282, 350, 381`).
- `CallToolResponse::Task(CreateTaskResult)` is the third variant of the
  `call_tool` return enum next to `Complete` and `InputRequired`.
- `ServerHandler` hooks `get_task`, `update_task`, `cancel_task`, default to
  method-not-found (`crates/rmcp/src/handler/server.rs:606-636`).

Server flow (from `task_demo.rs:79-166`, and `crates/rmcp/tests/test_task.rs`):

1. Hold a `TaskManager` in the server struct; it is `Clone` (Arc inside).
2. Override `call_tool` by hand (the `#[tool_handler]` macro cannot do this).
   Check `context.client_capabilities().is_some_and(|c| c.supports_tasks())`.
3. `let task = self.tasks.spawn(TaskOptions::default(), move |ctx| Box::pin(async move { ... Ok(CallToolResult) }))`
   — the closure gets a `TaskContext` with `set_status_message`,
   `is_cancel_requested`, `cancelled().await`, `request_input`
   (`task_manager.rs:100-135, 71`). Return `Err(TaskExit::Cancelled)` to
   settle as `cancelled`, `Err(TaskExit::Error(McpError))` to settle as
   `failed` (`task_manager.rs:155-162`). The task is observable by
   `tasks/get` before `spawn` returns (`task_manager.rs:309-311`).
4. Return `Ok(CallToolResponse::Task(CreateTaskResult::new(task)))`.
5. Implement `get_task` → `GetTaskResult::new(self.tasks.get_task(&id)?)`,
   `update_task` → `self.tasks.update_task(&id, request.input_responses)`,
   `cancel_task` → `self.tasks.cancel_task(&id)` (`task_demo.rs:129-152`).
6. Fall back to synchronous execution via
   `ToolCallContext::new(self, request, context)` + `self.tool_router.call(tcc).await`
   (`task_demo.rs:117-118`), and implement `list_tools` with
   `ListToolsResult::with_all_items(self.tool_router.list_all())`.
7. Declare `.enable_tasks()` in `get_info`.

Defaults: TTL 5 min (`DEFAULT_TASK_TTL_MS`, `task_manager.rs:39`) — expired
tasks are marked `failed`; poll interval 1 s (`:42`). There is no background
sweeper; expiry is checked on each `spawn`/access (`task_manager.rs:296-298, 339-341`).
For a 20-minute gate you must raise the TTL with
`TaskOptions::new().with_ttl_ms(..)` (`:254`) or pass `None` for unlimited.
The client polls `tasks/get` honouring `pollIntervalMs`
(`examples/clients/src/task_stdio.rs:82-94`). There is no push notification
to the client when the task finishes: `notifications/tasks` exists only as a
model type (`model.rs:4689-4707`) with a client-side handler
(`handler/client.rs:263`); the server `Peer` has no `notify_task_status`
method (`service/server.rs:917-951` generates none) and `TaskManager` never
sends it, so polling is the only way a client learns the outcome.

## 6. Logging to stderr

stdout is the wire, so anything written there corrupts the stream. Every
stdio example initialises tracing with the stderr writer
(`examples/servers/src/counter_stdio.rs:10-14`,
`examples/servers/src/task_stdio.rs:13-17`):

```rust
tracing_subscriber::fmt()
    .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
    .with_writer(std::io::stderr)
    .with_ansi(false)
    .init();
```

`structured_output.rs:120-152` likewise uses `eprintln!` for banners. Rules
for Sluice: never `println!`; forward subprocess stdout/stderr of the test
suite to our stderr or to a file, never to our stdout; `with_ansi(false)`
because Claude Code captures stderr as plain text. rmcp itself logs at
`debug`/`trace` for every message (`service.rs:1449, 1549, 1595`), so keep
the default filter at `info` or the stderr log gets noisy.

MCP-level logging (`notifications/message` via `peer.notify_logging_message`,
`README.md:957-967`) is deprecated by SEP-2577 (`README.md:923`) — do not
build on it.

## 7. Recommendations for Sluice

**Result shape.** Return `Result<Json<VerifyReport>, McpError>` from `verify`
so the macro emits `outputSchema` and the result carries
`structuredContent`. Note that `CallToolResult::structured(value)`
(`model.rs:4318-4326`) also puts the compact JSON (`value.to_string()`) in
`content` (`model.rs:4322`); that is what the model reads. If a human-readable
summary matters (it does for `verify`), build the result by hand:
`CallToolResult::success(vec![ContentBlock::text(summary)])` and then set
`structured_content = Some(json)` (public field, `model.rs:4155`), with
`#[tool(output_schema = ...)]` or `Tool::with_output_schema::<T>()`
(`crates/rmcp/src/model/tool.rs:242`) to keep the schema. Since `2026-07-28`
`structuredContent` may be any JSON value and `outputSchema` any schema
(`README.md:158-161`). For
`logs` return plain text (`String`); for `gate_list` and `status` use
`Json<T>`.

**Errors.** Follow `README.md:297-331` / `handler/server.rs:525-530`: a gate
that *fails* is a normal tool result with `is_error = true`
(`CallToolResult::error(vec![ContentBlock::text(..)])` or
`CallToolResult::structured_error(json)`, `model.rs:4345`), because the
model sees the content. Reserve `Err(McpError)` (`ErrorData` constructors
`invalid_params`, `internal_error`, `invalid_request`, `model.rs:713-725`)
for malformed requests or an unreachable queue; clients render those
opaquely. A `Result<T, String>` return also works: the `Err` string becomes
an `is_error` text result (`handler/server/tool.rs:125-133`).

**Blocking vs Tasks for `verify` in v1.** Use the plain blocking handler in
v1, with cancellation and progress:

- The server side imposes no timeout; a handler may await for 20 min
  (section 4). The only risk is the client timeout, which progress
  notifications are designed to keep alive.
- Tasks require that Claude Code declare `io.modelcontextprotocol/tasks` in
  its client capabilities; nothing in this repo shows which clients do, and
  the server is forbidden from returning a task otherwise, so we would need
  the blocking path anyway as fallback (`task_demo.rs:85-90, 116-118`).
- Tasks force hand-written `call_tool`/`list_tools`/`get_task`/
  `update_task`/`cancel_task`, a TTL policy, and a client that polls
  `tasks/get` — the model would also have to drive the polling loop itself.
- The blocking path keeps the `#[tool_router]`/`#[tool_handler]` macros and
  gives Claude the result in the same turn.

Mandatory pieces for the blocking v1: (a) take a `CancellationToken`
argument and `select!` it against the gate run, killing the child process
group on cancel; (b) read `ctx.meta.get_progress_token()` and send
`notify_progress` on a timer (e.g. every 10-30 s with queue position /
elapsed) while the gate runs; (c) cap output size in the result and point to
`logs` for the rest. Revisit Tasks in v2 once we confirm Claude Code
declares the extension; the `TaskManager` API is small enough that adding
it later is additive (keep the gate runner independent of rmcp types so the
same future can be handed to `TaskManager::spawn`).
