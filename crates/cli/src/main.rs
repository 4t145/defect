//! `defect` 二进制入口。
//!
//! v0：根据显式 provider 配置装配 LLM provider，组装 [`DefaultAgentCore`]，
//! 以 stdio 启动 ACP server。

#![warn(clippy::indexing_slicing, clippy::unwrap_used)]
//!
//! Provider 选择：
//! 1. `--provider <name>` 命令行参数
//! 2. `DEFECT_PROVIDER` 环境变量
//! 3. 配置文件
//! 4. 默认 `echo`（无外部依赖，便于无凭证环境冒烟）
//!
//! 取值：`echo` | `anthropic` | `openai` | `deepseek` | `litellm`。
//! 凭证仍由各 provider 从 `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` /
//! `DEEPSEEK_API_KEY` / `LITELLM_API_KEY` 读取。
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::{
    EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerSse, McpServerStdio,
};
use clap::Parser;
use defect_acp::EchoProvider;
use defect_agent::hooks::builtin::BuiltinRegistry;
use defect_agent::llm::{
    LlmProvider, ModelCapabilityOverrides, ModelInfo, ProviderEntry, ProviderRegistry,
};
use defect_agent::policy::{
    AskWritesPolicy, DenyAllPolicy, OpenPolicy, ReadOnlyPolicy, SandboxPolicy,
};
use defect_agent::session::{
    AgentCore, DefaultAgentCore, StaticToolRegistry, ToolRegistry, TurnConfig,
};
use defect_config::{
    CliOverrides, HttpClientConfig, HttpProxyMode, HttpProxySettings, LoadConfigOptions,
    LoadedConfig, McpServerConfig as ConfigMcpServerConfig, ProviderKind as ConfigProviderKind,
    ProviderProtocol, ReasoningEffort as ConfigReasoningEffort, SandboxMode, load_dotenv_compat,
    parse_cli_override,
};
use defect_llm::protocol::openai_chat::ReasoningEffort as LlmReasoningEffort;
use defect_llm::provider::anthropic::{AnthropicConfig, AnthropicProvider};
use defect_llm::provider::bedrock::{BedrockConfig, BedrockProvider};
use defect_llm::provider::deepseek::{DeepSeekConfig, DeepSeekProvider};
use defect_llm::provider::openai::{OpenAiConfig, OpenAiProvider};
use defect_mcp::McpToolFactory;
use defect_storage::StorageObserver;
use defect_tools::{BashTool, EditFileTool, FetchTool, ReadFileTool, SearchTool, WriteFileTool};
use http::{HeaderName, HeaderValue};
use tracing_subscriber::EnvFilter;

mod hooks;

