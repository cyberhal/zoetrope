<p align="center">
  <img src="https://raw.githubusercontent.com/furkankly/zoetrope/main/assets/icon.svg" alt="" width="80">
</p>

<h1 align="center">zoetrope</h1>

<p align="center">
  <em>Watch Claude Code and Codex sessions as live flow graphs.</em>
</p>

<p align="center">
  <a href="https://crates.io/crates/zoetrope"><img src="https://img.shields.io/crates/v/zoetrope.svg?style=flat&labelColor=121212&color=d7af00&logo=Rust&logoColor=white" alt="crates.io"></a>
  <a href="https://docs.rs/zoetrope"><img src="https://img.shields.io/docsrs/zoetrope?style=flat&labelColor=121212&color=d7af00&logo=docs.rs&logoColor=white" alt="docs.rs"></a>
  <a href="https://crates.io/crates/zoetrope"><img src="https://img.shields.io/crates/d/zoetrope.svg?style=flat&labelColor=121212&color=d7af00" alt="downloads"></a>
  <a href="https://github.com/furkankly/zoetrope/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/furkankly/zoetrope/ci.yml?branch=main&style=flat&labelColor=121212&color=d7af00&logo=GitHub%20Actions&logoColor=white" alt="build status"></a>
  <a href="https://crates.io/crates/zoetrope"><img src="https://img.shields.io/crates/msrv/zoetrope?style=flat&labelColor=121212&color=d7af00&label=MSRV" alt="minimum supported Rust version"></a>
</p>

<p align="center">
  <a href="https://zoetrope.furkankly.dev"><b>zoetrope.furkankly.dev</b></a> · the whole app in your browser, the same binary compiled to WASM
</p>

<p align="center">
  <img src="https://raw.githubusercontent.com/furkankly/zoetrope/main/assets/zoetrope.svg" alt="A session drawn as a flow graph: a main agent above the subagents it spawned, over a timeline of tool activity" width="620">
</p>

Claude Code and Codex write local JSONL session logs. zoetrope reads them and draws
the main agent, spawned agents, workflows, and tools as a graph. The native app can
discover, replay, inspect, and live-follow both providers. It is read-only; transcript
content is parsed locally and is never uploaded.

