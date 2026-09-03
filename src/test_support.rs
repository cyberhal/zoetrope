//! Neutral test builders backed by the real Claude adapter.

use crate::event::{EventKind, SessionEvent, SessionKey};
use crate::formats::claude::{ClaudeDecoder, ClaudeFile, decode_subagent_metadata};
use crate::transcript::{Entry, SubagentMeta};

pub(crate) fn claude_events(
    session: &SessionKey,
    file: ClaudeFile,
    entry: Entry,
) -> Vec<SessionEvent> {
    let mut decoder = ClaudeDecoder::new(session.clone(), file);
    decoder.decode_test_entry(entry)
}

pub(crate) fn claude_event(session: &SessionKey, file: ClaudeFile, entry: Entry) -> SessionEvent {
    let mut events: Vec<_> = claude_events(session, file, entry)
        .into_iter()
        .filter(|event| {
            !matches!(
                event.kind,
                EventKind::SessionMetadata(_) | EventKind::SessionInfo(_)
            )
        })
        .collect();
    assert_eq!(events.len(), 1, "fixture must decode to one semantic event");
    events.remove(0)
}

pub(crate) fn metadata_events(
    session: &SessionKey,
    agent_id: &str,
    workflow: Option<&str>,
    meta: &SubagentMeta,
) -> Vec<SessionEvent> {
    decode_subagent_metadata(
        session,
        agent_id,
        workflow,
        &serde_json::json!({
            "agentType": meta.agent_type,
            "description": meta.description,
            "toolUseId": meta.tool_use_id,
            "stoppedByUser": meta.stopped_by_user,
        })
        .to_string(),
    )
}
