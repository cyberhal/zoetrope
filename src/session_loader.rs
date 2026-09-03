//! Native snapshot loading for one provider-neutral session manifest.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::event::{
    ActorId, AgentDescriptor, AgentRole, EventKind, EventTime, SessionEvent, SessionKey,
    SpawnProvenance,
};
use crate::formats::claude::{ClaudeDecoder, ClaudeFile, decode_subagent_metadata};
use crate::formats::codex::CodexDecoder;
use crate::session_catalog::{ManifestFile, ManifestFileRole, SessionManifest};
use crate::state::SessionInfo;
use crate::tailer::ReplayItem;
use crate::tailer::bytes::TailState;

#[derive(Debug)]
pub(crate) enum FileDecoder {
    Claude(Box<ClaudeDecoder>),
    Codex(Box<CodexDecoder>),
}

impl FileDecoder {
    pub(crate) fn decode_line(&mut self, line: &str) -> Vec<SessionEvent> {
        match self {
            Self::Claude(decoder) => decoder.decode_line(line),
            Self::Codex(decoder) => decoder.decode_line(line),
        }
    }
}

#[derive(Debug)]
pub(crate) struct TrackedFile {
    pub(crate) tail: TailState,
    pub(crate) decoder: FileDecoder,
}

#[derive(Debug)]
pub struct SessionSnapshot {
    pub key: SessionKey,
    pub items: Vec<ReplayItem>,
    pub info: SessionInfo,
    pub diagnostics: Vec<String>,
    pub(crate) tracked: HashMap<PathBuf, TrackedFile>,
    pub(crate) pending_metadata: Vec<PathBuf>,
}

pub fn load_snapshot(manifest: &SessionManifest) -> SessionSnapshot {
    let mut events = synthetic_events(manifest);
    let mut tracked = HashMap::new();
    let mut pending_metadata = Vec::new();
    let mut diagnostics = Vec::new();
    for file in &manifest.files {
        match &file.role {
            ManifestFileRole::ClaudeSubagentMetadata { agent_id, workflow } => {
                match std::fs::read_to_string(&file.path) {
                    Ok(text) => {
                        let decoded = decode_subagent_metadata(
                            &manifest.root.key,
                            agent_id,
                            workflow.as_deref(),
                            &text,
                        );
                        if decoded.is_empty() {
                            pending_metadata.push(file.path.clone());
                            diagnostics.push(format!(
                                "{}: invalid subagent metadata",
                                file.path.display()
                            ));
                        } else {
                            events.extend(decoded);
                        }
                    }
                    Err(error) => {
                        pending_metadata.push(file.path.clone());
                        diagnostics.push(format!("{}: {error}", file.path.display()));
                    }
                }
            }
            _ => {
                let Some(mut decoder) = decoder_for(manifest, file) else {
                    continue;
                };
                let Ok((bytes, consumed, metadata)) = snapshot_bytes(&file.path) else {
                    diagnostics.push(format!("{}: unreadable", file.path.display()));
                    // The manifest already established that this path belongs
                    // to the session. Retain its fresh decoder at offset zero
                    // so a transient read race recovers on an ordinary poll.
                    tracked.insert(
                        file.path.clone(),
                        TrackedFile {
                            tail: TailState::default(),
                            decoder,
                        },
                    );
                    continue;
                };
                let text = String::from_utf8_lossy(&bytes);
                for line in text.lines() {
                    events.extend(decoder.decode_line(line));
                }
                tracked.insert(
                    file.path.clone(),
                    TrackedFile {
                        tail: TailState::at_snapshot(consumed, Some(&metadata)),
                        decoder,
                    },
                );
            }
        }
    }
    let (items, info) = crate::tailer::item::finish(events);
    SessionSnapshot {
        key: manifest.root.key.clone(),
        items,
        info,
        diagnostics,
        tracked,
        pending_metadata,
    }
}

pub fn manifest_for_file(path: &Path) -> Option<SessionManifest> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let mut catalog = crate::session_catalog::SessionCatalog::new(
        crate::session_catalog::DiscoveryRoots::from_home(&home),
    );
    catalog.manifest(&crate::session_catalog::WatchTarget::File(path.to_owned()))
}

pub(crate) fn decoder_for(manifest: &SessionManifest, file: &ManifestFile) -> Option<FileDecoder> {
    match &file.role {
        ManifestFileRole::Root => match manifest.root.key.provider {
            crate::event::Provider::Claude => Some(FileDecoder::Claude(Box::new(
                ClaudeDecoder::new(manifest.root.key.clone(), ClaudeFile::Root),
            ))),
            crate::event::Provider::Codex => {
                Some(FileDecoder::Codex(Box::new(CodexDecoder::new())))
            }
        },
        ManifestFileRole::Spawned => Some(FileDecoder::Codex(Box::new(CodexDecoder::new()))),
        ManifestFileRole::ClaudeSubagent { agent_id, workflow } => {
            Some(FileDecoder::Claude(Box::new(ClaudeDecoder::new(
                manifest.root.key.clone(),
                ClaudeFile::Subagent {
                    agent_id: agent_id.clone(),
                    workflow: workflow.clone(),
                },
            ))))
        }
        ManifestFileRole::ClaudeWorkflowJournal { workflow } => {
            Some(FileDecoder::Claude(Box::new(ClaudeDecoder::new(
                manifest.root.key.clone(),
                ClaudeFile::WorkflowJournal {
                    workflow: workflow.clone(),
                },
            ))))
        }
        ManifestFileRole::ClaudeSubagentMetadata { .. } => None,
    }
}

