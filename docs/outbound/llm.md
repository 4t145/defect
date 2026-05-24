# `defect-llm` 模块设计

`defect-llm` crate 负责把"消息 + 工具 + 采样参数 → 流式增量结果"这一过程，从两家不同 wire format（Anthropic Messages / OpenAI Chat Completions）以及多家厂商的传输/鉴权差异背后隐藏起来。

## 1. 两层架构

模块切分为**协议层**与**厂商层**两层，各管各的事：

```
┌─────────────────── defect-llm ───────────────────┐
│                                                   │
│  provider/   厂商层：实现 LlmProvider trait       │
│  ├── anthropic    ┐                               │
│  ├── bedrock      ├ 共用 AnthropicMessages 编解码 │
│  ├── vertex       ┘                               │
│  ├── openai       ┐                               │
│  ├── azure        ├ 共用 OpenAiChat 编解码        │
│  ├── deepseek     │                               │
│  └── ...          ┘                               │
│                                                   │
│              ↓ 调用                                │
│                                                   │
│  protocol/   协议层：纯编解码器，无 trait         │
│  ├── anthropic_messages   AnthropicMessagesCodec  │
│  └── openai_chat          OpenAiChatCodec         │
│                                                   │
└───────────────────────────────────────────────────┘
```

### 1.1 协议层（`protocol/`）

只做一件事：**wire JSON ↔ defect-llm 内部表示** 的相互转换。

- `encode_request(req: &CompletionRequest) -> Value`
- `decode_event(event: &SseEvent) -> Result<ProviderChunk, _>`

**不包含**：transport、auth、URL 模板、重试、tracing。这些都属于厂商层。

形态上是若干普通 struct + 函数，**不暴露 trait**——它是厂商层的内部实现细节，没有多态需求。

### 1.2 厂商层（`provider/`）

实现 `LlmProvider` trait，承担：

- **HTTP transport**：reqwest 直连 / Bedrock event-stream 二进制帧 / OAuth 流程
- **认证**：bearer token / AWS SigV4 / OAuth2 / Azure `api-key` header
- **URL 模板**：含 region / deployment / project 等 vendor-specific 拼装
- **能力声明**：每家厂商 `Capabilities` 不同（Anthropic 报 `prompt_cache: Supported`，DeepSeek 报 `thinking: Supported`，Qwen 不报）
- **错误 hint**：`forgot ANTHROPIC_API_KEY?`、`Bedrock model id 应为 anthropic.xxx:0 格式` 之类
- **模型元信息表**：token 上限、定价提示
- **vendor-specific tracing 字段**：`x-request-id`、Bedrock region、Azure deployment 名

### 1.3 这样切分的理由

不只是"美观"。真实的厂商差异列出来：

| 厂商 | 协议 | 真实定制点 |
| --- | --- | --- |
| Anthropic 官方 | AnthropicMessages | bearer、`anthropic-version` header、prompt cache 标记 |
| AWS Bedrock | AnthropicMessages | **AWS SigV4 签名**、模型 ID 加前缀、**transport 不是 SSE 是 AWS event-stream** |
| GCP Vertex | AnthropicMessages | OAuth2、URL 含 project/location、版本号语法不同 |
| OpenAI 官方 | OpenAiChat | bearer、`organization` header |
| Azure OpenAI | OpenAiChat | `api-key` header、URL 用 deployment 名、`api-version` query |
| DeepSeek | OpenAiChat | bearer、响应多 `reasoning_content` 字段 |
| GitHub Copilot | OpenAiChat | OAuth + token 刷新 |

关键观察：**Bedrock 用 AnthropicMessages 的 JSON 形态但 transport 不是 SSE**——若把 transport 锁死在协议层，Bedrock 就无法复用 codec。所以 transport **必须** 归厂商层。

## 2. 目录结构

按 `claude.md` 第 10 条 `submodule/` + `submodule.rs` 风格组织：

```
crates/llm/src/
├── lib.rs
├── protocol.rs                    # protocol mod 入口
├── protocol/
│   ├── anthropic_messages.rs
│   ├── anthropic_messages/        # 按 wire / decode 等子职责切
│   │   ├── wire.rs
│   │   └── decode.rs
│   ├── openai_chat.rs
│   └── openai_chat/
│       ├── wire.rs
│       └── decode.rs
├── provider.rs                    # provider mod 入口（trait + 共享 helper）
└── provider/
    ├── anthropic.rs
    ├── openai.rs
    ├── bedrock.rs                 # P1
    ├── azure.rs                   # P2
    └── ...                        # 按需添加
```

`LlmProvider` trait 本身定义在 `defect-agent`，不在 `defect-llm`——`defect-llm` 是 trait 的**实现者**，不是定义者。

## 3. v0 范围

只做 `provider/anthropic.rs` + `provider/openai.rs` 两家，把协议层与厂商层的接缝跑通。其余厂商（Bedrock / Vertex / Azure / DeepSeek 等）作为后续证明"两层架构"价值的扩展点，不阻塞 v0 闭环。

## 4. 与三个参考仓库的对比

| 维度 | codex | claw-code | opencode | defect |
| --- | --- | --- | --- | --- |
| 抽象单位 | provider（轻量代理） | **协议**（前缀路由） | provider（无 trait） | **协议 + 厂商两层** |
| trait | `ModelProvider` | `Provider<Stream>` | 工厂函数 | `LlmProvider`（带关联 Stream） |
| OpenAI 兼容 | base_url 参数化 | 单 `OpenAiCompat` 实现 + 前缀路由 | endpoint union | 厂商层每家独立 + 协议层共享 codec |
| harness 能力 | 弱 | 中（错误 hint） | 中 | 强（per-vendor tracing/auth/URL） |
| 维护成本 | 最低 | 中（新厂商动同一文件） | 中 | 略高（每家一个文件），但**线性扩展** |

claw-code 的"前缀路由 + 单实现"在 v0 漂亮，但每加一家厂商都要动同一个 `OpenAiCompat` 文件，per-vendor harness 难做。defect 选**两层切分**，代价是多一些文件、多一层间接，但每加一家厂商只是**新增一个文件**，不动既有代码。

## 5. 与 `defect-agent` 共享的类型

下列值类型定义在 `defect-agent`，`defect-llm` 通过依赖 agent 引用，**不**重复定义：

- `ToolSchema { name, description, input_schema }` —— 工具的"对外名片"，`Tool` trait 的字段。`LlmProvider::complete` 接受 `Vec<ToolSchema>`，provider 不持有 `dyn Tool`，只把 schema 序列化进 wire JSON
- `LlmProvider` trait 本身
- `ProviderChunk` / `ProviderError` / `Capabilities` 等流式与元信息类型

理由：`defect-agent` 是架构里的中心 crate，定位就是"提供共享 trait 与类型"；`defect-llm`/`defect-tools`/`defect-mcp` 都依赖它来实现 trait。这与"agent 不依赖具体实现"的约束不冲突——被依赖与依赖具体实现是两回事。

不切独立 `defect-types` crate，因为目前没有"想单独用 `defect-llm` 而不带 agent"的真实需求，提前抽象只会模糊边界。

## 6. 待定（在后续文档中沉淀）

- `LlmProvider` trait 的精确签名（`docs/internal/llm-trait.md`）
- `ProviderChunk` 枚举的拆分粒度（细到 `ContentBlockStart/End` 还是只到 `Delta`）
- `Capabilities` 的具体字段集合
- `ProviderError` 的精确分类
- `protocol/` 与 `provider/` 之间的具体接口边界