const BEDROCK_PROVIDER: &str = "bedrock";
const LITELLM_API_KEY_ENV: &str = "LITELLM_API_KEY";
const LITELLM_DEFAULT_BASE_URL: &str = "http://localhost:4000/v1";
const CUSTOM_OPENAI_DISPLAY_NAME: &str = "Custom OpenAI-compatible";
const CUSTOM_BEDROCK_DISPLAY_NAME: &str = "Amazon Bedrock";
const LITELLM_DISPLAY_NAME: &str = "LiteLLM Gateway";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cwd = env::current_dir()?;
    load_dotenv_compat(&cwd).map_err(|e| anyhow::anyhow!("dotenv load failed: {e}"))?;

    let cli = CliArgs::parse();
    let config = defect_config::load_config(LoadConfigOptions {
        cwd,
        cli: cli.to_overrides()?,
        ..LoadConfigOptions::default()
    })
    .map_err(|e| anyhow::anyhow!("config load failed: {e}"))?;
    init_tracing(config.effective.tracing.filter.as_deref())?;

    for warning in &config.warnings {
        tracing::warn!("{warning:?}");
    }

    let (registry, turn_config) = build_registry(&config).await?;

    let default_entry = registry.default_entry();
    tracing::info!(
        provider = %default_entry.provider().info().vendor,
        model = %turn_config.model,
        "starting defect ACP server on stdio"
    );

    let http_stack_config = build_http_stack_config(&config.effective.http)?;
    let http_client = defect_http::build_fetch_client_arc(&http_stack_config)
        .map_err(|e| anyhow::anyhow!("fetch http client init failed: {e}"))?;

    let tools = build_process_tools(&config);
    let storage = Arc::new(StorageObserver::new(default_sessions_root()?));

    let builtin_registry = BuiltinRegistry::defaults();
    let hook_engine = hooks::build_engine_arc(&config.effective.hooks, &builtin_registry)
        .map_err(|e| anyhow::anyhow!("hook engine build failed: {e}"))?;

    let agent = DefaultAgentCore::builder()
        .registry(registry)
        .process_tools(tools)
        .policy(build_policy(config.effective.sandbox.mode))
        .observe_session(storage.clone())
        .session_loader(storage)
        .session_tool_factory(Arc::new(McpToolFactory::with_default_servers(
            build_default_mcp_servers(&config),
        )))
        .config(turn_config)
        .http(http_client)
        .hook_engine(hook_engine)
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
    #[arg(long, env = "DEFECT_PROVIDER")]
    provider: Option<String>,

    /// Override the default model id. CLI flag wins over DEFECT_MODEL env.
    #[arg(long, env = "DEFECT_MODEL")]
    model: Option<String>,

    /// Additional dotted-path config overrides. May be repeated.
    #[arg(long = "config", value_name = "KEY=VALUE")]
    config_override: Vec<String>,
}

impl CliArgs {
    fn to_overrides(&self) -> anyhow::Result<CliOverrides> {
        let config_overrides = self
            .config_override
            .iter()
            .map(|spec| parse_cli_override(spec).map_err(|e| anyhow::anyhow!("{e}")))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(CliOverrides {
            provider: self.provider.as_deref().map(ConfigProviderKind::from),
            model: self.model.clone(),
            config_overrides,
        })
    }
}

async fn build_registry(
    config: &LoadedConfig,
) -> anyhow::Result<(Arc<ProviderRegistry>, TurnConfig)> {
    let http_config = build_http_stack_config(&config.effective.http)?;
    let entries = build_provider_entries(config, http_config).await?;
    let turn_config = config.effective.turn.clone();
    let registry = ProviderRegistry::new(entries, &turn_config.model)
        .map_err(|e| anyhow::anyhow!("provider registry init failed: {e}"))?;
    Ok((Arc::new(registry), turn_config))
}

