# Provider / Protocol Polyfill Backlog

## 动机

`defect` 的 LLM 层已经把 `provider` 和 `protocol` 分开：provider 负责
transport/auth/endpoint/model 表，protocol 负责 wire 编解码。后续扩展不应再引入
`instance` 这类中间概念，而是优先复用现有 protocol，再在 provider 层补具体网关或厂商差异。

## 当前决议

- **采用：LiteLLM provider preset**。LiteLLM 是常见网关，今天先作为内置 provider，复用 `openai-chat`。
- **采用：Amazon Bedrock provider**。Bedrock 复用 `anthropic-messages` 请求/解码，但 transport 用 AWS Runtime SDK event-stream。
- **暂缓：更多 provider/protocol polyfill**。先放入 backlog，等真实用户场景驱动。

## TODO

- `azure-openai` provider flavor：复用 `openai-chat`，但支持 deployment URL、`api-version` query、`api-key` header。
- OpenAI-compatible provider presets：OpenRouter、Groq、Mistral、Gemini compatibility、Ollama、SiliconFlow、DashScope、Moonshot、Zhipu。优先通过配置模板/文档解决，只有出现 auth/header/URL 特殊性时再升为内置 provider。
- `openai-responses` protocol：覆盖 Responses API 的 reasoning、tool、hosted tool、continuation 等新语义。
- `gemini-generate-content` protocol：覆盖 Gemini native 多模态、function calling、thinking/grounding。
- `bedrock-converse` protocol：使用 Bedrock Converse/ConverseStream，支持 Bedrock 上非 Anthropic 模型。
- `ollama-native` protocol/provider：当需要本地模型列表、keep_alive、pull/show 等本地生命周期能力时再做。
- `vertex-anthropic` provider：复用 Anthropic Messages 编解码，但处理 GCP OAuth、project/location URL。

## 非目标

- 不引入 `instance` 概念。`protocol = "openai-chat"` / `protocol = "anthropic-messages"` 是 wire 语义选择，provider 名才是运行时接入点。
- 不为每个 OpenAI-compatible 服务复制一份 provider 代码。没有 transport/auth/URL 差异的服务默认走 custom provider 或 preset。

