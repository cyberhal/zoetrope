//! Native snapshot loading for one provider-neutral session manifest.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::event::{
    ActorId, AgentCompletionPolicy, AgentDescriptor, AgentRole, EventKind, EventTime, SessionEvent,
    SessionKey, SessionOrigin, SpawnProvenance,
};
use crate::formats::claude::{ClaudeFile, decode_subagent_metadata};
use crate::formats::{ClaudeIdentity, FileDecoder, SessionProbe, probe_session_bytes};
use crate::session_catalog::SyntheticMetadataEvent;
use crate::session_catalog::{ManifestFile, ManifestFileRole, SessionKind, SessionManifest};
use crate::state::SessionInfo;
use crate::tailer::ReplayItem;
use crate::tailer::bytes::TailState;

#[derive(Debug)]
pub(crate) struct TrackedFile {
    pub(crate) tail: TailState,
    pub(crate) decoder: FileDecoder,
}

pub(crate) struct LoadedFile {
    pub(crate) events: Vec<SessionEvent>,
    pub(crate) tracked: TrackedFile,
}

#[derive(Debug)]
pub struct SessionSnapshot {
    pub key: SessionKey,
    pub items: Vec<ReplayItem>,
    pub info: SessionInfo,
    pub diagnostics: Vec<String>,
    pub(crate) tracked: BTreeMap<PathBuf, TrackedFile>,
    pub(crate) pending_files: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotError {
    pub path: PathBuf,
    pub expected: SessionKey,
    pub found_provider: Option<crate::event::Provider>,
    pub found_key: Option<SessionKey>,
    pub expected_parent: Option<String>,
    pub found_parent: Option<String>,
    pub expected_actor: Option<String>,
    pub found_actors: Vec<String>,
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} changed while loading: expected {} session {}",
            self.path.display(),
            self.expected.provider.label(),
            self.expected.id
        )?;
        match (&self.found_provider, &self.found_key) {
            (_, Some(found)) => write!(
                formatter,
                ", found {} session {}",
                found.provider.label(),
                found.id
            )?,
            (Some(provider), None) => write!(formatter, ", found {} transcript", provider.label())?,
            (None, None) => write!(formatter, ", no complete provider header is available")?,
        }
        if self.expected_parent != self.found_parent {
            write!(
                formatter,
                " (expected parent {:?}, found {:?})",
                self.expected_parent, self.found_parent
            )?;
        }
        if let Some(expected) = &self.expected_actor
            && self.found_actors.iter().any(|found| found != expected)
        {
            write!(
                formatter,
                " (expected actor {expected:?}, found {:?})",
                self.found_actors
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for SnapshotError {}

pub fn load_snapshot(manifest: &SessionManifest) -> Result<SessionSnapshot, Box<SnapshotError>> {
    let mut events = Vec::new();
    let mut accepted_sessions = BTreeSet::new();
    let mut tracked = BTreeMap::new();
    let mut pending_files = Vec::new();
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
                            pending_files.push(file.path.clone());
                            diagnostics.push(format!(
                                "{}: invalid subagent metadata",
                                file.path.display()
                            ));
                        } else {
                            events.extend(decoded);
                        }
                    }
                    Err(error) => {
                        pending_files.push(file.path.clone());
                        diagnostics.push(format!("{}: {error}", file.path.display()));
                    }
                }
            }
            _ => match load_manifest_file(manifest, file)? {
                Some(loaded) => {
                    events.extend(loaded.events);
                    tracked.insert(file.path.clone(), loaded.tracked);
                    if let Some(session) = &file.session {
                        accepted_sessions.insert(session.clone());
                    } else if matches!(file.role, ManifestFileRole::Root) {
                        accepted_sessions.insert(manifest.root.key.clone());
                    }
                }
                None if matches!(file.role, ManifestFileRole::Root) => {
                    return Err(snapshot_error(manifest, file, None, None, None));
                }
                None => {
                    pending_files.push(file.path.clone());
                    diagnostics.push(format!(
                        "{}: waiting for a complete provider record",
                        file.path.display()
                    ));
                }
            },
        }
    }
    events.extend(
        manifest
            .metadata
            .iter()
            .filter(|metadata| accepted_sessions.contains(&metadata.child))
            .map(synthetic_event),
    );
    let (items, info) = crate::tailer::item::finish(events, &manifest.root.key);
    Ok(SessionSnapshot {
        key: manifest.root.key.clone(),
        items,
        info,
        diagnostics,
        tracked,
        pending_files,
    })
}

