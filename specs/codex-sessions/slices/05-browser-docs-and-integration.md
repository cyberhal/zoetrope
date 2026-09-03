# Slice 05 — Browser, Documentation, and Integration

## Contract

All public entry points tell the truth about two-provider support, one Codex rollout replays in the browser, temporary architecture sediment is gone, and the complete feature passes independent review.

## API seam and ownership

- Route `replay_from_jsonl` and WASM append state through the same content-detected per-file decoder.
- Preserve Claude browser multi-file behavior. For Codex, support static single-JSONL replay only; parent lifecycle records may create placeholder agents when child content is unavailable.
- Update CLI help, package metadata, README, architecture/usage documentation, and browser drop/status copy to say Claude Code and Codex precisely.
- Audit imports and delete direct wire parsing, provider branching in the model/UI, duplicate discovery/loader paths, temporary bridges, and stale hardcoded Claude root copy.
- Keep runtime dependency/tree/network claims accurate.

## Runnable artifact

Drag the sanitized Codex root fixture into the local browser build and inspect the same prompt/model/tool/agent facts reported by native `inspect`.

## Verification

```text
cargo fmt --all --check
cargo test --all-targets
cargo test --no-default-features --lib
cargo clippy --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps
cd web/wasm && cargo check --target wasm32-unknown-unknown
cargo publish --dry-run --locked
```

Also inspect the dependency tree for network clients, grep for stale public wire DTO imports/duplicate parsing/hardcoded Claude-only copy, smoke-test native root/child/dir modes, and compare browser output with native inspect fixture expectations.

## Review

Provisional tier: medium for this slice alone. Final integration is feature-wide high risk and must run the repository's complete `review` workflow after the full diff exists.

Implementer discretion is limited to concise copy and private frontend state placement. Browser scope, shared decoding, truthful docs, cleanup, and all gates are locked.

Must stay green: native and portable builds, WASM, docs, packaging, zero-network/read-only behavior, and every Claude regression gate.

