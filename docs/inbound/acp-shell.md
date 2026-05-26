# ACP Shell（Terminal）委托设计

ACP 的 `terminal/create` / `terminal/output` / `terminal/release` / `terminal/wait_for_exit` / `terminal/kill` 是**反向请求**（agent → client）：让 agent 把 shell 执行委托给客户端去做，而不是自己在 agent 进程内 `sh -c`。在 zed / vscode 这类有集成终端 UI 的客户端里，这条委托链让 agent 的 shell 操作在客户端的 PTY 里跑——用户能看到实时输出、能 Ctrl+C、客户端能做资源回收。

当前 `bash` 工具（[`tools-bash.md`]）是纯本地实现：`tokio::process::Command` spawn 到 `/bin/sh -c`。本文设计"shell 委托"模式——与 `fs` 委托（[`acp-fs.md`]）走同一架构，引入 [`ShellBackend`] trait 让工具层不感知后端。

设计原则：

1. **shell 委托是后端选择，工具实现不变**——改造后的 `bash` 工具只看 [`ShellBackend`] trait，由 [`defect-acp`] 在 session 创建时决定塞 [`LocalShellBackend`] 还是 [`AcpShellBackend`]，工具层完全不感知。
2. **能力协商是单向硬契约**——客户端在 `initialize` 里声明终端能力；agent 严格按清单选后端，不试探。
3. **保守降级**——客户端没声明完整 terminal 能力则整组退回本地。不混用（"create 走客户端、output 走本地"会导致状态撕裂）。
4. **工具层的工作区边界由 agent 自己守**——即便客户端有 PTY 隔离，agent 不依赖客户端的边界检查。
5. **v0 范围：非交互命令**——ACP terminal 协议是为交互式 PTY 设计的，但 v0 只用它跑非交互命令（`stdin=null`），与当前 `bash` 工具语义对齐。交互式 terminal 作为独立工具后续引入。

[`tools-bash.md`]: ../internal/tools-bash.md
[`acp-fs.md`]: ./acp-fs.md
[`ShellBackend`]: #3-shellbackend-抽象
[`LocalShellBackend`]: #4-localshellbackend
[`AcpShellBackend`]: #5-acpshellbackend
[`defect-acp`]: ./acp-bridge.md

## 1. 能力协商

### 1.1 ACP 的 terminal 能力

ACP `InitializeRequest` 携带 [`ClientCapabilities`]；v0 关注的 terminal 相关字段（当前 schema 0.13.2，未来若有变化以实际版本为准）：

```
ClientCapabilities
  └── terminal: Option<...>   // 客户端是否支持 terminal/* 反向请求
```

v0 的决策粒度是"全有或全无"——如果客户端声明支持 terminal，就认为它支持完整的 terminal 生命周期（create / output / release / wait_for_exit / kill）。不逐方法拆分——与 `fs` 的"read+write 全才委托"逻辑一致（[`acp-fs.md` §1.2]）。

> **注意**：若 ACP schema 当前版本尚未正式定义 terminal capabilities 字段，v0 采用保守策略——始终回退到 [`LocalShellBackend`]。等协议版本就绪后只需在 `decide_shell_mode` 里加一行判断，其余代码不变。

[`ClientCapabilities`]: https://docs.rs/agent-client-protocol-schema/0.13/agent_client_protocol_schema/v1/struct.ClientCapabilities.html
[`acp-fs.md` §1.2]: ./acp-fs.md#12-决策表

### 1.2 决策表

```
┌──────────────────────────────────────┬──────────────────────────────────┐
│ 客户端 terminal 能力                   │ defect-acp 装配的 ShellBackend   │
├──────────────────────────────────────┼──────────────────────────────────┤
│ 声明支持 terminal/*                   │ AcpShellBackend                 │
│ 未声明 / 字段缺失                      │ LocalShellBackend（降级）        │
└──────────────────────────────────────┴──────────────────────────────────┘
```

### 1.3 实现位置

```rust
// crates/acp/src/serve.rs
enum ShellMode { Local, Delegated }

fn decide_shell_mode(client_caps: &ClientCapabilities) -> ShellMode {
    // v0：保守——等 schema 正式定义 terminal capabilities 后再启用 Delegated
    ShellMode::Local
}
```

