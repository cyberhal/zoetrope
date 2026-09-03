//! Native snapshot replay followed by tailing from the exact snapshot boundary.

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::session_catalog::{DiscoveryRoots, SessionCatalog, WatchTarget};
use crate::session_loader::load_snapshot;

use super::live::{LiveSession, tail_loop};
use super::{Flow, TailRequest, UiEvent};

pub(crate) async fn run_replay(
    target: &WatchTarget,
    ui_tx: &mpsc::Sender<UiEvent>,
    req_rx: &mut mpsc::Receiver<TailRequest>,
    speed: f64,
) -> Flow {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let mut catalog = SessionCatalog::new(DiscoveryRoots::from_home(&home));
    let Some(manifest) = catalog.manifest(target) else {
        let display = match target {
            WatchTarget::File(path) | WatchTarget::LatestForCwd(path) => path.display(),
        };
        let _ = ui_tx
            .send(UiEvent::Error(format!(
                "unrecognized session file: {display}"
            )))
            .await;
        return Flow::Exit;
    };
    let snapshot = match tokio::task::spawn_blocking({
        let manifest = manifest.clone();
        move || load_snapshot(&manifest)
    })
    .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let _ = ui_tx
                .send(UiEvent::Error(format!("failed to load session: {error}")))
                .await;
            return Flow::Exit;
        }
    };
    let session = snapshot.key.clone();
    if ui_tx
        .send(UiEvent::ReplayLoaded {
            session: session.clone(),
            items: snapshot.items,
            speed: if speed > 0.0 { speed } else { 1.0 },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventKind;
    use std::io::Write;

    #[test]
    fn replay_dates_metadata_to_first_subagent_activity() {
        let base =
            std::env::temp_dir().join(format!("zoetrope_replay_order_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let session = "55555555-5555-5555-5555-555555555555";
        let sub_dir = base.join(session).join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let main = base.join(format!("{session}.jsonl"));
        std::fs::File::create(&main)
            .unwrap()
            .write_all(concat!(
                r#"{"type":"user","uuid":"u1","parentUuid":null,"timestamp":"2026-06-05T10:00:00Z","message":{"role":"user","content":"start"}}"#,
                "\n",
                r#"{"type":"user","uuid":"u2","parentUuid":null,"timestamp":"2026-06-05T10:02:00Z","message":{"role":"user","content":"later"}}"#,
                "\n",
            ).as_bytes())
            .unwrap();
        std::fs::write(
            sub_dir.join("agent-aaaaaaaaaaaaaaaaa.jsonl"),
            concat!(
                r#"{"type":"user","uuid":"s1","isSidechain":true,"agentId":"aaaaaaaaaaaaaaaaa","timestamp":"2026-06-05T10:01:00Z","message":{"role":"user","content":"task"}}"#,
                "\n",
            ),
        )
        .unwrap();
        std::fs::write(
            sub_dir.join("agent-aaaaaaaaaaaaaaaaa.meta.json"),
            r#"{"agentType":"guide","toolUseId":"t1"}"#,
        )
        .unwrap();

        let manifest = crate::session_loader::manifest_for_file(&main).unwrap();
        let snapshot = load_snapshot(&manifest);
        let discovered = snapshot
            .items
            .iter()
            .position(|item| {
                matches!(
                    &item.event.kind,
                    EventKind::AgentDiscovered(agent) if agent.id.0 == "aaaaaaaaaaaaaaaaa"
                )
            })
            .unwrap();
        let child = snapshot
            .items
            .iter()
            .position(|item| {
                item.event.actor.0 == "aaaaaaaaaaaaaaaaa"
                    && matches!(item.event.kind, EventKind::Activity)
            })
            .unwrap();
        assert_eq!(
            snapshot.items[discovered].ts(),
            Some("2026-06-05T10:01:00Z".parse().unwrap())
        );
        assert!(
            discovered < child,
            "birth metadata sorts before child activity"
        );
        let _ = std::fs::remove_dir_all(base);
    }
}
