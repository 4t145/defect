# 会话存储与恢复日志

`defect-storage` 负责把 session 的可恢复状态落盘，并在 `session/load`
时重建 [`History`](./session.md#4-history)。本文定义存储日志的目标形态：
运行时仍然发布完整 [`AgentEvent`](./event-model.md)，但磁盘上的恢复日志不再
直接保存 `AgentEvent`，而是保存由 storage 侧投影出的最小可恢复记录。

## 1. 背景问题

当前实现中，storage 订阅 `AgentEvent` 后直接写入 `journal.jsonl`：

```text
AgentEvent -> RecordProjector -> StoredRecord -> journal.jsonl
```

resume 时再由 `ReplayState` 解释这些事件，折叠出 `Vec<Message>`。这能工作，
但把三类消费者绑在了同一种事件形态上：

- ACP 客户端订阅需要过程态：文本增量、tool 进度、权限请求、turn 结束。
- tracing / 审计需要诊断态：LLM retry、policy 决策、cache usage、压缩信息。
- resume 只需要最小可重建态：用户消息、助手消息、工具调用、工具结果、turn
  边界与必要 usage。

当我们想压缩会话日志时，直接压缩 `AgentEvent` 会和 ACP / tracing 的需求冲突。
正确边界是：`AgentEvent` 是运行时事件流；磁盘恢复日志是 storage 对事件流的
稳定投影。

## 2. 设计目标

- **恢复日志只保存可恢复语义**：能重建 `History` 与基础 turn 状态即可。
- **ACP 实时订阅不受影响**：`defect-acp` 继续消费完整 `AgentEvent`。
- **诊断信息不污染恢复日志**：retry、policy、progress 等走 tracing 或可选
  raw event 审计日志。
- **便于压缩**：可以用 `snapshot.json` + journal tail 恢复，不需要重放全量过程
  事件。
- **不兼容旧开发期数据**：当前仍处初期开发阶段，旧 `events.jsonl` 不作为恢复
  输入；需要保留诊断信息时另开 raw event 审计日志。

## 3. 文件布局

目标布局：

```text
<sessions_root>/<session_id>/
  meta.json
  journal.jsonl
  snapshot.json       # 可选，保存完整恢复态与 next_seq
  raw-events.jsonl    # 可选，调试 / 审计用，不参与 resume
```

`meta.json` 仍然保存 session 元数据：

- `schema_version`
- `session_id`
- `cwd`
- `mcp_servers`

`journal.jsonl` 保存恢复日志，每一行是一条 `StoredRecord`。`snapshot.json` 保存
某个时刻的完整恢复态与 `next_seq`，用于缩短回放距离并从 journal tail 继续恢复。

## 4. 存储记录

第一阶段建议直接以 `Message` 为恢复日志的核心单位。因为 `History` 的真实形态
就是 `Vec<Message>`，工具结果也已经表达为 `Role::User` 的
`MessageContent::ToolResult`，不需要再引入额外的 tool-result 批次类型。

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredRecord {
    pub schema_version: u32,
    pub seq: u64,
    pub record: SessionRecord,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRecord {
    TurnStarted,
    TurnEnded {
        reason: agent_client_protocol::schema::StopReason,
        usage: defect_agent::llm::Usage,
    },
    Message {
        message: defect_agent::llm::Message,
    },
    Snapshot {
        history: Vec<defect_agent::llm::Message>,
        turn_count: u64,
        last_turn_ended: bool,
    },
}
```

`TurnEnded.usage` 保留在恢复日志里。它不是重建下一轮请求的必要输入，但它是
session 级账本的一部分，包含 cache read token 等生产排障信息。后续如果
usage 膨胀或需要单独计费索引，可以再投影到 metrics 表，不影响恢复语义。

## 5. 投影器

storage 订阅 `AgentEvent` 后不再直接 append，而是先经过 `RecordProjector`：

```text
AgentEvent -> RecordProjector -> Vec<SessionRecord> -> journal.jsonl
```

投影器维护少量状态：

```rust
#[derive(Debug, Default)]
struct RecordProjector {
    current_assistant: Option<AssistantReplay>,
    pending_tool_results: Vec<MessageContent>,
}

#[derive(Debug, Default)]
struct AssistantReplay {
    text: String,
    thinking_text: String,
    thinking_signature: Option<String>,
    tool_uses: Vec<MessageContent>,
}
```

`AssistantReplay` 的职责和当前 `ReplayState` 里的同名累加逻辑一致，只是从
回放时前移到写入时。

## 6. 投影规则

| `AgentEvent` | `SessionRecord` |
| --- | --- |
| `UserPromptCommitted { content }` | 先 flush assistant 和 pending tool results，再输出 `Message { role: User, content }` |
| `AssistantThought { content }` | 累加到 `current_assistant.thinking_text` |
| `AssistantText { content }` | 累加到 `current_assistant.text` |
| `ToolCallStarted { id, name, fields }` | 累加为 `MessageContent::ToolUse { id, name, args }` |
| `ToolCallFinished { id, fields }` | 转成 `MessageContent::ToolResult`，放入 `pending_tool_results` |
| `TurnStarted` | 丢弃未完成 assistant 累加器，输出 `TurnStarted` |
| `TurnEnded { reason, usage }` | flush assistant，再 flush pending tool results，最后输出 `TurnEnded { reason, usage }` |
| `ToolCallProgress` | 不进入恢复日志 |
| `PolicyDecision` | 不进入恢复日志 |
| `PermissionResolved` | 不进入恢复日志 |
| `LlmCallStarted` / `LlmCallFinished` | 不进入恢复日志 |
| `ContextCompressed` | 不进入恢复日志；真正压缩应写 `Snapshot` |

### 6.1 flush assistant

`current_assistant` flush 成一条 `Role::Assistant` message：

```rust
Message {
    role: Role::Assistant,
    content: [
        Thinking { .. }?,  // 如果有 thinking 文本或 signature
        Text { .. }?,      // 如果有文本
        ToolUse { .. }*,
    ],
}
```

Thinking 必须排在 Text / ToolUse 前面，保持 provider 编码侧的历史回放顺序约束。
如果 content 为空，不写记录。

### 6.2 flush tool results

`pending_tool_results` flush 成一条 `Role::User` message：

```rust
Message {
    role: Role::User,
    content: [ToolResult { .. }, ...],
}
```

这保持了当前 OpenAI / Anthropic 内部历史语义：工具结果属于下一轮模型输入，
在内部消息模型中用 user role 表示。

## 7. 回放规则

新格式回放只解释 `SessionRecord`，不再理解 ACP 的 `ToolCallUpdateFields` 或
`ContentBlock` 细节：

```rust
impl ReplayState {
    fn apply(&mut self, record: SessionRecord) {
        match record {
            SessionRecord::TurnStarted => {
                self.turn_count = self.turn_count.saturating_add(1);
                self.last_turn_ended = false;
            }
            SessionRecord::TurnEnded { .. } => {
                self.last_turn_ended = true;
            }
            SessionRecord::Message { message } => {
                self.history.push(message);
            }
            SessionRecord::Snapshot {
                history,
                turn_count,
                last_turn_ended,
            } => {
                self.history = history;
                self.turn_count = turn_count;
                self.last_turn_ended = last_turn_ended;
            }
        }
    }
}
```

`snapshot.json` 是单独的文件级快照，不混入 `journal.jsonl`。读取时先加载
`snapshot.json`，再从 `next_seq` 开始回放 `journal.jsonl` 的 tail。

## 8. 写入路径

1. `StorageObserver` 持有 `RecordProjector`。
2. 每收到一个 `AgentEvent`，调用 `projector.project(event)`。
3. 对返回的每个 `SessionRecord` 调 `append_record`。
4. `seq` 只按实际写入的 `StoredRecord` 递增；被忽略的 `AgentEvent` 不占序号。
5. `snapshot.json` 记录 `next_seq`，读取时从该序号开始回放 journal tail。

读取侧优先读取 `snapshot.json`，不存在时回退到完整 `journal.jsonl` 回放。如果文件不
存在或内容不完整，直接返回存储错误；开发期不维护旧 `events.jsonl` 的兼容 reader。

## 9. Raw event 审计

恢复日志不承担完整审计职责。如果需要保留完整过程事件，提供独立开关写
`raw-events.jsonl`：

```text
AgentEvent -> raw-events.jsonl      # 调试 / 审计，可选
AgentEvent -> RecordProjector -> journal.jsonl
```

`raw-events.jsonl` 不参与 resume。这样它可以保留大体积 progress、retry、
permission 等诊断信息，也可以按时间或大小独立清理。

## 10. 测试计划

最小测试集：

- `append_record_then_replay_preserves_order`：验证 `journal.jsonl` 顺序与 seq。
- `project_user_and_assistant_messages`：`UserPromptCommitted + AssistantText + TurnEnded`
  投影后能恢复 user / assistant 两条 history。
- `project_tool_use_and_tool_result`：`ToolCallStarted + ToolCallFinished` 投影后能恢复
  assistant tool use 与 user tool result。
- `project_ignores_non_replay_events`：progress、policy、permission、llm call 不写
  journal。
- `turn_ended_flushes_pending_records_before_boundary`：turn 结束前 flush assistant 与
  tool results，再写 `TurnEnded`。
- `write_then_load_snapshot_roundtrips`：验证快照文件读写。
- `replay_state_uses_snapshot_and_journal_tail`：验证 snapshot + tail 的恢复路径。
- `snapshot_tail_rejects_sequence_gap`：验证 tail 序号连续性。

## 11. 落地顺序

建议按以下顺序实现：

1. 新增 `SessionRecord` / `StoredRecord`。
2. 新增 `RecordProjector`，先用单测覆盖投影规则。
3. 新增 `append_record` / `replay_records`，让 `replay_state` 直接回放 journal。
4. `StorageObserver` 从直接写事件切到 projector + `append_record`。
5. 实现 `snapshot.json` 写入与 journal tail 恢复。
6. 后续再做 journal 压缩与自动快照刷新。

第一阶段不需要改 `AgentEvent`、`Session` trait、`defect-acp`，只改
`defect-storage` 的持久化投影。