fn snapshot_bytes(path: &Path) -> std::io::Result<(Vec<u8>, u64, std::fs::Metadata)> {
    let mut file = std::fs::File::open(path)?;
    snapshot_from_file(&mut file)
}

fn snapshot_from_file(
    file: &mut std::fs::File,
) -> std::io::Result<(Vec<u8>, u64, std::fs::Metadata)> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    // This identity belongs to the opened handle, not a second lookup of the
    // path. If the path was atomically replaced during the read, the first tail
    // stat compares the replacement against this original handle and resets.
    let metadata = file.metadata()?;
    let newline_end = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let tail = &bytes[newline_end..];
    let consume_tail = tail.iter().all(u8::is_ascii_whitespace)
        || serde_json::from_slice::<serde_json::Value>(tail).is_ok();
    let consumed = if consume_tail {
        bytes.len()
    } else {
        newline_end
    };
    bytes.truncate(consumed);
    Ok((bytes, consumed as u64, metadata))
}

fn synthetic_events(manifest: &SessionManifest) -> Vec<SessionEvent> {
    manifest
        .metadata
        .iter()
        .map(|metadata| SessionEvent {
            actor: ActorId(metadata.parent.id.clone()),
            time: EventTime::AtAgentStart(ActorId(metadata.child.id.clone())),
            kind: EventKind::AgentDiscovered(AgentDescriptor {
                id: ActorId(metadata.child.id.clone()),
                parent: ActorId(metadata.parent.id.clone()),
                spawn: SpawnProvenance {
                    tool_call_id: None,
                    time: EventTime::AtAgentStart(ActorId(metadata.child.id.clone())),
                    preceding_context: None,
                },
                role: AgentRole::Subagent,
                label: metadata.agent_path.clone(),
                agent_type: None,
                description: None,
                interactive: false,
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Provider;
    use crate::session_catalog::{ManifestFile, ManifestFileRole, SessionKind, SessionRef};
    use std::time::SystemTime;

    #[test]
    fn malformed_sibling_does_not_poison_valid_codex_root() {
        let fixture = PathBuf::from("tests/fixtures/codex/root-current.jsonl");
        let missing = std::env::temp_dir().join(format!(
            "zoetrope_missing_sibling_{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&missing);
        let manifest = SessionManifest {
            root: SessionRef {
                key: SessionKey {
                    provider: Provider::Codex,
                    id: "root-thread".into(),
                },
                path: fixture.clone(),
                cwd: None,
                kind: SessionKind::Root,
                parent_thread_id: None,
                agent_path: None,
                modified: SystemTime::UNIX_EPOCH,
            },
            files: vec![
                ManifestFile {
                    path: fixture,
                    role: ManifestFileRole::Root,
                    session: None,
                },
                ManifestFile {
                    path: missing.clone(),
                    role: ManifestFileRole::Spawned,
                    session: None,
                },
            ],
            metadata: Vec::new(),
        };
        let mut snapshot = load_snapshot(&manifest);
        assert!(
            snapshot
                .items
                .iter()
                .any(|item| matches!(item.event.kind, EventKind::Prompt { .. }))
        );
        assert_eq!(snapshot.diagnostics.len(), 1);
        let tracked = snapshot.tracked.get_mut(&missing).unwrap();
        std::fs::write(&missing, "{}\n").unwrap();
        assert!(matches!(
            crate::tailer::bytes::read_appended(&missing, &mut tracked.tail),
            crate::tailer::bytes::ReadResult::Lines(lines) if lines == ["{}"]
        ));
        let _ = std::fs::remove_file(missing);
    }

    #[test]
    fn valid_final_record_without_newline_is_included_in_snapshot() {
        let path = std::env::temp_dir().join(format!(
            "66666666-6666-6666-6666-{:012}.jsonl",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"{"type":"assistant","uuid":"a","timestamp":"2026-06-05T10:00:00Z","message":{"role":"assistant","model":"claude-test","content":[],"usage":{"output_tokens":7}}}"#,
        )
        .unwrap();
        let manifest = manifest_for_file(&path).unwrap();
        let snapshot = load_snapshot(&manifest);
        assert!(snapshot.items.iter().any(|item| matches!(
            &item.event.kind,
            EventKind::ModelSelected { model } if model == "claude-test"
        )));
        assert!(snapshot.items.iter().any(|item| matches!(
            &item.event.kind,
            EventKind::UsageObserved(usage) if usage.output_tokens == Some(7)
        )));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_identity_comes_from_the_handle_that_supplied_bytes() {
        let watched = std::env::temp_dir().join(format!(
            "zoetrope_snapshot_handle_{}.jsonl",
            std::process::id()
        ));
        let incoming = std::env::temp_dir().join(format!(
            "zoetrope_snapshot_incoming_{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&watched);
        let _ = std::fs::remove_file(&incoming);
        std::fs::write(&watched, "{\"v\":\"old\"}\n").unwrap();
        let mut opened = std::fs::File::open(&watched).unwrap();
        std::fs::write(&incoming, "{\"v\":\"new\"}\n").unwrap();
        std::fs::rename(&incoming, &watched).unwrap();

        let (bytes, consumed, metadata) = snapshot_from_file(&mut opened).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "{\"v\":\"old\"}\n");
        let mut tail = TailState::at_snapshot(consumed, Some(&metadata));
        assert!(matches!(
            crate::tailer::bytes::read_appended(&watched, &mut tail),
            crate::tailer::bytes::ReadResult::Reset
        ));
        let _ = std::fs::remove_file(watched);
    }
}
