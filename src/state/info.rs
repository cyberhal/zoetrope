//! Session-level metadata, folded independently of the agent model.
//!
//! These flat-metadata entries carry no timestamp and aren't activity, so they
//! stay OFF the timeline (kept on `App`, surfaced in the `i` overlay and the
//! `inspect` header) rather than cluttering the graph. Entries arrive in file
//! (chronological) order, so latest-wins for the "current" values; counts
//! accumulate.

use crate::event::{EventKind, Provider, SessionEvent, SessionInfoPatch, SessionKey};

/// Session-level metadata that carries no timestamp and isn't activity.
#[derive(Debug, Default, Clone)]
pub struct SessionInfo {
    pub cwd: Option<String>,
    /// Header title, from the `ai-title` entry. Session identity, not an event —
    /// it carries no timestamp and belongs here, not on the timeline.
    pub title: Option<String>,
    /// Final permission mode (`default` / `acceptEdits` / `plan` / `bypass…`).
    pub permission_mode: Option<String>,
    pub approval_policy: Option<String>,
    pub sandbox_policy: Option<String>,
    pub permission_profile: Option<String>,
    pub effort: Option<String>,
    /// Final editor/agent mode (e.g. `normal`).
    pub mode: Option<String>,
    /// The most recent prompt text recorded.
    pub last_prompt: Option<String>,
    /// How many messages were enqueued over the session.
    pub queued_ops: Option<u32>,
    /// File-edit checkpoints (`file-history-snapshot` count).
    pub file_snapshots: Option<u32>,
}

impl SessionInfo {
    /// Recorded execution settings shared by the overlay and headless inspect.
    pub fn execution_details(&self) -> impl Iterator<Item = (&'static str, &str)> {
        [
            ("approval", self.approval_policy.as_deref()),
            ("sandbox", self.sandbox_policy.as_deref()),
            ("profile", self.permission_profile.as_deref()),
            ("effort", self.effort.as_deref()),
        ]
        .into_iter()
        .filter_map(|(label, value)| Some((label, value?)))
    }

    pub fn new(provider: Provider) -> Self {
        Self {
            // Only Claude's adapter consumes queue/checkpoint records. Absence
            // in another provider is unknown, not evidence of zero operations.
            queued_ops: (provider == Provider::Claude).then_some(0),
            file_snapshots: (provider == Provider::Claude).then_some(0),
            ..Self::default()
        }
    }

    /// Consume metadata off the timeline, but fold only the selected root's
    /// values. An explicitly opened child is the root of its own view.
    pub fn consume(&mut self, event: &SessionEvent, root: &SessionKey) -> bool {
        let EventKind::SessionInfo(patch) = &event.kind else {
            return false;
        };
        if event.actor.0 == root.id {
            self.apply(patch);
        }
        true
    }

