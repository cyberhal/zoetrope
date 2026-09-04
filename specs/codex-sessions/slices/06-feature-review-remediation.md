# Slice 06 — Feature Review Remediation

Status: implementation and required gates are complete on
`codex-review-fixes`. The original feature reviewer returned `CLEAN` on its
third directed recheck. The first recheck exposed two high- and two medium-risk
extensions; the second found one remaining Claude identity-validation bypass;
the third confirmed all findings closed with no new high- or medium-risk issue.

## Contract

The feature-wide review's counterexamples are fixed at their owning seams:

- A snapshot or late attachment validates provider and family identity from
  the same open handle that supplies its decoded bytes and tail state. Codex
  validates thread ancestry; Claude subagents validate every session and actor
  id on every strictly decoded record the adapter could consume, while workflow
  journals may name several actors. Positive provider evidence remains a
  separate requirement. Conflicting or incomplete content emits no facts and
  is retried.
- Native catalog and portable replay share one bounded-memory, stateful positive
  provider probe. Automatic discovery reads at most 64 KiB per candidate;
  pinned and replacement paths scan complete records in fixed-size chunks.
  Replacement reload overlays streaming-resolved changed members before
  recomputing the family, so a tracked child with a large header is not lost.
- Snapshot identity is announced before replay data, so a replacement between
  selection and background loading cannot be discarded by the stale-session
  guard.
- A complete JSON record at physical EOF is delivered once without waiting for
  a newline; an incomplete record remains buffered, and a delimiter arriving as
  spaces or segmented CRLF cannot duplicate an already delivered record.
- Synthetic child metadata is visible only after that child's transcript has
  passed same-handle validation, including when a pending child attaches later.
- Assistant and reasoning de-duplication uses adapter-owned stable fact identity,
  never display text.

## Verification

The 286 library and 8 binary behavior tests cover Codex and cross-provider
snapshot replacement, Claude child family and actor replacement, incomplete
late-child retry, pending-child discovery, no-newline/segmented-CRLF
convergence, the two-parse replay window, provider classification beyond the
automatic 64 KiB budget, and repeated text across turns and arrival orders.
They also pin early-stop I/O after a decisive header, bounded Claude identity
conflict evidence, strict UTF-8 parity between probing and decoding, identity
validation on decoder-accepted records without positive role evidence, and a
tracked child's large-header replacement.

Required closeout gates are the root Rust 1.88 full/portable/fmt/clippy/rustdoc
matrix, stable WASM check/clippy, native inspect smokes, dependency/read-only
inspection, and a directed recheck by the reviewer that found the five issues.

Current evidence: Rust 1.88 passes 286 library + 8 binary full tests, 195
portable tests/check, formatting, strict clippy, and rustdoc warnings-as-errors.
Stable passes the separate browser workspace's locked wasm32 check and strict
clippy. Claude/root-Codex/child-Codex inspect oracles pass; offline packaging
contains 34 allow-listed files; dependency inspection finds no HTTP client or
Tokio networking feature.

```yaml
review:
  pass_id: codex-sessions-06
  risk: high
  lane: subagent
  effort_class: critical
  session_id: /root/codex_feature_final_review
  initial_verdict: findings
  recheck_count: 3
  latest_verdict: clean
```

Do not archive the feature spec here; final feature closeout remains owned by
the integrating agent.
