//! 厂商层：实现 [`defect_agent::llm::LlmProvider`]。
//!
//! 厂商层承担 transport / auth / URL 模板 / 能力声明 / 错误 hint /
//! 模型元信息表 / vendor-specific tracing。每家厂商一个子模块。

#[cfg(feature = "provider-anthropic")]
pub mod anthropic;
#[cfg(feature = "provider-bedrock")]
pub mod bedrock;
#[cfg(feature = "provider-deepseek")]
pub mod deepseek;
#[cfg(feature = "provider-openai")]
pub mod openai;