    /// Fold a root-owned metadata patch (latest-wins / counts).
    fn apply(&mut self, patch: &SessionInfoPatch) {
        if patch.cwd.is_some() {
            self.cwd = patch.cwd.clone();
        }
        if patch.title.is_some() {
            self.title = patch.title.clone();
        }
        if patch.mode.is_some() {
            self.mode = patch.mode.clone();
        }
        if patch.permission_mode.is_some() {
            self.permission_mode = patch.permission_mode.clone();
        }
        if patch.approval_policy.is_some() {
            self.approval_policy = patch.approval_policy.clone();
        }
        if patch.sandbox_policy.is_some() {
            self.sandbox_policy = patch.sandbox_policy.clone();
        }
        if patch.permission_profile.is_some() {
            self.permission_profile = patch.permission_profile.clone();
        }
        if patch.effort.is_some() {
            self.effort = patch.effort.clone();
        }
        if patch.last_prompt.is_some() {
            self.last_prompt = patch.last_prompt.clone();
        }
        if patch.queued_ops_delta > 0 {
            self.queued_ops = Some(
                self.queued_ops
                    .unwrap_or(0)
                    .saturating_add(patch.queued_ops_delta),
            );
        }
        if patch.file_snapshots_delta > 0 {
            self.file_snapshots = Some(
                self.file_snapshots
                    .unwrap_or(0)
                    .saturating_add(patch.file_snapshots_delta),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventKind, Provider, SessionKey};
    use crate::formats::claude::{ClaudeDecoder, ClaudeFile};

    #[test]
    fn child_settings_never_replace_root_info_in_bulk_or_live_loading() {
        let transcript = |id: &str,
                          source: serde_json::Value,
                          cwd: &str,
                          policy: &str,
                          prompt: &str| {
            [
                serde_json::json!({"type":"session_meta","payload":{"id":id,"source":source,"cwd":cwd}}),
                serde_json::json!({"type":"inter_agent_communication_metadata","payload":{"trigger_turn":true}}),
                serde_json::json!({"type":"turn_context","payload":{"approval_policy":policy,"collaboration_mode":{"mode":"plan"}}}),
                serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":prompt}}),
            ].map(|record| record.to_string()).join("\n")
        };
        let root_text = transcript(
            "root",
            serde_json::json!("cli"),
            "/root-project",
            "on-request",
            "Root task",
        );
        let child_text = transcript(
            "child",
            serde_json::json!({"subagent":{"thread_spawn":{"parent_thread_id":"root","agent_path":"/root/child"}}}),
            "/child-project",
            "never",
            "Child task",
        );
        let decode = |text: &str| {
            let mut decoder = crate::formats::codex::CodexDecoder::new();
            text.lines()
                .flat_map(|line| decoder.decode_line(line))
                .collect::<Vec<_>>()
        };
        let root = SessionKey::new(Provider::Codex, "root");
        let child = SessionKey::new(Provider::Codex, "child");
        let all_events: Vec<_> = decode(&root_text)
            .into_iter()
            .chain(decode(&child_text))
            .collect();
        let (_, info) = crate::tailer::item::finish(all_events.clone(), &root);
        assert_eq!(
            (
                info.cwd.as_deref(),
                info.approval_policy.as_deref(),
                info.last_prompt.as_deref()
            ),
            (Some("/root-project"), Some("on-request"), Some("Root task"))
        );
        let (_, info) = crate::tailer::item::finish(all_events, &child);
        assert_eq!(
            (
                info.cwd.as_deref(),
                info.approval_policy.as_deref(),
                info.last_prompt.as_deref()
            ),
            (Some("/child-project"), Some("never"), Some("Child task"))
        );

        let mut app = crate::test_support::app_from_jsonl(&root_text);
        app.handle_ui_event(crate::tailer::UiEvent::Batch {
            session: root,
            events: decode(&child_text),
        });
        assert_eq!(
            (
                app.session_info.cwd.as_deref(),
                app.session_info.approval_policy.as_deref(),
                app.session_info.last_prompt.as_deref()
            ),
            (Some("/root-project"), Some("on-request"), Some("Root task"))
        );
    }

    #[test]
    fn session_info_extracts_metadata_latest_wins() {
        let mut info = SessionInfo::default();
        let mut decoder = ClaudeDecoder::new(
            SessionKey {
                provider: Provider::Claude,
                id: "s".into(),
            },
            ClaudeFile::Root,
        );
        for line in [
            r#"{"type":"ai-title","aiTitle":"Build the thing"}"#,
            r#"{"type":"mode","mode":"normal"}"#,
            r#"{"type":"permission-mode","permissionMode":"default"}"#,
            r#"{"type":"permission-mode","permissionMode":"acceptEdits"}"#,
            r#"{"type":"last-prompt","lastPrompt":"hey"}"#,
            r#"{"type":"queue-operation","operation":"enqueue"}"#,
            r#"{"type":"queue-operation","operation":"dequeue"}"#,
            r#"{"type":"file-history-snapshot","messageId":"x"}"#,
            r#"{"type":"file-history-snapshot","messageId":"y"}"#,
        ] {
            for event in decoder.decode_line(line) {
                if let EventKind::SessionInfo(patch) = event.kind {
                    info.apply(&patch);
                }
            }
        }

        assert_eq!(info.title.as_deref(), Some("Build the thing"));
        assert_eq!(info.mode.as_deref(), Some("normal"));
        assert_eq!(info.permission_mode.as_deref(), Some("acceptEdits")); // latest wins
        assert_eq!(info.last_prompt.as_deref(), Some("hey"));
        assert_eq!(info.queued_ops, Some(1), "only enqueues counted");
        assert_eq!(info.file_snapshots, Some(2));
    }
}
