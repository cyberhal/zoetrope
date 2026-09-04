//! zoetrope — visualize coding-agent sessions as a live flow graph.
//!
//! CLI (hand-rolled over `std::env::args`, no clap):
//!
//! ```text
//! zoe                       follow the current project's live session
//! zoe <file.jsonl>          replay a recording, played from the start
//! zoe <dir>                 follow another project's live session
//! zoe <file> --follow       follow a file's live edge instead of replaying
//! zoe <file> --speed N      playback speed multiplier (default 8.0)
//! zoe inspect <file.jsonl>  headless: print the session tree + info
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::mpsc;

use zoetrope::event::{Provider, SessionKey};
use zoetrope::session_catalog::{DiscoveryRoots, SessionCatalog, WatchTarget};
use zoetrope::session_loader::{load_snapshot, manifest_for_file};
use zoetrope::state::session::{AgentKind, SessionModel, ToolState};
use zoetrope::state::{App, Mode};
use zoetrope::tailer::{TailRequest, UiEvent};
use zoetrope::{tailer, tui};

/// Channel capacity for the bounded request/event channels.
const CHANNEL_CAP: usize = 32;

/// Parsed CLI invocation.
///
/// One TUI command (`View`) over the unified timeline engine, plus the headless
/// `Inspect`. The launch only sets defaults: *what* to open and *where the
/// playhead starts*. Everything (scrub, follow, go-live, pause) is available
/// once running, regardless of how it launched.
#[derive(Debug, Clone)]
pub enum Cli {
    /// View a session in the TUI. `target`: a session file (replay it from the
    /// start), a project dir (follow its live session), or `None` (the current
    /// project). `follow` starts at the live edge instead of replaying.
    View {
        target: Option<PathBuf>,
        follow: bool,
        speed: f64,
    },
    /// Headless: parse and print the session tree + info; no TUI.
    Inspect { file: PathBuf },
}

/// Default replay speed multiplier.
const DEFAULT_REPLAY_SPEED: f64 = 8.0;

const USAGE: &str = "\
zoetrope — visualize Claude Code and Codex sessions as a flow graph

USAGE:
    zoe                     follow the current project's live session
    zoe <file.jsonl>        replay a recording, played from the start
    zoe <dir>               follow another project's live session
    zoe <file> --follow     follow a file's live edge instead of replaying
    zoe <file> --speed N    playback speed (default 8.0)
    zoe inspect <file>      headless: print the session tree + info
    zoe --version           print the version and exit

Once open, scrub/follow/pause/go-live are available no matter how you launched.";

/// Parse `std::env::args` into a [`Cli`]. Returns a usage error on bad input.
fn parse_cli(args: impl Iterator<Item = String>) -> Result<Cli> {
    // Skip argv[0].
    let mut args = args.skip(1).peekable();

    // `inspect <file>` is the one distinct (headless) subcommand.
    if args.peek().map(String::as_str) == Some("inspect") {
        args.next();
        let file = args
            .next()
            .ok_or_else(|| anyhow!("inspect requires a <file.jsonl>\n\n{USAGE}"))?;
        if args.next().is_some() {
            bail!("inspect takes a single file argument\n\n{USAGE}");
        }
        return Ok(Cli::Inspect {
            file: PathBuf::from(file),
        });
    }

    // Otherwise: an optional positional target + flags.
    let mut target: Option<PathBuf> = None;
    let mut follow = false;
    let mut speed = DEFAULT_REPLAY_SPEED;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            // Packaging depends on this: the Homebrew formula's `test do`
            // block runs `zoe --version`, and it has to exit 0.
            "-V" | "--version" => {
                println!("zoe {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--follow" => follow = true,
            "--speed" => {
                let v = args
                    .next()
                    .ok_or_else(|| anyhow!("--speed requires a number\n\n{USAGE}"))?;
                speed = v
                    .parse::<f64>()
                    .with_context(|| format!("invalid --speed value: {v:?}"))?;
                if !(speed.is_finite() && speed > 0.0) {
                    bail!("--speed must be a positive number, got {v:?}");
                }
            }
            other if other.starts_with('-') => {
                bail!("unknown flag {other:?}\n\n{USAGE}");
            }
            _ => {
                if target.is_some() {
                    bail!("expected a single path argument\n\n{USAGE}");
                }
                target = Some(PathBuf::from(arg));
            }
        }
    }

    Ok(Cli::View {
        target,
        follow,
        speed,
    })
}