async fn build_single_llm_provider(
    provider_kind: &ConfigProviderKind,
    config: &LoadedConfig,
    http_config: defect_http::HttpStackConfig,
) -> anyhow::Result<Arc<dyn LlmProvider>> {
    match provider_kind {
        ConfigProviderKind::Echo => Ok(Arc::new(EchoProvider::new()) as Arc<dyn LlmProvider>),
        ConfigProviderKind::Anthropic => Ok(Arc::new(
            AnthropicProvider::new(AnthropicConfig {
                api_key: None,
                api_key_env: config.effective.providers.anthropic.api_key_env.clone(),
                base_url: config.effective.providers.anthropic.base_url.clone(),
                http: http_config,
            })
            .map_err(|e| anyhow::anyhow!("anthropic provider init failed: {e}"))?,
        ) as Arc<dyn LlmProvider>),
        ConfigProviderKind::Openai => build_openai_provider(
            "openai",
            "OpenAI Chat Completions",
            config.effective.providers.openai.clone(),
            http_config,
        ),
        ConfigProviderKind::Deepseek => Ok(Arc::new(
            DeepSeekProvider::new(DeepSeekConfig {
                api_key: None,
                api_key_env: config.effective.providers.deepseek.api_key_env.clone(),
                base_url: config.effective.providers.deepseek.base_url.clone(),
                reasoning_effort: config
                    .effective
                    .providers
                    .deepseek
                    .reasoning_effort
                    .map(map_reasoning_effort),
                http: http_config,
            })
            .map_err(|e| anyhow::anyhow!("deepseek provider init failed: {e}"))?,
        ) as Arc<dyn LlmProvider>),
        ConfigProviderKind::Litellm => {
            build_litellm_provider(config.effective.providers.litellm.clone(), http_config)
        }
        ConfigProviderKind::Custom(name) => {
            let Some(provider) = config
                .effective
                .providers
                .get(&ConfigProviderKind::Custom(name.clone()))
            else {
                return Err(anyhow::anyhow!("missing [providers.{name}] configuration"));
            };
            // 协议默认值：bedrock / aws 段存在 → anthropic-messages；
            // 否则按 OpenAI Chat。这条派遣前没有兜底——`bedrock` 习惯写
            // `[providers.bedrock] aws = { ... }` 不显式标 protocol，被默认
            // 路由到 OpenAI builder 后报 "missing OPENAI_API_KEY"，与实际
            // 配置完全不沾边。详见此 commit。
            let protocol = provider.protocol.unwrap_or_else(|| {
                if name == BEDROCK_PROVIDER || provider.aws.is_some() {
                    ProviderProtocol::AnthropicMessages
                } else {
                    ProviderProtocol::OpenaiChat
                }
            });
            match protocol {
                ProviderProtocol::OpenaiChat => build_openai_provider(
                    name,
                    provider
                        .display_name
                        .as_deref()
                        .unwrap_or(CUSTOM_OPENAI_DISPLAY_NAME),
                    provider.clone(),
                    http_config,
                ),
                ProviderProtocol::AnthropicMessages => {
                    if name == BEDROCK_PROVIDER || provider.aws.is_some() {
                        build_bedrock_provider(name, provider.clone()).await
                    } else {
                        Err(anyhow::anyhow!(
                            "custom provider `{name}` uses protocol `anthropic-messages`, \
                             but only AWS Bedrock transport is implemented for custom providers"
                        ))
                    }
                }
            }
        }
    }
}

async fn build_provider_entries(
    config: &LoadedConfig,
    http_config: defect_http::HttpStackConfig,
) -> anyhow::Result<Vec<ProviderEntry>> {
    let default_kind = config.effective.cli.provider.clone();
    let default_provider =
        build_single_llm_provider(&default_kind, config, http_config.clone()).await?;
    let mut entries = vec![ProviderEntry::new(
        default_provider,
        entry_models(
            provider_config_for_kind(&config.effective.providers, &default_kind),
            Some(config.effective.turn.model.as_str()),
        ),
        provider_session_capabilities(config, &default_kind),
    )];

    for provider_kind in configured_entry_kinds(config) {
        if provider_kind == default_kind {
            continue;
        }
        let models = entry_models(
            provider_config_for_kind(&config.effective.providers, &provider_kind),
            None,
        );
        if models.is_empty() {
            continue;
        }
        let provider =
            build_single_llm_provider(&provider_kind, config, http_config.clone()).await?;
        entries.push(ProviderEntry::new(
            provider,
            models,
            provider_session_capabilities(config, &provider_kind),
        ));
    }

    Ok(entries)
}

/// 把全局 [`capabilities`] 与 `providers.<p>.capabilities` 合并，再投影成
/// agent 侧的 [`SessionCapabilitiesConfig`]。供每个 entry 自带——这样
/// session 跨 provider 切 model 时也能拿到正确的 capability 配置。
///
/// [`capabilities`]: defect_config::CapabilitiesConfig
fn provider_session_capabilities(
    config: &LoadedConfig,
    provider: &ConfigProviderKind,
) -> defect_agent::session::SessionCapabilitiesConfig {
    match provider {
        ConfigProviderKind::Anthropic => config
            .effective
            .providers
            .anthropic
            .capabilities
            .merge_into(config.effective.capabilities),
        ConfigProviderKind::Openai => config
            .effective
            .providers
            .openai
            .capabilities
            .merge_into(config.effective.capabilities),
        ConfigProviderKind::Deepseek => config
            .effective
            .providers
            .deepseek
            .capabilities
            .merge_into(config.effective.capabilities),
        ConfigProviderKind::Litellm => config
            .effective
            .providers
            .litellm
            .capabilities
            .merge_into(config.effective.capabilities),
        ConfigProviderKind::Echo => config.effective.capabilities,
        ConfigProviderKind::Custom(name) => config
            .effective
            .providers
            .get(&ConfigProviderKind::Custom(name.clone()))
            .map(|provider| {
                provider
                    .capabilities
                    .merge_into(config.effective.capabilities)
            })
            .unwrap_or(config.effective.capabilities),
    }
    .to_session_capabilities()
}

