//! 协议层：wire JSON ↔ [`defect_agent::llm`] 内部表示的相互转换。
//!
//! 协议层只做编解码，不包含 transport / auth / URL 模板。每个子模块
//! 对应一个 [`defect_agent::llm::ProtocolId`]。

// anthropic_messages 由 anthropic / bedrock 共用（bedrock 走 Anthropic Messages 形状）。
#[cfg(any(feature = "provider-anthropic", feature = "provider-bedrock"))]
pub mod anthropic_messages;
// deepseek_chat 仅 deepseek 用，且依赖 openai_chat。
#[cfg(feature = "provider-deepseek")]
pub mod deepseek_chat;
// openai_chat 由 openai / deepseek 共用。
#[cfg(any(feature = "provider-openai", feature = "provider-deepseek"))]
pub mod openai_chat;
