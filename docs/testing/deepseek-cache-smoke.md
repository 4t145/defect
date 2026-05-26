# DeepSeek Cache Smoke 踩坑总结

本文记录一次 `deepseek_smoke -- cache-smoke` 排障里踩到的坑，目标是把“为什么看起来不命中”和“现在应该怎么验”说清楚，避免后面重复绕圈。

## 1. 现象

最早的现象不是单一问题，而是分成了两个阶段：

1. `cache-smoke` 连续两次请求后，`TurnEnded.usage` 里完全没有 usage。
2. usage 能拿到以后，仍然出现类似下面的结果：

```text
first_cache_read=384
second_cache_read=0
```

旧版 smoke 把它判成 FAIL，因为它默认假设：

- 第一次请求不命中缓存；
- 第二次请求一定命中缓存。

这个假设对 DeepSeek 不成立。

## 2. 根因拆解

### 2.1 请求侧一开始没有稳定缓存锚点

OpenAI-compatible 路径最开始虽然在 capability 上声明了 `prompt_cache: Supported`，但实际编码请求时没有把稳定的缓存锚点发出去：

- Anthropic 路径没有打 `cache_control`；
- OpenAI-compatible 路径没有稳定的 `prompt_cache_key`。

这会让“同一个前缀请求应尽量复用缓存”停留在 capability 声明上，而没有真正落到 wire request。

相关修复：

- `crates/llm/src/protocol/anthropic_messages.rs`
- `crates/llm/src/protocol/openai_chat.rs`

### 2.2 DeepSeek 的命中字段和 OpenAI 官方不同

DeepSeek 走的是 OpenAI Chat Completions 兼容协议，但 usage 字段不是完全照抄 OpenAI 官方。

最关键的差异是：

- OpenAI 常见路径关注 `prompt_tokens_details.cached_tokens`；
- DeepSeek 的 KV cache 命中统计是 `usage.prompt_cache_hit_tokens`。

如果只按 OpenAI 官方字段解码，DeepSeek 的缓存命中会被看成“没有缓存信息”。

相关修复：

- `crates/llm/src/protocol/deepseek_chat.rs`

### 2.3 turn loop 过早收尾，吃掉了尾随 usage chunk

即使 provider 已经把 usage chunk 解出来了，agent 侧最开始也可能看不到。

原因是 `TurnRunner::drain_provider_stream` 早期在看到 `ProviderChunk::Stop` 后就立即返回；但 OpenAI-compatible SSE 的 usage 往往出现在 stop 之后、`[DONE]` 之前。结果就是：

- 文本和 stop reason 都正常；
- `TurnEnded.usage` 却还是 `None` 或不完整。

这里的正确行为不是“看到 Stop 立刻结束读取”，而是：

- 记录 stop reason；
- 继续把流尾读完；
- 吃掉尾随 usage；
- 真正 EOF 时再结束本轮。

相关修复：

- `crates/agent/src/session/turn.rs`

### 2.4 smoke 自己的判定条件对 DeepSeek 过强

当 request/response/turn 聚合都修完以后，仍然可能出现：

- 第一轮已经有 `cache_read_input_tokens`；
- 第二轮反而是 `0`。

这并不自动说明实现有 bug，更可能是因为旧版 smoke 的前提不适用于 DeepSeek：

1. DeepSeek 的缓存是 best effort，不承诺每次都命中。
2. 缓存构建不是严格同步完成，存在秒级延迟。
3. 同一个 prompt 前缀可能在更早的真实请求里就已经被上游缓存过，所以“第一次必须为 0”这个前提不可靠。
4. 服务端还可能按公共前缀做复用，因此“第一次不命中、第二次才命中”的线性模型过于理想化。

因此，`second_cache_read == 0` 只能说明“这一次 probe 没观察到 hit”，不能直接推出“本地实现仍然有 bug”。

## 3. 这次排障后的结论

“DeepSeek 不命中缓存”这件事，实际上经历了两类问题：

1. **前半段是真问题**
   请求没带稳定缓存锚点、DeepSeek usage 字段没解对、turn loop 又把尾随 usage 吃掉了。
2. **后半段是验证方法错了**
   当链路打通后，旧 smoke 仍然要求“第二次必须命中”，这个要求本身不符合 DeepSeek 的真实行为。

简化成一句话：

> 最早是不支持，后来是支持了但 smoke 的判定模型太理想化。

## 4. 现在的 `cache-smoke` 应该怎么理解

`crates/llm/examples/deepseek_smoke.rs` 里的 `cache-smoke` 已经改成更稳的策略：

1. 先做 1 次 warm-up。
2. 每轮之间等待一小段时间，让缓存有机会落盘。
3. 再做多次 probe。
4. 只要 warm-up 之后任意一次出现 `cache_read_input_tokens > 0`，就算 PASS。
5. 如果流里始终没有 usage，就返回 SKIP。
6. 如果 usage 有了，但多次 probe 都没观察到 hit，才返回 FAIL。

这个 smoke 的意义现在是：

- 验证“我们能否通过现有 provider/agent 链路看见 DeepSeek 的缓存命中”；
- 而不是验证“DeepSeek 一定会在第二次请求命中”。

## 5. 后续排障 checklist

以后再看 DeepSeek cache 问题，先按这个顺序排：

1. 看请求是否稳定：
   相同 prompt bytes、相同 model、相同工具描述、相同系统提示。
2. 看请求侧是否真的带了缓存锚点：
   OpenAI-compatible 路径重点看 `prompt_cache_key`。
3. 看 provider 解码是否拿的是 DeepSeek 字段：
   `usage.prompt_cache_hit_tokens`，不是只看 OpenAI 官方字段。
4. 看 `TurnEnded.usage` 是否完整：
   防止 stop 后的 usage chunk 被提前截断。
5. 最后再判断上游行为：
   如果 usage 已经能拿到，但 probe 仍然不稳定，优先怀疑上游缓存策略，不要先怀疑本地协议层。

## 6. 相关文件

- `crates/llm/src/protocol/openai_chat.rs`
- `crates/llm/src/protocol/deepseek_chat.rs`
- `crates/llm/src/provider/deepseek.rs`
- `crates/agent/src/session/turn.rs`
- `crates/llm/examples/deepseek_smoke.rs`
- `crates/llm/tests/deepseek_e2e.rs`
