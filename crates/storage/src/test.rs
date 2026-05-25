use std::fs;
use std::path::PathBuf;

use agent_client_protocol::schema::{
    Content, ContentBlock, SessionId, StopReason, TextContent, ToolCallContent, ToolCallStatus,
    ToolCallUpdateFields,
};
use defect_agent::event::AgentEvent;
use defect_agent::llm::{MessageContent, Role, ToolResultBody, Usage};
use serde_json::json;
use tempfile::tempdir;

use crate::{SessionMeta, SessionStore, StorageError, StoredEvent};

fn user_text_event(text: &str) -> AgentEvent {
    AgentEvent::AssistantText {
        content: ContentBlock::Text(TextContent::new(text)),
    }
}

#[test]
fn init_creates_meta_and_event_files() {
    let dir = tempdir().expect("tempdir");
    let session_id = SessionId::new("sess-1");
    let store = SessionStore::for_session(dir.path(), &session_id);
    let meta = SessionMeta::new(
        session_id.clone(),
        PathBuf::from("/tmp/project"),
        Vec::new(),
    );

    store.init(&meta).expect("init store");

    assert!(store.meta_path().exists());
    assert!(store.events_path().exists());
    assert_eq!(store.load_meta().expect("load meta"), meta);
}

#[test]
fn append_then_replay_preserves_order() {
    let dir = tempdir().expect("tempdir");
    let session_id = SessionId::new("sess-2");
    let store = SessionStore::for_session(dir.path(), &session_id);
    store
        .init(&SessionMeta::new(
            session_id,
            PathBuf::from("/tmp/project"),
            Vec::new(),
        ))
        .expect("init store");

    let first = StoredEvent::new(0, AgentEvent::TurnStarted);
    let second = StoredEvent::new(
        1,
        AgentEvent::UserPromptCommitted {
            content: vec![ContentBlock::Text(TextContent::new("hello"))],
        },
    );
    let third = StoredEvent::new(2, user_text_event("hello"));
    let fourth = StoredEvent::new(
        3,
        AgentEvent::TurnEnded {
            reason: StopReason::EndTurn,
            usage: Usage::default(),
        },
    );

    store.append_event(&first).expect("append first");
    store.append_event(&second).expect("append second");
    store.append_event(&third).expect("append third");
    store.append_event(&fourth).expect("append fourth");

    let replayed = store.replay().expect("replay");
    assert_eq!(replayed, vec![first, second, third, fourth]);
}

#[test]
fn replay_rejects_sequence_gaps() {
    let dir = tempdir().expect("tempdir");
    let session_id = SessionId::new("sess-3");
    let store = SessionStore::for_session(dir.path(), &session_id);
    store
        .init(&SessionMeta::new(
            session_id,
            PathBuf::from("/tmp/project"),
            Vec::new(),
        ))
        .expect("init store");

    store
        .append_event(&StoredEvent::new(1, AgentEvent::TurnStarted))
        .expect("append event");

    let err = store.replay().expect_err("should reject sequence gap");
    assert!(matches!(
        err,
        StorageError::SequenceGap {
            expected: 0,
            actual: 1
        }
    ));
}

#[test]
fn replay_reports_invalid_jsonl_line_number() {
    let dir = tempdir().expect("tempdir");
    let session_id = SessionId::new("sess-4");
    let store = SessionStore::for_session(dir.path(), &session_id);
    store
        .init(&SessionMeta::new(
            session_id,
            PathBuf::from("/tmp/project"),
            Vec::new(),
        ))
        .expect("init store");
    fs::write(store.events_path(), b"{not json}\n").expect("write bad line");

    let err = store.replay().expect_err("should reject invalid line");
    assert!(matches!(
        err,
        StorageError::InvalidEventLine { line: 1, .. }
    ));
}