fn validate_claude_file(
    manifest: &SessionManifest,
    file: &ManifestFile,
    bytes: &[u8],
) -> Result<bool, Box<SnapshotError>> {
    match probe_session_bytes(bytes, true) {
        Some(SessionProbe::Codex(header)) => Err(snapshot_error(
            manifest,
            file,
            Some(crate::event::Provider::Codex),
            Some(header.session),
            None,
        )),
        Some(SessionProbe::Claude(identity)) => {
            validate_claude_identity(manifest, file, &identity)?;
            Ok(true)
        }
        None if matches!(file.role, ManifestFileRole::Root)
            && is_empty_claude_placeholder(bytes) =>
        {
            Ok(true)
        }
        None if matches!(file.role, ManifestFileRole::Root) => {
            Err(snapshot_error(manifest, file, None, None, None))
        }
        None => Ok(false),
    }
}

fn validate_claude_identity(
    manifest: &SessionManifest,
    file: &ManifestFile,
    identity: &ClaudeIdentity,
) -> Result<(), Box<SnapshotError>> {
    if let Some(found) = identity
        .session_ids
        .iter()
        .find(|id| id.as_str() != manifest.root.key.id)
    {
        return Err(snapshot_error(
            manifest,
            file,
            Some(crate::event::Provider::Claude),
            Some(SessionKey::new(
                crate::event::Provider::Claude,
                found.clone(),
            )),
            None,
        ));
    }
    if let ManifestFileRole::ClaudeSubagent { agent_id, .. } = &file.role {
        if identity.session_ids.is_empty() || identity.agent_ids.is_empty() {
            return Err(snapshot_error(
                manifest,
                file,
                Some(crate::event::Provider::Claude),
                identity
                    .first_session_id()
                    .map(|id| SessionKey::new(crate::event::Provider::Claude, id.to_owned())),
                None,
            ));
        }
        if identity.agent_ids.iter().any(|found| found != agent_id) {
            let mut error = snapshot_error(
                manifest,
                file,
                Some(crate::event::Provider::Claude),
                identity
                    .first_session_id()
                    .map(|id| SessionKey::new(crate::event::Provider::Claude, id.to_owned())),
                None,
            );
            error.expected_actor = Some(agent_id.clone());
            error.found_actors.clone_from(&identity.agent_ids);
            return Err(error);
        }
    }
    Ok(())
}

fn is_empty_claude_placeholder(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    text.lines().all(|line| {
        line.trim().is_empty()
            || matches!(
                serde_json::from_str::<serde_json::Value>(line),
                Ok(serde_json::Value::Object(object)) if object.is_empty()
            )
    })
}