/// Run the `inspect` subcommand: fully parse the session and print a tree to
/// stdout. Returns an error (non-zero exit) on an unreadable file. This is the
/// headless smoke test — no TTY required.
async fn run_inspect(file: PathBuf) -> Result<()> {
    if !file.is_file() {
        bail!("not a readable file: {}", file.display());
    }
    let manifest = manifest_for_file(&file)
        .ok_or_else(|| anyhow!("unrecognized session file: {}", file.display()))?;
    let snapshot = load_snapshot(&manifest).context("loading session snapshot")?;
    let mut model = SessionModel::new(snapshot.key);
    for item in &snapshot.items {
        model.apply_event(&item.event);
    }
    model.recompute_workflow_status();
    model.recompute_liveness(Some(chrono::Utc::now()));
    let info = snapshot.info;

    let title = info.title.as_deref().unwrap_or("(untitled)");
    println!("session {} — {title}", model.session.id);
    // Session-level metadata (the `i` overlay's content, headless).
    println!(
        "  mode: {} · permission: {}",
        info.mode.as_deref().unwrap_or("—"),
        info.permission_mode.as_deref().unwrap_or("—"),
    );
    println!(
        "  {} agent(s), {} tool call(s) · {} queued · {} file edit(s)",
        model.agent_count(),
        model.tool_count(),
        info.queued_ops,
        info.file_snapshots,
    );
    if let Some(p) = &info.last_prompt {
        println!("  last prompt: {p:?}");
    }
    println!();

    // Print roots first, then children indented underneath, in spawn order.
    print_agent_tree(&model, None, 0);

    Ok(())
}

/// Recursively print agents whose `parent` equals `parent`, in spawn order.
fn print_agent_tree(model: &SessionModel, parent: Option<&str>, depth: usize) {
    for id in &model.spawn_order {
        let Some(agent) = model.agent(id) else {
            continue;
        };
        if agent.parent.as_deref() != parent {
            continue;
        }

        let indent = "  ".repeat(depth + 1);
        let kind = match agent.kind {
            AgentKind::Main => "main",
            AgentKind::Subagent => "subagent",
            AgentKind::WorkflowGroup => "workflow",
        };
        // Single source: same wording + glyph the cards/panel use.
        let status = agent.status_word();
        let glyph = agent.status.glyph();

        let label = agent
            .agent_type
            .as_deref()
            .or(agent.description.as_deref())
            .unwrap_or(id);

        // Tool tallies.
        let mut ok = 0u32;
        let mut err = 0u32;
        let mut pending = 0u32;
        let mut unknown = 0u32;
        for t in &agent.tool_calls {
            match t.state {
                ToolState::Ok => ok += 1,
                ToolState::Err => err += 1,
                ToolState::Pending => pending += 1,
                ToolState::CompletedUnknown => unknown += 1,
            }
        }

        println!("{indent}{glyph} [{kind}] {label}  ({status}) — id={id}");
        if let Some(desc) = &agent.description
            && agent.agent_type.is_some()
        {
            println!("{indent}    {desc}");
        }
        if let Some(model_name) = &agent.model {
            println!("{indent}    model: {model_name}");
        }
        println!(
            "{indent}    tools: {} ({ok}✓ {err}✗ {pending}⏳)   tokens: {}",
            agent.tool_calls.len(),
            agent.output_tokens
        );
        if unknown > 0 {
            println!("{indent}    uncertain tool results: {unknown}");
        }

        // Provenance: what triggered this agent (the panel's `↳ prompt`/`↳ thought`).
        if let Some(ctx) = model.provenance(agent) {
            if let Some(prompt) = model.provenance_prompt(ctx) {
                println!("{indent}    ↳ prompt: {prompt}");
            }
            if let Some(reasoning) = &ctx.reasoning {
                println!("{indent}    ↳ thought: {reasoning}");
            }
        }

        // Recurse into this agent's children (workflow groups have subagent
        // children, main has direct subagents + workflow groups).
        print_agent_tree(model, Some(id), depth + 1);
    }
}