fn configured_entry_kinds(config: &LoadedConfig) -> Vec<ConfigProviderKind> {
    let mut kinds = vec![
        ConfigProviderKind::Anthropic,
        ConfigProviderKind::Openai,
        ConfigProviderKind::Deepseek,
        ConfigProviderKind::Litellm,
    ];
    kinds.extend(
        config
            .effective
            .providers
            .custom
            .keys()
            .cloned()
            .map(ConfigProviderKind::Custom),
    );
    kinds
}

fn provider_config_for_kind<'a>(
    providers: &'a defect_config::ProviderConfigs,
    kind: &ConfigProviderKind,
) -> Option<&'a defect_config::ProviderConfigFile> {
    providers.get(kind)
}

fn entry_models(
    provider: Option<&defect_config::ProviderConfigFile>,
    fallback_model: Option<&str>,
) -> Vec<ModelInfo> {
    let mut ids = Vec::new();
    if let Some(provider) = provider {
        if let Some(default_model) = &provider.default_model {
            ids.push(default_model.clone());
        }
        if let Some(models) = &provider.models {
            append_unique_model_ids(&mut ids, models.iter().cloned());
        }
    }
    if ids.is_empty()
        && let Some(fallback_model) = fallback_model
    {
        ids.push(fallback_model.to_string());
    }
    ids.into_iter().map(model_info_from_id).collect()
}

fn append_unique_model_ids(target: &mut Vec<String>, source: impl IntoIterator<Item = String>) {
    for model in source {
        if !target.iter().any(|existing| existing == &model) {
            target.push(model);
        }
    }
}

fn model_info_from_id(id: String) -> ModelInfo {
    ModelInfo {
        id,
        display_name: None,
        context_window: None,
        max_output_tokens: None,
        deprecated: false,
        capabilities_overrides: ModelCapabilityOverrides::default(),
    }
}

fn build_litellm_provider(
    provider: defect_config::ProviderConfigFile,
    http_config: defect_http::HttpStackConfig,
) -> anyhow::Result<Arc<dyn LlmProvider>> {
    let provider = ProviderDefaults {
        base_url: LITELLM_DEFAULT_BASE_URL,
        api_key_env: LITELLM_API_KEY_ENV,
    }
    .apply(provider);
    build_openai_provider("litellm", LITELLM_DISPLAY_NAME, provider, http_config)
}

async fn build_bedrock_provider(
    vendor: &str,
    provider: defect_config::ProviderConfigFile,
) -> anyhow::Result<Arc<dyn LlmProvider>> {
    let aws = provider.aws.unwrap_or_default();
    let provider = BedrockProvider::new(BedrockConfig {
        vendor: Some(vendor.to_string()),
        display_name: Some(
            provider
                .display_name
                .unwrap_or_else(|| CUSTOM_BEDROCK_DISPLAY_NAME.to_string()),
        ),
        base_url: provider.base_url,
        default_model: provider.default_model,
        models: provider.models.unwrap_or_default(),
        aws_profile: aws.profile,
        aws_region: aws.region,
    })
    .await
    .map_err(|e| anyhow::anyhow!("{vendor} provider init failed: {e}"))?;
    Ok(Arc::new(provider) as Arc<dyn LlmProvider>)
}

