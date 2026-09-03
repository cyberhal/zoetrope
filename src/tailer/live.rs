//! Poll-based live following over provider-neutral per-file decoders.

use std::collections::HashMap;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;

use super::bytes::{ReadResult, TailState, read_appended};
use super::{Flow, TailRequest, UiEvent};
use crate::event::{SessionEvent, SessionKey};
use crate::session_catalog::{
    DiscoveryRoots, ManifestFileRole, SessionCatalog, SessionManifest, WatchTarget,
};
use crate::session_loader::{TrackedFile, decoder_for, load_snapshot, manifest_for_file};

const POLL_INTERVAL: Duration = Duration::from_millis(200);
const SWITCH_SCAN_EVERY: u32 = 10;
const SWITCH_IDLE_TICKS: u32 = 150;

pub(crate) struct LiveSession {
    original_target: WatchTarget,
    catalog: SessionCatalog,
    manifest: SessionManifest,
    tracked: HashMap<PathBuf, TrackedFile>,
    ticks: u32,
    idle_ticks: u32,
}

impl LiveSession {
    pub(crate) fn from_snapshot(
        original_target: WatchTarget,
        catalog: SessionCatalog,
        mut manifest: SessionManifest,
        tracked: HashMap<PathBuf, TrackedFile>,
        pending_metadata: Vec<PathBuf>,
    ) -> Self {
        manifest
            .files
            .retain(|file| !pending_metadata.contains(&file.path));
        Self {
            original_target,
            catalog,
            manifest,
            tracked,
            ticks: 0,
            idle_ticks: 0,
        }
    }
}

pub(crate) async fn run_live(
    target: &WatchTarget,
    ui_tx: &mpsc::Sender<UiEvent>,
    req_rx: &mut mpsc::Receiver<TailRequest>,
) -> Flow {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let mut catalog = SessionCatalog::new(DiscoveryRoots::from_home(&home));
    let manifest = match await_first_session(target, &mut catalog, req_rx).await {
        Ok(manifest) => manifest,
        Err(flow) => return flow,
    };
    if !manifest.root.path.is_file() {
        let _ = ui_tx
            .send(UiEvent::Error(format!(
                "unrecognized session file: {}",
                manifest.root.path.display()
            )))
            .await;
        return Flow::Exit;
    }
    let snapshot = load_snapshot(&manifest);
    let session = snapshot.key.clone();
    let _ = ui_tx
        .send(UiEvent::SessionReset {
            session: session.clone(),
        })
        .await;
    if ui_tx
        .send(UiEvent::ReplayLoaded {
            session: session.clone(),
            items: snapshot.items,
            speed: 1.0,
            info: snapshot.info,
        })
        .await
        .is_err()
    {
        return Flow::Exit;
    }
    for diagnostic in snapshot.diagnostics {
        let _ = ui_tx.send(UiEvent::Error(diagnostic)).await;
    }
    let live = LiveSession::from_snapshot(
        target.clone(),
        catalog,
        manifest,
        snapshot.tracked,
        snapshot.pending_metadata,
    );
    tail_loop(live, session, ui_tx, req_rx).await
}

async fn await_first_session(
    target: &WatchTarget,
    catalog: &mut SessionCatalog,
    req_rx: &mut mpsc::Receiver<TailRequest>,
) -> Result<SessionManifest, Flow> {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if let Some(manifest) = catalog.manifest(target) {
            return Ok(manifest);
        }
        tokio::select! {
            request = req_rx.recv() => match request {
                Some(TailRequest::Watch(path)) => return Err(Flow::Switch(path)),
                None => return Err(Flow::Exit),
            },
            _ = ticker.tick() => {}
        }
    }
}

pub(crate) async fn tail_loop(
    mut live: LiveSession,
    session: SessionKey,
    ui_tx: &mpsc::Sender<UiEvent>,
    req_rx: &mut mpsc::Receiver<TailRequest>,
) -> Flow {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            request = req_rx.recv() => return match request {
                Some(TailRequest::Watch(path)) => Flow::Switch(path),
                None => Flow::Exit,
            },
            _ = ticker.tick() => {
                if poll_live(&mut live, &session, ui_tx).await {
                    return Flow::Reattach;
                }
            }
        }
    }
}

