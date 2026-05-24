//! 厂商层：实现 [`defect_agent::llm::LlmProvider`]。
//!
//! 厂商层承担 transport / auth / URL 模板 / 能力声明 / 错误 hint /
//! 模型元信息表 / vendor-specific tracing。每家厂商一个子模块。

pub mod anthropic;
pub mod deepseek;
pub mod openai;