#[test]
fn replay_state_rebuilds_user_and_assistant_history() {
    let dir = tempdir().expect("tempdir");
    let session_id = SessionId::new("sess-5");
    let store = SessionStore::for_session(dir.path(), &session_id);
    store
        .init(&SessionMeta::new(
            session_id,
            PathBuf::from("/tmp/project"),
            Vec::new(),
        ))
        .expect("init store");

    let events = [
        StoredEvent::new(
            0,
            AgentEvent::UserPromptCommitted {
                content: vec![ContentBlock::Text(TextContent::new("hello"))],
            },
        ),
        StoredEvent::new(1, AgentEvent::TurnStarted),
        StoredEvent::new(
            2,
            AgentEvent::AssistantText {
                content: ContentBlock::Text(TextContent::new("world")),
            },
        ),
        StoredEvent::new(
            3,
            AgentEvent::TurnEnded {
                reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ),
    ];
    for event in events {
        store.append_event(&event).expect("append event");
    }

    let replay = store.replay_state().expect("replay state");

    assert_eq!(replay.turn_count, 1);
    assert!(replay.last_turn_ended);
    assert_eq!(replay.history.len(), 2);
    assert_eq!(replay.history[0].role, Role::User);
    assert_eq!(
        replay.history[0].content,
        vec![MessageContent::Text {
            text: "hello".to_string()
        }]
    );
    assert_eq!(replay.history[1].role, Role::Assistant);
    assert_eq!(
        replay.history[1].content,
        vec![MessageContent::Text {
            text: "world".to_string()
        }]
    );
}

#[test]
fn replay_state_rebuilds_tool_use_and_tool_result_history() {
    let dir = tempdir().expect("tempdir");
    let session_id = SessionId::new("sess-6");
    let store = SessionStore::for_session(dir.path(), &session_id);
    store
        .init(&SessionMeta::new(
            session_id,
            PathBuf::from("/tmp/project"),
            Vec::new(),
        ))
        .expect("init store");

    let mut tool_started = ToolCallUpdateFields::default();
    tool_started.raw_input = Some(json!({ "msg": "hi" }));

    let mut tool_finished = ToolCallUpdateFields::default();
    tool_finished.status = Some(ToolCallStatus::Completed);
    tool_finished.content = Some(vec![ToolCallContent::Content(Content::new("hi"))]);

    let events = [
        StoredEvent::new(
            0,
            AgentEvent::UserPromptCommitted {
                content: vec![ContentBlock::Text(TextContent::new("hello"))],
            },
        ),
        StoredEvent::new(1, AgentEvent::TurnStarted),
        StoredEvent::new(
            2,
            AgentEvent::AssistantText {
                content: ContentBlock::Text(TextContent::new("calling tool")),
            },
        ),
        StoredEvent::new(
            3,
            AgentEvent::ToolCallStarted {
                id: "call-1".into(),
                name: "echo".to_string(),
                fields: tool_started,
            },
        ),
        StoredEvent::new(
            4,
            AgentEvent::ToolCallFinished {
                id: "call-1".into(),
                fields: tool_finished,
            },
        ),
        StoredEvent::new(
            5,
            AgentEvent::TurnEnded {
                reason: StopReason::EndTurn,
                usage: Usage::default(),
            },
        ),
    ];
    for event in events {
        store.append_event(&event).expect("append event");
    }

    let replay = store.replay_state().expect("replay state");

    assert_eq!(replay.history.len(), 3);
    assert_eq!(replay.history[0].role, Role::User);
    assert_eq!(replay.history[1].role, Role::Assistant);
    assert_eq!(replay.history[2].role, Role::User);
    assert_eq!(
        replay.history[1].content,
        vec![
            MessageContent::Text {
                text: "calling tool".to_string(),
            },
            MessageContent::ToolUse {
                id: "call-1".to_string(),
                name: "echo".to_string(),
                args: json!({ "msg": "hi" }),
            },
        ]
    );
    assert_eq!(
        replay.history[2].content,
        vec![MessageContent::ToolResult {
            tool_use_id: "call-1".to_string(),
            output: ToolResultBody::Text {
                text: "hi".to_string(),
            },
            is_error: false,
        }]
    );
}
