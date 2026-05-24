//! Codegen 出来的 wire 类型与 operation。
//!
//! 每个子模块由 `defect-llm-codegen` 从 `crates/llm/oas/<vendor>.yaml`
//! 生成，**不要手改**。重生方式：
//!
//! ```bash
//! cargo run -p defect-llm-codegen -- anthropic
//! cargo run -p defect-llm-codegen -- openai
//! ```

pub mod anthropic;
