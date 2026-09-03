//! Poll-based live following over provider-neutral per-file decoders.

use std::collections::{BTreeMap, BTreeSet};
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
#[cfg(test)]
use crate::session_loader::manifest_for_file;
use crate::session_loader::{TrackedFile, decoder_for, load_snapshot, synthetic_event};

const POLL_INTERVAL: Duration = Duration::from_millis(200);
const CATALOG_REFRESH_EVERY: u32 = 10;
const SWITCH_IDLE_TICKS: u32 = 150;

pub(crate) struct LiveSession {
    original_target: WatchTarget,
    catalog: SessionCatalog,
    manifest: SessionManifest,
    tracked: BTreeMap<PathBuf, TrackedFile>,
    seen_synthetic_metadata: BTreeSet<(SessionKey, SessionKey)>,
    ticks: u32,
    idle_ticks: u32,
    replay_speed: f64,
    pending_resets: BTreeSet<PathBuf>,
}

impl LiveSession {
    pub(crate) fn from_snapshot(
        original_target: WatchTarget,
        catalog: SessionCatalog,
        mut manifest: SessionManifest,
        tracked: BTreeMap<PathBuf, TrackedFile>,
        pending_metadata: Vec<PathBuf>,
        replay_speed: f64,
    ) -> Self {
        manifest
            .files
            .retain(|file| !pending_metadata.contains(&file.path));
        let seen_synthetic_metadata = manifest
            .metadata
            .iter()
            .map(|metadata| (metadata.parent.clone(), metadata.child.clone()))
            .collect();
        Self {
            original_target,
            catalog,
            manifest,
            tracked,
            seen_synthetic_metadata,
            ticks: 0,
            idle_ticks: 0,
            replay_speed,
            pending_resets: BTreeSet::new(),
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
        1.0,
    );
    tail_loop(live, ui_tx, req_rx).await
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
                if poll_live(&mut live, ui_tx).await {
                    return Flow::Reattach;
                }
            }
        }
    }
}

async fn poll_live(live: &mut LiveSession, ui_tx: &mpsc::Sender<UiEvent>) -> bool {
    if !live.pending_resets.is_empty() {
        live.idle_ticks = 0;
        reload_current(live, ui_tx).await;
        return false;
    }
    let mut events = Vec::new();
    live.ticks = live.ticks.wrapping_add(1);
    let refresh_tick = live.ticks.is_multiple_of(CATALOG_REFRESH_EVERY);
    if refresh_tick {
        let refresh_catalog = live.manifest.root.key.provider == crate::event::Provider::Codex;
        refresh_manifest(live, &mut events, refresh_catalog);
    }
    let mut resets = BTreeSet::new();
    let mut had_bytes = false;
    for (path, tracked) in &mut live.tracked {
        match read_appended(path, &mut tracked.tail) {
            ReadResult::Lines(lines) => {
                had_bytes = true;
                for line in lines {
                    events.extend(tracked.decoder.decode_line(&line));
                }
            }
            ReadResult::Reset => {
                resets.insert(path.clone());
            }
            ReadResult::Missing | ReadResult::NoChange => {}
        }
    }
    if !resets.is_empty() {
        live.pending_resets.extend(resets);
        reload_current(live, ui_tx).await;
        return false;
    }
    let had_activity = had_bytes || !events.is_empty();
    if had_activity {
        let _ = ui_tx
            .send(UiEvent::Batch {
                session: live.manifest.root.key.clone(),
                events,
            })
            .await;
    }
    live.idle_ticks = if had_activity {
        0
    } else {
        live.idle_ticks.saturating_add(1)
    };
    let quiet_directory_watch = refresh_tick
        && !had_activity
        && live.idle_ticks >= SWITCH_IDLE_TICKS
        && matches!(live.original_target, WatchTarget::LatestForCwd(_));
    if quiet_directory_watch && live.manifest.root.key.provider == crate::event::Provider::Claude {
        // A Claude session needs no Codex calendar walk while it is active.
        // Defer the cross-provider refresh until after this tick's appends have
        // proved it stayed quiet.
        live.catalog.refresh();
    }
    if refresh_tick
        && live.idle_ticks >= SWITCH_IDLE_TICKS
        && let WatchTarget::LatestForCwd(cwd) = &live.original_target
        && let Some(next) = live.catalog.latest_for_cwd_cached(cwd)
        && (next.key != live.manifest.root.key || next.path != live.manifest.root.path)
    {
        return true;
    }
    false
}