/// Resolve a `View` invocation into (session id, watch target, mode, feeder,
/// speed), then spawn the tailer and run the TUI.
///
/// A **file** target bulk-loads + tails (`replay` feeder); `--follow` only
/// changes the start position (head vs beginning) via the mode. A **dir** (or no
/// target → the current project) discovers the latest session and live-tails.
async fn run_tui(cli: Cli) -> Result<()> {
    let Cli::View {
        target,
        follow,
        speed,
    } = cli
    else {
        unreachable!("inspect handled in main");
    };

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let mut catalog = SessionCatalog::new(DiscoveryRoots::from_home(&home));
    let (session, watch_target, mode, replay, speed) = match target {
        // A concrete file → bulk-load + tail. Paced from the start unless
        // `--follow` asks to ride the (possibly still-growing) edge.
        Some(file) if file.is_file() => {
            let manifest = catalog
                .manifest(&WatchTarget::File(file.clone()))
                .ok_or_else(|| anyhow!("unrecognized session file: {}", file.display()))?;
            let mode = if follow { Mode::Live } else { Mode::Replay };
            (
                manifest.root.key,
                WatchTarget::File(file),
                mode,
                true,
                speed,
            )
        }
        // A directory (or none → cwd) → live: discover the latest session and
        // follow it. (The dir need not exist yet; the tailer waits.)
        other => {
            if let Some(p) = &other
                && !p.is_dir()
            {
                bail!("not found: {}", p.display());
            }
            let cwd = match other {
                Some(d) => d,
                None => std::env::current_dir().context("resolving current directory")?,
            };
            let session = catalog
                .latest_for_cwd(&cwd)
                .map(|found| found.key)
                .unwrap_or(SessionKey {
                    provider: Provider::Claude,
                    id: String::new(),
                });
            (
                session,
                WatchTarget::LatestForCwd(cwd),
                Mode::Live,
                false,
                DEFAULT_REPLAY_SPEED,
            )
        }
    };

    // Bounded request/event channels for backpressure.
    let (tail_tx, tail_rx) = mpsc::channel::<TailRequest>(CHANNEL_CAP);
    let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>(CHANNEL_CAP);

    // Kick off the watch before the tailer task starts consuming.
    tail_tx
        .send(TailRequest::Watch(watch_target))
        .await
        .map_err(|_| anyhow!("tailer channel closed before start"))?;

    // Spawn the tailer task — owns all files of the watched session.
    tokio::spawn(async move {
        if let Err(e) = tailer::run(tail_rx, ui_tx.clone(), replay, speed).await {
            let _ = ui_tx.send(UiEvent::Error(e.to_string())).await;
        }
    });

    let app = App::new(session, mode);
    tui::run(app, tail_tx, ui_rx).await
}