/// Load one transcript manifest member from a single file handle, validate
/// that handle's provider identity against the manifest, and transfer both
/// decoder and byte identity to tailing. `None` means the file is temporarily
/// unreadable or lacks enough positive evidence; conflicting evidence is an
/// error and no decoded facts are returned.
pub(crate) fn load_manifest_file(
    manifest: &SessionManifest,
    file: &ManifestFile,
) -> Result<Option<LoadedFile>, Box<SnapshotError>> {
    let Some(mut decoder) = decoder_for(manifest, file) else {
        return Ok(None);
    };
    let Ok((bytes, consumed, metadata)) = snapshot_bytes(&file.path) else {
        return Ok(None);
    };
    match manifest.root.key.provider {
        crate::event::Provider::Codex => match probe_session_bytes(&bytes, true) {
            Some(SessionProbe::Codex(header)) => validate_codex_header(manifest, file, &header)?,
            Some(SessionProbe::Claude(identity)) => {
                return Err(snapshot_error(
                    manifest,
                    file,
                    Some(crate::event::Provider::Claude),
                    identity
                        .first_session_id()
                        .map(|id| SessionKey::new(crate::event::Provider::Claude, id.to_owned())),
                    None,
                ));
            }
            None => return Ok(None),
        },
        crate::event::Provider::Claude => {
            if !validate_claude_file(manifest, file, &bytes)? {
                return Ok(None);
            }
        }
    }
    let mut events = Vec::new();
    let consumed_bytes = &bytes[..consumed as usize];
    for line in consumed_bytes.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Ok(line) = std::str::from_utf8(line) {
            events.extend(decoder.decode_line(line));
        }
    }
    Ok(Some(LoadedFile {
        events,
        tracked: TrackedFile {
            tail: TailState::at_snapshot(consumed, Some(&metadata), consumed_bytes),
            decoder,
        },
    }))
}

fn validate_codex_header(
    manifest: &SessionManifest,
    file: &ManifestFile,
    header: &crate::event::SessionMetadata,
) -> Result<(), Box<SnapshotError>> {
    let expected = file.session.as_ref().unwrap_or(&manifest.root.key).clone();
    let expected_parent = match &file.role {
        ManifestFileRole::Root => manifest.root.parent_thread_id.clone(),
        ManifestFileRole::Spawned => manifest
            .metadata
            .iter()
            .find(|metadata| metadata.child == expected)
            .map(|metadata| metadata.parent.id.clone()),
        _ => None,
    };
    let found_parent = match &header.origin {
        SessionOrigin::ThreadSpawn {
            parent_thread_id, ..
        } => Some(parent_thread_id.clone()),
        SessionOrigin::TopLevel | SessionOrigin::Auxiliary | SessionOrigin::Unknown => None,
    };
    let kind_matches = match &file.role {
        ManifestFileRole::Root => match manifest.root.kind {
            SessionKind::Root => matches!(header.origin, SessionOrigin::TopLevel),
            SessionKind::Spawned => matches!(header.origin, SessionOrigin::ThreadSpawn { .. }),
            SessionKind::Auxiliary => {
                matches!(
                    header.origin,
                    SessionOrigin::Auxiliary | SessionOrigin::Unknown
                )
            }
        },
        ManifestFileRole::Spawned => matches!(header.origin, SessionOrigin::ThreadSpawn { .. }),
        _ => true,
    };
    if header.session != expected || expected_parent != found_parent || !kind_matches {
        return Err(Box::new(SnapshotError {
            path: file.path.clone(),
            expected,
            found_provider: Some(header.session.provider),
            found_key: Some(header.session.clone()),
            expected_parent,
            found_parent,
            expected_actor: None,
            found_actors: Vec::new(),
        }));
    }
    Ok(())
}

fn snapshot_error(
    manifest: &SessionManifest,
    file: &ManifestFile,
    found_provider: Option<crate::event::Provider>,
    found_key: Option<SessionKey>,
    found_parent: Option<String>,
) -> Box<SnapshotError> {
    let expected = file.session.as_ref().unwrap_or(&manifest.root.key).clone();
    let expected_parent = match &file.role {
        ManifestFileRole::Root => manifest.root.parent_thread_id.clone(),
        ManifestFileRole::Spawned => manifest
            .metadata
            .iter()
            .find(|metadata| metadata.child == expected)
            .map(|metadata| metadata.parent.id.clone()),
        _ => None,
    };
    Box::new(SnapshotError {
        path: file.path.clone(),
        expected,
        found_provider,
        found_key,
        expected_parent,
        found_parent,
        expected_actor: None,
        found_actors: Vec::new(),
    })
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
            crate::event::Provider::Claude => Some(FileDecoder::claude(
                manifest.root.key.clone(),
                ClaudeFile::Root,
            )),
            crate::event::Provider::Codex => Some(FileDecoder::codex()),
        },
        ManifestFileRole::Spawned => Some(FileDecoder::codex()),
        ManifestFileRole::ClaudeSubagent { agent_id, workflow } => Some(FileDecoder::claude(
            manifest.root.key.clone(),
            ClaudeFile::Subagent {
                agent_id: agent_id.clone(),
                workflow: workflow.clone(),
            },
        )),
        ManifestFileRole::ClaudeWorkflowJournal { workflow } => Some(FileDecoder::claude(
            manifest.root.key.clone(),
            ClaudeFile::WorkflowJournal {
                workflow: workflow.clone(),
            },
        )),
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
    Ok((bytes, consumed as u64, metadata))
}

