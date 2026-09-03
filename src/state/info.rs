//! Session-level metadata, folded independently of the agent model.
//!
//! These flat-metadata entries carry no timestamp and aren't activity, so they
//! stay OFF the timeline (kept on `App`, surfaced in the `i` overlay and the
//! `inspect` header) rather than cluttering the graph. Entries arrive in file
//! (chronological) order, so latest-wins for the "current" values; counts
//! accumulate.

use crate::event::SessionInfoPatch;

/// Session-level metadata that carries no timestamp and isn't activity.
#[derive(Debug, Default, Clone)]
pub struct SessionInfo {
    pub cwd: Option<String>,
    /// Header title, from the `ai-title` entry. Session identity, not an event —
    /// it carries no timestamp and belongs here, not on the timeline.
    pub title: Option<String>,
    /// Final permission mode (`default` / `acceptEdits` / `plan` / `bypass…`).
    pub permission_mode: Option<String>,
    /// Final editor/agent mode (e.g. `normal`).
    pub mode: Option<String>,
    /// The most recent prompt text recorded.
    pub last_prompt: Option<String>,
    /// How many messages were enqueued over the session.
    pub queued_ops: u32,
    /// File-edit checkpoints (`file-history-snapshot` count).
    pub file_snapshots: u32,
}

impl SessionInfo {
    /// Fold one flat-metadata entry into the info (latest-wins / counts). Non-
    /// metadata entries are ignored.
    pub fn apply(&mut self, patch: &SessionInfoPatch) {
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
        if patch.last_prompt.is_some() {
            self.last_prompt = patch.last_prompt.clone();
        }
        self.queued_ops = self.queued_ops.saturating_add(patch.queued_ops_delta);
        self.file_snapshots = self
            .file_snapshots
            .saturating_add(patch.file_snapshots_delta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventKind, Provider, SessionKey};
    use crate::formats::claude::{ClaudeDecoder, ClaudeFile};

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
        assert_eq!(info.queued_ops, 1, "only enqueues counted");
        assert_eq!(info.file_snapshots, 2);
    }
}
