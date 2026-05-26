# ACP Slash Commands 设计

ACP 的 [Slash Commands](https://agentclientprotocol.com/protocol/slash-commands) 是 agent 向 client 发布**可调用命令清单**的机制。和 `fs/*`、`terminal/*` 不同——它**没有专属 JSON-RPC 方法**：

- 发布清单：复用 `session/update` 通知，多一个变体 [`SessionUpdate::AvailableCommandsUpdate`]（agent → client）
- 执行命令：客户端把 `/name args` 当作普通文本拼进 `session/prompt` 的 `prompt: Vec<ContentBlock>`——agent **自己**识别并路由

本文沉淀这套机制的形状与边界，**与 [`acp-shell.md`] / [`acp-fs.md`] 同模式**——先在 `defect-agent` 内部抽 [`SlashCommand`] 抽象，再让 `defect-acp` 投影对应事件，工具层 / turn 主循环不感知 wire。

[`SessionUpdate::AvailableCommandsUpdate`]: https://docs.rs/agent-client-protocol-schema/0.13/agent_client_protocol_schema/v2/enum.SessionUpdate.html
[`acp-shell.md`]: ./acp-shell.md
[`acp-fs.md`]: ./acp-fs.md
[`SlashCommand`]: #3-slashcommand-抽象

## 0. 设计原则

1. **命令是 session 级特性，不是协议方法**——只复用 `session/update` 与 `session/prompt`，不引入新方法。
2. **agent 是命令的权威**——客户端只是 UI（自动补全 / 渲染）。`/foo bar` 到底干什么完全由 agent 决定；客户端不识别 `name` 也不报错（按普通文本走）。
3. **a + b 双形态共存（v0）**（详见 §3）：
   - **a：prompt 重写型**——`/explain X` 替换成 `"Please explain X"` 后走正常 turn
   - **b：副作用型**——`/clear`、`/compact` 直接改 session 状态、不调 LLM、立刻 `TurnEnded`
   c（命令即工具调用）暂不做：用 a 等价表达即可，没必要再加一层。
4. **保守降级**——客户端没声明 / 不渲染 commands 不影响 agent；没有"必须有 commands 能力"的依赖位（schema 没有这个 capability bit，只能"发了不渲染"）。
5. **命令 v0 是静态注册的**——按 [`AgentCore`] / [`Session`] 装配时注入；不留"运行时增删"的口子（演进项 §8）。

## 1. ACP wire 形态回顾（schema 0.13.2）

### 1.1 命令清单的发布形态

```rust
// crates/agent-client-protocol-schema/src/v2/client.rs
pub enum SessionUpdate {
    // ...
    AvailableCommandsUpdate(AvailableCommandsUpdate),
}

pub struct AvailableCommandsUpdate {
    pub available_commands: Vec<AvailableCommand>,
    pub meta: Option<Meta>,
}

pub struct AvailableCommand {
    pub name: String,                          // 例如 "compact"
    pub description: String,                   // 人读的简介
    pub input: Option<AvailableCommandInput>,  // 输入提示
    pub meta: Option<Meta>,
}

pub enum AvailableCommandInput {
    Unstructured(UnstructuredCommandInput),    // schema 0.13.2 唯一变体
}

pub struct UnstructuredCommandInput {
    pub hint: String,  // 例如 "branch name" / "what to explain"
    pub meta: Option<Meta>,
}
```

- **`name`** 不带 `/` 前缀。客户端 UI 渲染时自己加。
- **`description`** 是命令的一句话简介（命令面板里给用户看的）。
- **`input.hint`** 仅在 `input` 为 `Some` 时表示"该命令期望一段自由文本输入"——客户端 UI 把它展示成 placeholder。`input = None` 表示"该命令不接受输入"，但 wire 上没有"拒绝带输入的调用"机制——agent 必须自己宽容地处理多余文本。

### 1.2 命令的调用形态

ACP 没有 `command/execute`；客户端把 `/name args` 当文本拼进 `session/prompt`：

```json
{
  "method": "session/prompt",
  "params": {
    "sessionId": "session-...",
    "prompt": [
      { "type": "text", "text": "/compact" }
    ]
  }
}
```

可能与其他 content block 共存：

```json
{
  "prompt": [
    { "type": "text",  "text": "/explain " },
    { "type": "image", "source": { ... } }
  ]
}
```

**v0 简化**：只识别 `prompt` 的**第一个 `ContentBlock::Text`**，且文本必须以 `/` 开头。其余 content block 当作命令的"附加上下文"原样传递给命令实现（命令决定怎么用——例如 `/explain` 把 image 一起塞进重写后的 prompt）。

### 1.3 时机

- ACP spec 原文："The Agent MAY send a list of available commands"——发不发由 agent 决定。客户端不应假设清单一定会到。
- v0 时机选择：**`session/new` 成功之后立刻发一次**；后续如果 session 状态变化（例如 `set_mode` 切换可用命令集）再发。
- 不在 `initialize` 阶段发——`AvailableCommandsUpdate` 是 `SessionUpdate` 变体，必须挂在某个 `session_id` 下。

## 2. AgentEvent 扩展

`AgentEvent` 新增一个变体，沿用既有"事件 → 投影 → wire"模式（见 [`event-model.md`]）：

```rust
// crates/agent/src/event.rs
pub enum AgentEvent {
    // ...
    /// 当前 session 可用的 slash 命令集发生变化（含首次发布）。
    /// 投影到 ACP `SessionUpdate::AvailableCommandsUpdate`。
    AvailableCommandsChanged {
        commands: Vec<SlashCommandInfo>,
    },
}
```

`SlashCommandInfo` 是 [`AvailableCommand`] 的内部 mirror（理由同 [`event-model.md` §1.2]——字段类型尽量复用 ACP 但变体由我们定义）：

```rust
// crates/agent/src/session/slash.rs（新文件）
pub struct SlashCommandInfo {
    pub name: String,
    pub description: String,
    pub input_hint: Option<String>,
}
```

这里**不直接复用 `AvailableCommand`**——它带 `_meta` / `Option<AvailableCommandInput>` 等 wire 元字段，事件流里只关心命令的"名+描述+是否取输入"三元信息；`defect-acp` 投影时再补 wire 元字段（与 `ContentBlock` 投成 `ContentChunk` 同款）。

[`event-model.md`]: ../internal/event-model.md
[`event-model.md` §1.2]: ../internal/event-model.md#12-但字段类型仍直接借-acp
[`AvailableCommand`]: https://docs.rs/agent-client-protocol-schema/0.13/agent_client_protocol_schema/v2/struct.AvailableCommand.html

### 2.1 投影规则

| `AgentEvent` 变体              | wire (ACP)                                              | storage | tracing |
| ------------------------------ | ------------------------------------------------------- | ------- | ------- |
| `AvailableCommandsChanged`     | → `SessionUpdate::AvailableCommandsUpdate`              | ✓       | ✓       |

`defect-acp::project` 新增 arm：

```rust
AgentEvent::AvailableCommandsChanged { commands } => {
    let acp_cmds = commands.into_iter().map(into_available_command).collect();
    Projection::Update(notification(
        session_id,
        SessionUpdate::AvailableCommandsUpdate(
            AvailableCommandsUpdate::new(acp_cmds)
        ),
    ))
}
```

`into_available_command` 把 `SlashCommandInfo` 拼成 `AvailableCommand`：

```rust
fn into_available_command(info: SlashCommandInfo) -> AvailableCommand {
    let mut cmd = AvailableCommand::new(info.name, info.description);
    if let Some(hint) = info.input_hint {
        cmd = cmd.input(AvailableCommandInput::Unstructured(
            UnstructuredCommandInput::new(hint),
        ));
    }
    cmd
}
```

## 3. `SlashCommand` 抽象

定义在 `defect-agent`，与 [`Tool`] / [`FsBackend`] / [`ShellBackend`] 同层：

```rust
// crates/agent/src/session/slash.rs

use agent_client_protocol::schema::ContentBlock;
use futures::future::BoxFuture;

/// 一条 slash 命令的实现。
///
/// 有两种执行形态（[`SlashOutcome`] 表达），同一个 trait 容纳：
/// - **prompt 重写型**：返回 [`SlashOutcome::RewritePrompt`]，主循环把
///   重写后的 `Vec<ContentBlock>` 当作正常用户输入跑 turn
/// - **副作用型**：返回 [`SlashOutcome::Handled`]，主循环不调 LLM、
///   直接以给定 `StopReason` 结束 turn
pub trait SlashCommand: Send + Sync {
    /// 命令清单里展示给客户端的元信息。
    fn info(&self) -> SlashCommandInfo;

    /// 执行命令。`args` 是命令名后面的所有原文（去掉 `/name` 与紧跟一个空格）；
    /// 若客户端没传输入则为空字符串。`extras` 是 prompt 中除"首个 text block"
    /// 以外的剩余 content block（image / resource_link 等）。
    ///
    /// # Errors
    ///
    /// 命令本身的语义错误（例如 `/model` 给了不存在的模型名）应通过返回
    /// [`SlashOutcome::Handled`] + AssistantText 解释，**不要**返回 Err——
    /// Err 留给"命令实现 panic / IO 灾难"等事故。
    fn execute<'a>(
        &'a self,
        ctx: SlashContext<'a>,
        args: &'a str,
        extras: Vec<ContentBlock>,
    ) -> BoxFuture<'a, Result<SlashOutcome, SlashError>>;
}

pub enum SlashOutcome {
    /// 把命令重写成等价的 prompt，主循环按正常 turn 继续。
    RewritePrompt { prompt: Vec<ContentBlock> },
    /// 命令已自行处理（产生了 AssistantText / 改了 history / 改了 config 等）；
    /// 主循环以给定 `StopReason` 结束当前 turn，不调 LLM。
    Handled { stop_reason: AcpStopReason },
}

#[derive(Debug, thiserror::Error)]
pub enum SlashError {
    #[error("slash command failed: {0}")]
    Internal(#[source] BoxError),
}
```

### 3.1 `SlashContext`

副作用型命令需要操作 session 状态。给 `execute` 传一份**最小**上下文：

```rust
pub struct SlashContext<'a> {
    pub session_id: &'a SessionId,
    pub cwd: &'a Path,
    pub history: &'a dyn History,
    /// 用于 `Handled` 路径下发 [`AssistantText`] / [`ToolCallStarted`] 等事件。
    /// 与 [`crate::tool::ToolContext`] 的 `events` 同款。
    pub events: &'a EventEmitter,
    /// 让命令能切模型（`/model`）/ 改 sampling（`/temp`）等。v0 命令清单
    /// 用不到，先暴露口子；落地批次只挑实际需要的字段。
    pub config: &'a RwLock<TurnConfig>,
}
```

**不暴露 `cancel` token**——副作用型命令应当短小、不阻塞；要长时间任务的命令请走 prompt 重写让 LLM 调工具。

**不暴露 `provider` / `tools`**——同理：要调 LLM 就 RewritePrompt。

### 3.2 `SlashRegistry`

进程级注册表（与 [`ToolRegistry`] 同层但分开存——slash 命令和 tool 不是一个东西）：

```rust
pub trait SlashRegistry: Send + Sync {
    fn list(&self) -> Vec<SlashCommandInfo>;
    fn get(&self, name: &str) -> Option<Arc<dyn SlashCommand>>;
}

pub struct StaticSlashRegistry { /* HashMap<String, Arc<dyn SlashCommand>> */ }
```

注入路径：`DefaultAgentCoreBuilder::slash_commands(Arc<dyn SlashRegistry>)`，与 `process_tools` 同层。Session 级（per-session）命令暂不做（演进项 §8）。

[`Tool`]: ../internal/tool-trait.md
[`FsBackend`]: ../internal/tools-fs.md#2-fsbackend-抽象
[`ShellBackend`]: ./acp-shell.md#3-shellbackend-抽象
[`ToolRegistry`]: ../internal/session.md

## 4. 主循环集成（`run_turn` 路由）

### 4.1 路由位置

在 [`DefaultSession::run_turn`] 进入 `TurnRunner::run` **之前**，先 peek `prompt` 是否是 slash 命令调用：

```rust
fn run_turn(&self, prompt: Vec<ContentBlock>) -> BoxFuture<'_, Result<StopReason, TurnError>> {
    Box::pin(async move {
        // ... 拿 cancel guard ...

        if let Some(call) = parse_slash_invocation(&prompt) {
            return self.dispatch_slash(call).await;  // §4.3
        }

        // 正常 turn——把 user prompt append 到 history，跑 TurnRunner
        let runner = TurnRunner { /* ... */ };
        runner.run(prompt).await
    })
}
```

理由：slash 路由是 session 级行为（要看 history、要发事件），不在 [`TurnRunner`] 的 LLM/tool loop 里——把 `TurnRunner` 留干净。

### 4.2 解析

```rust
struct SlashInvocation {
    name: String,
    args: String,
    extras: Vec<ContentBlock>,
}

fn parse_slash_invocation(prompt: &[ContentBlock]) -> Option<SlashInvocation> {
    let (head, rest) = prompt.split_first()?;
    let ContentBlock::Text(text) = head else { return None };
    let line = text.text.trim_start();
    let body = line.strip_prefix('/')?;
    // name 是首个空白前的 token；args 是剩余原文（保留内部空格 / 多行）
    let (name, args) = match body.find(char::is_whitespace) {
        Some(idx) => (&body[..idx], body[idx..].trim_start()),
        None => (body, ""),
    };
    if name.is_empty() {
        return None; // 单独的 "/"
    }
    Some(SlashInvocation {
        name: name.to_string(),
        args: args.to_string(),
        extras: rest.to_vec(),
    })
}
```

**留意**：
- `/  foo` 解析成 `name="", args="foo"`——单独的 `/` 不算命令，按普通用户输入跑 turn。
- 文本 leading whitespace 容忍（`  /help` 也算）。
- 未注册命令名（`/unknown`）→ 见 §4.4。

### 4.3 调度

```rust
async fn dispatch_slash(&self, call: SlashInvocation) -> Result<StopReason, TurnError> {
    self.events.emit(AgentEvent::TurnStarted);

    let cmd = match self.slash_commands.get(&call.name) {
        Some(c) => c,
        None => return self.handle_unknown_slash(&call.name).await, // §4.4
    };

    let ctx = SlashContext {
        session_id: &self.id,
        cwd: &self.cwd,
        history: self.history.as_ref(),
        events: self.events.as_ref(),
        config: &self.config,
    };

    match cmd.execute(ctx, &call.args, call.extras).await {
        Ok(SlashOutcome::Handled { stop_reason }) => {
            self.events.emit(AgentEvent::TurnEnded {
                reason: stop_reason,
                usage: Usage::default(),
            });
            Ok(stop_reason)
        }
        Ok(SlashOutcome::RewritePrompt { prompt }) => {
            // 用重写后的 prompt 跑正常 turn——TurnEnded 由 TurnRunner 发
            let runner = TurnRunner { /* ... */ };
            runner.run(prompt).await
        }
        Err(err) => Err(TurnError::Internal(BoxError::new(err))),
    }
}
```

**Handled 路径不走 `TurnRunner`**——它发 `TurnStarted` / `TurnEnded` 边界，命令实现期间发 `AssistantText` 反馈用户。Handled 不写 history（命令本身是 meta 操作，不应污染对话上下文）；如果某条命令需要写 history（罕见），它在 `execute` 里通过 `ctx.history.append(...)` 自己加。

### 4.4 未知命令

ACP 没规定"未知命令"该怎么响应。v0 行为：

```rust
async fn handle_unknown_slash(&self, name: &str) -> Result<StopReason, TurnError> {
    let msg = format!(
        "Unknown command: /{name}. Type /help to see available commands."
    );
    self.events.emit(AgentEvent::TurnStarted);
    self.events.emit(AgentEvent::AssistantText {
        content: ContentBlock::Text(TextContent::new(msg)),
    });
    self.events.emit(AgentEvent::TurnEnded {
        reason: AcpStopReason::EndTurn,
        usage: Usage::default(),
    });
    Ok(AcpStopReason::EndTurn)
}
```

**为什么不直接当普通文本走 LLM**：用户打了 `/foo` 一般是手误或客户端 UI 提示了不存在的命令；让 LLM 看到带 `/` 的输入会得到不可预期的回复（"我不能执行命令..."）。直接告诉用户找不到命令更友好。

### 4.5 取消

slash 命令的 cancel 与正常 turn 一致：`session/cancel` 触发 `CancellationToken`：

- **Handled 路径**：命令实现需要在内部 `select!` 里 race `cancel.cancelled()`（v0 的内置命令都是同步短任务，可以不主动 race，但 trait 层不强制）。被 cancel 时返回 `Handled { Cancelled }`。
- **RewritePrompt 路径**：cancel 行为完全等同正常 turn——已经在 `TurnRunner` 里覆盖。

## 5. v0 内置命令

v0 落地以下三条；其余演进项见 §8。

### 5.1 `/help`

| 字段          | 值                                                |
| ------------- | ------------------------------------------------- |
| `name`        | `help`                                            |
| `description` | `List available commands.`                        |
| `input_hint`  | `None`                                            |
| 形态          | b（副作用型）                                     |

实现：从 `SlashRegistry::list()` 拉清单，拼成多行 markdown，作为 `AssistantText` 发出，`Handled { EndTurn }`。

### 5.2 `/compact`

| 字段          | 值                                                |
| ------------- | ------------------------------------------------- |
| `name`        | `compact`                                         |
| `description` | `Compact conversation history to free context.`   |
| `input_hint`  | `None`                                            |
| 形态          | b（副作用型）                                     |

实现：调 `ctx.history.compact()`，发 `AgentEvent::ContextCompressed { tokens_before, tokens_after }`，再发一条 `AssistantText` 提示用户压缩结果，`Handled { EndTurn }`。

> v0 [`VecHistory::compact`] 是 no-op（[`history.rs`]）——`/compact` 在真压缩落地之前是占位。这条留在清单里是为了让客户端 UI 提前接入，避免日后再发一次 commands 列表更新。

[`VecHistory::compact`]: ../../crates/agent/src/session/history.rs
[`history.rs`]: ../../crates/agent/src/session/history.rs

### 5.3 `/clear`

| 字段          | 值                                                |
| ------------- | ------------------------------------------------- |
| `name`        | `clear`                                           |
| `description` | `Clear conversation history.`                     |
| `input_hint`  | `None`                                            |
| 形态          | b（副作用型）                                     |

实现：清空 `History`，发一条 `AssistantText("Conversation cleared.")`，`Handled { EndTurn }`。

> [`History`] trait v0 没有 `clear()` 方法。落地时给 trait 加：
> ```rust
> /// 清空所有消息。
> fn clear(&self);
> ```
> [`VecHistory`] 的实现就是 `self.inner.lock().clear()`。`clear` 与 `compact` 是兄弟操作，加在一起。

## 6. 装配

### 6.1 `DefaultAgentCore`

`DefaultAgentCoreBuilder` 增加 `slash_commands(...)`；不传则用 `StaticSlashRegistry::default_builtins()`（含 `/help` / `/compact` / `/clear`）。

```rust
let agent = DefaultAgentCore::builder()
    .provider(...)
    .process_tools(...)
    .slash_commands(Arc::new(StaticSlashRegistry::default_builtins()))
    .build();
```

`DefaultSession` 持有 `Arc<dyn SlashRegistry>`。

### 6.2 `defect-acp`：何时发 `AvailableCommandsUpdate`

两个时机，都在 `defect-acp` 桥接层处理（`defect-agent` 不关心"何时发"，只暴露 `AgentEvent::AvailableCommandsChanged`）：

1. **`session/new` / `session/load` 完成后**：handler 在 respond `NewSessionResponse` 之后，立即查 `session.list_slash_commands()` 拿到 `Vec<SlashCommandInfo>`，emit 一次 `AgentEvent::AvailableCommandsChanged`（投影后发 wire 通知）。
2. **session 主动改变命令集时**：例如未来 `/mode strict` 切换沙箱模式可能改可用命令；命令实现自己 emit `AvailableCommandsChanged`。

> 时机 1 不能写在 `AgentCore::create_session` 内部——那时 session 还没返回给 caller，`SessionObserver` 还没装上事件订阅。必须在 handler 拿到 session 之后、subscribe 已经建立之后再 emit。

`Session` trait 加最小一个方法：

```rust
trait Session {
    // ...
    /// 当前 session 可调用的 slash 命令清单（每条命令的元信息）。
    fn list_slash_commands(&self) -> Vec<SlashCommandInfo>;
}
```

`DefaultSession` 直接代理给 `self.slash_commands.list()`。

### 6.3 wire 类型再导出

`SlashCommandInfo` 在 `defect-agent::session` 模块下；`defect-acp` 投影时映射到 ACP 类型。无需把 ACP 类型泄露到 `defect-agent`。

## 7. e2e 测试

放在 `crates/acp/tests/slash_commands.rs`。

| #   | 场景                                                                | 验证                                                                                                          |
| --- | ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| 1   | `session/new` 后客户端应当看到 `AvailableCommandsUpdate` 通知       | 通知里至少含 `help` / `compact` / `clear` 三条；`description` 非空                                            |
| 2   | 客户端发 `session/prompt { prompt: [Text("/help")] }`               | 收到 `AgentMessageChunk`（含命令清单文本）；`PromptResponse::stop_reason = EndTurn`；不发 LLM 请求            |
| 3   | 客户端发 `/clear`                                                   | history 被清空（下一条 prompt 走 LLM 时 `messages` 不含上一轮）；`PromptResponse::stop_reason = EndTurn`      |
| 4   | 客户端发 `/compact`                                                 | 出 `AssistantText` 报告；`PromptResponse::stop_reason = EndTurn`；v0 不要求真压缩                              |
| 5   | 客户端发 `/unknown`                                                 | 出 `AssistantText("Unknown command: /unknown ...")`；`PromptResponse::stop_reason = EndTurn`；不发 LLM 请求    |
| 6   | 客户端发 `[Text("not a command")]`                                  | 走正常 turn（fake LLM 回一句话）；不被识别为 slash                                                            |
| 7   | 客户端发 `[Text("/explain "), Image{...}]`（假设有 `/explain` 命令）| 命令收到 `args = ""` 与 `extras = [Image{...}]`；按 RewritePrompt 模式跑正常 turn                              |
| 8   | turn 中途 `session/cancel` 期间 slash 命令在跑                      | `Handled { Cancelled }`；`PromptResponse::stop_reason = Cancelled`                                            |

测试用 fake `SlashRegistry` 对 #7 注册临时命令；#1–#6 用默认 builtin。

## 8. 演进项（v0 不做）

- **`/model <id>`、`/temp <f>`、`/sampling`** 等"改 session 配置"的副作用型命令——等 [`set_model`] / sampling 改动有更稳定的口子再加。
- **session 级动态注册**——MCP server 在 spec 层有 prompts/tools；如果上游某天加 "MCP slash commands"，会通过 [`SessionToolFactory`]-like 接口让 session 持有 per-session `SlashRegistry`。v0 只做进程级。
- **结构化输入**（`AvailableCommandInput::Structured`）——schema 0.13.2 只有 Unstructured；spec 演进给出 Structured 后再扩 [`SlashCommandInfo`]。
- **客户端能力位**——schema 没有 `slash_commands: bool`；agent 一律发 `AvailableCommandsUpdate`，客户端不识别就忽略（ACP 通知是 fire-and-forget）。
- **流式 RewritePrompt**——v0 是命令一次性返回完整 prompt；如果出现"边解析边补充上下文"的命令（`/grep keyword` 想边搜边塞 context），那时把 `SlashOutcome::RewritePrompt` 改成 stream / 引入 `RewritePromptStream` 变体。
- **命令调用记录到 storage**——slash 命令的"调用前 prompt"目前不入 history（Handled 形态）；`/clear` 之类操作的审计仅靠 `tracing` + `ContextCompressed`。如果 storage 层希望看到"用户在第 N 条用了 /clear"，要么加 `AgentEvent::SlashInvoked`，要么在 history 里写一条特殊 message——延后到 storage 设计稳定后决定。
- **取消的细粒度语义**——v0 内置命令短小，取消就 `Handled { Cancelled }`；将来若有长任务命令（例如 `/index` 重建项目索引），考虑给 `SlashContext` 加 `cancel: CancellationToken`。

[`set_model`]: ../inbound/acp-session.md

## 9. 落地步骤

> **本节列实现顺序，不在本 PR 写代码**——本文档先合，落地另起 PR。

1. **`crates/agent/src/event.rs`**：加 `AgentEvent::AvailableCommandsChanged { commands }`；新建 `SlashCommandInfo` 结构（放 `crates/agent/src/session/slash.rs`）。
2. **`crates/agent/src/session/slash.rs`**（新文件）：[`SlashCommand`] / [`SlashOutcome`] / [`SlashError`] / [`SlashContext`] / [`SlashRegistry`] / `StaticSlashRegistry`。
3. **`crates/agent/src/session.rs`**：`Session` trait 加 `fn list_slash_commands(&self) -> Vec<SlashCommandInfo>`。
4. **`crates/agent/src/session/history.rs`**：`History` trait 加 `fn clear(&self)`；`VecHistory` 实现。
5. **`crates/agent/src/session/builtins/`**（新模块）：内置 `/help` / `/compact` / `/clear` 三条命令，单文件一条。
6. **`crates/agent/src/session/default.rs`**：
   - `DefaultAgentCoreBuilder::slash_commands(...)` 注入；不传时用 builtins
   - `DefaultSession` 持有 `Arc<dyn SlashRegistry>`
   - `run_turn` 入口加 §4.1 路由；`dispatch_slash` 实现 §4.3
   - `list_slash_commands` 代理给注册表
7. **`crates/acp/src/project.rs`**：`AgentEvent::AvailableCommandsChanged` 投成 `SessionUpdate::AvailableCommandsUpdate`；新增 `into_available_command` 助手。
8. **`crates/acp/src/serve.rs`**：
   - `on_session_new` / `on_session_load` 在 `respond` 之后 emit `AgentEvent::AvailableCommandsChanged`（直接调 `events.emit`，无新通道）。注意 emit 时机要在桥接层 `events.subscribe()` 已经接好之后。
9. **`crates/acp/tests/slash_commands.rs`**：跑 §7 测试矩阵。
10. **文档**：
    - `docs/inbound/acp-bridge.md` §2 表格加 `session/update::AvailableCommandsUpdate` 行；§3 翻译表加 `AvailableCommandsChanged`
    - `docs/internal/event-model.md` §2 加新变体；§4 投影表加新行
    - `TODO.MD` 把"ACP slash commands"功能点加到任务表（inbound / P1）

## 10. 与现有文档的协同

- **`docs/inbound/acp-bridge.md`**：第 2 节方法表里 `session/update` 行已标 ✓；**`session/prompt`** 的 prompt 字段语义需补一句"prompt 首个 text content block 以 `/` 开头时由 slash 命令路由处理"。
- **`docs/internal/event-model.md`** §2 的 `AgentEvent` 变体清单与 §4 投影表新增一行。
- **`docs/internal/session.md`**（待写或已有）：`Session::list_slash_commands` 与"slash 路由发生在 `TurnRunner` 之外"两点要写进去。
- **`docs/inbound/acp-prompt.md`**：声明 prompt 路由策略——首个 text block 以 `/` 开头走 slash；不发 LLM。
