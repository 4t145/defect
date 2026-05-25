//! 配置加载与合并。
//!
//! P1 负责把用户配置、项目配置、本地项目覆盖与 CLI override 收敛成一份
//! 可直接用于启动的强类型配置对象。

mod loader;
mod overrides;
mod types;

pub use loader::load_config;
pub use overrides::parse_cli_override;
pub use types::{
    AnthropicConfigFile, CliOverrides, ConfigError, ConfigLayerEntry, ConfigLayerStack,
    ConfigSource, ConfigWarning, DeepSeekConfigFile, EffectiveConfig, LoadConfigOptions,
    LoadedConfig, OpenAiConfigFile, ProviderConfigs, ProviderKind, TracingConfig,
};
