//! Provider-owned wire decoders.
//!
//! Raw record DTOs stay private here; the rest of the crate consumes only
//! [`crate::event::SessionEvent`].

pub mod claude;
pub mod codex;

use crate::event::{Provider, SessionEvent, SessionKey, SessionMetadata};
use claude::{ClaudeDecoder, ClaudeFile};
use codex::CodexDecoder;

const MAX_PROBE_RECORD: usize = 8 * 1024 * 1024;

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

/// Positive provider evidence from a bounded transcript prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionProbe {
    Claude(ClaudeIdentity),
    Codex(SessionMetadata),
}

/// Recorded identity evidence from valid Claude envelopes in one file.
///
/// A transcript can omit identity on early records, and workflow journals can
/// mention more than one actor. Each vector retains at most two distinct values:
/// one proves consistency and a second proves conflict without growing with a
/// long journal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ClaudeIdentity {
    pub(crate) session_ids: Vec<String>,
    pub(crate) agent_ids: Vec<String>,
}

impl ClaudeIdentity {
    pub(crate) fn first_session_id(&self) -> Option<&str> {
        self.session_ids.first().map(String::as_str)
    }
}

/// Incremental positive provider classifier. It buffers at most one bounded
/// record, skips malformed records, and locks the first positively identified
/// provider. Claude identity evidence is retained from every valid JSON record
/// both before and after that lock, so validation covers every record a Claude
/// decoder could consume without letting an ambiguous record claim a provider.
#[derive(Debug)]
pub(crate) struct SessionProber {
    codex: CodexDecoder,
    claude_identity: ClaudeIdentity,
    claude_positive: bool,
    codex_result: Option<SessionMetadata>,
    partial: Vec<u8>,
    overflowed: bool,
}

impl Default for SessionProber {
    fn default() -> Self {
        Self {
            codex: CodexDecoder::new(),
            claude_identity: ClaudeIdentity::default(),
            claude_positive: false,
            codex_result: None,
            partial: Vec::new(),
            overflowed: false,
        }
    }
}

impl SessionProber {
    pub(crate) fn push_bytes(&mut self, bytes: &[u8]) {
        let mut start = 0;
        for (index, byte) in bytes.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            if self.overflowed {
                self.overflowed = false;
                self.partial.clear();
            } else {
                let segment = &bytes[start..index];
                if self.partial.len().saturating_add(segment.len()) <= MAX_PROBE_RECORD {
                    if self.partial.is_empty() {
                        self.push_line(segment);
                    } else {
                        self.partial.extend_from_slice(segment);
                        let line = std::mem::take(&mut self.partial);
                        self.push_line(&line);
                    }
                } else {
                    self.partial.clear();
                }
            }
            start = index + 1;
        }
        if !self.overflowed && start < bytes.len() {
            let trailing = &bytes[start..];
            if self.partial.len().saturating_add(trailing.len()) <= MAX_PROBE_RECORD {
                self.partial.extend_from_slice(trailing);
            } else {
                self.partial.clear();
                self.overflowed = true;
            }
        }
    }

    pub(crate) fn finish(mut self) -> Option<SessionProbe> {
        if !self.overflowed && !self.partial.is_empty() {
            let line = std::mem::take(&mut self.partial);
            self.push_line(&line);
        }
        self.probe()
    }

    pub(crate) fn probe(&self) -> Option<SessionProbe> {
        self.codex_result
            .clone()
            .map(SessionProbe::Codex)
            .or_else(|| {
                self.claude_positive
                    .then(|| SessionProbe::Claude(self.claude_identity.clone()))
            })
    }

    fn push_line(&mut self, bytes: &[u8]) {
        if self.codex_result.is_some() {
            return;
        }
        let Ok(line) = std::str::from_utf8(bytes) else {
            return;
        };
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            return;
        }
        if !self.claude_positive {
            self.codex.decode_line(line);
            if let Some(metadata) = self.codex.session() {
                self.codex_result = Some(metadata.clone());
                return;
            }
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        let claude_decoder_accepts = !matches!(
            crate::transcript::parse_line(line),
            None | Some(crate::transcript::Entry::Unknown)
        );
        if claude_decoder_accepts {
            push_nonempty_identity(
                &mut self.claude_identity.session_ids,
                value.get("sessionId"),
            );
            push_nonempty_identity(&mut self.claude_identity.agent_ids, value.get("agentId"));
        }
        let Some(kind) = value.get("type").and_then(|kind| kind.as_str()) else {
            return;
        };
        if is_claude_record(kind, &value) {
            self.claude_positive = true;
        }
    }
}

fn push_nonempty_identity(out: &mut Vec<String>, value: Option<&serde_json::Value>) {
    let Some(value) = value
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
    else {
        return;
    };
    if out.len() < 2 && !out.iter().any(|known| known == value) {
        out.push(value.to_owned());
    }
}

/// Classify complete records from a byte prefix. `at_eof` makes a final
/// newline-less record eligible; a bounded mid-record suffix is never parsed.
#[cfg(any(feature = "native", test))]
pub(crate) fn probe_session_bytes(bytes: &[u8], at_eof: bool) -> Option<SessionProbe> {
    let complete_len = if at_eof {
        bytes.len()
    } else {
        bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1)
    };
    let mut prober = SessionProber::default();
    prober.push_bytes(&bytes[..complete_len]);
    prober.finish()
}

