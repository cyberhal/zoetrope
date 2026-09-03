# zoetrope design map

This page is a map, not a second specification. The durable correctness rules
live in [ARCHITECTURE.md](ARCHITECTURE.md); user-facing commands and browser
capabilities live in the [usage guide](../web/src/content/docs/guides/usage.md).

The code is divided by ownership:

- `formats` owns private Claude Code and Codex wire schemas and emits
  provider-neutral session events.
- `session_catalog` and `session_loader` own native discovery, session-family
  membership, snapshot reads, and the decoder state handed to live tailing.
- `tailer` owns byte framing and the shared replay/feed boundary.
- `state` folds events into the timeline, session model, and graph projection.
- `ui` renders that projection without interpreting provider records.
- `web/wasm` supplies browser-selected bytes to the portable core. It does not
  discover native paths.

Provider formats are internal and can change without notice. New schema support
belongs in its format adapter; downstream modules should not learn provider DTOs
or guess ownership from filenames. Tests beside each owner are the executable
description of exact record shapes and edge cases.

The native crate supports Rust 1.88. The browser frontend is a separate,
unpublished wasm workspace built with stable Rust and currently declares Rust
1.90 because its renderer dependency requires it.
