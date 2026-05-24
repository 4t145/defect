# Anthropic provider 落地

`provider/anthropic.rs` 实现 [`LlmProvider`](../internal/llm-trait.md)，对接 Anthropic 官方 `https://api.anthropic.com/v1`。

设计前提：[`docs/outbound/llm.md`](./llm.md) §1 的两层架构（协议层做 wire 编解码 / 厂商层做 transport+auth+能力声明），[`docs/internal/llm-trait.md`](../internal/llm-trait.md) §4.1 给出 wire 事件 → `ProviderChunk` 的映射表。本文把"具体怎么搭起来"的部分补齐。

---

## 1. 客户端方案

不手撕 reqwest，**用 [`tower-openapi-client`](https://github.com/4t145/tower-openapi-client)（toac）从 OAS 生成客户端**。理由：

- Anthropic 没有官方 Rust SDK；社区 SDK（`anthropic-sdk-rs` 等）的错误模型/auth 接口跟我们已经定的 [`ProviderError`](../internal/llm-trait.md#7-providererror) / [`Capabilities`](../internal/llm-trait.md#5-capabilities) 全冲突，引入再适配相当于重写。
- 自己手写 wire types 也要写 200~300 行，且每次 Anthropic 加字段都要追，codegen 一劳永逸。
- toac 的 `text/event-stream` 已经是一等公民——response enum 直接出 `Status200Sse(SseEventStream)`；我们的协议层只需吃 `SseEvent` 写状态机，不再碰 SSE bytes。
- toac 的 transport 是 BYO `tower::Service<http::Request<toac::body::Body>>`，跟 reqwest 解耦——defect-llm 用 hyper 直连（`client-util` 提供 `HyperHttpsClient`），不依赖 workspace 已锁的 `reqwest = "0.13"`。

`tower-openapi-client` 通过 git rev 引入，`toac-build` 仅作为 codegen 工具用，不进运行期依赖。

## 2. OAS 范围与裁剪

放在 `crates/llm/oas/anthropic.yaml`，**手写**（Anthropic 没发布官方 OAS）。覆盖范围按"v0 闭环必需"裁剪：

### 2.1 Endpoints

| 用途 | 方法 | 路径 | 对应 `LlmProvider` 方法 |
| --- | --- | --- | --- |
| 流式生成 | POST | `/v1/messages` | `complete` |
| 列出模型 | GET | `/v1/models` | `list_models` |

`/v1/models/{model_id}` 不写——`list_models` 返回值在 provider 内缓存，`model_info` 直接查缓存（[`llm-trait.md`](../internal/llm-trait.md) §3.1）。

`/v1/messages/count_tokens`、`/v1/messages/batches`、`/v1/files`、`/v1/skills` v0 不要——主循环不调用。要的时候补 OAS 重新 codegen。

### 2.2 Schema 裁剪原则

**只写 `CompletionRequest` 实际会用到的字段**，理由：

- toac 把每个字段都生成 `Option<T>` + 全名结构体，全量 OAS 会让 `components::*` 翻倍——review codegen diff 时噪音盖掉信号。
- 漏字段时反正是手动跑 codegen 加进来，编译会立刻报错，不会静默失败。

裁剪后保留的核心 schema：

| schema | 含字段 |
| --- | --- |
| `MessagesRequest` | `model`, `messages`, `system`, `max_tokens`, `temperature`, `top_p`, `top_k`, `stop_sequences`, `stream`, `tools`, `tool_choice`, `thinking` |
| `Message` | `role`, `content` |
| `ContentBlock` | `text` / `tool_use` / `tool_result` / `thinking` 四个 variant（`#[serde(tag = "type")]`） |
| `Tool` | `name`, `description`, `input_schema` |
| `ToolChoice` | `auto` / `any` / `tool` 三个 variant |
| `ThinkingConfig` | `enabled` / `disabled`，`enabled` 带 `budget_tokens` |
| `Usage` | `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens` |
| `StopReason` | `end_turn` / `max_tokens` / `stop_sequence` / `tool_use` / `refusal` |
| `MessagesResponse`（非流式分支） | 同 wire；目前留着是因为 OAS 必须列出 200 的 JSON branch，但**主循环只用 SSE 分支** |
| `ModelInfo`（`/v1/models` 200 元素） | `id`, `display_name`, `created_at`, `type` |

### 2.3 SSE event schema

Anthropic 的 SSE 体系是 **named events**（每个 `event:` 名字对应一个 data schema），toac 的处理方式是把 200 状态下 `text/event-stream` content type 列一个 union schema，按 `event:` 名字 tag。具体形状：

```yaml
text/event-stream:
  schema:
    oneOf:
      - $ref: '#/components/schemas/MessageStartEvent'
      - $ref: '#/components/schemas/ContentBlockStartEvent'
      - $ref: '#/components/schemas/ContentBlockDeltaEvent'
      - $ref: '#/components/schemas/ContentBlockStopEvent'
      - $ref: '#/components/schemas/MessageDeltaEvent'
      - $ref: '#/components/schemas/MessageStopEvent'
      - $ref: '#/components/schemas/PingEvent'
      - $ref: '#/components/schemas/ErrorEvent'
    discriminator:
      propertyName: type
```

每个 `*Event` schema 对应 wire 上 `data:` 行的 JSON。注意 toac 的 SSE 解码出来的 `SseEvent` 还带 `event:` 名字，状态机会用它做主分发，`type` 字段做副校验。

待定：上游 wire 的 `event:` 名字与 `data.type` 相等（如 `event: message_start` + `data: {"type":"message_start", ...}`），冗余但不冲突；codegen 出来的 union 用 `data.type` 做 tag 是简单做法。落地时若 toac 对 SSE event-name discriminator 有更直接的支持，按它的形状走。

## 3. 鉴权

Anthropic 必须的 headers：

| header | 来源 | 备注 |
| --- | --- | --- |
| `x-api-key` | env `ANTHROPIC_API_KEY` | 主 auth；OAS `securitySchemes.ApiKeyAuth` 声明 |
| `anthropic-version` | 常量 `2023-06-01` | 写死在 provider，**不**走 OAS（API 强制必填、不是用户选项） |
| `anthropic-beta` | 按 capability 拼装 | extended thinking / prompt cache 历史上是 beta；新模型已 GA，不发即可 |

在 OAS 里只声明 `ApiKeyAuth`，让 codegen 出 `AuthConfig::builder().api_key_auth(...)`。`anthropic-version` / `anthropic-beta` 由 provider 在调 `client.call(request)` 之前用 `Request::with_header` 注入，**不污染 OAS**。

`AuthConfig` 在 `AnthropicProvider::new(config)` 时一次性装好；构造期 env 读不到 `ANTHROPIC_API_KEY` 立即返回 [`ProviderErrorKind::AuthMissing { var_hint }`](../internal/llm-trait.md#72-错误分类)，不延后到第一次 `complete` 调用。

## 4. transport 装配

```rust
// provider/anthropic.rs

use client_util::client::{build_https_client, HyperHttpsClient};
use toac::ApiClient;
use crate::wire::anthropic as wire;

type Http = HyperHttpsClient<toac::body::Body>;
type Client = ApiClient<Http>;

pub struct AnthropicProvider {
    client: Client,
    info: ProviderInfo,
    capabilities: Capabilities,
    models: Arc<RwLock<Option<Vec<ModelInfo>>>>, // list_models 后填
}

impl AnthropicProvider {
    pub fn new(config: AnthropicConfig) -> Result<Self, ProviderError> {
        let token = config.api_key()?;                       // env / config
        let auth = wire::security::AuthConfig::builder()
            .api_key_auth(token)
            .build();
        let http = build_https_client::<toac::body::Body>()
            .map_err(|e| ProviderError::new(transport_kind(e)))?;
        let client = ApiClient::new(http, config.base_url())
            .with_auth(auth);
        Ok(Self { client, info: ..., capabilities: ..., models: Arc::default() })
    }
}
```

主要点：

- **不放 tower middleware 进 stack 里搞 retry / 限流**——按 [`turn-loop.md`](../internal/turn-loop.md) §3.1，重试归 turn loop 的 attempt 维度。Provider 内部只做"一次 HTTP 调用→流"。
- **`base_url` 默认 `https://api.anthropic.com`**，可用 `ANTHROPIC_BASE_URL` 覆盖（mock 测试 / 私有代理）。

## 5. `complete` 实现

```rust
fn complete(&self, req: CompletionRequest, cancel: CancellationToken)
    -> BoxFuture<'_, Result<ProviderStream, ProviderError>>
{
    Box::pin(async move {
        // 1. CompletionRequest -> wire::MessagesRequest
        let body = crate::protocol::anthropic_messages::encode_request(&req)?;

        // 2. 装 SSE-Accept 强制走流分支
        let mut request = wire::operations::messages::post::Request { body };
        request = request.with_accept(http::HeaderValue::from_static("text/event-stream"));
        request = request.with_header("anthropic-version", "2023-06-01")?;
        // anthropic-beta：按 self.capabilities 决定

        // 3. 调用，绑 cancel
        let resp = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::new(ProviderErrorKind::Canceled)),
            r = self.client.clone().call(request) => r.map_err(|e| call_error_to_provider(e))?,
        };

        // 4. 取 SSE 分支
        let stream = match resp {
            wire::operations::messages::post::Response::Status200Sse(s) => s,
            wire::operations::messages::post::Response::Status200Json(_) => {
                return Err(ProviderError::new(ProviderErrorKind::ProtocolViolation {
                    hint: "server returned application/json despite Accept: text/event-stream".into()
                }));
            }
        };

        // 5. SseEventStream -> impl Stream<Item = Result<ProviderChunk, ProviderError>>
        Ok(Box::pin(crate::protocol::anthropic_messages::decode_stream(stream, cancel))
            as ProviderStream)
    })
}
```

要点：

- `cancel` 在两处生效：调用前 `tokio::select!`、流内 `decode_stream` 内 `select!`。drop stream 也应让 `decode_stream` 退出（hyper 的 body 会感知）。
- `self.client.clone()` 在 `tower::Service::call` 上为必要——`Service` 要 `&mut self`；`ApiClient` 自身轻量克隆。

## 6. 协议层：`SseEvent` → `ProviderChunk`

`crates/llm/src/protocol/anthropic_messages.rs` 暴露两个函数：

```rust
pub fn encode_request(req: &CompletionRequest) -> Result<wire::MessagesRequest, EncodeError>;

pub fn decode_stream(
    sse: toac::body::codec::sse::SseEventStream,
    cancel: CancellationToken,
) -> impl Stream<Item = Result<ProviderChunk, ProviderError>> + Send;
```

### 6.1 字段映射（`encode_request`）

| `CompletionRequest` 字段 | wire 字段 | 备注 |
| --- | --- | --- |
| `model` | `model` | 直传 |
| `system: Option<String>` | `system: Option<String>` | top-level 字段，不放 messages 数组 |
| `messages` | `messages` | 见下表 |
| `tools: Vec<ToolSchema>` | `tools: Vec<Tool>` | `name` / `description` / `input_schema` 同名直传 |
| `tool_choice: Auto` | `{"type": "auto"}` | |
| `tool_choice: Required` | `{"type": "any"}` | Anthropic 用 `any` 不是 `required` |
| `tool_choice: Named(s)` | `{"type": "tool", "name": s}` | |
| `tool_choice: None` | 不发字段 | Anthropic 不支持 "禁止工具"；漏发 = `auto` |
| `sampling.max_tokens` | `max_tokens` | **必填**——为 `None` 时按模型默认（`max_output_tokens` 或 4096）填 |
| `sampling.temperature` | `temperature` | |
| `sampling.top_p` / `top_k` | 同名 | |
| `sampling.stop_sequences` | `stop_sequences` | |
| `sampling.thinking: Enabled { budget_tokens }` | `{"type": "enabled", "budget_tokens": ...}` | budget 必填，`None` → 1024 默认 |
| `sampling.thinking: Disabled` | 不发字段 | |
| `stream` | `true` | 由协议层强制写死 |

`MessageContent` → wire `ContentBlock` 映射：

| 内部 | wire |
| --- | --- |
| `Text(s)` | `{"type": "text", "text": s}` |
| `ToolUse { id, name, args }` | `{"type": "tool_use", "id", "name", "input": args}` |
| `ToolResult { tool_use_id, output: Text(s), is_error }` | `{"type": "tool_result", "tool_use_id", "content": [{"type": "text", "text": s}], "is_error"}` |
| `ToolResult { ..., output: Json(v) }` | 同上但 `text = serde_json::to_string(&v)` |
| `Image { ... }` | *(P2)* — 形状随 [`llm-trait.md`](../internal/llm-trait.md) §8 待定 |

### 6.2 SSE → `ProviderChunk` 状态机

输入是 `Stream<Item = Result<SseEvent, _>>`，按 wire `event:` 名字（次要：`data.type` 字段）分发。状态机维持的内部状态：

```rust
struct DecoderState {
    /// content_block index → 该块的种类。
    blocks: HashMap<u32, BlockKind>,
    /// 是否已发过 MessageStart（防御性，按规范不会重复）。
    started: bool,
    /// 是否已发过 Stop（防御性）。
    stopped: bool,
}

enum BlockKind {
    Text,
    Thinking,
    ToolUse { id: String },  // index → tool_use_id 反查
}
```

事件处理表：

| wire event | wire data 关键字段 | 状态机动作 | 产出 chunk |
| --- | --- | --- | --- |
| `message_start` | `message: { id, model, usage }` | `started=true` | `MessageStart{id,model}`，再 `Usage{input_tokens=..., cache_*}` |
| `content_block_start` | `index, content_block: { type=text }` | `blocks[index] = Text` | （无） |
| `content_block_start` | `index, content_block: { type=thinking }` | `blocks[index] = Thinking` | （无） |
| `content_block_start` | `index, content_block: { type=tool_use, id, name }` | `blocks[index] = ToolUse{id}` | `ToolUseStart{id, name}` |
| `content_block_delta` | `index, delta: { type=text_delta, text }` | – | `TextDelta{text}` |
| `content_block_delta` | `index, delta: { type=thinking_delta, thinking }` | – | `ThinkingDelta{text=thinking}` |
| `content_block_delta` | `index, delta: { type=signature_delta, signature }` | – | `ThinkingSignature{signature}` |
| `content_block_delta` | `index, delta: { type=input_json_delta, partial_json }` | 查 `blocks[index]` 取 `tool_use_id` | `ToolUseArgsDelta{id, fragment=partial_json}` |
| `content_block_stop` | `index` | 查 `blocks[index]` | 若是 `ToolUse{id}` → `ToolUseEnd{id}`；否则无 |
| `message_delta` | `delta: { stop_reason }, usage` | `stopped=true` | `Stop{reason}`，再 `Usage{output_tokens, cache_*}` |
| `message_stop` | – | – | （无） |
| `ping` | – | – | （吞掉） |
| `error` | `error: { type, message }` | – | 流上 `Err(ProviderError)`（见 §7） |

**stop_reason 翻译**：`end_turn`→`EndTurn`、`max_tokens`→`MaxTokens`、`stop_sequence`→`StopSequence`、`tool_use`→`ToolUse`、`refusal`→`Refusal`。未知值翻 `EndTurn` + tracing::warn（兼容上游加新值）。

### 6.3 错误形态

`decode_stream` 每条 yield 一个 `Result`：

- 单条 SSE event JSON 解码失败 → `Err(ProviderError::new(ProviderErrorKind::Malformed(BoxError)))`，**继续消费下一条**。这条规则与 [`llm-trait.md`](../internal/llm-trait.md) §1.3 "可恢复的协议噪声"对齐。
- 收到 wire `event: error` → `Err(ProviderError::new(...))`，**终止流**。
- transport 层错误（toac `CallError::Transport`）→ provider 把首发的 `Err` yield 出来，`SseEventStream` 自然终结。
- 完整流跑完没收到 `Stop` → 在流末尾追加一条 `Err(ProtocolViolation { hint: "stream ended without message_delta stop" })`。
- `cancel.cancelled()` 触发 → 流静默终结（不 yield `Err`），与 [`llm-trait.md`](../internal/llm-trait.md) §2.2 一致。

## 7. 错误映射

`toac::CallError<E>` 与 HTTP status / Anthropic `error.type` 字段三方信号合在一起决定 `ProviderErrorKind`：

| 来源 | 触发条件 | 映射 |
| --- | --- | --- |
| `CallError::Encode(_)` | request 编码失败 | `BadRequest{ hint: encode reason }` |
| `CallError::Auth(_)` | toac auth layer 报错（key 缺失等） | `AuthMalformed{ hint }` |
| `CallError::Transport(_)` | DNS/TCP/TLS/HTTP 失败 | `Transport(BoxError)`；`reqwest::Error::is_timeout()` 风格判定 → `Timeout{phase}` |
| `CallError::Decode(_)` | 响应体解码失败 | `Malformed(BoxError)` |
| HTTP 401 | – | `AuthRejected{ hint: error.message }` |
| HTTP 400 + `error.type=invalid_request_error` | 含 `max_tokens` 字眼 | `MaxTokensInvalid{}` |
| HTTP 400 + 其它 | – | `BadRequest{ hint }` |
| HTTP 403 + `error.type=permission_error` | – | `AuthRejected{}` |
| HTTP 404 + `error.type=not_found_error` | 模型不存在 | `ModelNotFound{ model }` |
| HTTP 413 | – | `BadRequest{ hint: "payload too large" }` |
| HTTP 429 | `Retry-After` header | `RateLimit{ retry_after, scope: Unspecified }` |
| HTTP 500 / 502 / 503 / 504 | – | `ServerError{ status }` |
| HTTP 529 (overloaded) | – | `ServerError{ status: Some(529), hint: "overloaded" }` |
| `error.type=overloaded_error` | – | `ServerError{ hint: "overloaded" }` |
| 流中 wire `event: error` | – | 按 `error.type` 同上规则 |

`request_id` 全部从 `request-id` header 抽取（404/429/5xx 都有）。

## 8. `list_models` / `model_info`

```rust
fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, ProviderError>> {
    Box::pin(async move {
        // 1. 查缓存
        if let Some(v) = self.models.read().await.clone() { return Ok(v); }

        // 2. GET /v1/models
        let resp = self.client.clone().call(wire::operations::models::get::Request {})
            .await.map_err(call_error_to_provider)?;
        let list = match resp {
            wire::operations::models::get::Response::Status200(l) => l,
            other => return Err(...),
        };

        // 3. wire ModelInfo -> internal ModelInfo
        let mapped: Vec<_> = list.data.into_iter().map(|m| ModelInfo {
            id: m.id,
            display_name: m.display_name,
            // Anthropic /v1/models 不返回 context_window / max_output_tokens —— 留 None
            context_window: None,
            max_output_tokens: None,
            deprecated: false, // /v1/models 不直接告
            capabilities_overrides: ModelCapabilityOverrides::default(),
        }).collect();

        *self.models.write().await = Some(mapped.clone());
        Ok(mapped)
    })
}

fn model_info(&self, model_id: &str) -> Option<ModelInfo> {
    self.models.try_read().ok()
        .and_then(|g| g.as_ref()?.iter().find(|m| m.id == model_id).cloned())
}
```

要点：

- `model_info` 同步、不触发网络，按 [`llm-trait.md`](../internal/llm-trait.md) §3.1 契约。
- v0 不解析 `context_window` / `max_output_tokens`——Anthropic `/v1/models` 不返回这些，要走另一份硬编码表（按 model_id 前缀匹配）；那份表放在 `provider::anthropic::models` 子模块，`list_models` 时合并进去。本表 v0 只列 `claude-opus-4-7-*` / `claude-sonnet-4-6-*` / `claude-haiku-4-5-*` 三档，其余按 `None`。

## 9. Capabilities

```rust
Capabilities {
    tool_calls: Supported,
    parallel_tool_calls: Supported,  // Anthropic 自 2024-06 默认开
    thinking: Supported,             // 但 Haiku 等不支持 → ModelCapabilityOverrides 覆写
    vision: Supported,
    prompt_cache: Supported,
}
```

`ModelCapabilityOverrides` v0 只为 Haiku 4.5 覆 `thinking: Unsupported`——主循环要用 `model_info` 查到才会按需关思考链；查不到直接信全局值是已知 false-positive，目前可接受。

## 10. 测试策略

- **单测**：`crates/llm/src/protocol/anthropic_messages/test.rs`（按 [`coding-reference.md`](../coding-reference.md) §10 走 `submodule/test.rs`）。喂构造好的 `Vec<SseEvent>` 给 `decode_stream`、断言产出的 `Vec<ProviderChunk>` 序列。覆盖：
  - 单 tool_use 的 Start→ArgsDelta×N→Stop→ToolUseEnd→message_delta 完整路径
  - 两个 tool_use 并发（不同 `index`）的 ArgsDelta 交错
  - thinking + signature_delta 交替
  - `event: error` 在流中出现 → 终止
  - `event: ping` → 吞掉
- **集测**：`crates/llm/tests/anthropic_e2e.rs`，用 `wiremock` 起本地 server，让 provider 走 `https://localhost:port`（`ANTHROPIC_BASE_URL` 覆盖）；验证 round-trip、auth header、cancel 中断。**不打真的 Anthropic API**。

## 11. Codegen 工作流

```
crates/llm/
├── Cargo.toml
├── oas/
│   └── anthropic.yaml               ← 我们手写 + commit
└── src/
    └── wire/
        ├── anthropic.rs             ← codegen 产物，commit 进仓库
        └── anthropic.gen.txt        ← 顶部 header 标 OAS sha + toac rev

scripts/llm-codegen/
├── Cargo.toml                       ← [[bin]] name = "defect-llm-codegen"
└── src/main.rs                      ← toac_build::Builder::new(oas).emit_to(out)
```

工作流：

1. 改 `crates/llm/oas/anthropic.yaml`
2. `cargo run -p defect-llm-codegen -- anthropic`（或 `make llm-codegen`）
3. 把生成的 `wire/anthropic.rs` commit
4. CI 跑 `make llm-codegen-check`：重生 → diff，没变化才过

CI 守门保证"OAS 改了但代码没 regenerate"会被红灯拦下。

`scripts/llm-codegen` 是 workspace member 但 `default-members` 排除，平时 `cargo build` 不编。