pub(crate) fn probe_session_text(text: &str) -> Option<SessionProbe> {
    let mut prober = SessionProber::default();
    prober.push_bytes(text.as_bytes());
    prober.finish()
}

pub(crate) fn detect_session(text: &str) -> (Provider, Option<String>) {
    match probe_session_text(text) {
        Some(SessionProbe::Codex(metadata)) => (Provider::Codex, Some(metadata.session.id)),
        Some(SessionProbe::Claude(identity)) => (
            Provider::Claude,
            identity.first_session_id().map(str::to_owned),
        ),
        None => (Provider::Claude, None),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_provider_lock_still_finds_a_later_recorded_identity() {
        let text = concat!(
            r#"{"type":"user","message":{"role":"user","content":"start"}}"#,
            "\n",
            r#"{"type":"session_meta","payload":{"id":"must-not-win","source":"cli"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"claude-real","message":{"role":"assistant","content":[]}}"#,
        );
        assert_eq!(
            probe_session_text(text),
            Some(SessionProbe::Claude(ClaudeIdentity {
                session_ids: vec!["claude-real".into()],
                agent_ids: Vec::new(),
            }))
        );
    }

    #[test]
    fn invalid_utf8_after_a_valid_header_does_not_poison_the_probe() {
        let mut bytes = br#"{"type":"session_meta","payload":{"id":"codex-real","source":"cli"}}
"#
        .to_vec();
        bytes.extend_from_slice(&[0xff, b'\n']);
        assert!(matches!(
            probe_session_bytes(&bytes, true),
            Some(SessionProbe::Codex(metadata)) if metadata.session.id == "codex-real"
        ));
    }

    #[test]
    fn nonempty_recorded_identity_is_validated_trimmed_but_preserved_verbatim() {
        let text = r#"{"type":"session_meta","payload":{"id":"  codex-real  ","source":"cli"}}"#;
        assert!(matches!(
            probe_session_text(text),
            Some(SessionProbe::Codex(metadata)) if metadata.session.id == "  codex-real  "
        ));
    }

    #[test]
    fn claude_probe_collects_every_recorded_session_and_actor_identity() {
        let text = concat!(
            r#"{"type":"user","sessionId":"session-a","agentId":"agent-a","message":{"role":"user","content":"one"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"session-b","agentId":"agent-b","message":{"role":"assistant","content":[]}}"#,
        );
        assert_eq!(
            probe_session_text(text),
            Some(SessionProbe::Claude(ClaudeIdentity {
                session_ids: vec!["session-a".into(), "session-b".into()],
                agent_ids: vec!["agent-a".into(), "agent-b".into()],
            }))
        );
    }

    #[test]
    fn claude_probe_collects_identity_from_decoder_accepted_non_evidence_records() {
        let text = concat!(
            r#"{"type":"assistant","sessionId":"session-b","agentId":"agent-b","message":{"content":[{"type":"text","text":"must be validated"}]}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"session-a","agentId":"agent-a","message":{"role":"assistant","content":[]}}"#,
        );
        assert_eq!(
            probe_session_text(text),
            Some(SessionProbe::Claude(ClaudeIdentity {
                session_ids: vec!["session-b".into(), "session-a".into()],
                agent_ids: vec!["agent-b".into(), "agent-a".into()],
            }))
        );
    }

    #[test]
    fn unrecognized_json_identity_cannot_hijack_a_later_claude_record() {
        let text = concat!(
            r#"{"type":"unrelated","sessionId":"noise","agentId":"noise-agent"}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"session-a","agentId":"agent-a","message":{"role":"assistant","content":[]}}"#,
        );
        assert_eq!(
            probe_session_text(text),
            Some(SessionProbe::Claude(ClaudeIdentity {
                session_ids: vec!["session-a".into()],
                agent_ids: vec!["agent-a".into()],
            }))
        );
    }

    #[test]
    fn streaming_probe_skips_an_oversized_record_then_resynchronizes() {
        let mut prober = SessionProber::default();
        prober.push_bytes(&vec![b'x'; MAX_PROBE_RECORD + 1]);
        prober.push_bytes(b"\n");
        prober.push_bytes(
            br#"{"type":"session_meta","payload":{"id":"after-large","source":"cli"}}"#,
        );
        assert!(matches!(
            prober.finish(),
            Some(SessionProbe::Codex(metadata)) if metadata.session.id == "after-large"
        ));
    }

    #[test]
    fn claude_identity_conflict_evidence_is_bounded() {
        let mut prober = SessionProber::default();
        for index in 0..10_000 {
            let record = serde_json::json!({
                "type": "started",
                "agentId": format!("agent-{index}"),
            });
            prober.push_bytes(record.to_string().as_bytes());
            prober.push_bytes(b"\n");
        }
        let Some(SessionProbe::Claude(identity)) = prober.finish() else {
            panic!("valid Claude journal records must be recognized");
        };
        assert_eq!(identity.agent_ids, ["agent-0", "agent-1"]);
        assert!(identity.session_ids.is_empty());
    }
}
