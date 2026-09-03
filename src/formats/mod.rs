//! Provider-owned wire decoders.
//!
//! Raw record DTOs stay private here; the rest of the crate consumes only
//! [`crate::event::SessionEvent`].

pub mod claude;
pub mod codex;

use crate::event::{Provider, SessionEvent, SessionKey};
use claude::{ClaudeDecoder, ClaudeFile};
use codex::CodexDecoder;

/// Stateful provider decoder shared by native snapshots and portable feeds.
#[derive(Debug)]
pub(crate) enum FileDecoder {
    Claude(Box<ClaudeDecoder>),
    Codex(Box<CodexDecoder>),
}

impl FileDecoder {
    pub(crate) fn claude(session: SessionKey, source: ClaudeFile) -> Self {
        Self::Claude(Box::new(ClaudeDecoder::new(session, source)))
    }

    pub(crate) fn codex() -> Self {
        Self::Codex(Box::new(CodexDecoder::new()))
    }

    pub(crate) fn decode_line(&mut self, line: &str) -> Vec<SessionEvent> {
        match self {
            Self::Claude(decoder) => decoder.decode_line(line),
            Self::Codex(decoder) => decoder.decode_line(line),
        }
    }

    pub(crate) fn session_key(&self) -> Option<SessionKey> {
        match self {
            Self::Claude(_) => None,
            Self::Codex(decoder) => decoder.session().map(|meta| meta.session.clone()),
        }
    }
}

pub(crate) fn detect_session(text: &str) -> (Provider, Option<String>) {
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(kind) = value.get("type").and_then(|kind| kind.as_str()) else {
            continue;
        };
        if kind == "session_meta"
            && value
                .get("payload")
                .and_then(|payload| payload.get("id"))
                .and_then(|id| id.as_str())
                .is_some_and(|id| !id.trim().is_empty())
        {
            return (
                Provider::Codex,
                value["payload"]["id"].as_str().map(str::to_owned),
            );
        }
        if is_claude_record(kind, &value) {
            return (Provider::Claude, claude_session_id(text));
        }
    }
    (Provider::Claude, None)
}

fn claude_session_id(text: &str) -> Option<String> {
    text.lines().find_map(|candidate| {
        let value = serde_json::from_str::<serde_json::Value>(candidate).ok()?;
        let kind = value.get("type")?.as_str()?;
        is_claude_record(kind, &value).then_some(())?;
        value
            .get("sessionId")?
            .as_str()
            .filter(|id| !id.trim().is_empty())
            .map(str::to_owned)
    })
}

/// A shared `type` spelling is not enough to claim a provider. Require a
/// field unique to the corresponding Claude envelope so unrelated JSONL can
/// remain noise until a later, authoritative header is seen.
fn is_claude_record(kind: &str, value: &serde_json::Value) -> bool {
    let string = |field: &str| {
        value
            .get(field)
            .and_then(serde_json::Value::as_str)
            .is_some()
    };
    match kind {
        "user" => {
            value
                .get("message")
                .and_then(|message| message.get("role"))
                .and_then(serde_json::Value::as_str)
                == Some("user")
        }
        "assistant" => {
            value
                .get("message")
                .and_then(|message| message.get("role"))
                .and_then(serde_json::Value::as_str)
                == Some("assistant")
        }
        "system" => string("subtype"),
        "attachment" => value.get("attachment").is_some(),
        "ai-title" => string("aiTitle"),
        "last-prompt" => string("lastPrompt"),
        "mode" => string("mode"),
        "permission-mode" => string("permissionMode"),
        "queue-operation" => string("operation"),
        "file-history-snapshot" => {
            value.get("messageId").is_some() || value.get("snapshot").is_some()
        }
        "started" | "result" => string("agentId") || string("key"),
        _ => false,
    }
}
