---
title: Design & architecture
description: Why zoetrope normalizes provider logs, keeps decoder state per file, and uses one timeline for live and replay.
---

zoetrope treats Claude Code and Codex JSONL as append-only event logs. Private
provider records are normalized once, then the model, timeline, inspect output,
and browser all consume the same facts.

Three rules carry most of the design:

- A model is a projection of facts, not arrival order. Cross-file delivery,
  replay, and backward seeks must converge.
- Decoder state belongs to its file. Offsets alone cannot preserve copied-prefix
  gates, deduplication, or lifecycle joins across snapshot and append.
- Content time determines session truth; presentation time only animates the
  view. Seeking never changes what an event means.

The browser and native app share the portable decoder/model path but intentionally
offer different IO. Native supports discovery and live follow for both providers.
The browser can live-follow Claude folders and statically replay one Codex rollout.

The complete, canonical invariant set is maintained in
[`docs/ARCHITECTURE.md`](https://github.com/furkankly/zoetrope/blob/main/docs/ARCHITECTURE.md).
Exact record variants and edge cases live with their adapter tests rather than in
a prose mirror of the code.
