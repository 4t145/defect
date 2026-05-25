use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use defect_agent::error::BoxError;
use defect_agent::llm::SamplingParams;
use defect_agent::session::{TurnConfig, TurnRequestLimit};
use toml::Value as TomlValue;

use crate::overrides::{
    build_cli_layer, merge_toml_values, remove_toml_path, remove_toml_table_key,
};
use crate::types::{
    AnthropicConfigFile, ConfigError, ConfigLayerEntry, ConfigLayerStack, ConfigSource, ConfigToml,
    ConfigWarning, DEFAULT_ANTHROPIC_MODEL, DEFAULT_DEEPSEEK_MODEL, DEFAULT_ECHO_MODEL,
    DEFAULT_OPENAI_MODEL, DeepSeekConfigFile, EffectiveConfig, LoadConfigOptions, LoadedConfig,
    OpenAiConfigFile, PROJECT_CONFIG_RELATIVE, PROJECT_LOCAL_CONFIG_RELATIVE, ProviderConfigs,
    ProviderKind, TracingConfig, USER_CONFIG_RELATIVE,
};

/// 加载并合并 `defect` 的有效配置。
///
/// precedence 为：`default < user < project < project-local < CLI`。
///
/// # Errors
///
/// 当用户配置路径无法解析、任一配置文件读盘失败、TOML 解析失败，或合并后的
/// 配置无法反序列化为强类型结构时返回 [`ConfigError`]。
pub fn load_config(opts: LoadConfigOptions) -> Result<LoadedConfig, ConfigError> {
    let cwd = canonicalize_or_original(&opts.cwd);
    let user_path = resolve_user_config_path(&opts)?;
    let repo_root = find_repo_root(&cwd);
    let project_path = repo_root
        .as_ref()
        .map(|root| root.join(PROJECT_CONFIG_RELATIVE));
    let project_local_path = repo_root
        .as_ref()
        .map(|root| root.join(PROJECT_LOCAL_CONFIG_RELATIVE));

    let mut layers = Vec::new();
    let mut warnings = Vec::new();

    let defaults = TomlValue::Table(Default::default());
    layers.push(ConfigLayerEntry {
        source: ConfigSource::Defaults,
        path: None,
        raw_toml: None,
        value: defaults.clone(),
    });

    let mut merged = defaults;

    if let Some(user_layer) = load_optional_layer(ConfigSource::User, user_path)? {
        merge_toml_values(&mut merged, &user_layer.value);
        layers.push(user_layer);
    }

    if let Some(project_layer) = load_optional_layer_opt(ConfigSource::Project, project_path)? {
        let (value, layer_warnings) =
            sanitize_shared_project_layer(project_layer.path.as_ref(), &project_layer.value);
        warnings.extend(layer_warnings);
        merge_toml_values(&mut merged, &value);
        layers.push(ConfigLayerEntry {
            value,
            ..project_layer
        });
    }

    if let Some(project_local_layer) =
        load_optional_layer_opt(ConfigSource::ProjectLocal, project_local_path)?
    {
        merge_toml_values(&mut merged, &project_local_layer.value);
        layers.push(project_local_layer);
    }

    if let Some(cli_layer) = build_cli_layer(&opts.cli)? {
        merge_toml_values(&mut merged, &cli_layer.value);
        layers.push(cli_layer);
    }

    let parsed: ConfigToml = merged
        .clone()
        .try_into()
        .map_err(|err| ConfigError::Invalid {
            path: PathBuf::from("<merged>"),
            message: err.to_string(),
        })?;
    let effective = build_effective_config(parsed);

    Ok(LoadedConfig {
        layers: ConfigLayerStack { layers },
        effective,
        warnings,
    })
}

