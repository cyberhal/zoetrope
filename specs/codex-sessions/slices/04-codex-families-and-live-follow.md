# Slice 04 — Codex Families and Live Follow

Status: complete on `codex-slice4`, pending integration. Live sessions retain
the original watch intent, current manifest, ordered file trackers, per-file
decoder/byte identity, emitted structural metadata, replay speed, refresh
cadence, and reset state. Late direct and nested Codex rollouts attach from byte
zero exactly once; active Claude watches refresh local manifests without
walking Codex history. Replacement/truncation discards that tick, waits for a
valid provider record/header for every changed path, then emits one reset plus
one complete snapshot from the freshly resolved manifest without mixing old
and new families. Codex descendants are emitted parent-first by ancestry depth.

Codex children use explicit lifecycle completion: silence or finite EOF may
make an uncompleted child idle, but never terminal. Spawn provenance joins by
stable call/activity id, then by a unique exact output/header reference; an
ambiguous reference remains unlinked, and a header-only parent edge invents no
call context.

Verification on Rust 1.88 covers late/nested attachment and follow-up appends,
copied-prefix suppression, partial/malformed activity, live/bulk convergence,
throttled discovery, cross-provider quiet switching, explicit pinning,
root and child replacement/truncation/rotation, missing-child reappearance,
retryable Claude metadata, lifecycle cycles, and provenance arrival orders.
The full native suite passes 246 library and 8 binary tests; the portable suite
passes 180 tests. Formatting, strict clippy, rustdoc warnings-as-errors,
portable check, inspect smokes, package verification, and dependency inspection
are clean.

```yaml
review:
  pass_id: codex-sessions-04
  risk: high
  lane: subagent
  effort_class: critical
  session_id: null
  initial_verdict: findings
  recheck_count: 1
```

The initial review found one high-risk child-replacement family leak and two
medium-risk ordering issues at the Claude idle-scan boundary and nested family
discovery. The same reviewer confirmed all fixes, the reverse-lexical graph
case, missing-child reappearance, and nested append behavior, and returned a
clean verdict. It judged the focused model-observable live/bulk convergence
comparison sufficient alongside the adjacent lifecycle, provenance, decoder,
and exact-once tests.

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
