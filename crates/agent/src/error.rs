//! 跨模块的错误工具。
//!
//! agent 各模块的错误类型（[`crate::llm::ProviderError`]、
//! [`crate::tool::ToolError`] 等）若需要在 variant 中透传任意 std error
//! 来源，**统一使用 [`BoxError`]** 而非裸
//! `Box<dyn std::error::Error + Send + Sync>`。
//!
//! 用 newtype（而不是 type alias）的好处：
//! - 类型签名短、好读
//! - 与"随便一个 dyn Error"在类型上区分，调用点意图更清晰
//! - 后续要换实现（接 `anyhow::Error`、带 backtrace 等）只改一处

use std::error::Error as StdError;
use std::fmt;

/// 类型擦除的错误值。在公共 API 中携带任意来源 error 而不暴露具体类型。
///
/// 创建方式：
/// - [`BoxError::new`]：从任意 `E: Error + Send + Sync + 'static` 包装
/// - `From<Box<dyn Error + Send + Sync>>`：从已 boxed 的形式迁移
///
/// **没有**为任意 `E: Error` 提供 `From<E>`：Rust 一致性规则下，
/// 这会与 `From<T> for T` 的反射 impl 重叠（因为 `BoxError` 自身也实现
/// `Error`）。调用方使用 [`BoxError::new`] 显式包装。
#[derive(Debug)]
pub struct BoxError(Box<dyn StdError + Send + Sync>);

impl BoxError {
    /// 从任意 std error 包装。
    pub fn new<E>(err: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self(Box::new(err))
    }
}

impl fmt::Display for BoxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl StdError for BoxError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.0.source()
    }
}

impl From<Box<dyn StdError + Send + Sync>> for BoxError {
    fn from(value: Box<dyn StdError + Send + Sync>) -> Self {
        Self(value)
    }
}
