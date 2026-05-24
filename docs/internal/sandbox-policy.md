# Sandbox policy 设计

`SandboxPolicy` 是 `defect-agent` 主循环用来回答 **"这次工具调用能不能直接放行 / 该不该问用户 / 直接拒绝"** 的决策器。本文沉淀它的形状、与 ACP 权限请求的对接、与未来 OS 级沙箱的边界、以及 v0 的几个内置策略。

设计前提：[`tool-trait.md`](./tool-trait.md) §4 已经规定 **工具自己只回答"我想做什么"（`safety_hint`），不决定"能不能做"**；[`turn-loop.md`](./turn-loop.md) §3.3 规定决策由主循环串行驱动，[`event-model.md`](./event-model.md) §3.4 规定事件流上把"决策时刻"与"用户应答时刻"切成两条事件。这里把这三处都依赖、但还没人定的中间层补上。

## 1. 定位与术语

- **policy（决策）**：本文范围。一个**纯函数式**的判决：输入 `(tool_name, safety_hint, args, ctx)`，输出 `PolicyDecision`。不做 IO、不做副作用、不持有 LLM。
- **sandbox（隔离）**：OS 级的执行隔离（landlock / seatbelt / 容器 / 子进程权限降级）。**v0 不做**——工具直接在 agent 进程内跑。本文最后一节给出未来的 `ToolSandbox` trait 草图，主循环不需要为它现在改形状。

两者关系：policy 决定**要不要执行**，sandbox 决定**执行时给多大权限**。policy 是入口闸门、sandbox 是行进护栏，正交。v0 只做 policy。

## 2. 决策模型

```rust
#[non_exhaustive]
pub enum PolicyDecision {
    /// 直接放行，不打扰用户。
    Allow,
    /// 直接拒绝；主循环把"denied by policy"当作 tool_result 喂回 LLM。
    Deny,
    /// 需要用户确认。主循环发 ACP `session/request_permission`，
    /// 等用户在 [`Ask::options`] 里选一项。
    Ask(Ask),
}

pub struct Ask {
    /// 给客户端展示的选项列表。空向量等价于 [`PolicyDecision::Deny`]。
    pub options: Vec<AskOption>,
}

pub struct AskOption {
    pub id: PermissionOptionId,        // ACP wire id
    pub name: String,                  // 给用户看的标签，如 "Allow"
    pub kind: PermissionOptionKind,    // ACP 的 AllowOnce/AllowAlways/RejectOnce/RejectAlways
    /// 用户选了这一项之后该执行（`true`）还是拒绝（`false`）。
    /// 主循环按它分支：true 进 [`crate::session::Approved::Run`]，false 进 `Approved::Denied`。
    pub allows: bool,
}
```

要点：