Built on [ratatui](https://ratatui.rs) and [rataflow](https://github.com/furkankly/rataflow).

![zoetrope replaying a Claude Code session as a flow graph](https://raw.githubusercontent.com/furkankly/zoetrope/main/assets/zoetrope-demo.gif)

## Installation

**Homebrew** — macOS and Linux:

```bash
brew install furkankly/tap/zoetrope
```

**Cargo** — needs a Rust toolchain:

```bash
cargo install zoetrope
```

**Prebuilt binaries** — no toolchain needed. Every
[release](https://github.com/furkankly/zoetrope/releases) carries archives for
macOS (Apple Silicon and Intel), Linux (`musl`, arm64 and x86_64) and Windows
(x86_64). Unpack one and put `zoe` on your `PATH`.

Whichever route you take, the command is `zoe`. Or build from source:

```bash
git clone https://github.com/furkankly/zoetrope
cd zoetrope
cargo build --release
./target/release/zoe
```

No install at all: **[try it in your browser](https://zoetrope.furkankly.dev/app)**.
Drop a transcript on the page and get the same graph.

## Usage

```bash
zoe                          # follow the current project's live session
zoe <dir>                    # follow another project's session
zoe <file.jsonl>             # replay a recording from the start
zoe <file.jsonl> --follow    # open a recording at its live edge
zoe <file.jsonl> --speed N   # playback speed (default 8.0)
zoe inspect <file.jsonl>     # print the session tree and exit (no TUI)
```

Give it a file and it reads the whole transcript, then keeps watching for new lines.
Give it a directory, or no argument at all, and it finds the newest session in that
project and follows it live. Whichever way you start, the controls are the same:
scrub, follow, pause, jump back to live.

The same engine also runs [in the browser](https://zoetrope.furkankly.dev/app).
Drag in a Claude session family or one static Codex rollout JSONL. The browser's
Sessions picker and live folder following remain Claude-only. See the
[usage guide](https://zoetrope.furkankly.dev/guides/usage/) for the precise matrix.

## Features

**The graph**
- A node per agent: the main session, its subagents, and workflow groups with their
  children nested underneath
- Status, current tool, tool count and output tokens on every card
- Edges animate while an agent is working, and settle when it finishes
- Tool calls surface as chips beneath their agent (`⚒ bash ×5`, or `⚒ bash 0.5s`
  ticking during a single call), resolving to `✓` or `✗`
- A minimap showing where your viewport sits once the graph outgrows the screen

**Time travel**
- One scrubbable timeline over both live and replayed sessions
- Indexed by event rather than wall-clock, so a busy minute gets room instead of
  collapsing into a sliver
- Scrub, pause, step between prompt eras, or snap back to the live edge
- Seek backwards and you see the session exactly as it stood at that moment. Agents
  un-finish, tool counts fall, the graph shrinks back
- Optional gap compression, to skip dead air or keep faithful real-time pacing

**Inspection**
- Click any agent for its provenance: the prompt that spawned it, the reasoning
  around it, its model, and every tool call it made with timings
- Session info overlay: mode, permissions, queued ops, file edits, last prompt
- `zoe inspect` prints the whole tree headlessly, so it runs anywhere without a TTY

**Reading your sessions**
- Follows a running session live, or replays a finished one
- Reads everything a session writes: the main transcript, its subagents, and
  workflows with their own children, so the graph is the whole picture
- Keeps going when a provider writes something it hasn't seen: unfamiliar records
  are skipped, never fatal
- Read-only; transcript data is never uploaded

## Keys

`space` play/pause · `[` `]` prev/next prompt · `End` or `g` jump to live · drag the
bar to seek · `?` for everything else.

<details>
<summary>Full keymap</summary>

| Key | Action |
| --- | --- |
| `space` | play / pause (resumes from the playhead) |
| `[` / `]` | previous / next prompt era |
| `End` / `g` | jump to the live edge |
| `s` | toggle gap compression (faithful pacing vs. skip idle stretches) |
| mouse drag | seek along the scrubber |
| `o` / `f` | camera: Overview / Follow |
| `r` | relayout (tidy the graph) |
| arrows / `Tab` / `Shift-Tab` | move between agents |
| `h` `j` `k` `l` | pan the graph |
| `+` / `-` / `0` | zoom in / out / reset |
| `c` | center on the selected agent |
| click | open an agent's detail panel |
| `j` / `k` / `PgUp` / `PgDn` | scroll the detail panel |
| `i` | session info overlay |
| `?` | help overlay |
| `esc` | close an overlay / clear the selection |
| `q` / `ctrl-c` | quit |

`j` / `k` scroll the detail panel when an agent is selected, otherwise they pan the graph.

</details>

Hand the camera to the action with `f` and it glides to whichever agent just did
something. This is what watching a live run looks like:

![zoetrope in follow mode, the camera tracking whichever agent is working](https://raw.githubusercontent.com/furkankly/zoetrope/main/assets/zoetrope-follow.gif)

Or drive it yourself: pan, zoom where you point, open an agent's panel, and drag the
scrubber to travel back through the session.

![Panning, zooming, opening a detail panel, and dragging the scrubber](https://raw.githubusercontent.com/furkankly/zoetrope/main/assets/zoetrope-tour.gif)

## Under the Hood

Provider adapters normalize private JSONL schemas into one event stream. The model
fold is order-independent, live and replay share one timeline, and decoder state
travels with each file from snapshot into tailing. The portable core has no IO; the
native and browser frontends only supply bytes and events.

The durable invariants live in
[`docs/ARCHITECTURE.md`](https://github.com/furkankly/zoetrope/blob/main/docs/ARCHITECTURE.md),
with a short ownership map in
[`docs/DESIGN.md`](https://github.com/furkankly/zoetrope/blob/main/docs/DESIGN.md).

## A note on the transcript format

The JSONL formats zoetrope reads are undocumented provider internals, so they can
change without warning. zoetrope is built to degrade rather than break: unrecognized
records are skipped, missing fields fall back, and a malformed line never takes down
the session. If a provider release makes something render oddly, please
[open an issue](https://github.com/furkankly/zoetrope/issues).

## Contributing

Pull requests are welcome.

- This project follows [Conventional Commits](https://www.conventionalcommits.org/) for all commit messages (e.g. `feat(timeline): index the playhead by event instead of wall-clock`, `fix(tailer): fold appends at the live edge without rebuilding`). The changelog is generated from them with [git-cliff](https://github.com/orhun/git-cliff), and non-conforming commits are dropped.
- Run `cargo fmt`, `cargo clippy` and `cargo test` before opening a PR.
- Those cover the Rust 1.88 portable core and native frontend. The browser frontend is a
  second crate (`zoetrope-web`, in `web/wasm/`) that only builds for wasm32, so it is
  excluded from the root workspace and follows stable Rust (currently Rust 1.90+
  because of its renderer). From the
  repo root, build it with `bash web/scripts/build-wasm.sh` and lint it with
  `cd web/wasm && cargo clippy` (its `.cargo/config.toml` defaults the target to wasm32).

## License

[MIT](https://github.com/furkankly/zoetrope/blob/main/LICENSE).

## Acknowledgements

- [ratatui](https://github.com/ratatui/ratatui) for the terminal UI framework
- [rataflow](https://github.com/furkankly/rataflow) for the node-graph widget
- [ratzilla](https://github.com/ratatui/ratzilla) for the WebAssembly backend
