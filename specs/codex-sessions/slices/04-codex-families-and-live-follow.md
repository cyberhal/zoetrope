# Slice 04 — Codex Families and Live Follow

## Contract

A Codex root plus late-arriving direct/nested child rollouts becomes one live graph, and directory watches switch between providers only under the existing idle policy. No byte is lost or delivered twice at snapshot/tail boundaries.

## API seam and ownership

- `LiveSession` consumes a watch target, catalog, and current manifest.
- Maintain one byte state and one decoder per manifest file.
- Refresh catalog/manifests on a throttled schedule; attach newly visible descendants idempotently.
- Join spawn provenance by strongest evidence: current or legacy activity id to `spawn_agent` call id, then parent call output/path to child header, then header parent edge without invented provenance.
- A parent activity may create a placeholder before its file appears; attaching the file enriches that node rather than creating another.
- Explicit typed completed/interrupted activity governs spawned status. Interactive roots stay Running/Idle across turn-level completion.
- Directory watches retain the cwd query and compare eligible Claude/Codex roots after the existing quiet-session gate. Explicit files never switch.

## Runnable artifact

A deterministic live harness starts with a parent snapshot, appends a partial call/result, creates a child and nested child file, completes a child, then introduces a newer root from the other provider. Its event log shows each transition exactly once.

## Verification

- Existing partial-line, runaway-line, inode replacement, truncation reset, stale-session stamping, and replay-to-tail race tests stay green.
- Parent spawn, late child attachment, child append, nested child, follow-up owned turn, and terminal status appear once.
- Copied prefix remains suppressed in replay and append modes.
- Snapshot plus appended bytes converges with a fresh bulk load for every family file.
- Replacement/truncation reattaches once without duplicated tools or usage.
- A child with pending work remains running.
- A newer provider root switches only after idle; child mtime cannot steal root selection.
- Explicit rollout remains pinned.
- Polling does not reread whole old rollouts or scan all history every 200 ms.

Expected commands:

```text
cargo test live
cargo test replay
cargo test discovery
cargo test --all-targets
```

## Review

Provisional tier: high. The exact risks are false graphs, wrong-session switches, and replay/live gaps. Scheduling and cache data structures are delegated; delivery, pinning, eligibility, and lifecycle semantics are locked.

Must stay green: all existing live invariants and Claude auto-switch behavior.

