//! OpenAI 兼容接口 provider。
//!
//! 通过 `base_url` 参数对接 OpenAI 官方与所有遵循 Chat Completions
//! 协议的兼容服务（DeepSeek、Qwen、本地 vllm 等）。bearer token + SSE。
//! v0 骨架阶段，待 LlmProvider 实现填充。
