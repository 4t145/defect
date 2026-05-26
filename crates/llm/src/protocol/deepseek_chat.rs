//! DeepSeek Chat Completions 响应兼容层。
//!
//! 请求侧复用 OpenAI Chat Completions 编码；只在响应 usage 字段上补
//! DeepSeek 的私货：`prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`。

use defect_agent::llm::{ProviderChunk, ProviderError, Usage};
use futures::Stream;
use toac::body::codec::sse::SseEventStream;
use tokio_util::sync::CancellationToken;

use super::openai_chat;
use crate::wire::openai::components as wire;

/// DeepSeek SSE 流 → ProviderChunk 流。
///
/// 复用 OpenAI-compatible 状态机，只改 usage 提取逻辑。
pub fn decode_stream(
    sse: SseEventStream,
    cancel: CancellationToken,
) -> impl Stream<Item = Result<ProviderChunk, ProviderError>> + Send {
    openai_chat::decode_stream_with_usage_parser(sse, cancel, usage_from_deepseek_wire)
}

fn usage_from_deepseek_wire(
    raw_usage: Option<&serde_json::Value>,
    wire_usage: &wire::CompletionUsage,
) -> Usage {
    let raw_cache_hit_tokens = raw_usage
        .and_then(|usage| usage.get("prompt_cache_hit_tokens"))
        .and_then(serde_json::Value::as_u64);

    Usage {
        input_tokens: u64::try_from(wire_usage.prompt_tokens).ok(),
        output_tokens: u64::try_from(wire_usage.completion_tokens).ok(),
        cache_read_input_tokens: raw_cache_hit_tokens.or_else(|| {
            wire_usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|details| details.cached_tokens)
                .and_then(|value| u64::try_from(value).ok())
        }),
        // DeepSeek 文档中的 `prompt_cache_miss_tokens` 表示未命中的 prompt
        // token 数，不等价于 Anthropic `cache_creation_input_tokens` 的
        // "写入缓存成本"；这里不混用字段。
        cache_creation_input_tokens: None,
    }
}