连接级状态存储：与 `FsMode` 并列——`initialize` handler 里决定，存到连接级状态，`session/new` handler 读取后构造对应的 `Arc<dyn ShellBackend>`。

## 2. ACP Terminal 反向请求：形状回顾

ACP wire 形态（基于 schema 0.13.2，`v1/client.rs`）：

### 2.1 `terminal/create`

```rust
pub struct CreateTerminalRequest {
    pub session_id: SessionId,
    pub command: String,
    pub args: Vec<String>,          // 默认空
    pub env: Vec<EnvVariable>,      // 默认空
    pub cwd: Option<PathBuf>,       // 绝对路径
    pub max_output_bytes: Option<u64>, // 输出字节上限，客户端负责截断
}

pub struct CreateTerminalResponse {
    pub terminal_id: TerminalId,
}
```

- **`command` + `args` 分离**——与 `bash` 工具的 `command: String`（走 `sh -c`）不同。`AcpShellBackend` 需要把 shell 行拆成 `command` + `args`。v0 方案：`command = "/bin/sh"`, `args = ["-c", user_command]`，保持与当前 `bash` 工具一致。
- **`cwd` 必须绝对**——agent 端先 `resolve_workspace_path` 校验边界，再塞进请求。
- **`max_output_bytes`**——默认不设（由客户端自行决定上限）；如果用户传了 `timeout_ms` 相关约束，不在此字段体现（超时由 agent 侧 `select!` 管理）。

### 2.2 `terminal/output`

```rust
pub struct TerminalOutputRequest {
    pub session_id: SessionId,
    pub terminal_id: TerminalId,
}

pub struct TerminalOutputResponse {
    pub output: String,
    pub truncated: bool,
    pub exit_status: Option<TerminalExitStatus>,
}
```

- **轮询模式**——agent 调 `terminal/output` 拿当前已累积的输出。可多次调用（中间取进度），或等到进程退出后一次性拿全量。
- `exit_status` 为 `None` 时表示进程还在跑。
- `truncated` 为 `true` 时表示客户端的输出已超过 `max_output_bytes` 上限被截断。

### 2.3 `terminal/wait_for_exit`

```rust
pub struct WaitForTerminalExitRequest {
    pub session_id: SessionId,
    pub terminal_id: TerminalId,
}

pub struct WaitForTerminalExitResponse {
    pub exit_status: TerminalExitStatus,
}
```

- **阻塞等待**——客户端在进程退出后才回复。agent 端在 `select!` 里 race `cancel.cancelled()` vs `wait_for_exit`。

### 2.4 `terminal/release`

```rust
pub struct ReleaseTerminalRequest {
    pub session_id: SessionId,
    pub terminal_id: TerminalId,
}
// response 无字段
```

- **幂等语义**——重复 release 同一个 terminal_id 不应报错。

### 2.5 `terminal/kill`

```rust
pub struct KillTerminalRequest {
    pub session_id: SessionId,
    pub terminal_id: TerminalId,
}
// response 无字段（ACP 用 impl_jsonrpc_request! 宏定义）
```

- **取消路径用**——`ctx.cancel` 触发时 agent 调 `kill` 而非直接 drop future。

## 3. `ShellBackend` 抽象

在 `defect-agent` 中定义，与 [`FsBackend`] 同层：

