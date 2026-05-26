# 提案：用 `schemars` 生成 Tool 参数 schema

## 1. 动机

当前 [`Tool::schema`] 返回的 [`ToolSchema::input_schema`] 是 `serde_json::Value`，由各内置工具用 `json!({ ... })` 宏手写。例如 [`bash.rs:57`]：

```rust
input_schema: json!({
    "type": "object",
    "properties": {
        "command":   { "type": "string", "description": "..." },
        "workdir":   { "type": "string", "description": "..." },
        "timeout_ms":{ "type": "integer", "minimum": 1, "maximum": max_timeout_ms, "description": "..." }
    },
    "required": ["command"]
})
```

紧接着工具内部把 LLM 传来的 `serde_json::Value` 反序列化成自己的 args struct（[`bash.rs:92`]）：

```rust
#[derive(Debug, Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}
```

**两份字段定义**——一份给 LLM（schema），一份给工具（struct）——必须靠人工对齐：

- 字段名 / 类型 / 是否必填 / 默认值 / 边界（min/max/enum）
- `description` 可以只写在 schema 一侧，但前几条不行

漂移就是 LLM 安静地传错参数，或被 Rust 反序列化拒掉之后再让 LLM 重试一遍——没有编译期红线。

[`Tool::schema`]: ../../crates/agent/src/tool.rs
[`ToolSchema::input_schema`]: ../../crates/agent/src/tool.rs
[`bash.rs:57`]: ../../crates/tools/src/bash.rs
[`bash.rs:92`]: ../../crates/tools/src/bash.rs

## 2. 提案

让 `defect-agent` 依赖 [`schemars`]，所有内置工具的 args struct 加 `#[derive(JsonSchema)]`，schema 由 `schemars::schema_for!()` 在工具构造时一次性生成：

```rust
use schemars::{JsonSchema, schema_for};

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
struct BashArgs {
    /// The shell command to execute (passed to `sh -c` on unix, `cmd /C` on windows).
    command: String,

    /// Optional working directory. Must resolve inside the session cwd; ...
    #[serde(default)]
    workdir: Option<String>,

    /// Per-call timeout in milliseconds.
    #[serde(default)]
    #[schemars(range(min = 1, max = MAX_TIMEOUT_MS))]
    timeout_ms: Option<u64>,
}

let schema = schema_for!(BashArgs);
ToolSchema {
    name: "bash".into(),
    description: "...".into(),
    input_schema: serde_json::to_value(&schema).expect("schemars output is valid JSON"),
}
```

字段的 doc-comment 自动变成 schema 的 `description`；字段名 / 类型 / 必填性由 struct 形状直接决定；自定义约束（min/max/enum/pattern）通过 `#[schemars(...)]` 属性表达。

### 2.1 还需要解决的小问题

- **运行期参数**：`bash` 的 `timeout_ms.maximum` 来自 `BashToolConfig::max_timeout_ms`，是构造时才知道的。`schemars` 的 `#[schemars(range(max = ...))]` 接的是**编译期常量**——这种动态约束要么放弃在 schema 里表达（让工具运行时自己 clamp），要么在 `schema_for!` 之后用 `serde_json::Value` 修补——这是 schemars 路线下的一处真实损失。
- **嵌套 enum** 的 tag 形态要选对：默认 `serde(tag = "type")` 与 schemars 的 `untagged` 默认行为不一致；混用 ACP wire 类型时会踩坑（参考 [`project-toac-discriminator-bug`] 的同款陷阱）。
- **schema 版本**：schemars v1 默认产 Draft 2020-12，与 `tool-trait.md` 约定一致；不需要降版本。

[`schemars`]: https://docs.rs/schemars/1
[`project-toac-discriminator-bug`]: ../../../.claude/projects/-home-atlas-Github-defect/memory/project-toac-discriminator-bug.md

## 3. 评价

> 用户原始观点：**好处是严格、坏处是多一个依赖。**

我同意"严格"是核心好处，但**"多一个依赖"在本仓库已经不成立**——`schemars 1.2.1` 已经作为 [`agent-client-protocol-schema`] 的 transitive dep 进了 `Cargo.lock`（见 lockfile），把它从 transitive 提到 direct 不会增加编译图、不会引入新的 license / supply-chain 面。所以这条"坏处"其实是错的；真正的成本在别处。

下面把好坏拐点列细。

### 3.1 好处

