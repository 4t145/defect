# 事件模型 `AgentEvent`

`AgentEvent` 是 `defect-agent` 主循环对外发布的事件流。三个独立消费者都从同一条流上取数据：

```text
                ┌──► defect-acp     (翻译成 SessionUpdate / PromptResponse)
AgentEvent ────┼──► defect-storage (jsonl 持久化、resume)
                └──► tracing        (结构化日志、observability)
```

本文沉淀这套设计的形状、字段约定与权衡。

## 1. 总体定位

主循环只产生一种事件类型；wire / 存储 / 日志各自从中投影出自己关心的子集。这一节回答两个问题：**为什么不直接吐 ACP wire 类型？** 与 **为什么字段还是直接借 ACP 类型？**

### 1.1 为什么不直接吐 ACP wire 类型（`SessionUpdate`）

我们已经因为 `Tool` trait 让 `defect-agent` 依赖了 `agent-client-protocol`。再让 *事件流的 item* 也是 wire 类型，技术上完全可行；但有几条硬约束让我们退一步：

1. **持久化与 wire 解耦**。`docs/internal/storage-jsonl.md` 计划是 jsonl append-only、可 resume。如果落盘格式 == ACP wire，ACP crate 升级 = on-disk schema 变 = 老会话不能 resume。让 `AgentEvent` 有自己的 serde 表示，跟 wire 无关，存档稳定性独立掌控。
2. **存在 wire 上没有的事件**。`LlmCallStarted` / `LlmCallFinished` / `ContextCompressed` / `PolicyDecision` 是排障与审计的核心信号，不应该塞进 ACP `SessionUpdate`（污染 wire 语义），也不应该走副通道（消费者要从两处听同一个 turn）。
3. **turn 边界在 ACP 里没有 update 形态**。ACP 把"turn 结束"放在 `PromptResponse`（request 的返回值）而不是 `SessionUpdate` 的 variant。我们的事件流必须能表达 `TurnEnded`，否则桥接层就得用别的通道知道"该 respond 了"。
4. **ACP 的 `unstable_*` feature gate**。ACP 把若干字段藏在 cargo feature 后面（`unstable_session_usage` / `unstable_message_id` 等）。直接吐 wire 类型意味着我们要么强开 unstable 跟 ACP 节奏走，要么主循环里就丢掉这些字段。`AgentEvent` 自己定义稳定字段，桥接层按 feature 决定是否往 wire 推。

### 1.2 但字段类型仍直接借 ACP

