//! 把 [`SandboxMode`] 翻成具体的 [`SandboxPolicy`] 实例。

use std::sync::Arc;

use defect_agent::policy::{
    AskWritesPolicy, DenyAllPolicy, OpenPolicy, ReadOnlyPolicy, SandboxPolicy,
};
use defect_config::SandboxMode;

/// 按 `[sandbox].mode` 选择 policy 实现。
pub fn build_policy(mode: SandboxMode) -> Arc<dyn SandboxPolicy> {
    match mode {
        SandboxMode::ReadOnly => Arc::new(ReadOnlyPolicy),
        SandboxMode::AskWrites => Arc::new(AskWritesPolicy::new()),
        SandboxMode::Open => Arc::new(OpenPolicy),
        SandboxMode::DenyAll => Arc::new(DenyAllPolicy),
    }
}