async fn poll_live(
    live: &mut LiveSession,
    session: &SessionKey,
    ui_tx: &mpsc::Sender<UiEvent>,
) -> bool {
    let mut events = Vec::new();
    discover_claude_files(live, &mut events);
    let mut reset = false;
    for (path, tracked) in &mut live.tracked {
        match read_appended(path, &mut tracked.tail) {
            ReadResult::Lines(lines) => {
                for line in lines {
                    events.extend(tracked.decoder.decode_line(&line));
                }
            }
            ReadResult::Reset => reset = true,
            ReadResult::Missing | ReadResult::NoChange => {}
        }
    }
    if reset {
        let _ = ui_tx
            .send(UiEvent::SessionReset {
                session: session.clone(),
            })
            .await;
        return true;
    }
    let had_activity = !events.is_empty();
    if had_activity {
        let _ = ui_tx
            .send(UiEvent::Batch {
                session: session.clone(),
                events,
            })
            .await;
    }
    live.idle_ticks = if had_activity {
        0
    } else {
        live.idle_ticks.saturating_add(1)
    };
    live.ticks = live.ticks.wrapping_add(1);
    if live.ticks.is_multiple_of(SWITCH_SCAN_EVERY)
        && live.idle_ticks >= SWITCH_IDLE_TICKS
        && matches!(live.original_target, WatchTarget::LatestForCwd(_))
        && let Some(next) = live.catalog.manifest(&live.original_target)
        && (next.root.key != live.manifest.root.key || next.root.path != live.manifest.root.path)
    {
        let _ = ui_tx
            .send(UiEvent::SessionReset {
                session: next.root.key,
            })
            .await;
        return true;
    }
    false
}

