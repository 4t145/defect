//! 路径解析 helper——session storage root 等。

use std::env;
use std::path::PathBuf;

/// 默认 session 持久化根目录。优先级：
/// 1. `XDG_STATE_HOME/defect/sessions`
/// 2. `$HOME/.local/state/defect/sessions`
///
/// # Errors
///
/// 当 `XDG_STATE_HOME` 与 `HOME` 均未设置时返回错误。
pub fn default_sessions_root() -> anyhow::Result<PathBuf> {
    if let Ok(xdg_state_home) = env::var("XDG_STATE_HOME") {
        return Ok(PathBuf::from(xdg_state_home).join("defect/sessions"));
    }
    if let Ok(home) = env::var("HOME") {
        return Ok(PathBuf::from(home).join(".local/state/defect/sessions"));
    }
    Err(anyhow::anyhow!(
        "cannot resolve session storage root: neither XDG_STATE_HOME nor HOME is set"
    ))
}
