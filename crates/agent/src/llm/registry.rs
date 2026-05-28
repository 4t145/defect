//! `ProviderRegistry`: catalog of configured providers + their model candidates.
//!
//! 用于 ACP 层向客户端暴露 `(provider, model)` 候选列表，并按 model id
//! 解析当前 turn 应该走哪个真实 provider。registry 本身**不实现**
//! [`LlmProvider`]——它是装配期落地的一份只读目录，session 在每次
//! `set_model` / `run_turn` 时按 model id 取出对应的真实 provider 跑。
//!
//! 设计要点：
//! - 每个 [`ProviderEntry`] 一份显式 `Vec<ModelInfo>`：CLI 装配期把
//!   `providers.<p>.default_model` 与 `providers.<p>.models` 翻成模型表，
//!   保证 ACP `list_models` 不依赖具体 adapter 的 `list_models` 网络调用。
//! - 模型 id 在 registry 内必须唯一（ACP `set_model` 只按 id 切）。
//! - 每个 entry 还携带 [`SessionCapabilitiesConfig`]——跨 provider 切换
//!   model 时 session 需要重新 resolve hosted capabilities。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::model::{ModelInfo, ProviderInfo};
use super::provider::LlmProvider;
use crate::session::SessionCapabilitiesConfig;

/// 一组 provider + 它公开的模型 id + 该 provider 的 session capability 配置。
#[derive(Clone)]
pub struct ProviderEntry {
    provider: Arc<dyn LlmProvider>,
    models: Vec<ModelInfo>,
    capabilities: SessionCapabilitiesConfig,
}

impl std::fmt::Debug for ProviderEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderEntry")
            .field("provider", &self.provider.info())
            .field("models", &self.models)
            .field("capabilities", &self.capabilities)
            .finish()
    }
}

impl ProviderEntry {
    #[must_use]
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        models: Vec<ModelInfo>,
        capabilities: SessionCapabilitiesConfig,
    ) -> Self {
        Self {
            provider,
            models,
            capabilities,
        }
    }

    #[must_use]
    pub fn provider(&self) -> &Arc<dyn LlmProvider> {
        &self.provider
    }

    #[must_use]
    pub fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    #[must_use]
    pub fn capabilities(&self) -> SessionCapabilitiesConfig {
        self.capabilities
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderRegistryError {
    #[error("provider registry requires at least one entry")]
    Empty,
    #[error(
        "duplicate model id `{model}` declared by providers `{first_provider}` and `{second_provider}`"
    )]
    DuplicateModel {
        model: String,
        first_provider: String,
        second_provider: String,
    },
    #[error(
        "default model `{model}` is not declared by any provider entry; \
         add it under one of the configured providers"
    )]
    UnknownDefaultModel { model: String },
}

/// 装配期落地的"provider 目录"。session 持有 `Arc<ProviderRegistry>`。
#[derive(Debug)]
pub struct ProviderRegistry {
    entries: Vec<ProviderEntry>,
    /// model id → entries 索引。
    model_index: HashMap<String, usize>,
    /// 默认 (provider, model) 对应的 entries 索引 + entry.models 索引。
    default: (usize, usize),
}

impl ProviderRegistry {
    /// 单 provider 单 model 的便捷构造。测试 / EchoProvider / `provider()`
    /// builder 入口走此路径——保持 `ProviderRegistry::new` 的不变量校验
    /// （非空 + default_model 必须落在某 entry）成立的最小形态。
    #[must_use]
    pub fn single(provider: Arc<dyn LlmProvider>, default_model: ModelInfo) -> Arc<Self> {
        let model_id = default_model.id.clone();
        let entries = vec![ProviderEntry::new(
            provider,
            vec![default_model],
            SessionCapabilitiesConfig::default(),
        )];
        Arc::new(
            Self::new(entries, &model_id)
                .expect("single-entry registry with matching default model is always valid"),
        )
    }

