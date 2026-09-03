# Slice 03 — Core Replay and Inspect

## Contract

The session model, timeline, info summary, replay loader, and inspect command consume only normalized events. Claude behavior remains equivalent, and an explicit Codex rollout replays and inspects correctly.

## API seam and ownership

- Adapt Claude records into the shared events, then remove public domain consumers of Claude wire types.
- Move timestamp anchoring and usage observation folding into the normalized contract/model.
- Generalize agent metadata to explicit parentage and provider-qualified session identity; remove hardcoded Claude root labels.
- Make byte framing return complete lines; each tracked manifest file owns a decoder and actor/role context.
- Add one snapshot loader returning replay items, session info, per-file offsets, and synthetic facts. Both replay and inspect call it.
- Delete duplicate full-session parsing from `main` and any temporary event bridge from Slice 01.
- Format detection uses content/header, never filename alone.

## Runnable artifact

`zoe inspect <sanitized-codex-root-fixture>` reports provider/thread identity, prompt, model, output tokens, tools, and available agent placeholders. `zoe <fixture>` produces a coherent replay. A Claude golden summary demonstrates unchanged output.

## Verification

- All prior Claude parser/model/timeline tests pass without weakened assertions.
- Claude demo golden parity covers model, prompt eras, tools/results, tokens, graph structure, timestamps, and replay ordering.
- Codex root and explicit child fixtures inspect/replay with correct logical root.
- Usage observations are order-stable and never inflated.
- Tool results with unknown status are not presented as proven success.
- Backward seek reconstructs the same model; equal-time spawn ordering is stable.
- Malformed siblings do not poison valid session files.

Expected commands:

```text
cargo test --all-targets
cargo check --no-default-features --lib
cargo run -- inspect tests/fixtures/codex/root.jsonl
cargo clippy --all-targets -- -D warnings
```

## Review

Provisional tier: high because this changes the shared fold and replay/inspect ownership. Internal naming is delegated; domain semantics and the removal of parallel consumers are locked.

Must stay green: order-independent/idempotent folding, exact seek, content/presentation clock separation, all Claude output, and portable-core compilation.