fn discover_claude_files(live: &mut LiveSession, events: &mut Vec<SessionEvent>) {
    if live.manifest.root.key.provider != crate::event::Provider::Claude {
        return;
    }
    let Some(current) = manifest_for_file(&live.manifest.root.path) else {
        return;
    };
    for file in &current.files {
        if live
            .manifest
            .files
            .iter()
            .any(|known| known.path == file.path)
        {
            continue;
        }
        match &file.role {
            ManifestFileRole::ClaudeSubagentMetadata { agent_id, workflow } => {
                if let Ok(text) = std::fs::read_to_string(&file.path) {
                    let decoded = crate::formats::claude::decode_subagent_metadata(
                        &live.manifest.root.key,
                        agent_id,
                        workflow.as_deref(),
                        &text,
                    );
                    if decoded.is_empty() {
                        continue;
                    }
                    events.extend(decoded);
                }
            }
            _ => {
                if let Some(decoder) = decoder_for(&current, file) {
                    live.tracked.insert(
                        file.path.clone(),
                        TrackedFile {
                            tail: TailState::default(),
                            decoder,
                        },
                    );
                }
            }
        }
        live.manifest.files.push(file.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(tag: &str) -> (PathBuf, DiscoveryRoots) {
        let base =
            std::env::temp_dir().join(format!("zoetrope_slice3_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let roots = DiscoveryRoots {
            claude_projects: base.join("claude-projects"),
            codex_sessions: base.join("codex-sessions"),
        };
        (base, roots)
    }

    fn live_from_file(path: &Path, roots: DiscoveryRoots) -> LiveSession {
        let mut catalog = SessionCatalog::new(roots);
        let target = WatchTarget::File(path.to_owned());
        let manifest = catalog.manifest(&target).unwrap();
        let snapshot = load_snapshot(&manifest);
        LiveSession::from_snapshot(
            target,
            catalog,
            manifest,
            snapshot.tracked,
            snapshot.pending_metadata,
        )
    }

    #[test]
    fn snapshot_live_session_retains_the_original_watch_target() {
        let manifest =
            manifest_for_file(Path::new("tests/fixtures/claude/characterization.jsonl")).unwrap();
        let snapshot = load_snapshot(&manifest);
        let watched = WatchTarget::LatestForCwd(PathBuf::from("/tmp/a-project"));
        let catalog = SessionCatalog::new(DiscoveryRoots::from_home(Path::new("/tmp/no-home")));
        let live = LiveSession::from_snapshot(
            watched.clone(),
            catalog,
            manifest,
            snapshot.tracked,
            snapshot.pending_metadata,
        );
        assert_eq!(live.original_target, watched);
    }

    #[test]
    fn malformed_metadata_is_retried_after_snapshot() {
        let (base, roots) = roots("meta_retry");
        let project = base.join("project");
        let session = "11111111-1111-1111-1111-111111111111";
        let main = project.join(format!("{session}.jsonl"));
        let subagents = project.join(session).join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(&main, "").unwrap();
        let transcript = subagents.join("agent-aaaaaaaaaaaaaaaaa.jsonl");
        let metadata = subagents.join("agent-aaaaaaaaaaaaaaaaa.meta.json");
        std::fs::write(&transcript, "").unwrap();
        std::fs::write(&metadata, r#"{"agentTy"#).unwrap();
        let metadata = metadata.canonicalize().unwrap();

        let mut live = live_from_file(&main, roots);
        assert!(!live.manifest.files.iter().any(|file| file.path == metadata));
        std::fs::write(&metadata, r#"{"agentType":"guide"}"#).unwrap();
        let mut events = Vec::new();
        discover_claude_files(&mut live, &mut events);
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            crate::event::EventKind::AgentDiscovered(agent)
                if agent.id.0 == "aaaaaaaaaaaaaaaaa"
        )));
        assert!(live.manifest.files.iter().any(|file| file.path == metadata));
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn auto_switch_follows_a_newer_session_when_idle() {
        let (base, roots) = roots("auto_switch");
        let cwd = base.join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let project = roots
            .claude_projects
            .join(crate::transcript::sanitize_cwd(&cwd));
        std::fs::create_dir_all(&project).unwrap();
        let a = project.join("11111111-1111-1111-1111-111111111111.jsonl");
        let b = project.join("99999999-9999-9999-9999-999999999999.jsonl");
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();

        let mut catalog = SessionCatalog::new(roots);
        let manifest = catalog.manifest(&WatchTarget::File(a)).unwrap();
        let snapshot = load_snapshot(&manifest);
        let mut live = LiveSession::from_snapshot(
            WatchTarget::LatestForCwd(cwd),
            catalog,
            manifest,
            snapshot.tracked,
            snapshot.pending_metadata,
        );
        live.idle_ticks = SWITCH_IDLE_TICKS;
        live.ticks = SWITCH_SCAN_EVERY - 1;
        let old = live.manifest.root.key.clone();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(poll_live(&mut live, &old, &tx).await);
        assert!(matches!(
            rx.try_recv(),
            Ok(UiEvent::SessionReset { session }) if session.id == "99999999-9999-9999-9999-999999999999"
        ));
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn replay_seed_resumes_where_snapshot_stopped() {
        use std::io::Write;
        let (base, roots) = roots("snapshot_resume");
        std::fs::create_dir_all(&base).unwrap();
        let main = base.join("22222222-2222-2222-2222-222222222222.jsonl");
        std::fs::write(
            &main,
            concat!(
                r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-05T10:00:00Z","message":{"role":"user","content":"one"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut live = live_from_file(&main, roots);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&main)
            .unwrap()
            .write_all(concat!(
                r#"{"type":"assistant","uuid":"a1","timestamp":"2026-06-05T10:01:00Z","message":{"role":"assistant","content":[{"type":"text","text":"two"}]}}"#,
                "\n",
            ).as_bytes())
            .unwrap();
        let session = live.manifest.root.key.clone();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(!poll_live(&mut live, &session, &tx).await);
        let events = match rx.try_recv().unwrap() {
            UiEvent::Batch { events, .. } => events,
            other => panic!("expected appended batch, got {other:?}"),
        };
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind, crate::event::EventKind::AssistantText { .. }))
                .count(),
            1
        );
        assert!(!events.iter().any(|event| matches!(
            &event.kind,
            crate::event::EventKind::Prompt { text } if text == "one"
        )));
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn tracked_file_truncation_reattaches() {
        let (base, roots) = roots("child_truncate");
        let session = "33333333-3333-3333-3333-333333333333";
        let main = base.join(format!("{session}.jsonl"));
        let sub_dir = base.join(session).join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        std::fs::write(&main, "{}\n").unwrap();
        let child = sub_dir.join("agent-bbbbbbbbbbbbbbbbb.jsonl");
        std::fs::write(&child, "long child transcript\nsecond line\n").unwrap();
        let mut live = live_from_file(&main, roots);
        std::fs::write(&child, "{}\n").unwrap();
        let session = live.manifest.root.key.clone();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(poll_live(&mut live, &session, &tx).await);
        assert!(matches!(rx.try_recv(), Ok(UiEvent::SessionReset { .. })));
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn truncation_triggers_full_reattach() {
        let (base, roots) = roots("root_truncate");
        std::fs::create_dir_all(&base).unwrap();
        let main = base.join("44444444-4444-4444-4444-444444444444.jsonl");
        std::fs::write(&main, "a much longer original session line\n").unwrap();
        let mut live = live_from_file(&main, roots);
        std::fs::write(&main, "{}\n").unwrap();
        let session = live.manifest.root.key.clone();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(poll_live(&mut live, &session, &tx).await);
        assert!(matches!(rx.try_recv(), Ok(UiEvent::SessionReset { .. })));
        assert!(matches!(live.original_target, WatchTarget::File(_)));
        let _ = std::fs::remove_dir_all(base);
    }
}
