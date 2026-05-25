# Config P1 Design

## Goal

把 `docs/internal/config.md` 定义的 P1 配置能力在当前代码库中收口为一个可运行闭环：

- `defect-config` 成为唯一的产品配置入口
- CLI 不再自己决定 precedence，也不再自己实现 `.env` 兼容加载
- `tools` / `sandbox` 配置既有 schema，也真正接入运行时装配路径
- 测试覆盖 `config.md` 中列出的 P1 行为与边界

## Non-Goals

本次不进入以下后续能力：

- managed config
- profile
- remote config
- strict mode 升级为 hard error
- 配置写回 API

## Architecture

### Responsibility Boundaries

配置职责拆分为三层：

1. CLI 只负责解析原始命令行参数，以及通过 `clap` 的 `env` 支持接入
   `DEFECT_PROVIDER` / `DEFECT_MODEL`
2. `defect-config` 负责 `.env` 兼容加载、配置文件发现、layer merge、warning 生成、
   有效配置构造
3. provider / tools / sandbox 消费显式传入的配置对象，不再自行承担产品配置选择逻辑

这样可以把 precedence、默认值和安全边界都集中在 `defect-config` 一处实现。

### Data Model

`LoadedConfig` 继续保留三部分：

- `layers`: 用于 debug / doctor / 测试断言
- `effective`: 用于运行时消费
- `warnings`: 用于向 CLI 与后续诊断功能暴露 ignored / unknown / deprecated 信息

`EffectiveConfig` 扩展为以下配置域：

- `cli`: CLI 自身消费的默认 provider / model 等选择结果
- `turn`: 映射到 `defect-agent::session::TurnConfig`
- `providers`: 各 provider 的显式配置
- `tools`: 内置工具默认行为，P1 先覆盖 `bash` / `fs`
- `sandbox`: 默认 sandbox mode
- `tracing`: tracing filter 与为 `otlp` 预留的 schema/warning 位点

不额外引入复杂的 “disabled entry” 状态机。P1 用 “sanitize 后的 layer + warning”
表达共享项目层中被忽略的字段即可。

## Merge Semantics

P1 保持 `docs/internal/config.md` 里定义的 merge 语义：

- 标量：高优先级覆盖低优先级
- 表：递归 merge
- 数组：整体替换，不做拼接
- CLI override：先转成一层虚拟 TOML，再按普通 layer merge

precedence 固定为：

```text
default < user < project < project-local < CLI
```

共享项目层 `config.toml` 继续走 denylist sanitize：

- 命中字段时忽略该字段
- 产出 warning
- 不让整个配置文件 hard fail

`config.local.toml` 不走这份 denylist。

## Warning Model

warning 枚举补齐为三类：

- `IgnoredProjectKey`
- `UnknownKey`
- `DeprecatedKey`

其中：

- `IgnoredProjectKey` 在共享项目层 sanitize 时产生
- `UnknownKey` 在每个文件层解析后、merge 前产生，并携带源文件路径
- `DeprecatedKey` 本次至少把类型与检测入口补齐；如果当前没有实际废弃字段，可以暂不产出实例

这能让文档中的 warning 模型与实现对齐，并为后续 strict mode 预留入口。

## Dotenv Strategy

`.env` 兼容逻辑迁入 `defect-config`，但调用时机仍由 CLI 控制。

原因是 `DEFECT_PROVIDER` / `DEFECT_MODEL` 通过 `clap` 的 `env` 机制参与参数解析，
所以 `.env` 必须在 `CliArgs::parse()` 前生效。

最终顺序为：

1. CLI 启动时先调用 `defect_config::load_dotenv_compat(cwd)`
2. 再执行 `CliArgs::parse()`
3. 再调用 `defect_config::load_config(...)`

`load_dotenv_compat` 只负责兼容读取 `cwd/.env` 并将尚未在进程环境中设置的键注入当前进程。
它不构成独立配置层。

## Schema Scope

本次补齐 `ConfigToml` / `EffectiveConfig` 所需的 P1 配置域：

- `[default]`
- `[turn]`
- `[providers.*]`
- `[tools.bash]`
- `[tools.fs]`
- `[sandbox]`
- `[tracing]`
- `tracing.otlp` 的 schema / sanitize / warning 位点

其中相对路径字段必须按“声明它的配置文件所在目录”解析；即使当前可用字段不多，
也要把解析入口设计在 layer-aware 的位置，而不是 merge 后再全局处理。

## Runtime Integration

这次不只补 schema，也要补消费方接线。

### CLI

`crates/cli/src/main.rs` 改为：

1. 调用 `defect-config` 的 dotenv 兼容入口
2. parse CLI
3. load config
4. 用 `LoadedConfig.effective` 装配 provider、turn、tools、sandbox、tracing

CLI 不再自己实现 precedence，也不再自己维护 `.env` 读取逻辑。

### Providers

provider 初始化继续只接收显式 provider config：

- base URL
- organization / project
- 其余显式 provider 字段

凭证仍由 provider 从标准环境变量读取。

### Tools

`tools` 的 P1 接线目标是把配置真正传到内置工具：

- `tools.bash`：默认 timeout、最大 timeout
- `tools.fs`：默认读取上限、最大读取上限

如现有工具构造函数还不支持注入配置，则扩展它们的配置对象或新增构造方式，但不改变工具对外语义。

### Sandbox

`sandbox.mode` 要进入 agent 启动路径，而不是只停留在 `EffectiveConfig` 中。
如果当前 sandbox 装配点尚未消费这一配置，则补齐到现有权限/策略装配路径中。

## Testing

`crates/config` 的白箱测试需要覆盖：

1. 用户配置单层加载成功
2. 用户 + 项目 merge，项目覆盖同名标量
3. 用户 + 项目 + 本地项目 merge，本地项目覆盖共享项目层
4. 递归表对象保留非冲突 key
5. CLI override 覆盖本地项目层
6. `DEFECT_PROVIDER` / `DEFECT_MODEL` 等价于 CLI 上层覆盖
7. 共享项目层设置 `providers.openai.base_url` 被忽略并产 warning
8. `config.local.toml` 设置 `providers.openai.base_url` 生效
9. 相对路径字段按声明文件目录解析
10. 数组字段整体替换而非拼接
11. 缺失配置文件不报错
12. TOML 语法错误带 path
13. unknown key warning 带源文件路径

可以补一条 golden-style 断言，同时覆盖：

- `LoadedConfig.layers`
- `LoadedConfig.warnings`
- `LoadedConfig.effective`

CLI 或运行时侧再补薄集成验证，确认：

- `.env` 在参数解析前生效
- `tools` / `sandbox` 配置进入实际装配路径

## File Impact

预计主要修改：

- `crates/config/src/types.rs`
- `crates/config/src/loader.rs`
- `crates/config/src/overrides.rs`
- `crates/config/src/loader/test.rs`
- `crates/config/src/lib.rs`
- `crates/cli/src/main.rs`
- 与 tools / sandbox 注入点直接相关的少量文件

## Completion Criteria

满足以下条件时，这个 P1 可视为闭环：

- `docs/internal/config.md` 第 15 节的最终决策都能在代码中对上
- `tools` / `sandbox` 同时具备 schema 与运行时接线
- `.env` 兼容逻辑迁入 `defect-config`
- 测试能证明 precedence、安全边界、warning、路径解析和运行时消费都生效
