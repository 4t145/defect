use std::fmt;
use std::path::PathBuf;

use defect_agent::error::BoxError;
use defect_agent::session::TurnConfig;
use serde::{Deserialize, Serialize};
use toml::Value as TomlValue;

pub(crate) const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-5";
pub(crate) const DEFAULT_OPENAI_MODEL: &str = "gpt-4o-mini";
pub(crate) const DEFAULT_DEEPSEEK_MODEL: &str = "deepseek-chat";
pub(crate) const DEFAULT_ECHO_MODEL: &str = "echo";

pub(crate) const USER_CONFIG_RELATIVE: &str = "defect/config.toml";
pub(crate) const PROJECT_CONFIG_RELATIVE: &str = ".defect/config.toml";
pub(crate) const PROJECT_LOCAL_CONFIG_RELATIVE: &str = ".defect/config.local.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    Echo,
    Anthropic,
    Openai,
    Deepseek,
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Echo => "echo",
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
            Self::Deepseek => "deepseek",
        };
        f.write_str(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    Defaults,
    User,
    Project,
    ProjectLocal,
    Cli,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigLayerEntry {
    pub source: ConfigSource,
    pub path: Option<PathBuf>,
    pub raw_toml: Option<String>,
    pub value: TomlValue,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigLayerStack {
    pub layers: Vec<ConfigLayerEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigWarning {
    IgnoredProjectKey {
        path: PathBuf,
        key: String,
        reason: &'static str,
    },
}

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: BoxError,
    },

    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: BoxError,
    },

    #[error("invalid config at {path}: {message}")]
    Invalid { path: PathBuf, message: String },

    #[error(transparent)]
    Source(#[from] BoxError),
}

#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub provider: Option<ProviderKind>,
    pub model: Option<String>,
    pub config_overrides: Vec<(String, TomlValue)>,
}

#[derive(Debug, Clone, Default)]
pub struct LoadConfigOptions {
    pub cwd: PathBuf,
    pub cli: CliOverrides,
    pub xdg_config_home: Option<PathBuf>,
    pub home_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub layers: ConfigLayerStack,
    pub effective: EffectiveConfig,
    pub warnings: Vec<ConfigWarning>,
}

#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub provider: ProviderKind,
    pub model: String,
    pub turn: TurnConfig,
    pub providers: ProviderConfigs,
    pub tracing: TracingConfig,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProviderConfigs {
    pub anthropic: AnthropicConfigFile,
    pub openai: OpenAiConfigFile,
    pub deepseek: DeepSeekConfigFile,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AnthropicConfigFile {
    pub base_url: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct OpenAiConfigFile {
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub organization: Option<String>,
    pub project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DeepSeekConfigFile {
    pub base_url: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TracingConfig {
    pub filter: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ConfigToml {
    #[serde(default)]
    pub(crate) default: DefaultSection,
    #[serde(default)]
    pub(crate) turn: TurnSection,
    #[serde(default)]
    pub(crate) providers: ProvidersSection,
    #[serde(default)]
    pub(crate) tracing: TracingSection,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct DefaultSection {
    pub(crate) provider: Option<ProviderKind>,
    pub(crate) model: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct TurnSection {
    pub(crate) system_prompt: Option<String>,
    pub(crate) request_limit: Option<u32>,
    pub(crate) compact_threshold_tokens: Option<u64>,
    pub(crate) max_llm_retries: Option<u32>,
    pub(crate) max_concurrent_tools: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ProvidersSection {
    pub(crate) anthropic: Option<AnthropicProviderSection>,
    pub(crate) openai: Option<OpenAiProviderSection>,
    pub(crate) deepseek: Option<DeepSeekProviderSection>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct AnthropicProviderSection {
    pub(crate) base_url: Option<String>,
    pub(crate) model: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct OpenAiProviderSection {
    pub(crate) base_url: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) organization: Option<String>,
    pub(crate) project: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct DeepSeekProviderSection {
    pub(crate) base_url: Option<String>,
    pub(crate) model: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct TracingSection {
    pub(crate) filter: Option<String>,
}
