//! 协议层：wire JSON ↔ [`defect_agent::llm`] 内部表示的相互转换。
//!
//! 协议层只做编解码，不包含 transport / auth / URL 模板。每个子模块
//! 对应一个 [`defect_agent::llm::ProtocolId`]。

pub mod anthropic_messages;
pub mod deepseek_chat;
pub mod openai_chat;