`AgentEvent` 的 *变体* 是我们的、*字段类型* 尽量复用 ACP 的被动数据结构（`ToolCallUpdateFields`、`ContentBlock`、`StopReason`、`ToolCallId`、`PermissionOptionId`）。理由跟 [Tool trait](./tool-trait.md#3-toolcalldescription) 同源：

- ACP 的字段集已经覆盖了我们能想到的所有需求，重新发明只会延迟拿到新字段
- 翻译层不需要拼字段，只需要按 enum variant 决定"是否推到 wire、推成哪个 SessionUpdate"
- `defect-storage` 做 jsonl 持久化时，复用 ACP 字段的 `Serialize` / `Deserialize` 实现，零成本

代价是 `defect-agent` 必依赖 `agent-client-protocol`（已经因为 Tool trait 接受了）。

## 2. AgentEvent 定义

```rust
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    // ── turn 边界 ──────────────────────────────
    TurnStarted,
    TurnEnded { reason: AcpStopReason, usage: Usage },

    // ── 助手输出（推给 wire） ──────────────────
    AssistantText { content: ContentBlock },
    AssistantThought { content: ContentBlock },

    // ── 工具调用（推给 wire） ──────────────────
    ToolCallStarted  { id: ToolCallId, fields: ToolCallUpdateFields },
    ToolCallProgress { id: ToolCallId, fields: ToolCallUpdateFields },
    ToolCallFinished { id: ToolCallId, fields: ToolCallUpdateFields },

    // ── 权限决策（部分推给 wire） ──────────────
    PolicyDecision { id: ToolCallId, decision: PolicyDecision },
    PermissionResolved { id: ToolCallId, outcome: PermissionResolution },

    // ── 主循环编排（不入 wire；仅 storage / tracing） ──
    LlmCallStarted { model: String, attempt: u32 },
    LlmCallFinished { model: String, attempt: u32, usage: Usage, error: Option<String> },
    ContextCompressed { tokens_before: u64, tokens_after: u64 },
}
```

辅助类型：

```rust
#[non_exhaustive]
pub enum PolicyDecision { Allow, Deny, Ask }

#[non_exhaustive]
pub enum PermissionResolution {
    Selected { option_id: PermissionOptionId },
    Cancelled,
}
```

## 3. 变体设计要点

### 3.1 turn 边界

- `TurnStarted` 没有字段：`session_id` 由订阅者自己持有；prompt 内容已经在主循环外传入。变体存在的意义是给消费者一个明确的"边界"信号。
- `TurnEnded::reason` 直接借 `agent_client_protocol::schema::StopReason` 的值（包含 `Cancelled` / `EndTurn` / `MaxTokens` / `MaxTurnRequests` / `Refusal`），桥接层零成本把它放进 `PromptResponse`。
- `TurnEnded::usage` 用我们自己的 [`Usage`](./llm-trait.md#1-providerchunk)（来自 `ProviderChunk::Usage` 累加），跟 ACP 的 `unstable_session_usage` 解耦：v0 不开 unstable，仍然能给 storage / tracing 看到完整用量。

### 3.2 助手输出

- `AssistantText` / `AssistantThought` 都用 `ContentBlock`（不是 `ContentChunk`），因为 `ContentChunk` 包含 `unstable_message_id` feature。桥接层包一层 `ContentChunk::new(content)` 就能上 wire。
- "增量"由消费者按时间序拼接，事件本身不携带"我是第几个 chunk"——LLM provider 的 SSE 已经按时间序列发，主循环转发即可。

### 3.3 工具调用三段式

`Started` / `Progress` / `Finished` 跟 [Tool trait 的 `ToolEvent`](./tool-trait.md#5-toolevent--toolstream) 形成 1:1 镜像：

| Tool 流 | AgentEvent | ACP wire |
| --- | --- | --- |
| (主循环在 execute 之前) | `ToolCallStarted { id, fields }` | `SessionUpdate::ToolCall(...)` |
| `ToolEvent::Progress(fields)` | `ToolCallProgress { id, fields }` | `SessionUpdate::ToolCallUpdate(...)` |
| `ToolEvent::Completed(fields)` 或 `Failed(error)` | `ToolCallFinished { id, fields }` | `SessionUpdate::ToolCallUpdate(...)` |

`ToolCallId` 由主循环统一分配（用 LLM 给的 `tool_use_id` 或自生成 UUID），见 [Tool trait §3](./tool-trait.md#3-toolcalldescription)。

`fields` 的 status 字段由主循环填：`Started` → `Pending` 或 `InProgress`、`Finished` 在 success 时 `Completed`、failure 时 `Failed`（并把错误文本塞进 `content`）。

### 3.4 权限决策一分为二

ACP 的权限路径是双向异步：agent 发 `RequestPermissionRequest` → 等客户端 respond → 拿到 `PermissionOptionId`。这一段在内部要表达成两个事件：

- `PolicyDecision { decision: Ask }` — sandbox 决定要请权限。桥接层据此发 `RequestPermissionRequest`。`Allow` / `Deny` 也通过同一个事件发（不入 wire，仅审计）。
- `PermissionResolved { outcome }` — 客户端 respond 后主循环把结果回写成事件。审计能看到完整决策链。

为什么不合并成一个 `PermissionResolved`？因为"决策时刻"和"用户应答时刻"对 storage / tracing 是两个独立的时间点；合并会丢失"等了多久"的信息（这条线对超时排查很关键）。

### 3.5 主循环编排事件

`LlmCallStarted` / `LlmCallFinished` / `ContextCompressed` 是 wire **看不见**的：客户端只关心助手输出与工具调用，不关心 agent 内部跑了几次 LLM、压缩了多少 token。但这三类对：

- **storage**：resume 时要重建主循环状态（context 大小、attempt 计数）
- **tracing**：排障第一信号源（哪次 LLM 调用慢、哪一轮触发了压缩）

至关重要，所以放在 `AgentEvent` 而不是另一条 stream。`error` 字段保留为 `Option<String>`（不放完整错误对象）——完整错误进 tracing；事件流里只留可序列化的摘要。

## 4. 投影：谁消费哪些变体

| 变体 | wire (ACP) | storage (jsonl) | tracing |
| --- | --- | --- | --- |
| `TurnStarted` | — | ✓ | ✓ |
| `TurnEnded` | → `PromptResponse` | ✓ | ✓ |
| `AssistantText` | → `AgentMessageChunk` | ✓ | ✓ (debug) |
| `AssistantThought` | → `AgentThoughtChunk` | ✓ | ✓ (debug) |
| `ToolCallStarted` | → `ToolCall` | ✓ | ✓ |
| `ToolCallProgress` | → `ToolCallUpdate` | ✓ | ✓ (debug) |
| `ToolCallFinished` | → `ToolCallUpdate` | ✓ | ✓ |
| `PolicyDecision (Allow/Deny)` | — | ✓ | ✓ |
| `PolicyDecision (Ask)` | → `RequestPermissionRequest` | ✓ | ✓ |
| `PermissionResolved` | — (是 ACP request 的返回，不再发) | ✓ | ✓ |
| `LlmCallStarted` / `Finished` | — | ✓ | ✓ |
| `ContextCompressed` | — | ✓ | ✓ |

**约束**：投影必须**只读**——`defect-acp` 看到事件流后只发 wire 通知，不能"修改"主循环状态。所有状态变更都由主循环自己驱动，事件是结果而不是命令。

## 5. 流的形状（订阅模型）

主循环对每个 session 暴露一个事件流。订阅模型的具体技术选型（`tokio::sync::broadcast` 多消费者广播 / mpsc + 内部 fan-out / `Stream<Item = AgentEvent>`）留给 [Session 设计](./session.md)（待写）决定，本文只约束语义：

- 多消费者并发订阅
- 同一个 session 内事件**严格保序**（不允许 wire 看到的顺序与 storage 看到的顺序不同）
- 事件**不丢**（slow consumer 必须 backpressure 而不是 drop；丢事件会导致 storage 不一致）

## 6. 演进口子

- 新增变体直接加（`#[non_exhaustive]` 已经标了）。优先把语义独立的事件挖出来，避免在现有变体里塞 enum-of-enums。
- ACP 升级带来的字段变化由 `agent-client-protocol` 自己 bump 反映；我们的 `AgentEvent` 字段类型直接重新拉齐。
- 真要改 wire 协议（v0 不会，但 v1+ 可能加 HTTP/socket）时，只需要写新桥接层订阅同一条 `AgentEvent` 流——主循环代码不动。这正是"形式上解耦"的兑付。
