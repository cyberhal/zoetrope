//! Background task: live tailing and replay pacing.
//!
//! A single tailer task owns every file, byte cursor, and decoder in the watched
//! session. It polls tracked files frequently while throttling manifest/catalog
//! refreshes; late family members attach from byte zero through the same
//! provider decoder used by replay. Replay parses everything up front and hands
//! the merged timeline to the application-owned playhead.
//!
//! Everything is stamped with a provider-qualified session key; the UI drops
//! events whose key is not current (see [`crate::state::App::is_current`]).
//!
//! Layout: this module holds the task entry (`run`) and provider-neutral feeder
//! messages ([`TailRequest`] / [`UiEvent`]); `bytes` is the pure incremental
//! reader, `live` the live poll loop, and `replay` the up-front assembly. Both
//! feeders converge on `live::tail_loop` so every session keeps tailing.

#[cfg(feature = "native")]
use tokio::sync::mpsc;

use crate::event::{SessionEvent, SessionKey};

#[cfg(test)]
use crate::transcript::{Entry, SubagentMeta};

// Portable: the timeline item + its ordering (no IO → compiles on wasm).
pub(crate) mod item;
pub use item::ReplayItem;
pub(crate) use item::Timing;
#[cfg(test)]
pub(crate) use item::date_and_sort;
pub(crate) use item::date_and_sort_live;
pub use item::{DemoSubagent, replay_from_jsonl, replay_from_session};

// Native-only feeders: incremental byte reading, live polling, replay assembly —
// they pull tokio + the filesystem, so the `native` feature gates them out of the
// portable core (the browser frontend feeds bytes straight in, no tailing).
#[cfg(feature = "native")]
pub(crate) mod bytes;
#[cfg(feature = "native")]
mod live;
#[cfg(feature = "native")]
mod replay;

#[cfg(feature = "native")]
use live::run_live;
#[cfg(feature = "native")]
use replay::run_replay;

/// Requests the UI sends to the tailer task.
///
/// The App owns the playhead (unified Timeline model), so the tailer is a pure
/// feeder — its only request is which session to watch.
#[derive(Debug, Clone)]
#[cfg(feature = "native")]
pub enum TailRequest {
    /// Switch to watching/replaying a session. In live mode the tailer
    /// discovers the session file under the project dir; in replay it is the
    /// explicit transcript path.
    Watch(crate::session_catalog::WatchTarget),
}

/// Events the tailer task sends to the UI.
#[derive(Debug)]
pub enum UiEvent {
    /// A batch of updates produced in one live poll tick (appended to the
    /// timeline's head as they arrive).
    Batch {
        session: SessionKey,
        events: Vec<SessionEvent>,
    },
    /// The whole merged, timestamp-ordered replay stream, handed to the App
    /// once. The App owns pacing/seeking from here (the tailer does not pace).
    /// `info` carries the untimed session-level metadata (kept off the timeline).
    ReplayLoaded {
        session: SessionKey,
        items: Vec<ReplayItem>,
        speed: f64,
        info: crate::state::SessionInfo,
    },
    /// File truncation/rotation detected — the UI should reset its model.
    SessionReset { session: SessionKey },
    /// A non-fatal error string for display.
    Error(String),
}

/// Claude-shaped fixture input retained only so the pre-cutover behavior tests
/// exercise the real adapter before reaching neutral consumers.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) enum Update {
    Entry {
        source: Source,
        entry: Entry,
    },
    SubagentMeta {
        agent_id: String,
        workflow: Option<String>,
        meta: SubagentMeta,
    },
    Event(SessionEvent),
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Source {
    Main,
    Sub(String),
    Journal(String),
}

// ---------------------------------------------------------------------------
// Public task entry point
// ---------------------------------------------------------------------------

/// Run the tailer task: receive [`TailRequest`]s, emit [`UiEvent`]s.
///
/// Lives for the program's duration; switches sessions on
/// [`TailRequest::Watch`]. `replay` selects live-tail vs timestamp-paced replay;
/// `speed` is the replay speed multiplier (ignored in live mode).
#[cfg(feature = "native")]
pub async fn run(
    mut req_rx: mpsc::Receiver<TailRequest>,
    ui_tx: mpsc::Sender<UiEvent>,
    replay: bool,
    speed: f64,
) -> anyhow::Result<()> {
    // Wait for the first Watch before doing anything (Watch is the only request).
    let mut current = match wait_for_watch(&mut req_rx).await {
        Some(path) => path,
        None => return Ok(()),
    };

    loop {
        let next = if replay {
            run_replay(&current, &ui_tx, &mut req_rx, speed).await
        } else {
            run_live(&current, &ui_tx, &mut req_rx).await
        };

        match next {
            Flow::Switch(target) => current = target,
            Flow::Reattach => {}
            Flow::Exit => return Ok(()),
        }
    }
}

/// What to do after a live/replay session loop returns.
#[cfg(feature = "native")]
pub(crate) enum Flow {
    /// Replace the current watch intent after a `Watch` request.
    Switch(crate::session_catalog::WatchTarget),
    /// Rebuild the current target without changing directory-follow vs pinning.
    Reattach,
    /// The request channel closed — shut down.
    Exit,
}

/// Block until the first [`TailRequest::Watch`].
#[cfg(feature = "native")]
async fn wait_for_watch(
    req_rx: &mut mpsc::Receiver<TailRequest>,
) -> Option<crate::session_catalog::WatchTarget> {
    match req_rx.recv().await? {
        TailRequest::Watch(path) => Some(path),
    }
}
