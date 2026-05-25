//! `defect` 二进制入口。
//!
//! v0：根据显式 provider 配置装配 LLM provider，组装 [`DefaultAgentCore`]，
//! 以 stdio 启动 ACP server。
//!
//! Provider 选择：
//! 1. `--provider <name>` 命令行参数
//! 2. `DEFECT_PROVIDER` 环境变量
//! 3. 配置文件
//! 4. 默认 `echo`（无外部依赖，便于无凭证环境冒烟）
//!
//! 取值：`echo` | `anthropic` | `openai` | `deepseek`。
//! 凭证仍由各 provider 从 `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` /
//! `DEEPSEEK_API_KEY` 读取。

use std::env;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use clap::{Parser, ValueEnum};
use defect_acp::EchoProvider;
use defect_agent::llm::LlmProvider;
use defect_agent::session::{
    AgentCore, DefaultAgentCore, StaticToolRegistry, ToolRegistry, TurnConfig,
};
use defect_config::{
    CliOverrides, LoadConfigOptions, LoadedConfig, ProviderKind as ConfigProviderKind,
    parse_cli_override,
};
use defect_llm::provider::anthropic::{AnthropicConfig, AnthropicProvider};
use defect_llm::provider::deepseek::{DeepSeekConfig, DeepSeekProvider};
use defect_llm::provider::openai::{OpenAiConfig, OpenAiProvider};
use defect_tools::{BashTool, EditFileTool, ReadFileTool, WriteFileTool};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // .env 必须在 tracing_subscriber init 前加载——否则 RUST_LOG 这类 env 不生效。
    // 已在进程 env 里的同名变量优先（user-set wins），避免文件覆盖 shell 显式覆盖。
    load_env_file(Path::new(".env"));

    let cli = CliArgs::parse();
    let config = defect_config::load_config(LoadConfigOptions {
        cwd: env::current_dir()?,
        cli: cli.to_overrides()?,
        ..LoadConfigOptions::default()
    })
    .map_err(|e| anyhow::anyhow!("config load failed: {e}"))?;
    init_tracing(config.effective.tracing.filter.as_deref())?;

    for warning in &config.warnings {
        tracing::warn!("{warning:?}");
    }

    let (provider, turn_config) = build_provider(&config)?;

    tracing::info!(
        provider = %provider.info().vendor,
        model = %turn_config.model,
        "starting defect ACP server on stdio"
    );

    let tools: Arc<dyn ToolRegistry> = Arc::new(
        StaticToolRegistry::builder()
            .insert(Arc::new(BashTool::new()))
            .insert(Arc::new(ReadFileTool::new()))
            .insert(Arc::new(WriteFileTool::new()))
            .insert(Arc::new(EditFileTool::new()))
            .build(),
    );
    let agent = DefaultAgentCore::builder()
        .provider(provider)
        .process_tools(tools)
        .config(turn_config)
        .build();
    let agent: Arc<dyn AgentCore> = Arc::new(agent);

    defect_acp::serve(agent).await?;
    Ok(())
}

/// Headless agent over ACP/stdio.
#[derive(Debug, Parser)]
#[command(
    name = "defect",
    about = "Headless agent over ACP/stdio",
    long_about = "defect — headless agent over ACP/stdio.\n\n\
                  Auth env: ANTHROPIC_API_KEY / OPENAI_API_KEY / DEEPSEEK_API_KEY.\n\
                  Logging: RUST_LOG controls tracing-subscriber EnvFilter (default: info)."
)]
struct CliArgs {
    /// LLM provider to use. CLI flag wins over DEFECT_PROVIDER env and config.
    #[arg(long, value_enum, env = "DEFECT_PROVIDER")]
    provider: Option<ProviderKind>,

    /// Override the default model id. CLI flag wins over DEFECT_MODEL env.
    #[arg(long, env = "DEFECT_MODEL")]
    model: Option<String>,