fn build_openai_provider(
    vendor: &str,
    display_name: &str,
    provider: defect_config::ProviderConfigFile,
    http_config: defect_http::HttpStackConfig,
) -> anyhow::Result<Arc<dyn LlmProvider>> {
    let provider = OpenAiProvider::new(OpenAiConfig {
        api_key: provider
            .api_key_env
            .as_deref()
            .and_then(|env| std::env::var(env).ok()),
        base_url: provider.base_url,
        organization: provider.organization,
        project: provider.project,
        vendor: vendor.to_string(),
        display_name: display_name.to_string(),
        api_key_env: provider.api_key_env,
        headers: provider_headers(provider.headers)?,
        capabilities_override: None,
        reasoning_effort: provider.reasoning_effort.map(map_reasoning_effort),
        chat_dialect: defect_llm::protocol::openai_chat::ChatDialect::OpenAi,
        http: http_config,
    })
    .map_err(|e| anyhow::anyhow!("{vendor} provider init failed: {e}"))?;
    Ok(Arc::new(provider) as Arc<dyn LlmProvider>)
}

struct ProviderDefaults {
    base_url: &'static str,
    api_key_env: &'static str,
}

impl ProviderDefaults {
    fn apply(
        self,
        mut provider: defect_config::ProviderConfigFile,
    ) -> defect_config::ProviderConfigFile {
        provider
            .base_url
            .get_or_insert_with(|| self.base_url.to_string());
        provider
            .api_key_env
            .get_or_insert_with(|| self.api_key_env.to_string());
        provider
    }
}

fn provider_headers(
    headers: std::collections::BTreeMap<String, String>,
) -> anyhow::Result<HashMap<HeaderName, HeaderValue>> {
    let mut parsed = HashMap::with_capacity(headers.len());
    for (name, value) in headers {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| anyhow::anyhow!("invalid provider header name `{name}`: {e}"))?;
        let header_value = HeaderValue::from_str(&value)
            .map_err(|e| anyhow::anyhow!("invalid provider header value for `{name}`: {e}"))?;
        parsed.insert(header_name, header_value);
    }
    Ok(parsed)
}

/// 把 `defect-config` 的 typed 配置翻译成 `defect_http::HttpStackConfig`。
///
/// `defect-config` 不直接依赖 `defect-http` 是为了保持 crate 单向依赖
/// （详见 `defect_config::HttpClientConfig` 注释），翻译动作放在 CLI 装配
/// 期最自然——同一份 stack config 三家 provider 共用，proxy URI 解析失败
/// 在这里集中报错。
fn build_http_stack_config(
    config: &HttpClientConfig,
) -> anyhow::Result<defect_http::HttpStackConfig> {
    use std::time::Duration;

    let mut stack = defect_http::HttpStackConfig::default();
    if let Some(ms) = config.total_timeout_ms {
        stack.total_timeout = if ms == 0 {
            None
        } else {
            Some(Duration::from_millis(ms))
        };
    }
    if let Some(retries) = config.transport_retries {
        stack.transport_retries = retries;
    }
    if let Some(ms) = config.initial_backoff_ms {
        stack.initial_backoff = Duration::from_millis(ms);
    }
    if let Some(ua) = &config.user_agent {
        stack.user_agent = Some(ua.clone());
    }
    stack.proxy = match config.proxy.mode {
        HttpProxyMode::FromEnv => defect_http::ProxyConfig::FromEnv,
        HttpProxyMode::Disabled => defect_http::ProxyConfig::Disabled,
        HttpProxyMode::Explicit => {
            defect_http::ProxyConfig::Explicit(parse_proxy_settings(&config.proxy.explicit)?)
        }
    };
    Ok(stack)
}

fn parse_proxy_settings(
    settings: &HttpProxySettings,
) -> anyhow::Result<defect_http::ProxySettings> {
    let parse_uri = |raw: &str, field: &str| -> anyhow::Result<http::Uri> {
        raw.parse::<http::Uri>()
            .map_err(|e| anyhow::anyhow!("invalid http.proxy.{field} `{raw}`: {e}"))
    };
    Ok(defect_http::ProxySettings {
        http_proxy: settings
            .http_proxy
            .as_deref()
            .map(|raw| parse_uri(raw, "http_proxy"))
            .transpose()?,
        https_proxy: settings
            .https_proxy
            .as_deref()
            .map(|raw| parse_uri(raw, "https_proxy"))
            .transpose()?,
        no_proxy: settings.no_proxy.clone(),
    })
}

