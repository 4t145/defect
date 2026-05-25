# Defect 架构

## 1. 目标与定位

Defect 是一个**无头**（headless）通用 agent，强调：

- **高可配置性**：LLM provider、工具集、沙箱策略、存储后端都可替换
- **高兼容性**：通过 ACP 协议对接任意前端（Zed 等）；同时通过 MCP 接入第三方工具生态
- **优秀 harness**：主循环、事件流、工具调用语义统一抽象，避免每加一个 provider/tool 都要动核心
- **节省资源**：纯 Rust 实现，单二进制部署

本仓库**不提供任何 UI**。前端通过 ACP（Agent Client Protocol）与 defect 通信。

## 2. 关键架构决策

| 决策 | 选择 | 理由 |
| --- | --- | --- |
| 对外协议 | Zed 的 ACP（[agentclientprotocol.com](https://agentclientprotocol.com)） | 已有规范、Zed 等前端可直接对接，无需自造协议 |
| LLM provider 范围 | Anthropic + OpenAI 兼容接口 | 覆盖 Claude 与 OpenAI/DeepSeek/Qwen/本地 vllm 等绝大多数后端 |
| 工具扩展形态 | 内置 trait + MCP 双轨 | crate 内 trait 用于内置工具（性能/语义最优），MCP 用于第三方生态 |
| 沙箱（v0 范围） | 策略决策层（read-only / auto / full + 路径白名单） | OS 级隔离（landlock/seatbelt/seccomp）作为后续可插拔后端 |
| 会话持久化 | v0 即落盘，jsonl append-only，可 resume | 配 sqlite 等带索引存储为后续演进 |
| 命名 | crate 目录裸名，package 名加 `defect-` 前缀 | 目录简洁，发布命名空间明确 |

## 2.1 配置层级

当前配置层级为：

```text
default < user < project < project-local < CLI
```

对应位置：

- 用户配置：`$XDG_CONFIG_HOME/defect/config.toml` 或 `~/.config/defect/config.toml`
- 项目共享配置：`<repo>/.defect/config.toml`
- 项目本地覆盖：`<repo>/.defect/config.local.toml`

共享项目配置面向仓库内容，带安全限制；本地项目覆盖面向机器本地使用，默认不进 git。

## 3. Crate 拆分

```
crates/
├── agent/    → defect-agent     核心：Session/Turn/Event，LlmProvider/Tool trait
├── llm/      → defect-llm       Anthropic + OpenAI 兼容 provider
├── tools/    → defect-tools     内置工具：fs/edit/bash/grep/...
├── mcp/      → defect-mcp       MCP client，把外部 server 包装为 Tool
├── sandbox/  → defect-sandbox   权限策略 + 路径白名单
├── storage/  → defect-storage   会话持久化（jsonl）
├── config/   → defect-config    配置解析与合并
├── acp/      → defect-acp       ACP server 实现（协议适配，不含业务）
└── cli/      → defect-cli       bin: defect，组装入口
```

### 3.1 依赖图

```
                    defect-cli (bin)
                         │
            ┌────────────┼────────────┐
            ▼            ▼            ▼
       defect-acp   defect-config  defect-storage
            │            │            │
            └──────► defect-agent ◄────┘
                    ▲    ▲    ▲
              ┌─────┘    │    └─────┐
              │          │          │
        defect-llm  defect-tools  defect-mcp
                         │
                         ▼
                   defect-sandbox
```

### 3.2 依赖约束

- **`defect-agent` 是中心**，定义所有 trait 与事件类型，**不依赖**任何具体 provider/tool 实现
- **`defect-llm` / `defect-tools` / `defect-mcp` 互不感知**，三者都仅依赖 `defect-agent` 的 trait
- **`defect-acp` 是协议适配层**，把 `defect-agent` 暴露的事件流翻译成 ACP wire format，不参与业务逻辑
- **`defect-cli` 是唯一组装点**，把所有 provider/tool 注册到 `Session`，并启动 ACP server

这套约束保证：新增一个 LLM provider 或一个工具，不会引发跨 crate 修改；ACP 升级不会污染核心。

## 4. 命名与目录约定

- workspace 内 crate 目录使用**裸名**（`agent` 而非 `defect-agent`）；`Cargo.toml` 里 `package.name` 加 `defect-` 前缀
- bin crate 的目录名为 `cli`，package 名 `defect-cli`，二进制名 `defect`
- 子模块组织遵循 `submodule/` + `submodule.rs`，不使用 `mod.rs`（详见 [`coding-reference.md`](../claude.md) 第 10 条）

## 5. 外部依赖锁版

`Cargo.toml` `[workspace.dependencies]` 锁定核心依赖版本，子 crate 通过 `xxx.workspace = true` 引用：

| 依赖 | 版本 | 用途 |
| --- | --- | --- |
| `tokio` | 1 | 异步运行时 |
| `futures` | 0.3 | Stream/Sink 抽象 |
| `serde` / `serde_json` | 1 | 序列化 |
| `thiserror` / `anyhow` | 2 / 1 | 错误类型 |
| `tracing` / `tracing-subscriber` | 0.1 / 0.3 | 结构化日志 |
| `agent-client-protocol` | 0.12 | ACP 类型与 trait |
| `rmcp` | 1 | MCP Rust SDK |
| `reqwest` | 0.13 | HTTP 客户端 |

## 6. 后续设计文档索引

具体子系统的设计沉淀在 `docs/<域>/<feature>.md`，由 [`todo.md`](../todo.md) 追踪进度。当前未完成的核心设计文档：

- `docs/internal/event-model.md` —— 事件流模型
- `docs/internal/llm-trait.md` —— `LlmProvider` trait 形状
- `docs/internal/tool-trait.md` —— `Tool` trait 形状（含权限钩子时机）
- `docs/internal/turn-loop.md` —— 主循环编排
- `docs/inbound/acp-handshake.md` —— ACP 握手与能力协商
- `docs/outbound/llm-anthropic.md` —— Anthropic provider 落地

完整列表见 [`todo.md`](../todo.md)。
