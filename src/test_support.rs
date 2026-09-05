//! Test builders and rendering driven by the production decoders and app.

use crate::event::{EventKind, SessionEvent, SessionKey};
use crate::formats::claude::{ClaudeDecoder, ClaudeFile, decode_subagent_metadata};
use crate::transcript::{Entry, SubagentMeta};

pub(crate) fn app_from_jsonl(text: &str) -> crate::state::App {
    let decoded = crate::tailer::replay_from_jsonl(text, "test-session");
    let mut app = crate::state::App::new(decoded.session.clone(), crate::state::Mode::Live);
    app.handle_ui_event(crate::tailer::UiEvent::ReplayLoaded {
        session: decoded.session,
        items: decoded.items,
        info: Box::new(decoded.info),
        speed: 1.0,
    });
    app
}

pub(crate) fn render_app(
    app: &mut crate::state::App,
    width: u16,
    height: u16,
) -> ratatui::buffer::Buffer {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
    terminal.backend().buffer().clone()
}

/// Read only the target card, excluding matching labels in chips or other nodes.
pub(crate) fn node_text(
    app: &crate::state::App,
    buffer: &ratatui::buffer::Buffer,
    id: &str,
) -> String {
    let (left, top, right, bottom) = app.flow.node_terminal_rect(id).unwrap();
    let mut text = String::new();
    for y in top.max(0)..bottom.min(i32::from(buffer.area.height)) {
        for x in left.max(0)..right.min(i32::from(buffer.area.width)) {
            text.push_str(buffer[(x as u16, y as u16)].symbol());
        }
        text.push('\n');
    }
    text
}

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