```rust
// crates/agent/src/shell.rs（新文件）

use std::path::PathBuf;
use futures::future::BoxFuture;

/// Shell 执行后端抽象。
///
/// v0 语义：每条命令创建独立 terminal，跑完后取输出再 release。
/// 不暴露"持久 terminal 跨 turn 复用"——等交互式 terminal 工具再做。
pub trait ShellBackend: Send + Sync {
    /// 创建 terminal 并启动命令。返回客户端分配的 terminal_id。
    fn create(
        &self,
        command: String,
        cwd: PathBuf,
    ) -> BoxFuture<'_, Result<TerminalId, ShellError>>;

    /// 轮询 terminal 当前输出。
    ///
    /// # 输出语义
    /// - 可多次调用（中间取进度）
    /// - `exit_status = Some(_)` 表示进程已退出
    fn output(
        &self,
        id: &TerminalId,
    ) -> BoxFuture<'_, Result<ShellOutput, ShellError>>;

    /// 阻塞等待 terminal 进程退出。
    fn wait_for_exit(
        &self,
        id: &TerminalId,
    ) -> BoxFuture<'_, Result<TerminalExitStatus, ShellError>>;

    /// 释放 terminal 资源。
    fn release(
        &self,
        id: &TerminalId,
    ) -> BoxFuture<'_, Result<(), ShellError>>;

    /// 强制终止 terminal 进程。
    fn kill(
        &self,
        id: &TerminalId,
    ) -> BoxFuture<'_, Result<(), ShellError>>;
}

#[derive(Debug, Clone)]
pub struct ShellOutput {
    pub text: String,
    pub truncated: bool,
    pub exit_status: Option<TerminalExitStatus>,
}

#[derive(Debug, Clone)]
pub struct TerminalExitStatus {
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TerminalId(String);

impl TerminalId {
    pub fn new(id: impl Into<String>) -> Self { Self(id.into()) }
}

#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("terminal not found: {0}")]
    NotFound(TerminalId),
    #[error("shell execution failed: {0}")]
    Execution(BoxError),
    #[error("backend error: {0}")]
    Backend(BoxError),
}
```

设计取舍：

- **`create` 不暴露 `args` / `env` / `max_output_bytes`**——v0 一律 `sh -c`，env 继承 agent 进程。与当前 `bash` 工具的字段取舍（[`tools-bash.md` §1]）完全一致。
- **`TerminalId` 是 newtype 而非 ACP schema 的 `TerminalId`**——解耦 agent 与 ACP 协议层。`LocalShellBackend` 用自己的 id 生成策略（进程 PID + 单调计数器），`AcpShellBackend` 映射到 schema 的 `TerminalId`。
- **`output` 与 `wait_for_exit` 分开**——前者是轮询（非阻塞），后者是阻塞。v0 的 `bash` 工具只用 `wait_for_exit`（跑完一次性取），`output` 留给后续流式输出需求。
- **不引入 `async fn`**——用 `BoxFuture` 与项目其他 trait 保持一致。

[`FsBackend`]: ../internal/tools-fs.md#2-fsbackend-抽象
[`tools-bash.md` §1]: ../internal/tools-bash.md#1-工具名片

## 4. `LocalShellBackend`

在 `defect-tools` 中实现，直接复用现有 `bash` 工具的进程管理逻辑：

```rust
// crates/tools/src/shell.rs（新文件，或扩展 bash 模块）

use std::collections::HashMap;
use std::sync::Mutex;

pub struct LocalShellBackend {
    terminals: Mutex<HashMap<TerminalId, TerminalState>>,
    cwd: PathBuf,     // session workspace root
}

struct TerminalState {
    child: tokio::process::Child,
    output_buf: Vec<u8>,
    truncated: bool,
    timed_out: bool,
}
```

实现要点：

- **`create`**：spawn `sh -c command`（同现有 [`tools-bash.md` §4.1]），`kill_on_drop(false)`（因为 lifecycle 由 backend 显式管理），存到 `HashMap`。
- **`output`**：从 `child.stdout` / `child.stderr` 读累积输出（1 MiB 上限），返回 `ShellOutput`。
- **`wait_for_exit`**：`child.wait().await`，返回 `TerminalExitStatus`。
- **`release`**：从 `HashMap` 移除，drop `Child`（触发 kill_on_drop 或已退出）。
- **`kill`**：`child.start_kill()`，然后从 `HashMap` 移除。

与现有 `bash` 工具的关系：改造 `BashTool`——去掉内部的 `Command::new("sh")`，改为持有 `Arc<dyn ShellBackend>` + `Arc<dyn FsBackend>`（后者用于路径校验），通过 backend 创建 terminal 并等待结果。

[`tools-bash.md` §4.1]: ../internal/tools-bash.md#41-进程生成

## 5. `AcpShellBackend`

完全镜像 [`AcpFsBackend`]（[`acp-fs.md` §3]）：

