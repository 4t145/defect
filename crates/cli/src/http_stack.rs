//! 把 `defect-config` 的 typed HTTP 配置翻译成 `defect_http::HttpStackConfig`。
//!
//! `defect-config` 不直接依赖 `defect-http` 是为了保持 crate 单向依赖
//! （详见 [`defect_config::HttpClientConfig`] 注释）。翻译动作放 CLI
//! 装配期最自然——同一份 stack config 三家 provider 共用，proxy URI
//! 解析失败在这里集中报错。

use std::time::Duration;

use defect_config::{HttpClientConfig, HttpProxyMode, HttpProxySettings};

/// 按 typed config 构造一份 [`defect_http::HttpStackConfig`]。
///
/// # Errors
///
/// 当 `proxy.mode = Explicit` 且 `http_proxy` / `https_proxy` URI 无法
/// 解析时返回错误，避免后续 provider 装配时再触发同一错误。
pub fn build_http_stack_config(
    config: &HttpClientConfig,
) -> anyhow::Result<defect_http::HttpStackConfig> {
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