fn build_effective_config(config: ConfigToml) -> EffectiveConfig {
    let provider = config.default.provider.unwrap_or_default();
    let provider_model = match provider {
        ProviderKind::Echo => Some(DEFAULT_ECHO_MODEL.to_string()),
        ProviderKind::Anthropic => config
            .providers
            .anthropic
            .as_ref()
            .and_then(|cfg| cfg.model.clone())
            .or_else(|| Some(DEFAULT_ANTHROPIC_MODEL.to_string())),
        ProviderKind::Openai => config
            .providers
            .openai
            .as_ref()
            .and_then(|cfg| cfg.model.clone())
            .or_else(|| Some(DEFAULT_OPENAI_MODEL.to_string())),
        ProviderKind::Deepseek => config
            .providers
            .deepseek
            .as_ref()
            .and_then(|cfg| cfg.model.clone())
            .or_else(|| Some(DEFAULT_DEEPSEEK_MODEL.to_string())),
    };
    let model = config
        .default
        .model
        .or(provider_model)
        .unwrap_or_else(|| DEFAULT_ECHO_MODEL.to_string());

    let mut turn = TurnConfig {
        model: model.clone(),
        ..TurnConfig::default()
    };
    if let Some(system_prompt) = config.turn.system_prompt {
        turn.system_prompt = Some(system_prompt);
    }
    if let Some(request_limit) = config.turn.request_limit {
        turn.request_limit = TurnRequestLimit::Adaptive {
            initial: request_limit,
            expand_on_progress: true,
        };
    }
    if let Some(compact_threshold_tokens) = config.turn.compact_threshold_tokens {
        turn.compact_threshold_tokens = Some(compact_threshold_tokens);
    }
    if let Some(max_llm_retries) = config.turn.max_llm_retries {
        turn.max_llm_retries = max_llm_retries;
    }
    if let Some(max_concurrent_tools) = config.turn.max_concurrent_tools {
        turn.max_concurrent_tools = max_concurrent_tools;
    }
    if turn.sampling == SamplingParams::default() {
        // 保持 default sampling 显式落在 effective config 中，方便后续扩字段。
    }

    EffectiveConfig {
        provider,
        model,
        turn,
        providers: ProviderConfigs {
            anthropic: config
                .providers
                .anthropic
                .map(|cfg| AnthropicConfigFile {
                    base_url: cfg.base_url,
                    model: cfg.model,
                })
                .unwrap_or_default(),
            openai: config
                .providers
                .openai
                .map(|cfg| OpenAiConfigFile {
                    base_url: cfg.base_url,
                    model: cfg.model,
                    organization: cfg.organization,
                    project: cfg.project,
                })
                .unwrap_or_default(),
            deepseek: config
                .providers
                .deepseek
                .map(|cfg| DeepSeekConfigFile {
                    base_url: cfg.base_url,
                    model: cfg.model,
                })
                .unwrap_or_default(),
        },
        tracing: TracingConfig {
            filter: config.tracing.filter,
        },
    }
}

fn sanitize_shared_project_layer(
    path: Option<&PathBuf>,
    value: &TomlValue,
) -> (TomlValue, Vec<ConfigWarning>) {
    let mut sanitized = value.clone();
    let mut warnings = Vec::new();
    let Some(path) = path.cloned() else {
        return (sanitized, warnings);
    };

    if remove_toml_path(&mut sanitized, &["default", "provider"]) {
        warnings.push(ConfigWarning::IgnoredProjectKey {
            path: path.clone(),
            key: "default.provider".into(),
            reason: "shared project config must not redirect model traffic",
        });
    }

    if let Some(providers) = sanitized
        .get_mut("providers")
        .and_then(TomlValue::as_table_mut)
    {
        for (provider_name, provider_value) in providers.iter_mut() {
            for key in ["base_url", "organization", "project", "api_key", "token"] {
                if remove_toml_table_key(provider_value, key) {
                    warnings.push(ConfigWarning::IgnoredProjectKey {
                        path: path.clone(),
                        key: format!("providers.{provider_name}.{key}"),
                        reason: "shared project config must not redirect endpoints or credentials",
                    });
                }
            }
        }
    }

    if remove_toml_path(&mut sanitized, &["tracing", "otlp"]) {
        warnings.push(ConfigWarning::IgnoredProjectKey {
            path,
            key: "tracing.otlp".into(),
            reason: "shared project config must not redirect observability sinks",
        });
    }

    (sanitized, warnings)
}

fn load_optional_layer(
    source: ConfigSource,
    path: PathBuf,
) -> Result<Option<ConfigLayerEntry>, ConfigError> {
    load_optional_layer_opt(source, Some(path))
}

fn load_optional_layer_opt(
    source: ConfigSource,
    path: Option<PathBuf>,
) -> Result<Option<ConfigLayerEntry>, ConfigError> {
    let Some(path) = path else {
        return Ok(None);
    };
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(ConfigError::Io {
                path,
                source: BoxError::new(err),
            });
        }
    };
    let value: TomlValue = raw.parse::<TomlValue>().map_err(|err| ConfigError::Parse {
        path: path.clone(),
        source: BoxError::new(err),
    })?;
    Ok(Some(ConfigLayerEntry {
        source,
        path: Some(path),
        raw_toml: Some(raw),
        value,
    }))
}

fn resolve_user_config_path(opts: &LoadConfigOptions) -> Result<PathBuf, ConfigError> {
    if let Some(xdg) = &opts.xdg_config_home {
        return Ok(xdg.join(USER_CONFIG_RELATIVE));
    }
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join(USER_CONFIG_RELATIVE));
    }
    if let Some(home) = &opts.home_dir {
        return Ok(home.join(".config/defect/config.toml"));
    }
    if let Ok(home) = env::var("HOME") {
        return Ok(PathBuf::from(home).join(".config/defect/config.toml"));
    }

    Err(ConfigError::Invalid {
        path: PathBuf::from("<env>"),
        message: "neither XDG_CONFIG_HOME nor HOME is set".into(),
    })
}

fn find_repo_root(cwd: &Path) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        let git_dir = dir.join(".git");
        if git_dir.exists() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

fn canonicalize_or_original(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
#[path = "loader/test.rs"]
mod test;
