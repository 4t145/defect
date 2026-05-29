# Defect 缺陷机器人

Defect 是一个高可配置性，高兼容性，节省资源的无头通用agent

Defect 本仓库不提供任何ui实现，你可以通过ACP协议，在您喜欢的任何前端中来调用它

Defect 的远期目标是：让您在冰箱上也能运行Agent

## 特点

1. **开放**，我们开放每一层可配置的配置，让Defect做到高度的可定制，可裁剪
2. **紧凑**，我们争取用最小的代价，实现最全面的功能，我们的可执行文件只有**12MB**，专为沙盒场景设计
3. **ACP 原生**，我们不绑定一个cli，而是鼓励您通过ACP来和Defect交互

## 安装
目前，本项目还在开发中，您可以通过在项目下执行`just install-force`来安装当前编译产物。

## 配置

Defect 使用 TOML 配置文件，按下面的优先级从低到高合并：

1. 内建默认值
2. 用户配置：`$XDG_CONFIG_HOME/defect/config.toml`，否则 `~/.config/defect/config.toml`
3. 项目共享配置：`<repo>/.defect/config.toml`（随仓库提交，团队共享）
4. 项目本地覆盖：`<repo>/.defect/config.local.toml`（机器本地，建议加入 `.gitignore`）
5. CLI override（`--config key.path=value`，以及 `--provider` / `--model`、`DEFECT_PROVIDER` / `DEFECT_MODEL`）

凭证（`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `DEEPSEEK_API_KEY` 等）只通过环境变量注入，不写入 TOML。出于安全考虑，仓库共享层（`config.toml`）中的 `default.provider`、`providers.*.base_url`、`http.proxy` 等出站/凭证相关字段会被忽略并产生 warning；如需本地覆盖请放到 `config.local.toml`。

一份可直接起步的最小配置（放在 `~/.config/defect/config.toml` 或 `<repo>/.defect/config.local.toml`）：

```toml
[default]
provider = "deepseek"
model = "deepseek-v4-pro"

[providers.deepseek]
default_model = "deepseek-v4-pro"
models = ["deepseek-v4-pro", "deepseek-v4-flash"]

# 全局 system prompt overlay，所有会话生效
[base_prompt]
text = "你是名为 defect 的助手"

