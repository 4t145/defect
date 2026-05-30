//! [`History`] 的具体实现 [`VecHistory`]：`Vec<Message>` + token 计量。
//!
//! 纯存储，不做压缩——压缩编排在 turn 主循环（`session/turn/compact.rs`）。
//! 设计权衡见 `docs/internal/session.md` §4。
//!
//! ## token 估算
//!
//! 不引入 tokenizer 依赖（对齐 opencode：trigger 用真实 usage，内部估算用
//! 字符启发式）。两段拼起来：
//! - **基线**：上一次 LLM 调用回报的真实输入 token（`record_input_tokens`），
//!   由 turn 主循环在每次调用后喂入。这是最准的一段。
//! - **增量**：基线之后 `append` 的消息按 `chars/4` 估算累加——这部分还没进过
//!   LLM，没有真实 token 可依。
//!
//! `replace`（压缩后回写）会清空基线：新列表的 token 数得等下一次真实调用回报。
//! 基线缺失时（session 刚建、或刚 replace 完）整份 snapshot 走字符启发式兜底。

use std::sync::Mutex;

use crate::llm::{Message, MessageContent};
use crate::session::History;

/// 多模态图片在字符估算中按固定 token 数记账——对齐 Claude Code microcompact
/// 的图片计数（无法按字符估，给个保守常量）。
const IMAGE_TOKEN_ESTIMATE: usize = 2_000;

/// 字符到 token 的启发式比率：`chars / 4`（对齐 codex / opencode）。
const CHARS_PER_TOKEN: usize = 4;

/// `Vec<Message>` + `Mutex` 的 [`History`] 实现，带 token 计量。
#[derive(Default)]
pub struct VecHistory {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    messages: Vec<Message>,
    /// 上一次 LLM 调用回报的真实输入 token。`None` = 尚无真实基线
    /// （新建 / 刚 replace），此时 `token_estimate` 整份走字符启发式。
    last_real_input: Option<u64>,
    /// 真实基线之后 `append` 的消息的字符启发式估算累加（token 数）。
    est_since_baseline: u64,
}

impl VecHistory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                messages,
                last_real_input: None,
                est_since_baseline: 0,
            }),
        }
    }
}

impl History for VecHistory {
    fn append(&self, msg: Message) {
        let mut inner = self.inner.lock().expect("VecHistory mutex poisoned");
        // 基线已建立时，新消息的估算单独累加到增量上；基线缺失时无需累加
        // （token_estimate 会整份重算）。
        if inner.last_real_input.is_some() {
            inner.est_since_baseline = inner
                .est_since_baseline
                .saturating_add(estimate_message_tokens(&msg));
        }
        inner.messages.push(msg);
    }

    fn snapshot(&self) -> Vec<Message> {
        self.inner
            .lock()
            .expect("VecHistory mutex poisoned")
            .messages
            .clone()
    }

    fn replace(&self, messages: Vec<Message>) {
        let mut inner = self.inner.lock().expect("VecHistory mutex poisoned");
        inner.messages = messages;
        // 新列表的真实 token 数未知，等下一次 LLM 调用回报。
        inner.last_real_input = None;
        inner.est_since_baseline = 0;
    }

    fn record_input_tokens(&self, tokens: u64) {
        let mut inner = self.inner.lock().expect("VecHistory mutex poisoned");
        inner.last_real_input = Some(tokens);
        // 基线刷新——其后 append 的增量从零重新计。
        inner.est_since_baseline = 0;
    }

    fn token_estimate(&self) -> Option<u64> {
        let inner = self.inner.lock().expect("VecHistory mutex poisoned");
        match inner.last_real_input {
            // 有真实基线：基线 + 其后新增消息的字符启发式增量。
            Some(real) => Some(real.saturating_add(inner.est_since_baseline)),
            // 无基线：整份走字符启发式兜底。空历史返回 None。
            None => {
                if inner.messages.is_empty() {
                    return None;
                }
                Some(
                    inner
                        .messages
                        .iter()
                        .map(estimate_message_tokens)
                        .fold(0u64, u64::saturating_add),
                )
            }
        }
    }
}

/// 单条消息的字符启发式 token 估算（`chars/4`，图片记常量）。
///
/// `pub(crate)`：压缩模块（`session/turn/compact.rs`）选保留边界时复用同一
/// 把尺子，避免两处估算口径漂移。
pub(crate) fn estimate_message_tokens(msg: &Message) -> u64 {
    let chars: usize = msg
        .content
        .iter()
        .map(|c| match c {
            MessageContent::Text { text } => text.len() / CHARS_PER_TOKEN,
            MessageContent::Thinking { text, signature } => {
                (text.len() + signature.as_ref().map_or(0, |s| s.len())) / CHARS_PER_TOKEN
            }
            MessageContent::ToolUse { name, args, .. } => {
                (name.len() + args.to_string().len()) / CHARS_PER_TOKEN
            }
            MessageContent::ToolResult { output, .. } => {
                tool_result_chars(output) / CHARS_PER_TOKEN
            }
            MessageContent::Image { .. } => IMAGE_TOKEN_ESTIMATE,
            // hosted activity 的 payload 跨进程不持久化，估算上忽略。
            MessageContent::ProviderActivity { .. } => 0,
        })
        .sum();
    chars as u64
}

fn tool_result_chars(output: &crate::llm::ToolResultBody) -> usize {
    match output {
        crate::llm::ToolResultBody::Text { text } => text.len(),
        crate::llm::ToolResultBody::Json { value } => value.to_string().len(),
    }
}

#[cfg(test)]
mod test;