    /// Additional dotted-path config overrides. May be repeated.
    #[arg(long = "config", value_name = "KEY=VALUE")]
    config_override: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ProviderKind {
    Echo,
    Anthropic,
    Openai,
    Deepseek,
}

impl CliArgs {
    fn to_overrides(&self) -> anyhow::Result<CliOverrides> {
        let config_overrides = self
            .config_override
            .iter()
            .map(|spec| parse_cli_override(spec).map_err(|e| anyhow::anyhow!("{e}")))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(CliOverrides {
            provider: self.provider.map(Into::into),
            model: self.model.clone(),
            config_overrides,
        })
    }
}

fn build_provider(config: &LoadedConfig) -> anyhow::Result<(Arc<dyn LlmProvider>, TurnConfig)> {
    let provider: Arc<dyn LlmProvider> = match config.effective.provider {
        ConfigProviderKind::Echo => Arc::new(EchoProvider::new()) as Arc<dyn LlmProvider>,
        ConfigProviderKind::Anthropic => Arc::new(
            AnthropicProvider::new(AnthropicConfig {
                api_key: None,
                base_url: config.effective.providers.anthropic.base_url.clone(),
            })
            .map_err(|e| anyhow::anyhow!("anthropic provider init failed: {e}"))?,
        ) as Arc<dyn LlmProvider>,
        ConfigProviderKind::Openai => Arc::new(
            OpenAiProvider::new(OpenAiConfig {
                api_key: None,
                base_url: config.effective.providers.openai.base_url.clone(),
                organization: config.effective.providers.openai.organization.clone(),
                project: config.effective.providers.openai.project.clone(),
                capabilities_override: None,
            })
            .map_err(|e| anyhow::anyhow!("openai provider init failed: {e}"))?,
        ) as Arc<dyn LlmProvider>,
        ConfigProviderKind::Deepseek => Arc::new(
            DeepSeekProvider::new(DeepSeekConfig {
                api_key: None,
                base_url: config.effective.providers.deepseek.base_url.clone(),
            })
            .map_err(|e| anyhow::anyhow!("deepseek provider init failed: {e}"))?,
        ) as Arc<dyn LlmProvider>,
    };

    Ok((provider, config.effective.turn.clone()))
}

impl From<ProviderKind> for ConfigProviderKind {
    fn from(value: ProviderKind) -> Self {
        match value {
            ProviderKind::Echo => Self::Echo,
            ProviderKind::Anthropic => Self::Anthropic,
            ProviderKind::Openai => Self::Openai,
            ProviderKind::Deepseek => Self::Deepseek,
        }
    }
}

fn init_tracing(filter: Option<&str>) -> anyhow::Result<()> {
    let default_filter = filter.unwrap_or("info,toac=warn");
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .try_init()
        .map_err(|e| anyhow::anyhow!("tracing init failed: {e}"))
}

/// 极简 `.env` 加载器：`KEY=VALUE` 一行一条，`#` 开头注释、空行跳过；
/// 支持外层 `"..."` / `'...'` 包裹去除。**已在进程 env 里的变量保留原值**，
/// 避免 .env 覆盖 shell 显式 export。读不到文件 / 解析失败仅 warn。
fn load_env_file(path: &Path) {
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => {
            eprintln!("warning: failed to read {}: {err}", path.display());
            return;
        }
    };

    for (lineno, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            eprintln!(
                "warning: {}:{} skipped: missing '=' in {line:?}",
                path.display(),
                lineno + 1
            );
            continue;
        };
        let key = key.trim();
        let value = strip_quotes(value.trim());
        if key.is_empty() {
            continue;
        }
        // 已显式 set 的不动；空字符串视作 unset。
        if env::var_os(key).is_some() {
            continue;
        }
        // SAFETY: 进入 main 前未起任何 spawn / signal handler，单线程读 env 安全。
        unsafe {
            env::set_var(key, value);
        }
    }
}

fn strip_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}