```rust
// crates/acp/src/shell.rs（新文件）

use agent_client_protocol::schema::{
    CreateTerminalRequest, KillTerminalRequest, ReleaseTerminalRequest,
    TerminalId as AcpTerminalId, TerminalOutputRequest, SessionId,
};
use agent_client_protocol::{Client, ConnectionTo};

pub struct AcpShellBackend {
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    workspace_root: PathBuf,
}

impl ShellBackend for AcpShellBackend {
    fn create(&self, command: String, cwd: PathBuf) -> BoxFuture<'_, Result<TerminalId, ShellError>> {
        Box::pin(async move {
            let abs_cwd = resolve_workspace_path(&self.workspace_root, &cwd)?;
            let req = CreateTerminalRequest::new(self.session_id.clone(), "/bin/sh")
                .args(vec!["-c".into(), command])
                .cwd(abs_cwd);
            let resp = self.cx.send_request(req).block_task().await
                .map_err(map_wire_error)?;
            Ok(TerminalId::new(resp.terminal_id.to_string()))
        })
    }

    fn output(&self, id: &TerminalId) -> BoxFuture<'_, Result<ShellOutput, ShellError>> {
        Box::pin(async move {
            let req = TerminalOutputRequest::new(
                self.session_id.clone(),
                AcpTerminalId::new(id.0.clone()),
            );
            let resp = self.cx.send_request(req).block_task().await
                .map_err(map_wire_error)?;
            Ok(ShellOutput {
                text: resp.output,
                truncated: resp.truncated,
                exit_status: resp.exit_status.map(|s| TerminalExitStatus {
                    exit_code: s.exit_code,
                    signal: None, // ACP schema 的 TerminalExitStatus 字段需要确认
                }),
            })
        })
    }

    // wait_for_exit / release / kill 同理
}
```

- **`ConnectionTo<Client>` 是 `Clone`**——它是 `Arc<...>` 的 newtype，`AcpShellBackend` 持一份即可。
- **`block_task`** 是 ACP SDK 的 await 模型。
- **工作区边界**：agent 自己调 `resolve_workspace_path` 校验（与 [`acp-fs.md` §4] 一致）。

[`AcpFsBackend`]: ./acp-fs.md#3-acpfsbackend
[`acp-fs.md` §3]: ./acp-fs.md#3-acpfsbackend
[`acp-fs.md` §4]: ./acp-fs.md#4-工作区边界agent-自我约束

## 6. `bash` 工具改造

### 6.1 变更点

当前 `BashTool` 的 `execute` 直接调 `tokio::process::Command::new("sh")` ([`tools-bash.md` §4])。改造后：

```rust
pub struct BashTool {
    schema: ToolSchema,
    shell: Arc<dyn ShellBackend>,
    fs: Arc<dyn FsBackend>,          // 新增：用于路径校验
    default_timeout_ms: u64,
    max_timeout_ms: u64,
}
```

`execute` 流程变为：

```text
1. 解析 args（command / workdir / timeout_ms）
2. fs.resolve_workspace_path(workdir) → 边界校验
3. shell.create(command, cwd) → 拿到 terminal_id
   ├── 超时：shell.kill(id) → ToolEvent::Completed(timeout 信息)
   ├── cancel：shell.kill(id) → ToolEvent::Failed(Canceled)
   └── 正常：shell.wait_for_exit(id) → 拿 exit_status
4. shell.output(id) → 拿输出文本
5. shell.release(id)
6. 组装 ToolEvent::Completed（与现有逻辑一致）
```

工具的行为与现有 `bash` 完全一致——LLM 感知不到后端差异。

### 6.2 不变的部分

- `safety_hint` 仍然返回 `Destructive`（[`tools-bash.md` §2]）
- `describe` 仍然产 `title="$ command"`、`kind=Execute`
- 输出格式仍然合并 stdout/stderr，1 MiB 上限
- 非零退出仍然是 `Completed`（不是 `Failed`）
- 超时 / 取消行为一致

[`tools-bash.md` §2]: ../internal/tools-bash.md#2-安全等级safety_hint
[`tools-bash.md` §4]: ../internal/tools-bash.md#4-execute

## 7. e2e 测试

放在 `crates/acp/tests/shell_delegation.rs`。结构与现有 `fs_delegation.rs` 一致。