fn refresh_manifest(live: &mut LiveSession, events: &mut Vec<SessionEvent>, refresh_catalog: bool) {
    if refresh_catalog {
        live.catalog.refresh();
    }
    let current = live.catalog.manifest_for_root(&live.manifest.root);
    for metadata in &current.metadata {
        let key = (metadata.parent.clone(), metadata.child.clone());
        if live.seen_synthetic_metadata.insert(key) {
            events.push(synthetic_event(metadata));
        }
    }
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
    live.manifest
        .files
        .sort_by(|left, right| left.path.cmp(&right.path));
    live.manifest.metadata = current.metadata;
}

async fn reload_current(live: &mut LiveSession, ui_tx: &mpsc::Sender<UiEvent>) -> bool {
    if !live
        .pending_resets
        .iter()
        .all(|path| live.catalog.replacement_ready(path))
    {
        return false;
    }
    let current_path = live.manifest.root.path.clone();
    let Some(mut manifest) = live.catalog.replacement_manifest(&current_path) else {
        return false;
    };
    if manifest.root.key == live.manifest.root.key {
        for known in &live.manifest.files {
            if known.path != live.manifest.root.path
                && !manifest.files.iter().any(|file| file.path == known.path)
                && std::fs::File::open(&known.path).is_err()
            {
                manifest.files.push(known.clone());
            }
        }
    }
    let snapshot = load_snapshot(&manifest);
    let session = snapshot.key.clone();
    live.seen_synthetic_metadata = manifest
        .metadata
        .iter()
        .map(|metadata| (metadata.parent.clone(), metadata.child.clone()))
        .collect();
    manifest
        .files
        .retain(|file| !snapshot.pending_metadata.contains(&file.path));
    live.manifest = manifest;
    live.tracked = snapshot.tracked;
    live.ticks = 0;
    live.idle_ticks = 0;
    live.pending_resets.clear();
    let _ = ui_tx
        .send(UiEvent::SessionReset {
            session: session.clone(),
        })
        .await;
    let _ = ui_tx
        .send(UiEvent::ReplayLoaded {
            session,
            items: snapshot.items,
            speed: live.replay_speed,
            info: snapshot.info,
        })
        .await;
    for diagnostic in snapshot.diagnostics {
        let _ = ui_tx.send(UiEvent::Error(diagnostic)).await;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventKind, Provider};
    use std::io::Write;

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
            1.0,
        )
    }

    fn codex_file(base: &Path, name: &str) -> PathBuf {
        let day = base.join("codex-sessions/2026/09/04");
        std::fs::create_dir_all(&day).unwrap();
        day.join(format!("rollout-{name}.jsonl"))
    }

    fn root_header(id: &str, cwd: &Path) -> String {
        format!(
            concat!(
                r#"{{"timestamp":"2026-09-04T10:00:00Z","type":"session_meta","payload":{{"id":"{}","cwd":{},"cli_version":"0.152.1","source":"cli"}}}}"#,
                "\n"
            ),
            id,
            serde_json::to_string(&cwd.display().to_string()).unwrap()
        )
    }

    fn child_text(id: &str, parent: &str, path: &str, text: &str) -> String {
        format!(
            concat!(
                r#"{{"timestamp":"2026-09-04T10:00:01Z","type":"session_meta","payload":{{"id":"{}","cwd":"/workspace/demo","cli_version":"0.152.1","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{}","agent_path":"{}"}}}}}}}}}}"#,
                "\n",
                r#"{{"timestamp":"2026-09-04T10:00:02Z","type":"response_item","payload":{{"type":"message","id":"copied","role":"assistant","phase":"commentary","content":[{{"type":"output_text","text":"copied"}}]}}}}"#,
                "\n",
                r#"{{"type":"inter_agent_communication_metadata","payload":{{"trigger_turn":true}}}}"#,
                "\n",
                r#"{{"timestamp":"2026-09-04T10:00:03Z","type":"response_item","payload":{{"type":"message","id":"owned-{}","role":"assistant","phase":"commentary","content":[{{"type":"output_text","text":"{}"}}]}}}}"#,
                "\n"
            ),
            id, parent, path, id, text
        )
    }

    fn claude_prompt(text: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({
                "type": "user",
                "uuid": "prompt",
                "timestamp": "2026-09-04T10:00:00Z",
                "message": { "role": "user", "content": text }
            })
        )
    }

    async fn poll_batch(live: &mut LiveSession) -> Vec<SessionEvent> {
        let (tx, mut rx) = mpsc::channel(8);
        assert!(!poll_live(live, &tx).await);
        match rx.try_recv() {
            Ok(UiEvent::Batch { events, .. }) => events,
            Ok(other) => panic!("expected live batch, got {other:?}"),
            Err(_) => Vec::new(),
        }
    }

    #[tokio::test]
    async fn snapshot_decoder_gate_continues_into_appended_child_bytes_once() {
        let (base, roots) = roots("codex_snapshot_gate");
        let root = codex_file(&base, "root-gate");
        let child = codex_file(&base, "child-gate");
        std::fs::write(
            &root,
            root_header("root-gate", Path::new("/workspace/demo")),
        )
        .unwrap();
        std::fs::write(
            &child,
            child_text("child-gate", "root-gate", "/root/child", "first owned"),
        )
        .unwrap();
        let mut live = live_from_file(&root, roots);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&child)
            .unwrap()
            .write_all(concat!(
                r#"{"timestamp":"2026-09-04T10:00:04Z","type":"response_item","payload":{"type":"message","id":"owned-append","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"appended owned"}]}}"#,
                "\n"
            ).as_bytes())
            .unwrap();
        let events = poll_batch(&mut live).await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    &event.kind,
                    EventKind::AssistantText { text, .. } if text == "appended owned"
                ))
                .count(),
            1
        );
        assert!(!events.iter().any(|event| matches!(
            &event.kind,
            EventKind::AssistantText { text, .. } if text == "copied"
        )));
        assert!(poll_batch(&mut live).await.is_empty());
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn partial_appended_line_completes_once_and_counts_as_activity() {
        let (base, roots) = roots("partial_once");
        let root = codex_file(&base, "root-partial");
        std::fs::write(
            &root,
            root_header("root-partial", Path::new("/workspace/demo")),
        )
        .unwrap();
        let mut live = live_from_file(&root, roots);
        live.idle_ticks = SWITCH_IDLE_TICKS;
        let line = r#"{"timestamp":"2026-09-04T10:01:00Z","type":"response_item","payload":{"type":"message","id":"partial","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"complete once"}]}}"#;
        let split = line.len() / 2;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&root)
            .unwrap();
        file.write_all(&line.as_bytes()[..split]).unwrap();
        assert!(poll_batch(&mut live).await.is_empty());
        assert_eq!(live.idle_ticks, 0, "partial bytes prevent an idle switch");
        file.write_all(&line.as_bytes()[split..]).unwrap();
        file.write_all(b"\n").unwrap();
        let events = poll_batch(&mut live).await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    &event.kind,
                    EventKind::AssistantText { text, .. } if text == "complete once"
                ))
                .count(),
            1
        );
        assert!(poll_batch(&mut live).await.is_empty());
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn malformed_and_unknown_appended_bytes_reset_idle_without_visible_events() {
        let (base, roots) = roots("unknown_activity");
        let root = codex_file(&base, "root-unknown");
        std::fs::write(
            &root,
            root_header("root-unknown", Path::new("/workspace/demo")),
        )
        .unwrap();
        let mut live = live_from_file(&root, roots);
        live.idle_ticks = SWITCH_IDLE_TICKS;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&root)
            .unwrap()
            .write_all(b"not-json\n{\"type\":\"future_record\"}\n")
            .unwrap();
        assert!(poll_batch(&mut live).await.is_empty());
        assert_eq!(live.idle_ticks, 0);
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn late_direct_and_nested_children_attach_once_in_parent_first_order() {
        let (base, roots) = roots("late_nested");
        let root = codex_file(&base, "root-family");
        std::fs::write(
            &root,
            root_header("root-family", Path::new("/workspace/demo")),
        )
        .unwrap();
        let mut live = live_from_file(&root, roots);
        let child = codex_file(&base, "z-child");
        let grandchild = codex_file(&base, "a-grandchild");
        std::fs::write(
            &child,
            child_text(
                "z-parent-family",
                "root-family",
                "/root/child",
                "child owned",
            ),
        )
        .unwrap();
        std::fs::write(
            &grandchild,
            child_text(
                "a-nested-family",
                "z-parent-family",
                "/root/child/nested",
                "nested owned",
            ),
        )
        .unwrap();
        live.ticks = CATALOG_REFRESH_EVERY - 1;
        let events = poll_batch(&mut live).await;
        let discovered: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::AgentDiscovered(agent) => Some(agent.id.0.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(discovered, ["z-parent-family", "a-nested-family"]);
        let mut model = crate::state::session::SessionModel::new(SessionKey {
            provider: Provider::Codex,
            id: "root-family".into(),
        });
        for event in &events {
            model.apply_event(event);
        }
        let mut flow = crate::state::graph::new_flow();
        crate::state::graph::sync(&mut flow, &model, false);
        let parent_position = flow.node("z-parent-family").unwrap().position;
        let nested_position = flow.node("a-nested-family").unwrap().position;
        assert!(parent_position.y > flow.node("main").unwrap().position.y);
        assert!(nested_position.y > parent_position.y);
        assert_eq!(live.tracked.len(), 3);
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::AssistantText { text, .. } if text == "nested owned"
        )));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&child)
            .unwrap()
            .write_all(concat!(
                r#"{"timestamp":"2026-09-04T10:00:04Z","type":"response_item","payload":{"type":"message","id":"child-followup","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"child follow-up"}]}}"#,
                "\n"
            ).as_bytes())
            .unwrap();
        let follow_up = poll_batch(&mut live).await;
        assert_eq!(
            follow_up
                .iter()
                .filter(|event| matches!(
                    &event.kind,
                    EventKind::AssistantText { text, .. } if text == "child follow-up"
                ))
                .count(),
            1
        );
        assert!(!follow_up.iter().any(|event| matches!(
            &event.kind,
            EventKind::AssistantText { text, .. } if text == "copied"
        )));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&grandchild)
            .unwrap()
            .write_all(concat!(
                r#"{"timestamp":"2026-09-04T10:00:05Z","type":"response_item","payload":{"type":"message","id":"nested-followup","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"nested follow-up"}]}}"#,
                "\n"
            ).as_bytes())
            .unwrap();
        let nested_follow_up = poll_batch(&mut live).await;
        assert_eq!(
            nested_follow_up
                .iter()
                .filter(|event| matches!(
                    &event.kind,
                    EventKind::AssistantText { text, .. } if text == "nested follow-up"
                ))
                .count(),
            1
        );
        assert!(!nested_follow_up.iter().any(|event| matches!(
            &event.kind,
            EventKind::AssistantText { text, .. } if text == "copied"
        )));
        live.ticks = CATALOG_REFRESH_EVERY - 1;
        assert!(poll_batch(&mut live).await.is_empty());
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn catalog_refresh_is_throttled_between_poll_ticks() {
        let (base, roots) = roots("refresh_throttle");
        let root = codex_file(&base, "root-throttle");
        std::fs::write(
            &root,
            root_header("root-throttle", Path::new("/workspace/demo")),
        )
        .unwrap();
        let mut live = live_from_file(&root, roots);
        let child = codex_file(&base, "child-throttle");
        std::fs::write(
            &child,
            child_text("child-throttle", "root-throttle", "/root/child", "late"),
        )
        .unwrap();
        for _ in 0..CATALOG_REFRESH_EVERY - 1 {
            assert!(poll_batch(&mut live).await.is_empty());
        }
        assert!(!live.tracked.contains_key(&child.canonicalize().unwrap()));
        let events = poll_batch(&mut live).await;
        assert!(!events.is_empty());
        assert!(live.tracked.contains_key(&child.canonicalize().unwrap()));
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn explicit_file_stays_pinned_when_a_new_root_appears() {
        let (base, roots) = roots("explicit_pin");
        let cwd = base.join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let root = codex_file(&base, "root-pinned");
        std::fs::write(&root, root_header("root-pinned", &cwd)).unwrap();
        let mut live = live_from_file(&root, roots);
        let newer = codex_file(&base, "root-newer");
        std::fs::write(&newer, root_header("root-newer", &cwd)).unwrap();
        live.idle_ticks = SWITCH_IDLE_TICKS;
        live.ticks = CATALOG_REFRESH_EVERY - 1;
        let (tx, _rx) = mpsc::channel(8);
        assert!(!poll_live(&mut live, &tx).await);
        assert_eq!(live.manifest.root.key.id, "root-pinned");
        assert_eq!(live.manifest.root.key.provider, Provider::Codex);
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn quiet_directory_watch_switches_to_a_newer_other_provider_root() {
        let (base, roots) = roots("cross_provider_switch");
        let cwd = base.join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let claude_project = roots
            .claude_projects
            .join(crate::transcript::sanitize_cwd(&cwd));
        std::fs::create_dir_all(&claude_project).unwrap();
        let claude = claude_project.join("11111111-1111-1111-1111-111111111111.jsonl");
        std::fs::write(&claude, "{}\n").unwrap();
        let mut catalog = SessionCatalog::new(roots.clone());
        let manifest = catalog
            .manifest(&WatchTarget::File(claude.clone()))
            .unwrap();
        let snapshot = load_snapshot(&manifest);
        let mut live = LiveSession::from_snapshot(
            WatchTarget::LatestForCwd(cwd.clone()),
            catalog,
            manifest,
            snapshot.tracked,
            snapshot.pending_metadata,
            1.0,
        );
        let codex = codex_file(&base, "new-codex-root");
        std::fs::write(&codex, root_header("new-codex-root", &cwd)).unwrap();
        live.idle_ticks = SWITCH_IDLE_TICKS;
        live.ticks = CATALOG_REFRESH_EVERY - 1;
        let (tx, _rx) = mpsc::channel(8);
        assert!(poll_live(&mut live, &tx).await);
        assert_eq!(live.manifest.root.key.provider, Provider::Claude);
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn active_claude_manifest_refresh_does_not_scan_codex_history() {
        let (base, roots) = roots("active_claude_no_codex_scan");
        let cwd = base.join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let claude_project = roots
            .claude_projects
            .join(crate::transcript::sanitize_cwd(&cwd));
        std::fs::create_dir_all(&claude_project).unwrap();
        let claude = claude_project.join("22222222-2222-2222-2222-222222222222.jsonl");
        std::fs::write(&claude, "{}\n").unwrap();
        let mut catalog = SessionCatalog::new(roots.clone());
        let manifest = catalog
            .manifest(&WatchTarget::File(claude.clone()))
            .unwrap();
        let snapshot = load_snapshot(&manifest);
        let mut live = LiveSession::from_snapshot(
            WatchTarget::LatestForCwd(cwd.clone()),
            catalog,
            manifest,
            snapshot.tracked,
            snapshot.pending_metadata,
            1.0,
        );
        let codex = codex_file(&base, "unscanned-root");
        std::fs::write(&codex, root_header("unscanned-root", &cwd)).unwrap();
        live.idle_ticks = SWITCH_IDLE_TICKS;
        live.ticks = CATALOG_REFRESH_EVERY - 1;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&claude)
            .unwrap()
            .write_all(claude_prompt("same-tick activity").as_bytes())
            .unwrap();
        let events = poll_batch(&mut live).await;
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::Prompt { text } if text == "same-tick activity"
        )));
        assert_eq!(live.catalog.last_refresh_stats().candidates_read, 0);
        assert_eq!(live.idle_ticks, 0);
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn truncation_reloads_once_without_duplicate_codex_facts_and_keeps_speed() {
        let (base, roots) = roots("codex_reload_once");
        let root = codex_file(&base, "reload-root");
        let body = concat!(
            r#"{"timestamp":"2026-09-04T10:00:01Z","type":"response_item","payload":{"type":"function_call","call_id":"tool","name":"exec_command","arguments":"{\"command\":\"true\"}"}}"#,
            "\n",
            r#"{"timestamp":"2026-09-04T10:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"output_tokens":7}}}}"#,
            "\n"
        );
        std::fs::write(
            &root,
            format!(
                "{}{}{}",
                root_header("reload-root", Path::new("/workspace/demo")),
                body,
                " ".repeat(256)
            ),
        )
        .unwrap();
        let mut live = live_from_file(&root, roots);
        live.replay_speed = 3.5;
        std::fs::write(
            &root,
            format!(
                "{}{}",
                root_header("reload-root", Path::new("/workspace/demo")),
                body
            ),
        )
        .unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(!poll_live(&mut live, &tx).await);
        assert!(matches!(rx.try_recv(), Ok(UiEvent::SessionReset { .. })));
        let items = match rx.try_recv().unwrap() {
            UiEvent::ReplayLoaded { items, speed, .. } => {
                assert_eq!(speed, 3.5);
                items
            }
            other => panic!("expected one reload, got {other:?}"),
        };
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item.event.kind, EventKind::ToolStarted(_)))
                .count(),
            1
        );
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item.event.kind, EventKind::UsageObserved(_)))
                .count(),
            1
        );
        assert!(rx.try_recv().is_err());
        assert!(poll_batch(&mut live).await.is_empty());
        assert!(matches!(live.original_target, WatchTarget::File(_)));
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rotation_reloads_once_from_the_new_handle_without_duplicate_facts() {
        let (base, roots) = roots("codex_rotate_once");
        let root = codex_file(&base, "rotate-root");
        let body = concat!(
            r#"{"timestamp":"2026-09-04T10:00:01Z","type":"response_item","payload":{"type":"function_call","call_id":"tool","name":"exec_command","arguments":"{}"}}"#,
            "\n",
            r#"{"timestamp":"2026-09-04T10:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"output_tokens":9}}}}"#,
            "\n"
        );
        std::fs::write(
            &root,
            root_header("rotate-root", Path::new("/workspace/demo")),
        )
        .unwrap();
        let mut live = live_from_file(&root, roots);
        let incoming = root.with_extension("incoming");
        std::fs::write(
            &incoming,
            format!(
                "{}{}",
                root_header("rotate-root", Path::new("/workspace/demo")),
                body
            ),
        )
        .unwrap();
        std::fs::rename(&incoming, &root).unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(!poll_live(&mut live, &tx).await);
        assert!(matches!(rx.try_recv(), Ok(UiEvent::SessionReset { .. })));
        let items = match rx.try_recv().unwrap() {
            UiEvent::ReplayLoaded { items, .. } => items,
            other => panic!("expected rotated replay, got {other:?}"),
        };
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item.event.kind, EventKind::ToolStarted(_)))
                .count(),
            1
        );
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item.event.kind, EventKind::UsageObserved(_)))
                .count(),
            1
        );
        assert!(rx.try_recv().is_err());
        assert!(poll_batch(&mut live).await.is_empty());
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn late_family_live_fold_converges_with_a_fresh_bulk_snapshot() {
        use crate::state::session::SessionModel;

        let (base, roots) = roots("bulk_live_convergence");
        let root = codex_file(&base, "root-converge");
        std::fs::write(
            &root,
            root_header("root-converge", Path::new("/workspace/demo")),
        )
        .unwrap();
        let mut catalog = SessionCatalog::new(roots.clone());
        let target = WatchTarget::File(root.clone());
        let manifest = catalog.manifest(&target).unwrap();
        let snapshot = load_snapshot(&manifest);
        let mut live_model = SessionModel::new(snapshot.key.clone());
        for item in &snapshot.items {
            live_model.apply_event(&item.event);
        }
        let mut live = LiveSession::from_snapshot(
            target,
            catalog,
            manifest,
            snapshot.tracked,
            snapshot.pending_metadata,
            1.0,
        );
        let child = codex_file(&base, "child-converge");
        std::fs::write(
            &child,
            child_text(
                "child-converge",
                "root-converge",
                "/root/child",
                "converged output",
            ),
        )
        .unwrap();
        live.ticks = CATALOG_REFRESH_EVERY - 1;
        for event in poll_batch(&mut live).await {
            live_model.apply_event(&event);
        }

        let mut fresh_catalog = SessionCatalog::new(roots);
        let fresh_manifest = fresh_catalog.manifest(&WatchTarget::File(root)).unwrap();
        let fresh = load_snapshot(&fresh_manifest);
        let mut fresh_model = SessionModel::new(fresh.key);
        for item in &fresh.items {
            fresh_model.apply_event(&item.event);
        }
        assert_eq!(live_model.spawn_order, fresh_model.spawn_order);
        let live_child = live_model.agent("child-converge").unwrap();
        let fresh_child = fresh_model.agent("child-converge").unwrap();
        assert_eq!(live_child.parent, fresh_child.parent);
        assert_eq!(live_child.agent_type, fresh_child.agent_type);
        assert_eq!(live_child.assistant_text, fresh_child.assistant_text);
        assert_eq!(live_child.output_tokens, fresh_child.output_tokens);
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn temporarily_missing_family_file_is_retained_and_reappears_once() {
        let (base, roots) = roots("missing_retained");
        let root = codex_file(&base, "root-retain");
        let child = codex_file(&base, "child-retain");
        std::fs::write(
            &root,
            root_header("root-retain", Path::new("/workspace/demo")),
        )
        .unwrap();
        std::fs::write(
            &child,
            child_text("child-retain", "root-retain", "/root/child", "owned"),
        )
        .unwrap();
        let child = child.canonicalize().unwrap();
        let mut live = live_from_file(&root, roots);
        assert!(live.tracked.contains_key(&child));
        std::fs::remove_file(&child).unwrap();
        live.ticks = CATALOG_REFRESH_EVERY - 1;
        assert!(poll_batch(&mut live).await.is_empty());
        assert!(live.tracked.contains_key(&child));

        std::fs::write(
            &child,
            child_text("child-retain", "root-retain", "/root/child", "reappeared"),
        )
        .unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(!poll_live(&mut live, &tx).await);
        assert!(matches!(rx.try_recv(), Ok(UiEvent::SessionReset { .. })));
        let items = match rx.try_recv().unwrap() {
            UiEvent::ReplayLoaded { items, .. } => items,
            other => panic!("expected reappeared family replay, got {other:?}"),
        };
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(
                    &item.event.kind,
                    EventKind::AssistantText { text, .. } if text == "reappeared"
                ))
                .count(),
            1
        );
        assert!(rx.try_recv().is_err());
        assert!(poll_batch(&mut live).await.is_empty());
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn partial_child_replacement_waits_for_a_complete_family_header() {
        let (base, roots) = roots("partial_child_replacement");
        let root = codex_file(&base, "root");
        let child = codex_file(&base, "child");
        std::fs::write(&root, root_header("root", Path::new("/workspace/demo"))).unwrap();
        std::fs::write(
            &child,
            format!(
                "{}{}",
                child_text("old-child", "root", "/root/old", "old output"),
                " ".repeat(1024)
            ),
        )
        .unwrap();
        let mut live = live_from_file(&root, roots);

        let replacement = child_text("new-child", "root", "/root/new", "new output");
        let split = replacement.find('\n').unwrap() / 2;
        std::fs::write(&child, &replacement.as_bytes()[..split]).unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(!poll_live(&mut live, &tx).await);
        assert!(!live.pending_resets.is_empty());
        assert!(rx.try_recv().is_err(), "partial child emits no reset");

        std::fs::OpenOptions::new()
            .append(true)
            .open(&child)
            .unwrap()
            .write_all(&replacement.as_bytes()[split..])
            .unwrap();
        assert!(!poll_live(&mut live, &tx).await);
        assert!(live.pending_resets.is_empty());
        assert!(matches!(rx.try_recv(), Ok(UiEvent::SessionReset { .. })));
        let items = match rx.try_recv().unwrap() {
            UiEvent::ReplayLoaded { items, .. } => items,
            other => panic!("expected complete child replay, got {other:?}"),
        };
        assert!(items.iter().any(|item| item.event.actor.0 == "new-child"));
        assert!(!items.iter().any(|item| item.event.actor.0 == "old-child"));
        assert!(rx.try_recv().is_err());
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn replaced_child_path_cannot_import_an_unrelated_family() {
        let (base, roots) = roots("unrelated_child_replacement");
        let root = codex_file(&base, "root");
        let child = codex_file(&base, "child");
        std::fs::write(&root, root_header("root", Path::new("/workspace/demo"))).unwrap();
        std::fs::write(
            &child,
            child_text("old-child", "root", "/root/old", "old output"),
        )
        .unwrap();
        let child = child.canonicalize().unwrap();
        let mut live = live_from_file(&root, roots);

        std::fs::write(
            &child,
            root_header("foreign-root", Path::new("/workspace/other")),
        )
        .unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(!poll_live(&mut live, &tx).await);
        assert!(live.pending_resets.is_empty());
        assert!(!live.tracked.contains_key(&child));
        assert!(matches!(rx.try_recv(), Ok(UiEvent::SessionReset { .. })));
        let items = match rx.try_recv().unwrap() {
            UiEvent::ReplayLoaded { items, .. } => items,
            other => panic!("expected isolated family replay, got {other:?}"),
        };
        assert!(
            !items.iter().any(|item| {
                matches!(item.event.actor.0.as_str(), "old-child" | "foreign-root")
            })
        );
        assert!(rx.try_recv().is_err());
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn partial_root_replacement_waits_for_valid_new_identity_before_reset() {
        let (base, roots) = roots("partial_replacement");
        let root = codex_file(&base, "replace-root");
        std::fs::write(
            &root,
            format!(
                "{}{}",
                root_header("old-root", Path::new("/workspace/demo")),
                " ".repeat(256)
            ),
        )
        .unwrap();
        let mut live = live_from_file(&root, roots);
        let replacement = root_header("new-root", Path::new("/workspace/demo"));
        let split = replacement.len() / 2;
        std::fs::write(&root, &replacement.as_bytes()[..split]).unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(!poll_live(&mut live, &tx).await);
        assert!(!live.pending_resets.is_empty());
        assert_eq!(live.manifest.root.key.id, "old-root");
        assert!(rx.try_recv().is_err(), "no stale-key reset is emitted");

        std::fs::OpenOptions::new()
            .append(true)
            .open(&root)
            .unwrap()
            .write_all(&replacement.as_bytes()[split..])
            .unwrap();
        assert!(!poll_live(&mut live, &tx).await);
        assert!(live.pending_resets.is_empty());
        assert_eq!(live.manifest.root.key.id, "new-root");
        assert!(matches!(
            rx.try_recv(),
            Ok(UiEvent::SessionReset { session }) if session.id == "new-root"
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(UiEvent::ReplayLoaded { session, .. }) if session.id == "new-root"
        ));
        assert!(rx.try_recv().is_err());
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn new_root_identity_does_not_inherit_the_replaced_roots_children() {
        let (base, roots) = roots("replacement_family_isolation");
        let root = codex_file(&base, "same-path");
        let child = codex_file(&base, "old-child");
        std::fs::write(
            &root,
            format!(
                "{}{}",
                root_header("old-root", Path::new("/workspace/demo")),
                " ".repeat(256)
            ),
        )
        .unwrap();
        std::fs::write(
            &child,
            child_text(
                "old-child",
                "old-root",
                "/root/old-child",
                "old child output",
            ),
        )
        .unwrap();
        let child = child.canonicalize().unwrap();
        let mut live = live_from_file(&root, roots);
        assert!(live.tracked.contains_key(&child));
        std::fs::write(&root, root_header("new-root", Path::new("/workspace/demo"))).unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(!poll_live(&mut live, &tx).await);
        assert_eq!(live.manifest.root.key.id, "new-root");
        assert!(!live.tracked.contains_key(&child));
        assert!(matches!(rx.try_recv(), Ok(UiEvent::SessionReset { .. })));
        let items = match rx.try_recv().unwrap() {
            UiEvent::ReplayLoaded { items, .. } => items,
            other => panic!("expected replacement replay, got {other:?}"),
        };
        assert!(!items.iter().any(|item| item.event.actor.0 == "old-child"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn claude_path_replacement_waits_then_adopts_valid_codex_identity() {
        let (base, roots) = roots("claude_to_codex_replacement");
        std::fs::create_dir_all(&base).unwrap();
        let root = base.join("88888888-8888-8888-8888-888888888888.jsonl");
        std::fs::write(
            &root,
            format!("{}{}", claude_prompt("old"), " ".repeat(256)),
        )
        .unwrap();
        let mut live = live_from_file(&root, roots);
        let replacement = root_header("new-codex", Path::new("/workspace/demo"));
        let split = replacement.len() / 2;
        std::fs::write(&root, &replacement.as_bytes()[..split]).unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(!poll_live(&mut live, &tx).await);
        assert!(!live.pending_resets.is_empty());
        assert!(rx.try_recv().is_err());
        std::fs::OpenOptions::new()
            .append(true)
            .open(&root)
            .unwrap()
            .write_all(&replacement.as_bytes()[split..])
            .unwrap();
        assert!(!poll_live(&mut live, &tx).await);
        assert_eq!(live.manifest.root.key.provider, Provider::Codex);
        assert_eq!(live.manifest.root.key.id, "new-codex");
        assert!(matches!(
            rx.try_recv(),
            Ok(UiEvent::SessionReset { session }) if session.provider == Provider::Codex
        ));
        let _ = std::fs::remove_dir_all(base);
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
            1.0,
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
        refresh_manifest(&mut live, &mut events, false);
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
            1.0,
        );
        live.idle_ticks = SWITCH_IDLE_TICKS;
        live.ticks = CATALOG_REFRESH_EVERY - 1;
        let (tx, mut rx) = mpsc::channel(8);
        assert!(poll_live(&mut live, &tx).await);
        assert!(
            rx.try_recv().is_err(),
            "the reattach emits one reset after reload"
        );
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
        let (tx, mut rx) = mpsc::channel(8);
        assert!(!poll_live(&mut live, &tx).await);
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
        std::fs::write(&main, claude_prompt("root")).unwrap();
        let child = sub_dir.join("agent-bbbbbbbbbbbbbbbbb.jsonl");
        std::fs::write(
            &child,
            format!("{}{}", claude_prompt("old child"), " ".repeat(256)),
        )
        .unwrap();
        let mut live = live_from_file(&main, roots);
        std::fs::write(&child, claude_prompt("replacement child")).unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(!poll_live(&mut live, &tx).await);
        assert!(matches!(rx.try_recv(), Ok(UiEvent::SessionReset { .. })));
        assert!(matches!(rx.try_recv(), Ok(UiEvent::ReplayLoaded { .. })));
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn truncation_triggers_full_reattach() {
        let (base, roots) = roots("root_truncate");
        std::fs::create_dir_all(&base).unwrap();
        let main = base.join("44444444-4444-4444-4444-444444444444.jsonl");
        std::fs::write(
            &main,
            format!("{}{}", claude_prompt("old"), " ".repeat(256)),
        )
        .unwrap();
        let mut live = live_from_file(&main, roots);
        std::fs::write(&main, claude_prompt("new")).unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(!poll_live(&mut live, &tx).await);
        assert!(matches!(rx.try_recv(), Ok(UiEvent::SessionReset { .. })));
        assert!(matches!(rx.try_recv(), Ok(UiEvent::ReplayLoaded { .. })));
        assert!(matches!(live.original_target, WatchTarget::File(_)));
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn reload_keeps_malformed_claude_sidecar_retryable() {
        let (base, roots) = roots("reload_meta_retry");
        let session = "77777777-7777-7777-7777-777777777777";
        let main = base.join(format!("{session}.jsonl"));
        let subagents = base.join(session).join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(
            &main,
            format!("{}{}", claude_prompt("old"), " ".repeat(256)),
        )
        .unwrap();
        std::fs::write(subagents.join("agent-ccccccccccccccccc.jsonl"), "{}\n").unwrap();
        let metadata = subagents.join("agent-ccccccccccccccccc.meta.json");
        std::fs::write(&metadata, r#"{"agentTy"#).unwrap();
        let metadata = metadata.canonicalize().unwrap();
        let mut live = live_from_file(&main, roots);
        std::fs::write(&main, claude_prompt("new")).unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        assert!(!poll_live(&mut live, &tx).await);
        assert!(matches!(rx.try_recv(), Ok(UiEvent::SessionReset { .. })));
        assert!(matches!(rx.try_recv(), Ok(UiEvent::ReplayLoaded { .. })));
        assert!(!live.manifest.files.iter().any(|file| file.path == metadata));

        std::fs::write(&metadata, r#"{"agentType":"guide"}"#).unwrap();
        let mut events = Vec::new();
        refresh_manifest(&mut live, &mut events, false);
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::AgentDiscovered(agent) if agent.id.0 == "ccccccccccccccccc"
        )));
        let _ = std::fs::remove_dir_all(base);
    }
}