1. **单一事实源（核心收益）**——args struct 的字段形状直接驱动 schema。改字段名 / 加字段 / 改默认值，编译器和 LLM wire 同步更新，不存在"忘了同步"的失败模式。
2. **doc-comment 复用**——`///` 注释自动变成 schema `description`。当前手写 `json!` 时 `description` 写在字符串字面量里，rustdoc 看不到；schemars 之后字段文档对 IDE / rustdoc / LLM 三处都生效，少一份重复。
3. **细约束更容易写对**——`#[schemars(range(min = 1, max = 100))]` / `#[schemars(regex = "...")]` / `#[schemars(length(min = 1))]`，比手写 JSON Schema 更不易拼错（拼错 `"minimum"` 写成 `"min"` 是 LLM 沉默 bug）。
4. **MCP 适配的反向收益**——`defect-mcp` 把远端 tool 包装成 [`Tool`] 时，远端 schema 已经是 JSON 形态，本地不需要 schemars；但 `defect-tools` 内置工具的形态向 schemars 收敛后，`Tool` trait 的契约 "input 是任意 JSON Schema" 不变，两侧并存无冲突。

### 3.2 坏处

1. **编译期常量限制**——前面 §2.1 提过：`bash` 的 `timeout_ms.maximum` 来自运行时配置，无法直接写 `#[schemars(range(max = ...))]`。这种 case 三种处理：
   - 放弃在 schema 里表达上限，描述里写一句"Default {default}; max {max}"，运行时再 clamp（最简单，也是当前 `bash.rs` 的隐式行为）
   - 在 `schema_for!` 后用 `Value` 修补 `properties.timeout_ms.maximum`（保留一处魔法，但比整段手写少）
   - 给该字段定义专用 newtype 用 `Validate` trait（过度设计）
   实际损失很小——内置工具里只有 `bash` 有这个问题，且影响仅限于 LLM 不知道精确上限（仍会被运行时拒绝）。
2. **`#[schemars(...)]` 的学习曲线**——属性宏的 DSL 需要查文档；对应的 JSON Schema 关键字（`pattern` / `format` / `enum` / `oneOf`）在两边都熟之前会走一阵弯路。规模上不大（内置工具就 4–6 个），一次性投资。
3. **生成的 schema 不一定 byte-for-byte 等价于现有手写版本**——字段顺序、`additionalProperties` 默认值、`$ref` 形态可能与现写的有差异。落地时必须人工 diff 每个工具的生成结果，确保 LLM 端不会因为 schema 形状变化而行为退化（这个工作量是一次性的，但不能跳过）。
4. **`schemars` 1.x 与 0.x 的 derive 不兼容**——本仓库 lockfile 里 `schemars 0.9.0` 与 `1.2.1` 共存，是因为不同 transitive 依赖锁了不同主版本。`defect-agent` 选 1.x，但要确认所有依赖 schemars 的内部 / 外部 crate 没有把 0.x 的 `JsonSchema` trait 隐式带进同一个泛型——一般不会，但落地时验证一下编译是否冲突。

### 3.3 拐点

- **工具数量**：现在 4–6 个内置工具，schemars 的"单一事实源"收益不算压倒性；到 10 + 时（fs 全家、bash、grep、glob、edit、apply_patch...）漂移概率非线性上升，schemars 的价值边际递增。
- **多 LLM provider 适配**：当前 `input_schema` 直接灌进 `CompletionRequest::tools`；不同 provider（Anthropic vs OpenAI）对 JSON Schema 子集的容忍度不同。手写时还能"按 provider 微调"；用 schemars 后想做 provider-specific 调整就得在生成的 `Value` 上后处理。如果未来真要做 provider 差异化，这条会成本反转。
- **MCP/远端工具占比**：内置工具占比下降时（绝大多数工具来自 MCP），schemars 改造的收益也下降——MCP 工具的 schema 不在我们手里。

## 4. 建议

**暂缓采用**，但在以下任一条件触发时重新评估：

1. 内置工具数量 ≥ 8，且其中至少 3 个 args struct 与 schema 出现过实际漂移（grep blame 找过修一边漏改另一边的提交）。
2. 有 PR 因为 LLM 给错参数被工具反序列化拒回去，且根因是 schema 与 struct 描述不一致（不是 LLM 自己看错文档）。
3. 单纯口味：如果团队达成"`json!` 写 schema 太丑"的共识，落地成本本来就不高（一周内能改完所有内置工具 + e2e 回归），随时可以推。

理由：当前主要矛盾不在 schema 一致性（4 个工具人工还能盯住）；`schemars` 的强项要在工具家族扩张到一定规模才显出来。**"多一个依赖"不是反对理由**——它已经在依赖图里。真正的反对理由是"现在改投入产出比不够"。

## 5. 不采用情况下的兜底措施

如果这条提案不上，仍要降低当前手写 schema 的漂移风险：

- 给每个工具的 `input_schema` 写**单元测试**：用 `serde_json::from_value::<Args>(example_request)` 跑通几条 LLM 风格的样例 args，至少能挡住"字段名拼错"和"required 漏标"两类典型错误。
- 在 [`tool-trait.md`] 里加一条规范：args struct 的字段名 / 必填性 / 类型必须与 `input_schema` 字面对应，PR review 时强制 diff 两侧。

[`agent-client-protocol-schema`]: https://docs.rs/agent-client-protocol-schema/0.13/
[`tool-trait.md`]: ../internal/tool-trait.md