| # | 场景 | 验证 |
| --- | --- | --- |
| 1 | 客户端声明 terminal 能力 → 跑 `bash` 工具 `echo hello` | 收到一条 `terminal/create` 反向请求；agent 正确拿到 output；`ToolCallFinished` |
| 2 | 同上 → 命令以非零退出 | `terminal/output` 返回 exit_code≠0；agent 产出 `Completed`（非 `Failed`） |
| 3 | 同上 → 超时 | agent 发 `terminal/kill`；`Completed` 含 timeout 信息 |
| 4 | 同上 → turn 中途 cancel | agent 发 `terminal/kill`；`TurnEnded = Cancelled` |
| 5 | 客户端未声明 terminal 能力 → 跑 `bash` | 不发 `terminal/*` 请求（退回 LocalShellBackend）；命令正常执行 |
| 6 | 委托模式下 workdir 越界 | agent 端 `resolve_workspace_path` 报错；**不**发 `terminal/create` |
| 7 | 委托模式下 client 返回 wire error | `ToolCallFinished` status=Failed；content 含 wire error 信息 |

## 8. 后续演进（不在 v0）

- **交互式 terminal 工具**——新增独立的 `terminal` 工具，暴露 `create` / `input` / `output` / `release`，让 LLM 能管理持久 PTY session（对位 ACP `terminal/create` 的交互式用途）。
- **流式输出**——当前 `bash` 工具单发 `Completed`（[`tools-bash.md` §4.2]）。委托模式下可以在 `wait_for_exit` 之前多次轮询 `output` 发 `Progress`，实现边跑边看。
- **后台任务**——terminal 生命周期跨 turn 存活，让 LLM 在后续 turn 里通过 `terminal/output` 查看后台任务进度。
- **argv 模式**——当客户端 terminal 能力就绪后，考虑让 `AcpShellBackend` 直接传 `command` + `args` 而非走 `sh -c`，从协议层规避 shell 注入。
- **env 透传**——当前 `bash` 工具不暴露 `env` 字段（[`tools-bash.md` §1]）。如果客户端 terminal 支持 env 隔离，可以扩展 `ShellBackend::create` 加 `env` 参数。
- **`session/load` 与 terminal**——恢复 session 时如果有未释放的 terminal，怎么处理？v0 不做 session/load 持久化（[`acp-bridge.md` §7]），延后。

[`tools-bash.md` §1]: ../internal/tools-bash.md#1-工具名片
[`tools-bash.md` §4.2]: ../internal/tools-bash.md#42-输出捕获策略
[`acp-bridge.md` §7]: ./acp-bridge.md#7-v0-不做的事明确划线

## 9. 落地步骤

1. **`crates/agent/src/shell.rs`**（新文件）：[`ShellBackend`] trait + `ShellError` + `ShellOutput` + `TerminalId` + `TerminalExitStatus`
2. **`crates/agent/src/lib.rs`**：`pub mod shell;`
3. **`crates/tools/src/shell.rs`**（新文件）：[`LocalShellBackend`] 实现
4. **`crates/tools/src/bash.rs`**：改造 `BashTool`——去掉内部 `Command::new("sh")`，注入 `Arc<dyn ShellBackend>` + `Arc<dyn FsBackend>`
5. **`crates/acp/src/shell.rs`**（新文件）：[`AcpShellBackend`] 实现（镜像 `acp/src/fs.rs`）
6. **`crates/acp/src/serve.rs`**：
   - `initialize` handler 加 `decide_shell_mode`（v0 始终 `Local`）
   - `session/new` handler 按 `ShellMode` 构造 `Arc<dyn ShellBackend>`，注入 session
7. **`crates/acp/tests/shell_delegation.rs`**：跑 §7 测试矩阵
8. **文档**：更新 `docs/internal/tools-bash.md` §8 的演进口子（把"ACP terminal 委托"从 v0 不做到已落地），更新 `TODO.MD`

## 10. 与现有文档的协同更新

落地时同步更新：

- **`docs/internal/tools-bash.md`**：§8 "v0 不做"中"ACP terminal 委托"项改为已完成；§4 加注说明后端可替换
- **`docs/inbound/acp-bridge.md`**：§2 表格加 `terminal/*` 方法行，指向本文
- **`docs/architecture.md`**：crate 依赖图加 `defect-acp → ShellBackend` 边
- **`TODO.MD`**：shell 委托项完成后翻到「已完成」
