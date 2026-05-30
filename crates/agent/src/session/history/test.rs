use super::*;
use crate::llm::{MessageContent, Role};

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![MessageContent::Text {
            text: text.to_string(),
        }]
        .into(),
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
fn token_estimate_none_when_empty() {
    let h = VecHistory::new();
    assert!(h.token_estimate().is_none());
}

#[test]
fn token_estimate_char_heuristic_without_baseline() {
    // 无真实基线：整份 snapshot 走 chars/4 兜底。
    let h = VecHistory::new();
    h.append(user(&"a".repeat(40))); // 40 chars → 10 token
    assert_eq!(h.token_estimate(), Some(10));
}

#[test]
fn record_input_tokens_becomes_baseline_plus_increment() {
    let h = VecHistory::new();
    h.append(user("seed"));
    // 真实基线 1000；其后追加的消息走字符增量叠加。
    h.record_input_tokens(1_000);
    assert_eq!(h.token_estimate(), Some(1_000));
    h.append(user(&"b".repeat(40))); // +10 token
    assert_eq!(h.token_estimate(), Some(1_010));
}

#[test]
fn record_input_tokens_refreshes_baseline_and_resets_increment() {
    let h = VecHistory::new();
    h.record_input_tokens(1_000);
    h.append(user(&"b".repeat(40))); // +10
    assert_eq!(h.token_estimate(), Some(1_010));
    // 新一轮真实回报：基线刷新、增量归零。
    h.record_input_tokens(2_000);
    assert_eq!(h.token_estimate(), Some(2_000));
}

#[test]
fn replace_swaps_messages_and_clears_baseline() {
    let h = VecHistory::new();
    h.append(user("old one"));
    h.append(user("old two"));
    h.record_input_tokens(5_000);
    assert_eq!(h.token_estimate(), Some(5_000));

    h.replace(vec![user(&"c".repeat(80))]); // 80 chars → 20 token
    let snap = h.snapshot();
    assert_eq!(snap.len(), 1);
    // 基线清空 → 整份字符启发式。
    assert_eq!(h.token_estimate(), Some(20));
}
