# Slice 05 — Browser, Documentation, and Integration

Status: complete on `codex-slice5`, pending integration. The portable replay
seam returns the real provider-qualified session key, replay items/info, and
the stateful per-file decoder feed used by browser appends. The browser accepts
a static Codex rollout or the existing Claude file family without exposing
provider wire records to the app.

Verification is clean: Rust 1.88 passes 252 library and 8 binary tests, plus
186 portable tests, formatting, strict clippy, rustdoc warnings-as-errors, and
offline package verification. Stable Rust 1.98 passes WASM check and strict
clippy. The production build passes through Trunk 0.21.14, release WASM,
Astro's six static pages, Pagefind, and sitemap generation. Dependency
inspection finds Tokio without its network feature and no HTTP client.

```yaml
review:
  pass_id: codex-sessions-05
  risk: high
  lane: subagent
  effort_class: high
  session_id: null
  initial_verdict: findings
  recheck_count: 1
```

The initial review found whitespace-only provider identities and a test-only
identity rewrite, then extended its cleanup to generated package-manager state,
a duplicate model-side tool summarizer, and dead transcript discovery/counting
helpers. The same reviewer confirmed the fixes and replacement adapter/catalog
coverage and returned a clean verdict. Last updated 2026-09-04.

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
Rust 1.88: cargo fmt --all --check
Rust 1.88: cargo test --all-targets --all-features --locked
Rust 1.88: cargo test --no-default-features --lib --locked
Rust 1.88: cargo clippy --all-targets --all-features --locked -- -D warnings
Rust 1.88: RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps --locked
stable (Rust 1.90+): cargo check --manifest-path web/wasm/Cargo.toml --target wasm32-unknown-unknown --locked
cargo publish --dry-run --locked --offline
```

Also inspect the dependency tree for network clients, grep for stale public wire DTO imports/duplicate parsing/hardcoded Claude-only copy, smoke-test native root/child/dir modes, and compare browser output with native inspect fixture expectations.

## Review

Actual tier: high because this slice replaced the portable replay boundary and
removed the final provider-wire test bridge. Its independent review is clean.
Final integration remains feature-wide high risk and must run the repository's
complete `review` workflow after the full diff exists.

Implementer discretion is limited to concise copy and private frontend state placement. Browser scope, shared decoding, truthful docs, cleanup, and all gates are locked.

Must stay green: native and portable builds, WASM, docs, packaging, the
read-only/no-network transcript runtime, and every Claude regression gate.