- **`PermissionOption` 由 policy 组装**而不是主循环。理由：选项语义（哪些选项展示、文案怎么写、"Always" 的语义是"以后所有 fs 工具"还是"以后这个 fs.read"）和策略本身耦合，跨过 policy 让主循环组装就把语义打散了。
- **`AskOption::allows` 是 policy 的产物**而不是从 `kind` 推断。`AllowOnce` / `AllowAlways` 都是放行；`RejectOnce` / `RejectAlways` 都是拒绝；但未来可能出现"AllowReadOnly"（部分允许）这类语义，让 policy 自己说更稳。
- **`AllowAlways` / `RejectAlways` 的"记住"语义**：由 policy 自己持有可变状态（在 [`SandboxPolicy::record`](#33-record_outcome) 里更新），主循环只把 outcome 转给 policy 就完事，不感知"持久化授权"这件事。

## 3. SandboxPolicy trait

```rust
pub trait SandboxPolicy: Send + Sync {
    /// 决策一个工具调用。
    fn classify(&self, ctx: PolicyCtx<'_>) -> PolicyDecision;

    /// 用户应答 Ask 之后回写——给 policy 一个机会更新内部"已授权"表。
    /// 主循环在收到 [`PermissionResolution::Selected`] 之后、`Approved::Run/Denied` 入队
    /// 之前调用一次。`outcome.allows()` 由 policy 自己根据 option_id 判定。
    fn record(&self, ctx: PolicyCtx<'_>, outcome: RecordedOutcome);
}

pub struct PolicyCtx<'a> {
    pub tool_name: &'a str,
    pub safety_hint: SafetyClass,
    pub args: &'a serde_json::Value,
    /// 当前 session 的工作目录。路径白名单策略要用。
    pub cwd: &'a Path,
}

#[non_exhaustive]
pub enum RecordedOutcome {
    /// 用户选了某项，这一项在 policy 看来 allows = true/false。
    Selected { option_id: PermissionOptionId, allows: bool },
    /// 用户取消了 turn——policy 不需要做什么；提供这条 variant 主要是为了
    /// 让 trait 实现能审计取消频率。
    Cancelled,
}
```

设计取舍：

- **trait 不带 `&mut self`**：典型实现持有 `Mutex<HashMap<...>>` 或 `DashMap` 自己上锁，trait 暴露 `&self` 让多线程调用零摩擦。
- **`PolicyCtx::args: &Value` 借用而非拥有**：policy 一般只读；如果需要保留就 `clone`。借用形态省掉无谓 clone。
- **`record` 不返回 Result**：policy 内部记录失败（例如 `AllowAlways` 表写满）只是策略宽严问题，不上抛；用日志即可。
- **`PolicyCtx::cwd` 必填，没用就忽略**：trait 不开 `Option`，避免实现去判 `if let Some(cwd) = ...`。policy 不需要 cwd 的就纯函数地不读它。
- **演进字段往 `PolicyCtx` 加**（`#[non_exhaustive]`）；trait 方法签名稳定。

### 3.1 为什么 Allow/Deny 不直接对应 PermissionOptionKind

`PermissionOptionKind` 是 ACP **客户端 UI 提示**——告诉 client 用哪个图标、放哪个按钮位置。policy 关心"决策语义"，wire 关心"展示语义"，两者不必一一对应。`AskOption::kind` 直接借 ACP 类型仅是为了组装 `RequestPermissionRequest` 时少做一次映射，逻辑判定走 `allows`。

### 3.2 为什么 Ask 一律带 options 列表

考虑过 `Ask` 不带 options，让主循环根据 `safety_hint` 拼一组通用选项（例如 read-only 给 [Allow once, Reject]、destructive 给 [Allow once, Allow always, Reject once, Reject always]）。**否决**——这样 policy 想做"白名单单按钮"或"特殊工具加自定义 prompt"就没办法。让 policy 全权组装、`AskOption` 字段放开，弹性最大。

`AskOption` 的内容由 policy 自己写：

- 文案："Allow `bash` once" vs "Allow `bash` always"
- id：自定义，policy 自己用来 round-trip 解析（典型："allow_once" / "allow_always" / "reject_once" / "reject_always"）
- kind：ACP UI hint
- allows：是否最终放行

### 3.3 record_outcome 的契约

主循环现有伪代码（[`turn-loop.md`](./turn-loop.md) §3.3）只在 `PermissionResolution::Selected` 拿到 `option_id` 之后查 "allows / not allows"。新形状把这一步走 policy：

```rust
PolicyDecision::Ask(ask) => {
    // ask.options 装到 RequestPermissionRequest, 等用户回执
    let outcome = permissions.wait(id, cancel).await;
    match outcome {
        PermissionResolution::Selected { option_id } => {
            let allows = ask.options.iter()
                .find(|o| o.id == option_id)
                .map(|o| o.allows)
                .unwrap_or(false);
            policy.record(ctx, RecordedOutcome::Selected { option_id, allows });
            if allows { /* Approved::Run */ } else { /* Approved::Denied */ }
        }
        PermissionResolution::Cancelled => {
            policy.record(ctx, RecordedOutcome::Cancelled);
            return Ok(AcpStopReason::Cancelled);  // 见 turn-loop.md §3.3 与 §5
        }
    }
}
```

`record` 让"以 `AllowAlways` 选过 fs.read，下次 fs.read 直接 Allow"这种行为成为 policy 内部状态机的事，主循环不感知。

## 4. 与主循环的接口

`TurnRunner` 借用一个 `policy: &'a dyn SandboxPolicy`，跟 `history` / `tools` / `provider` 一样的形状（[`turn-loop.md`](./turn-loop.md) §2）。`DefaultSession` 在构造 `TurnRunner` 时传入 `Arc<dyn SandboxPolicy>` 的 deref。`DefaultAgentCore` 持有进程级 policy 实例（默认值 + 用户配置覆盖）；session 不再单独持有——v0 不做 per-session 策略覆写。

### 4.1 turn 主循环改动点（针对当前实现）

当前 `crates/agent/src/session/turn.rs`：

1. `stub_policy()` 函数 + `option_allows()` 函数 → 删除，全部走 `&dyn SandboxPolicy`。
2. `decide_permissions` 在 `PolicyDecision::Ask` 分支里直接用 `ask.options` 装 `RequestPermissionRequest`（v0 没接 ACP wire——目前 PermissionGate 只 wait outcome、不主动发 wire；wire 由 `defect-acp` 投影 `AgentEvent::PolicyDecision { Ask }` 时自己组装）。事件载荷需要带 options 让桥接层装 wire——见 §5。
3. `PermissionResolution::Cancelled` 当前误判为 `TurnError::Internal`——按本文 §3.3 改回 `Ok(AcpStopReason::Cancelled)`。

### 4.2 事件流上的形状

[`event-model.md`](./event-model.md) §2 现有：

```rust
PolicyDecision { id: ToolCallId, decision: PolicyDecision }
```

`PolicyDecision::Ask` 现在带 `Ask { options }`——`AgentEvent::PolicyDecision` 的 `decision` 字段直接复用 `crate::policy::PolicyDecision`，桥接层从 `Ask::options` 拼 ACP 的 `Vec<PermissionOption>`。`Allow` / `Deny` 维持空 payload。

事件 `Serialize` / `Deserialize` 影响：`AskOption` 实现 serde derive；`PermissionOptionId` / `PermissionOptionKind` 已经是 ACP 自带的可序列化类型。

## 5. v0 内置 policy

```rust
// crates/agent/src/policy.rs (新建模块)

pub fn open() -> Arc<dyn SandboxPolicy>;       // 一切 Allow，等价 v0 当前 stub。供测试 / dev mode 用。
pub fn read_only() -> Arc<dyn SandboxPolicy>;  // ReadOnly Allow；其余 Deny。
pub fn ask_writes() -> Arc<dyn SandboxPolicy>; // ReadOnly Allow；Mutating/Destructive/Network 都 Ask。
pub fn deny_all() -> Arc<dyn SandboxPolicy>;   // 一切 Deny。冒烟测试用。
```

行为定义：

| safety_hint  | `open` | `read_only` | `ask_writes` | `deny_all` |
| ------------ | ------ | ----------- | ------------ | ---------- |
| ReadOnly     | Allow  | Allow       | Allow        | Deny       |
| Mutating     | Allow  | Deny        | Ask          | Deny       |
| Destructive  | Allow  | Deny        | Ask          | Deny       |
| Network      | Allow  | Deny        | Ask          | Deny       |

`ask_writes` 的 `Ask` 选项约定：

```text
options = [
  AskOption { id="allow_once",   name="Allow once",   kind=AllowOnce,   allows=true  },
  AskOption { id="allow_always", name="Allow always", kind=AllowAlways, allows=true  },
  AskOption { id="reject_once",  name="Reject once",  kind=RejectOnce,  allows=false },
]
```

`AllowAlways` 的"记住"语义：在 policy 内部维护 `HashSet<String> /* tool_name */`，命中即直接 `Allow`。最小可用，不带 args 维度——args 维度由后续 path-whitelist policy 演进。

`RejectAlways` v0 不放——v0 没有"持久化拒绝"的需求；用户拒绝一次重新调起时还会再问。需要时再加。

### 5.1 默认值

`TurnConfig` 不直接持 policy；`DefaultAgentCoreBuilder::policy(Arc<dyn SandboxPolicy>)` 注入。builder 默认值：`ask_writes()`——既不让"读取代码库"打扰用户、也不让 LLM 直接写文件 / 跑 bash 不打招呼。

## 6. 取消语义

`PermissionResolution::Cancelled` = 用户取消整个 turn，与"用户拒绝该工具"是不同的事。处理：

1. `permissions.wait(id, cancel).await` 返回 `Cancelled`
2. policy 收到 `record(ctx, RecordedOutcome::Cancelled)`，可以审计但不更新授权表
3. turn 主循环返回 `Ok(AcpStopReason::Cancelled)`——见 [`turn-loop.md`](./turn-loop.md) §5。**不是 `TurnError`**

acp 桥接侧 ACP 规范要求：客户端发了 `session/cancel` 后，对所有 pending `request_permission` respond `RequestPermissionOutcome::Cancelled`。这一段在 acp-bridge 那边由 `tokio::select!` 处理（见 [`acp-bridge.md`](../inbound/acp-bridge.md) §4），policy 不参与。

## 7. v0 不做的事

- **路径白名单 / 命令白名单**：`PolicyCtx::cwd` 已经留好。具体的白名单策略（`fs.read` 限制在 cwd 子树、`bash` 不准 `rm -rf /` 等）放进 `path_whitelist` policy 实现，但**v0 不实现**——内置工具自己（`fs.write` / `bash`）会先做最小防护，policy 暂时不下沉到 args 维度。
- **配置驱动的复合 policy**：用户在配置里写"fs.read = allow, bash.* = ask"这种规则匹配引擎，留给 [`config.md`](./config.md) 配套实现。trait 已经能承载（`SandboxPolicy` 用户随便写一个），v0 内置只给上面四种。
- **OS 级 sandbox**：参见 §8。
- **per-session policy**：`AgentCore` 进程级一份。`session/new` 不带 policy 配置。需要时往 `Session::create_session` 入参加。

## 8. 演进口子：OS 级 sandbox

未来要给工具加 OS 级隔离时，预期形状：

```rust
pub trait ToolSandbox: Send + Sync {
    fn wrap_command(&self, cmd: Command, allows: SandboxAllows) -> Command;
    /// 给非 spawn 类的工具（fs / 网络）注入路径 / 端口白名单。
    fn fs_view(&self, allows: SandboxAllows) -> Arc<dyn FsView>;
}
```

注入位置：`ToolContext` 增加一个 `sandbox: &dyn ToolSandbox` 字段（`#[non_exhaustive]` 已经留好向后兼容）。`SandboxPolicy::classify` 的返回类型多带一个 `SandboxAllows` 用来告诉 sandbox 该开多大权限。

v0 主循环不实现这条链路；本文仅承诺：**SafetyClass → policy → sandbox** 这条链路的语义切分清晰，加 sandbox 不需要回过头改 policy 形状。

## 9. 落地节奏

落地按下列顺序，不另开 ticket：

1. 新建 `crates/agent/src/policy.rs`，写 `SandboxPolicy` trait + `PolicyDecision::Ask(Ask)` + `AskOption` + `RecordedOutcome` + 四个内置实现
2. `event::AgentEvent::PolicyDecision::decision` 字段类型从 `crate::event::PolicyDecision` 改成 `crate::policy::PolicyDecision`（语义未变，结构体变，迁移单点）
3. `turn::TurnRunner` 加 `policy: &'a dyn SandboxPolicy`，删除 `stub_policy` / `option_allows`，按 §4.1 改 `decide_permissions`
4. `DefaultAgentCore` builder 加 `policy(Arc<dyn SandboxPolicy>)`，默认 `ask_writes()`
5. e2e 测试：`open()` 跑当前的 `full_turn_with_one_tool_call`；新增一个 `ask_writes` + scripted "AllowOnce" 路径的 e2e（用 `Session::resolve_permission` 模拟客户端应答）

测试策略详见 [`docs/testing/e2e.md`](../testing/e2e.md)（待写）。