#[tokio::main]
async fn main() -> Result<()> {
    // Answer and exit before the TUI touches the terminal, so this works over a
    // pipe — assets/build.sh asks for it and has no tty to spare. The tape's
    // Sleep has to be at least this long or the recording cuts mid-gesture, and
    // that number used to be copied into the tape by hand.
    if std::env::var("ZOETROPE_DEMO").as_deref() == Ok("duration") {
        println!("{:.2}", zoetrope::autopilot::tour_secs());
        return Ok(());
    }

    let cli = parse_cli(std::env::args())?;
    match cli {
        Cli::Inspect { file } => run_inspect(file).await,
        other => run_tui(other).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zoetrope::state::session::AgentStatus;

    fn cli(args: &[&str]) -> Result<Cli> {
        // parse_cli skips argv[0], so prepend a fake program name.
        let mut v = vec!["zoe".to_string()];
        v.extend(args.iter().map(|s| s.to_string()));
        parse_cli(v.into_iter())
    }

    #[test]
    fn bare_invocation_is_live_for_cwd() {
        match cli(&[]).unwrap() {
            Cli::View {
                target: None,
                follow: false,
                ..
            } => {}
            other => panic!("expected View{{target:None}}, got {other:?}"),
        }
    }

    #[test]
    fn positional_path_is_the_target() {
        match cli(&["/tmp/foo"]).unwrap() {
            Cli::View {
                target: Some(p), ..
            } => assert_eq!(p, PathBuf::from("/tmp/foo")),
            other => panic!("got {other:?}"),
        }
        // Works for a session file too (file-vs-dir is resolved at run time).
        match cli(&["s.jsonl"]).unwrap() {
            Cli::View {
                target: Some(p), ..
            } => assert_eq!(p, PathBuf::from("s.jsonl")),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn default_speed_and_no_follow() {
        match cli(&["s.jsonl"]).unwrap() {
            Cli::View { speed, follow, .. } => {
                assert_eq!(speed, DEFAULT_REPLAY_SPEED);
                assert!(!follow);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn speed_and_follow_flags_in_any_order() {
        match cli(&["s.jsonl", "--speed", "4", "--follow"]).unwrap() {
            Cli::View {
                target: Some(p),
                follow,
                speed,
            } => {
                assert_eq!(p, PathBuf::from("s.jsonl"));
                assert_eq!(speed, 4.0);
                assert!(follow);
            }
            other => panic!("got {other:?}"),
        }
        // Flags before the path, too.
        match cli(&["--speed", "2.5", "s.jsonl"]).unwrap() {
            Cli::View {
                target: Some(p),
                speed,
                ..
            } => {
                assert_eq!(p, PathBuf::from("s.jsonl"));
                assert_eq!(speed, 2.5);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_speed() {
        assert!(cli(&["s.jsonl", "--speed", "nope"]).is_err());
        assert!(cli(&["s.jsonl", "--speed", "0"]).is_err());
        assert!(cli(&["s.jsonl", "--speed", "-3"]).is_err());
        assert!(cli(&["s.jsonl", "--speed"]).is_err());
    }

    #[test]
    fn rejects_extra_positional_and_unknown_flags() {
        assert!(cli(&["a.jsonl", "b.jsonl"]).is_err());
        assert!(cli(&["--bogus"]).is_err());
    }

    #[test]
    fn inspect_takes_one_file() {
        match cli(&["inspect", "s.jsonl"]).unwrap() {
            Cli::Inspect { file } => assert_eq!(file, PathBuf::from("s.jsonl")),
            other => panic!("got {other:?}"),
        }
        assert!(cli(&["inspect"]).is_err());
        assert!(cli(&["inspect", "a", "b"]).is_err());
    }

    #[test]
    fn parse_session_fully_marks_quiet_main_idle() {
        // Inspect is a point-in-time view: a transcript whose last activity is
        // far in the past reports main as Idle (interactive agents never claim
        // completion — the format has no end marker to prove it).
        let tmp = std::env::temp_dir().join(format!(
            "zoetrope-fullparse-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(
            &tmp,
            b"{\"type\":\"user\",\"uuid\":\"u\",\"parentUuid\":null,\"timestamp\":\"2026-06-05T13:51:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        )
        .unwrap();

        let manifest = manifest_for_file(&tmp).expect("manifest");
        let snapshot = load_snapshot(&manifest).unwrap();
        let mut model = SessionModel::new(snapshot.key);
        for item in &snapshot.items {
            model.apply_event(&item.event);
        }
        model.recompute_liveness(Some(chrono::Utc::now()));
        assert_eq!(
            model
                .agent(zoetrope::state::session::MAIN_ID)
                .unwrap()
                .status,
            AgentStatus::Idle
        );

        let _ = std::fs::remove_file(&tmp);
    }
}
