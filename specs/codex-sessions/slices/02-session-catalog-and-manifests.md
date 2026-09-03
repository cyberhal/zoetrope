# Slice 02 — Session Catalog and Manifests

## Contract

One native catalog discovers eligible Claude and Codex roots for a cwd using bounded reads, and one manifest describes every file/actor belonging to a selected session. No tailer or CLI code reverse-engineers provider layouts.

## API seam and ownership

Introduce test-injectable discovery roots, an explicit `LatestForCwd` versus `File` watch target, provider-qualified session keys, session references, and manifests of files plus synthetic metadata events.

- Claude lookup preserves sanitized project directories, UUID filtering, sidecars, subagents, and workflows.
- Codex discovery walks active calendar buckets, reads a bounded header from unseen/changed rollout files, and caches in memory. It records thread id, cwd, root/child/auxiliary kind, parent id/path, and mtime.
- Eligible Codex roots have top-level sources. `thread_spawn` children and `subagent.other` helpers cannot win cwd discovery.
- A Codex manifest contains the selected root plus the transitive closure of `thread_spawn` descendants. Header parentage works without a parent activity record; a later exact spawn join enriches the same node.
- Explicit child files are pinned as logical roots and include only discoverable descendants.
- Existing paths are canonicalized for cwd comparison; otherwise compare normalized lexical absolute paths.
- Newest means root-session mtime semantics with deterministic tie-breaking. Child activity keeps its family current but never becomes a separate root candidate.

## Runnable artifact

A temporary-root catalog probe lists candidate keys and the chosen manifest without opening full transcript bodies.

## Verification

- Claude-only, Codex-only, and mixed-provider cwd selection.
- Other-cwd, malformed, child, auxiliary, and rollout-looking non-file exclusion.
- Direct and nested child closure independent of directory iteration order.
- Deterministic equal-mtime choice and explicit-child pinning.
- A new day bucket/file becomes visible on refresh.
- Instrumented fixture proves discovery reads at most the configured bounded header per candidate, never whole multi-megabyte content.

Expected commands:

```text
cargo test --lib discovery
cargo test --lib manifest
```

## Review

Provisional tier: high. Selecting the wrong cwd can expose an unrelated private session. The implementation may choose cache data structures and scan ordering, but not eligibility, bounded-read behavior, path comparison, or deterministic tie-breaking.

Must stay green: current Claude discovery tests and read-only/no-network guarantees.