fn build_process_tools(config: &LoadedConfig) -> Arc<dyn ToolRegistry> {
    let mut builder = StaticToolRegistry::builder()
        .insert(Arc::new(BashTool::from_config(
            &config.effective.tools.bash,
        )))
        .insert(Arc::new(ReadFileTool::from_config(
            &config.effective.tools.fs,
        )))
        .insert(Arc::new(WriteFileTool::new()))
        .insert(Arc::new(EditFileTool::new()));
    if config.effective.tools.fetch.enabled {
        builder = builder.insert(Arc::new(FetchTool::from_config(
            &config.effective.tools.fetch,
        )));
    }
    // 本地 `search` 工具（grep/glob）：仅看 `[tools.search].enabled`。与
    // hosted `web_search` capability 完全独立——两者可同时启用。
    if config.effective.tools.search.enabled {
        builder = builder.insert(Arc::new(SearchTool::from_config(
            &config.effective.tools.search,
        )));
    }
    Arc::new(builder.build())
}

fn build_default_mcp_servers(config: &LoadedConfig) -> Vec<McpServer> {
    config
        .effective
        .mcp
        .enabled_servers
        .iter()
        .filter_map(|name| {
            let server = config.effective.mcp.servers.get(name)?;
            Some(match server {
                ConfigMcpServerConfig::Stdio(server) => McpServer::Stdio(
                    McpServerStdio::new(name, PathBuf::from(&server.command))
                        .args(server.args.clone())
                        .env(
                            server
                                .env
                                .iter()
                                .map(|(name, value)| EnvVariable::new(name, value))
                                .collect(),
                        ),
                ),
                ConfigMcpServerConfig::Http(server) => McpServer::Http(
                    McpServerHttp::new(name, &server.url).headers(
                        server
                            .headers
                            .iter()
                            .map(|(name, value)| HttpHeader::new(name, value))
                            .collect(),
                    ),
                ),
                ConfigMcpServerConfig::Sse(server) => McpServer::Sse(
                    McpServerSse::new(name, &server.url).headers(
                        server
                            .headers
                            .iter()
                            .map(|(name, value)| HttpHeader::new(name, value))
                            .collect(),
                    ),
                ),
            })
        })
        .collect()
}

fn build_policy(mode: SandboxMode) -> Arc<dyn SandboxPolicy> {
    match mode {
        SandboxMode::ReadOnly => Arc::new(ReadOnlyPolicy),
        SandboxMode::AskWrites => Arc::new(AskWritesPolicy::new()),
        SandboxMode::Open => Arc::new(OpenPolicy),
        SandboxMode::DenyAll => Arc::new(DenyAllPolicy),
    }
}

fn default_sessions_root() -> anyhow::Result<std::path::PathBuf> {
    if let Ok(xdg_state_home) = env::var("XDG_STATE_HOME") {
        return Ok(std::path::PathBuf::from(xdg_state_home).join("defect/sessions"));
    }
    if let Ok(home) = env::var("HOME") {
        return Ok(std::path::PathBuf::from(home).join(".local/state/defect/sessions"));
    }
    Err(anyhow::anyhow!(
        "cannot resolve session storage root: neither XDG_STATE_HOME nor HOME is set"
    ))
}

fn map_reasoning_effort(value: ConfigReasoningEffort) -> LlmReasoningEffort {
    match value {
        ConfigReasoningEffort::None => LlmReasoningEffort::None,
        ConfigReasoningEffort::Minimal => LlmReasoningEffort::Minimal,
        ConfigReasoningEffort::Low => LlmReasoningEffort::Low,
        ConfigReasoningEffort::Medium => LlmReasoningEffort::Medium,
        ConfigReasoningEffort::High => LlmReasoningEffort::High,
        ConfigReasoningEffort::Xhigh => LlmReasoningEffort::Xhigh,
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

#[cfg(test)]
mod test;