# 项目级 prompt，默认会从 repo root 到 cwd 逐级拼接 AGENTS.md
[prompt]
file = "AGENTS.md"
```

更完整的字段说明、合并语义、安全边界与 MCP / HTTP 代理等高级配置，参见 [`docs/internal/config.md`](docs/internal/config.md)。

## CLI 可接入能力一览

下表记录 `defect-cli` 目前装配 / 暴露给 ACP 客户端的运行期能力。**支持** = 主二进制 `defect` 默认装配；**配置入口** = TOML 段落或 CLI flag；**实现位置** = lib 入口（`crates/cli/src/<module>.rs`），二次开发可单独替换某一项而不必 fork 整个 binary。

### Provider（LLM 后端）

| Provider | 协议 | API key env | 备注 | 实现 |
| --- | --- | --- | --- | --- |
| `echo` | — | — | 本地回声，无外部依赖；无凭证环境冒烟用 | `defect_acp::EchoProvider` |
| `anthropic` | Anthropic Messages | `ANTHROPIC_API_KEY` | 含 hosted web_search、extended thinking | `defect_llm::provider::anthropic` |
| `openai` | OpenAI Chat Completions | `OPENAI_API_KEY` | `reasoning_effort` 可下发 | `defect_llm::provider::openai` |
| `deepseek` | OpenAI 兼容 | `DEEPSEEK_API_KEY` | `reasoning_effort` 透传 | `defect_llm::provider::deepseek` |
| `litellm` | OpenAI 兼容 | `LITELLM_API_KEY` | 默认走 `localhost:4000/v1`，可本地代理多家上游 | `providers::build_litellm_provider` |
| `bedrock` | Anthropic Messages | AWS profile/region | 自定义 `[providers.bedrock]`，支持 `aws.profile` / `aws.region` | `defect_llm::provider::bedrock` |
| `<custom>` | OpenAI / Anthropic-Bedrock | 自填 `api_key_env` | `[providers.<name>]` + `protocol = "openai-chat" \| "anthropic-messages"` | `providers::build_single_llm_provider` |

切换：`--provider <name>` / `DEFECT_PROVIDER` / `[default].provider`；多 provider 同时装配后通过 ACP `set_model` 在客户端动态切。

### Tool（agent 自带工具）

| Tool | 安全级别 | 默认启用 | 配置段 | 实现 |
| --- | --- | --- | --- | --- |
| `bash` | Destructive | ✓ | `[tools.bash]` (`default_timeout_ms` / `max_timeout_ms`) | `defect_tools::BashTool` |
| `read_file` | ReadOnly | ✓ | `[tools.fs]` (`read_default_limit` / `read_max_limit`) | `defect_tools::ReadFileTool` |
| `write_file` | Mutating | ✓ | — | `defect_tools::WriteFileTool` |
| `edit_file` | Mutating | ✓ | — | `defect_tools::EditFileTool` |
| `fetch` | Network | ✓ | `[tools.fetch]` (`enabled`、超时、`max_response_bytes`、`html_to_markdown`) | `defect_tools::FetchTool` |
| `search` | ReadOnly | ✓ | `[tools.search]` (本地 grep/glob，与 hosted `web_search` 互不影响) | `defect_tools::SearchTool` |
| `web_search` (hosted) | Network | 按 capability | `[capabilities.web_search.mode]`，需 provider 支持 hosted | provider adapter |
| MCP 工具（任意） | 由 server 声明 | 按 server 声明 | `[mcp.servers.<name>]` + `[mcp].enabled_servers` | `defect_mcp::McpToolFactory` |

装配：`tools::build_process_tools` 拼进程内 tool registry；MCP 走 session-level factory。

### MCP transport

| Transport | 配置形态 |
| --- | --- |
| `stdio` | `transport = "stdio"`、`command` / `args` / `env` |
| `http` | `transport = "http"`、`url` / `headers` |
| `sse` | `transport = "sse"`、`url` / `headers` |

仅 `[mcp].enabled_servers` 列出的 server 会在 session 启动期被装配；不在白名单的 server 段即使存在也不暴露。

### Sandbox policy

| Mode | 行为 |
| --- | --- |
| `read-only` | 只放行 `ReadOnly` 工具 |
| `ask-writes` | 默认值；写类工具触发 ACP `RequestPermission` |
| `open` | 全开（含 destructive） |
| `deny-all` | 一律拒绝（用于干跑 / 离线 stub 场景） |

配置：`[sandbox].mode`；实现：`policy::build_policy`。

### Hook（v0：5 件套 Sync 拦截）

| 事件 | handler 形态 | 备注 |
| --- | --- | --- |
| `session_start` | builtin / command / prompt | append-only；引擎丢弃 `block` |
| `user_prompt_submit` | builtin / command / prompt | 支持 `block` / `patch.user_prompt` (prepend/append) / `append` |
| `pre_tool_use` | builtin / command / prompt | 支持 `block` / `patch.tool_args`；按 `match.tool` / `match.tool_glob` / `match.safety` 过滤 |
| `post_tool_use` | builtin / command / prompt | 仅 `append` 拼到 tool_result |
| `post_tool_use_failure` | builtin / command / prompt | 同上 |

handler 形态：

| Type | 说明 |
| --- | --- |
| `builtin` | 进程内 Rust handler；当前内置 `tracing-audit` / `redact-secrets` |
| `command` | argv 直接 spawn 或显式 shell（`sh` / `bash` / `pwsh` / `cmd` / `custom`）；stdin/stdout JSON、env 注入 |
| `prompt` | 调一次 LLM；不计入 turn budget；模板用 `{{key}}` 极简语法 |

详见 [`docs/internal/hooks.md`](docs/internal/hooks.md)。装配：`hooks::build_engine_arc`。

### Capability（hosted 能力裁决）

| Capability | 配置 | 决定项 |
| --- | --- | --- |
| `web_search` | `[capabilities.web_search.mode]`、`[providers.<p>.capabilities.web_search]` | 是否在 wire 上声明 hosted web_search tool |

Provider 级覆写在 session 启动期叠加；详见 [`docs/internal/capabilities.md`](docs/internal/capabilities.md)。

### HTTP 栈 / 代理

| 项 | 配置 |
| --- | --- |
| 总超时 | `http.total_timeout_ms`（`0` 表示无超时） |
| 传输重试 | `http.transport_retries`（默认 2） |
| 退避起点 | `http.initial_backoff_ms`（默认 200ms） |
| User-Agent | `http.user_agent` |
| 代理模式 | `http.proxy.mode` = `from-env` / `disabled` / `explicit` |
| 显式代理 | `http.proxy.{http_proxy,https_proxy,no_proxy}` |

实现：`http_stack::build_http_stack_config` —— 三家 provider 共用同一份 stack。

### Storage / 持久化

| 项 | 默认 | 备注 |
| --- | --- | --- |
| Session 根目录 | `XDG_STATE_HOME/defect/sessions` 或 `$HOME/.local/state/defect/sessions` | `defect_storage::StorageObserver` 同时实现观察者 + loader |

### Tracing / 日志

| 项 | 配置 |
| --- | --- |
| Filter | `RUST_LOG` env > `[tracing].filter` > `info,toac=warn` |
| 输出 | stderr，自动检测终端启用 ANSI |
| OTLP | `[tracing.otlp.endpoint]`（schema 已落地，subscriber 接入 P2） |

实现：`tracing_init::init_tracing`。

### CLI flag / env 一览

| 形态 | 名称 | 等价 TOML |
| --- | --- | --- |
| flag | `--provider <name>` | `[default].provider` |
| env | `DEFECT_PROVIDER` | 同上 |
| flag | `--model <id>` | `[default].model` |
| env | `DEFECT_MODEL` | 同上 |
| flag | `--config key.path=value` | 任意 TOML 字段 dotted-path 覆盖 |
| env | `RUST_LOG` | `[tracing].filter` |
| env | `XDG_CONFIG_HOME` / `HOME` | 配置文件查找位置 |
| env | `XDG_STATE_HOME` | session 持久化根目录 |
| env | `<provider>_API_KEY` | 凭证（不写入 TOML） |
| env | `.env` 自动加载 | `cwd` 下的 `.env` / `.env.local` |

详见 [`docs/internal/config.md`](docs/internal/config.md)。

## 二次开发/裁剪

`defect-cli` 已拆成 `lib` + `bin/cli`：所有装配 helper（provider / tool / hook / mcp / sandbox / http / storage / tracing）都在 `defect_cli::*` 模块导出，主 binary `src/bin/cli.rs` 只剩三段拼装逻辑。要做二次开发：

1. 把 `crates/cli/src/bin/cli.rs` 复制进你的 crate
2. 按需替换其中任意一步——例如换 provider 就跳过 `providers::build_registry`，自己组 `ProviderRegistry::new(...)`；换 hook 就传一份自己的 `Arc<dyn HookEngine>`
3. 其他 helper 直接 `use defect_cli::{tools, mcp_servers, paths, ...}` 复用

裁剪建议：
- 不需要 hook 系统？传 `Arc::new(NoopHookEngine)` 即可，引擎本身零开销
- 不需要 MCP？skip `session_tool_factory`
- 不需要持久化？skip `observe_session` / `session_loader`
- 想缩二进制体积？参考 `Cargo.toml` 里 `__test-servers` feature 的 gating 风格——把不需要的 provider / tool 用 feature 包起来，编译期裁掉
