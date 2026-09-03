# Slice 01 — Normalized Events and Codex Decoder

## Status

Complete on `codex-slice1`. The completion checkpoint is the commit containing
this status; its source hash is recorded in the integration handoff.

Rust 1.88 evidence: 8 focused Codex decoder tests pass; the full suite passes
with 191 library and 8 binary tests; `cargo check --no-default-features --lib`,
rustdoc, formatting, and diff checks pass. Strict all-target/all-feature clippy
still reaches two pre-existing warnings in `src/ui/panel.rs` and
`src/state/session.rs`; allowing only those two lint classes leaves clippy
clean. Child ownership, item de-duplication, current `final_answer`, tagged goal
extraction, and array-valued tool outcomes were each proven by red/green
falsification. Semantic tool category and per-scope usage revision assertions
were also proven red before restoration.

```yaml
review:
  pass_id: codex-sessions-01
  risk: high
  lane: subagent
  effort_class: critical
  session_id: null
  initial_verdict: findings
  recheck_count: 2
```

The directed recheck is clean. Integration must keep `event::{Provider,
SessionKey}` as the sole provider/session identity; if Slice 02 is already
present, remove its parallel catalog-owned identity types during integration.
The next implementation pickup is Slice 03 on the integrated branch.

## Contract

A stateful, portable decoder turns sanitized Codex JSONL into one provider-neutral event stream with deterministic de-duplication. It is independently testable and does not yet need to drive every production consumer.

## API seam and ownership

- Define provider/session identity, actor, timestamp semantics, prompts, assistant channels, reasoning, model selection, usage observation, tool start/finish, agent discovery/status, and session-info patches as domain types.
- Keep raw Claude and Codex serde DTOs private under provider format modules.
- Give each input file one decoder instance. The decoder owns format identity, turn-local prompt de-duplication, call ids, and the Codex child ownership gate.
- Model usage as an absolute observation keyed by a stable scope; do not encode provider-specific arithmetic in consumers.
- Represent tool outcome as `Succeeded`, `Failed`, or `CompletedUnknown`.

Codex canonical sources are locked: real user event first with response-item fallback for goal objectives; response items for assistant text/reasoning and calls/results; `turn_context` for model; cumulative `total_token_usage` for scoped usage; typed current and legacy subagent activity for lifecycle. Mirrored agent/event views are ignored.

For a `thread_spawn` child, retain its own header metadata, suppress activity until the first `inter_agent_communication_metadata.payload.trigger_turn == true`, then decode subsequent records. A missing marker yields metadata/diagnostic only, never heuristic attribution.

## Fixtures and runnable artifact

Add compact redacted fixtures for an interactive root, goal continuation, current/legacy spawn activity, nested child with copied ancestor history, auxiliary `subagent.other`, both tool families, successful/failed/unknown completion, cumulative tokens, empty/encrypted and plaintext reasoning, malformed/unknown records, and partial lines.

An adapter-level fixture test prints or snapshots the normalized semantic sequence so a human can verify the contract without launching the TUI.

## Verification

- Exact counts and values: one prompt, no injected context, one assistant message, one call/result pair per id, correct model, latest cumulative usage, correct parent/status.
- Duplicate response/event representations still emit one semantic fact.
- Copied child prompt/tools/tokens/spawns emit nothing before the ownership marker.
- Blank, malformed, missing-field, wrong-type, unknown, and bounded-large records never panic.
- Claude characterization fixtures are added before its adapter is moved.

Expected commands on Rust 1.88+:

```text
cargo test --lib formats
cargo test --lib codex
cargo check --no-default-features --lib
```

## Review

Provisional tier: high. The risk is silent double-counting or false attribution. Stop and reslice if prompt provenance, cumulative usage, or the child gate cannot be expressed as exact fixture assertions.

Implementer discretion is limited to internal module/file names and private DTO decomposition. Canonical sources, ownership behavior, event meaning, and defensive fallbacks are not delegated.

Must stay green: existing parser/model characterization tests and the portable no-default-features build.