    /// 用一组 entries + 默认 model id 装配。`default_model` 必须出现在某个
    /// entry 的 `models` 里。
    ///
    /// # Errors
    ///
    /// - [`ProviderRegistryError::Empty`]：entries 为空
    /// - [`ProviderRegistryError::DuplicateModel`]：同一个 model id 出现在
    ///   多个 entry 中
    /// - [`ProviderRegistryError::UnknownDefaultModel`]：`default_model` 不在
    ///   任何 entry 的 `models` 里
    pub fn new(
        entries: Vec<ProviderEntry>,
        default_model: &str,
    ) -> Result<Self, ProviderRegistryError> {
        if entries.is_empty() {
            return Err(ProviderRegistryError::Empty);
        }

        let mut model_index = HashMap::new();
        let mut default_pos = None;
        let mut providers_by_model = HashMap::<String, String>::new();
        for (entry_idx, entry) in entries.iter().enumerate() {
            let provider_vendor = entry.provider.info().vendor;
            let mut seen_in_entry = HashSet::new();
            for (model_idx, model) in entry.models.iter().enumerate() {
                if !seen_in_entry.insert(model.id.clone()) {
                    continue;
                }
                if let Some(first_provider) =
                    providers_by_model.insert(model.id.clone(), provider_vendor.clone())
                {
                    return Err(ProviderRegistryError::DuplicateModel {
                        model: model.id.clone(),
                        first_provider,
                        second_provider: provider_vendor,
                    });
                }
                model_index.insert(model.id.clone(), entry_idx);
                if model.id == default_model && default_pos.is_none() {
                    default_pos = Some((entry_idx, model_idx));
                }
            }
        }

        let default = default_pos.ok_or_else(|| ProviderRegistryError::UnknownDefaultModel {
            model: default_model.to_string(),
        })?;

        Ok(Self {
            entries,
            model_index,
            default,
        })
    }

    /// 默认 entry——session 启动时用来初始化当前 provider/model。
    #[must_use]
    pub fn default_entry(&self) -> &ProviderEntry {
        let (entry_idx, _) = self.default;
        self.entries
            .get(entry_idx)
            .expect("default index validated in `new`")
    }

    /// 默认 model id。
    #[must_use]
    pub fn default_model(&self) -> &str {
        let (entry_idx, model_idx) = self.default;
        let entry = self
            .entries
            .get(entry_idx)
            .expect("default index validated in `new`");
        entry
            .models
            .get(model_idx)
            .map(|m| m.id.as_str())
            .expect("default model index validated in `new`")
    }

    /// 按 model id 查找对应 entry。`None` 表示当前 registry 没有声明此 model。
    #[must_use]
    pub fn entry_for_model(&self, model_id: &str) -> Option<&ProviderEntry> {
        self.model_index
            .get(model_id)
            .and_then(|idx| self.entries.get(*idx))
    }

    /// 列出所有 entry（按装配顺序）。
    #[must_use]
    pub fn entries(&self) -> &[ProviderEntry] {
        &self.entries
    }

    /// 平铺出所有 (provider_info, model) 对。ACP `list_models` 用此构造
    /// `SessionModelState::available_models`。
    #[must_use]
    pub fn list_candidates(&self) -> Vec<ModelCandidate> {
        let mut out = Vec::new();
        for entry in &self.entries {
            let info = entry.provider.info();
            for model in &entry.models {
                out.push(ModelCandidate {
                    provider: info.clone(),
                    model: model.clone(),
                });
            }
        }
        out
    }

    /// 按 model id 查 candidate；用于 ACP 层渲染 description。
    #[must_use]
    pub fn candidate_for_model(&self, model_id: &str) -> Option<ModelCandidate> {
        let entry = self.entry_for_model(model_id)?;
        let model = entry.models.iter().find(|m| m.id == model_id)?.clone();
        Some(ModelCandidate {
            provider: entry.provider.info(),
            model,
        })
    }
}

/// `(provider, model)` 平铺一对——ACP `list_models` 的最小投影单元。
#[derive(Debug, Clone)]
pub struct ModelCandidate {
    pub provider: ProviderInfo,
    pub model: ModelInfo,
}
