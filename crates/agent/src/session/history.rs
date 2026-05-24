//! [`History`] 的 v0 trivial 实现。
//!
//! `Mutex<Vec<Message>>`，无 token 估算、无压缩。设计权衡见
//! `docs/internal/session.md` §4。

use std::sync::Mutex;

use futures::future::BoxFuture;

use crate::error::BoxError;
use crate::llm::Message;
use crate::session::{CompactionReport, History};

/// `Vec<Message>` + `Mutex` 的最小 [`History`] 实现。
#[derive(Default)]
pub struct VecHistory {
    inner: Mutex<Vec<Message>>,
}

impl VecHistory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self {
            inner: Mutex::new(messages),
        }
    }
}

impl History for VecHistory {
    fn append(&self, msg: Message) {
        self.inner
            .lock()
            .expect("VecHistory mutex poisoned")
            .push(msg);
    }

    fn snapshot(&self) -> Vec<Message> {
        self.inner
            .lock()
            .expect("VecHistory mutex poisoned")
            .clone()
    }

    fn token_estimate(&self) -> Option<u64> {
        None
    }

    fn compact(&self) -> BoxFuture<'_, Result<CompactionReport, BoxError>> {
        Box::pin(async {
            Ok(CompactionReport {
                tokens_before: 0,
                tokens_after: 0,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{MessageContent, Role};

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![MessageContent::Text {
                text: text.to_string(),
            }],
        }
    }

    #[test]
    fn append_then_snapshot() {
        let h = VecHistory::new();
        h.append(user("hi"));
        h.append(user("there"));
        let snap = h.snapshot();
        assert_eq!(snap.len(), 2);
    }

    #[test]
    fn token_estimate_is_none_in_v0() {
        let h = VecHistory::new();
        assert!(h.token_estimate().is_none());
    }

    #[tokio::test]
    async fn compact_is_noop_in_v0() {
        let h = VecHistory::new();
        let report = h.compact().await.expect("compact");
        assert_eq!(report.tokens_before, 0);
        assert_eq!(report.tokens_after, 0);
    }
}