pub(crate) fn synthetic_event(metadata: &SyntheticMetadataEvent) -> SessionEvent {
    SessionEvent {
        actor: ActorId(metadata.parent.id.clone()),
        time: EventTime::AtAgentStart(ActorId(metadata.child.id.clone())),
        kind: EventKind::AgentDiscovered(AgentDescriptor {
            id: ActorId(metadata.child.id.clone()),
            parent: ActorId(metadata.parent.id.clone()),
            spawn: SpawnProvenance {
                tool_call_id: None,
                time: EventTime::AtAgentStart(ActorId(metadata.child.id.clone())),
                preceding_context: None,
                task_description: None,
            },
            spawn_reference: metadata.agent_path.clone(),
            completion_policy: AgentCompletionPolicy::ExplicitLifecycle,
            role: AgentRole::Subagent,
            label: metadata.agent_path.clone(),
            agent_type: None,
            description: None,
            interactive: false,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Provider;
    use crate::session_catalog::{
        DiscoveryRoots, ManifestFile, ManifestFileRole, SessionCatalog, SessionKind, SessionRef,
        WatchTarget,
    };
    use std::io::Write;
    use std::time::SystemTime;

    fn claude_record(session: &str, agent: Option<&str>, text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "uuid": format!("{session}-{text}"),
            "sessionId": session,
            "agentId": agent,
            "message": { "role": "user", "content": text }
        })
        .to_string()
    }

    fn claude_family_manifest(
        root_path: PathBuf,
        child_path: PathBuf,
        session: &str,
        agent: &str,
    ) -> SessionManifest {
        let key = SessionKey::new(Provider::Claude, session);
        SessionManifest {
            root: SessionRef {
                key: key.clone(),
                path: root_path.clone(),
                cwd: None,
                kind: SessionKind::Root,
                parent_thread_id: None,
                agent_path: None,
                modified: SystemTime::UNIX_EPOCH,
            },
            files: vec![
                ManifestFile {
                    path: root_path,
                    role: ManifestFileRole::Root,
                    session: Some(key),
                },
                ManifestFile {
                    path: child_path,
                    role: ManifestFileRole::ClaudeSubagent {
                        agent_id: agent.into(),
                        workflow: None,
                    },
                    session: None,
                },
            ],
            metadata: Vec::new(),
        }
    }

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
                    session: Some(SessionKey::new(Provider::Codex, "retry-child-unique")),
                },
            ],
            metadata: vec![SyntheticMetadataEvent {
                parent: SessionKey::new(Provider::Codex, "root-thread"),
                child: SessionKey::new(Provider::Codex, "retry-child-unique"),
                agent_path: None,
            }],
        };
        let snapshot = load_snapshot(&manifest).unwrap();
        assert!(
            snapshot
                .items
                .iter()
                .any(|item| matches!(item.event.kind, EventKind::Prompt { .. }))
        );
        assert_eq!(snapshot.diagnostics.len(), 1);
        assert_eq!(snapshot.pending_files, [missing.clone()]);
        assert!(!snapshot.tracked.contains_key(&missing));
        assert!(!snapshot.items.iter().any(|item| matches!(
            &item.event.kind,
            EventKind::AgentDiscovered(agent) if agent.id.0 == "retry-child-unique"
        )));
        std::fs::write(
            &missing,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"retry-child-unique","source":{"subagent":{"thread_spawn":{"parent_thread_id":"root-thread"}}}}}"#,
                "\n"
            ),
        )
        .unwrap();
        assert!(
            load_manifest_file(&manifest, &manifest.files[1])
                .unwrap()
                .is_some()
        );
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
        let snapshot = load_snapshot(&manifest).unwrap();
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

    #[test]
    fn manifest_to_first_open_replacement_never_mixes_old_key_with_new_content() {
        let base = std::env::temp_dir().join(format!(
            "zoetrope-snapshot-replacement-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("recording.jsonl");
        let rollout = |id: &str, text: &str| {
            format!(
                concat!(
                    r#"{{"type":"session_meta","payload":{{"id":"{}","cwd":{},"source":"cli"}}}}"#,
                    "\n",
                    r#"{{"type":"response_item","payload":{{"type":"message","id":"message-{}","role":"assistant","content":[{{"type":"output_text","text":"{}"}}]}}}}"#,
                    "\n",
                ),
                id,
                serde_json::to_string(&base).unwrap(),
                id,
                text,
            )
        };
        std::fs::write(&path, rollout("session-a", "from A")).unwrap();
        let roots = DiscoveryRoots {
            claude_projects: base.join("claude"),
            codex_sessions: base.join("codex"),
        };
        let mut catalog = SessionCatalog::new(roots);
        let manifest = catalog.manifest(&WatchTarget::File(path.clone())).unwrap();

        std::fs::write(&path, rollout("session-b", "from B")).unwrap();
        let error = load_snapshot(&manifest).unwrap_err();
        assert_eq!(
            error.expected,
            SessionKey::new(Provider::Codex, "session-a")
        );
        assert_eq!(
            error.found_key,
            Some(SessionKey::new(Provider::Codex, "session-b"))
        );

        let mut refreshed = SessionCatalog::new(DiscoveryRoots {
            claude_projects: base.join("claude"),
            codex_sessions: base.join("codex"),
        });
        let current = refreshed.manifest(&WatchTarget::File(path)).unwrap();
        let snapshot = load_snapshot(&current).unwrap();
        assert_eq!(snapshot.key, SessionKey::new(Provider::Codex, "session-b"));
        assert!(snapshot.items.iter().any(|item| matches!(
            &item.event.kind,
            EventKind::AssistantText { text, .. } if text == "from B"
        )));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn claude_manifest_replaced_by_codex_before_first_open_is_rejected() {
        let base = std::env::temp_dir().join(format!(
            "zoetrope-claude-codex-replacement-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("77777777-7777-7777-7777-777777777777.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"user","uuid":"prompt-a","sessionId":"claude-a","message":{"role":"user","content":"hello"}}"#,
                "\n"
            ),
        )
        .unwrap();
        let roots = DiscoveryRoots {
            claude_projects: base.join("claude"),
            codex_sessions: base.join("codex"),
        };
        let mut catalog = SessionCatalog::new(roots);
        let manifest = catalog.manifest(&WatchTarget::File(path.clone())).unwrap();
        assert_eq!(
            manifest.root.key,
            SessionKey::new(Provider::Claude, "claude-a")
        );

        std::fs::write(
            &path,
            r#"{"type":"session_meta","payload":{"id":"codex-b","source":"cli"}}"#,
        )
        .unwrap();
        let error = load_snapshot(&manifest).unwrap_err();
        assert_eq!(
            error.expected,
            SessionKey::new(Provider::Claude, "claude-a")
        );
        assert_eq!(error.found_provider, Some(Provider::Codex));
        assert_eq!(
            error.found_key,
            Some(SessionKey::new(Provider::Codex, "codex-b"))
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn partial_codex_replacement_is_retried_before_a_claude_decoder_is_seeded() {
        let base = std::env::temp_dir().join(format!(
            "zoetrope-claude-partial-codex-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("88888888-8888-8888-8888-888888888888.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"user","uuid":"prompt-a","sessionId":"claude-a","message":{"role":"user","content":"hello"}}"#,
                "\n"
            ),
        )
        .unwrap();
        let roots = DiscoveryRoots {
            claude_projects: base.join("claude"),
            codex_sessions: base.join("codex"),
        };
        let mut catalog = SessionCatalog::new(roots.clone());
        let manifest = catalog.manifest(&WatchTarget::File(path.clone())).unwrap();

        let codex = r#"{"type":"session_meta","payload":{"id":"codex-b","source":"cli"}}"#;
        let split = codex.len() / 2;
        std::fs::write(&path, &codex.as_bytes()[..split]).unwrap();
        let error = load_snapshot(&manifest).unwrap_err();
        assert_eq!(
            error.expected,
            SessionKey::new(Provider::Claude, "claude-a")
        );
        assert_eq!(error.found_provider, None);

        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&codex.as_bytes()[split..])
            .unwrap();
        let mut refreshed = SessionCatalog::new(roots);
        let replacement = refreshed.manifest(&WatchTarget::File(path)).unwrap();
        assert_eq!(
            replacement.root.key,
            SessionKey::new(Provider::Codex, "codex-b")
        );
        assert_eq!(
            load_snapshot(&replacement).unwrap().key,
            replacement.root.key
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn child_snapshot_rejects_a_header_that_left_the_manifest_family() {
        let base =
            std::env::temp_dir().join(format!("zoetrope-child-replacement-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let day = base.join("codex/2026/09/04");
        std::fs::create_dir_all(&day).unwrap();
        let cwd = serde_json::to_string(&base).unwrap();
        let root_path = day.join("rollout-root.jsonl");
        let child_path = day.join("rollout-child.jsonl");
        std::fs::write(
            &root_path,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"root","cwd":{cwd},"source":"cli"}}}}"#
            ),
        )
        .unwrap();
        let child_header = |parent: &str| {
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"child","cwd":{cwd},"source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{parent}"}}}}}}}}}}"#
            )
        };
        std::fs::write(&child_path, child_header("root")).unwrap();
        let roots = DiscoveryRoots {
            claude_projects: base.join("claude"),
            codex_sessions: base.join("codex"),
        };
        let mut catalog = SessionCatalog::new(roots);
        let manifest = catalog.manifest(&WatchTarget::File(root_path)).unwrap();

        std::fs::write(&child_path, child_header("unrelated-root")).unwrap();
        let error = load_snapshot(&manifest).unwrap_err();
        assert_eq!(error.expected, SessionKey::new(Provider::Codex, "child"));
        assert_eq!(error.expected_parent.as_deref(), Some("root"));
        assert_eq!(error.found_parent.as_deref(), Some("unrelated-root"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn claude_child_replacement_cannot_cross_session_families() {
        let base = std::env::temp_dir().join(format!(
            "zoetrope-claude-child-family-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let root = base.join("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl");
        let child = base.join("agent-child-a.jsonl");
        std::fs::write(&root, claude_record("session-a", None, "root A")).unwrap();
        std::fs::write(
            &child,
            claude_record("session-a", Some("child-a"), "child A"),
        )
        .unwrap();
        let manifest = claude_family_manifest(root, child.clone(), "session-a", "child-a");

        std::fs::write(
            &child,
            claude_record("session-b", Some("child-b"), "must not leak"),
        )
        .unwrap();
        let error = load_snapshot(&manifest).unwrap_err();
        assert_eq!(
            error.found_key,
            Some(SessionKey::new(Provider::Claude, "session-b"))
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn claude_child_identity_must_match_its_manifest_actor() {
        let base = std::env::temp_dir().join(format!(
            "zoetrope-claude-child-actor-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let root = base.join("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl");
        let child = base.join("agent-child-a.jsonl");
        std::fs::write(&root, claude_record("session-a", None, "root A")).unwrap();
        std::fs::write(
            &child,
            claude_record("session-a", Some("wrong-child"), "must not leak"),
        )
        .unwrap();
        let manifest = claude_family_manifest(root, child, "session-a", "child-a");

        let error = load_snapshot(&manifest).unwrap_err();
        assert_eq!(error.expected_actor.as_deref(), Some("child-a"));
        assert_eq!(error.found_actors, ["wrong-child"]);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn claude_child_validates_identity_on_records_without_positive_role_evidence() {
        let base = std::env::temp_dir().join(format!(
            "zoetrope-claude-child-hidden-identity-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let root = base.join("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl");
        let child = base.join("agent-child-a.jsonl");
        std::fs::write(&root, claude_record("session-a", None, "root A")).unwrap();
        std::fs::write(
            &child,
            concat!(
                r#"{"type":"assistant","sessionId":"session-a","agentId":"child-a","message":{"role":"assistant","content":[]}}"#,
                "\n",
                r#"{"type":"assistant","sessionId":"session-b","agentId":"child-b","message":{"content":[{"type":"text","text":"must not leak"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let manifest = claude_family_manifest(root, child, "session-a", "child-a");

        let error = load_snapshot(&manifest).unwrap_err();
        assert_eq!(
            error.found_key,
            Some(SessionKey::new(Provider::Claude, "session-b"))
        );
        assert_eq!(error.expected_actor, None);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn claude_snapshot_and_probe_both_skip_invalid_utf8_records() {
        let base = std::env::temp_dir().join(format!(
            "zoetrope-claude-invalid-utf8-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let root = base.join("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl");
        let child = base.join("agent-child-a.jsonl");
        std::fs::write(&root, claude_record("session-a", None, "root A")).unwrap();
        let mut bytes = concat!(
            r#"{"type":"assistant","sessionId":"session-a","agentId":"child-a","message":{"role":"assistant","content":[]}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"session-b","agentId":"child-b","message":{"role":"assistant","content":[{"type":"text","text":"must "#,
        )
        .as_bytes()
        .to_vec();
        bytes.push(0xff);
        bytes.extend_from_slice(br#"not leak"}]}}"#);
        bytes.push(b'\n');
        std::fs::write(&child, bytes).unwrap();
        let manifest = claude_family_manifest(root, child, "session-a", "child-a");

        let snapshot = load_snapshot(&manifest).unwrap();
        assert!(!snapshot.items.iter().any(|item| matches!(
            &item.event.kind,
            EventKind::AssistantText { text, .. } if text.contains("must")
        )));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn claude_workflow_journal_can_name_multiple_family_agents() {
        let base = std::env::temp_dir().join(format!(
            "zoetrope-claude-workflow-identities-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let root = base.join("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa.jsonl");
        let journal = base.join("journal.jsonl");
        std::fs::write(
            &journal,
            concat!(
                r#"{"type":"started","sessionId":"session-a","agentId":"child-a"}"#,
                "\n",
                r#"{"type":"result","sessionId":"session-a","agentId":"child-b"}"#,
                "\n"
            ),
        )
        .unwrap();
        let key = SessionKey::new(Provider::Claude, "session-a");
        let manifest = SessionManifest {
            root: SessionRef {
                key,
                path: root,
                cwd: None,
                kind: SessionKind::Root,
                parent_thread_id: None,
                agent_path: None,
                modified: SystemTime::UNIX_EPOCH,
            },
            files: vec![ManifestFile {
                path: journal,
                role: ManifestFileRole::ClaudeWorkflowJournal {
                    workflow: "workflow-a".into(),
                },
                session: None,
            }],
            metadata: Vec::new(),
        };

        let loaded = load_manifest_file(&manifest, &manifest.files[0])
            .unwrap()
            .unwrap();
        let actors: BTreeSet<_> = loaded
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::AgentStatus { agent_id, .. } => Some(agent_id.0.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(actors, BTreeSet::from(["child-a", "child-b"]));
        let _ = std::fs::remove_dir_all(base);
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
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), "{\"v\":\"old\"}\n");
        let mut tail = TailState::at_snapshot(consumed, Some(&metadata), &bytes);
        assert!(matches!(
            crate::tailer::bytes::read_appended(&watched, &mut tail),
            crate::tailer::bytes::ReadResult::Reset
        ));
        let _ = std::fs::remove_file(watched);
    }
}
