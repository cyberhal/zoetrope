# zoetrope architecture

zoetrope reconstructs a faithful, navigable view of Claude Code and Codex
sessions from append-only JSONL. The formats are internal, partially duplicated,
and can be split across files. These invariants are the system's source of truth.

## Normalize once

Provider wire records stop at `formats::{claude,codex}`. Both adapters emit the
same `SessionEvent` facts; the model, timeline, inspect output, and browser never
parse provider DTOs or turn Codex into synthetic Claude records. One input line
may emit several facts, and every fact survives the boundary.

A session is identified by `SessionKey { provider, id }`. Codex identity comes
from its rollout header, including when a child rollout is opened explicitly.
Unknown and malformed records are skipped until a positive format discriminator
is found. Parsing remains bounded, defensive, and non-fatal.

## Keep decoder state with each file

Deduplication, copied-prefix suppression, and lifecycle joins depend on history.
Every tracked file therefore owns its decoder as well as its byte offset and file
identity. Snapshot-to-tail handoff moves all three together. Replacements and
truncations re-establish ownership before new bytes are folded.

The portable browser feed follows the same rule. Claude directory imports keep a
decoder for the root, each subagent, and each workflow journal across appends.
Codex browser support is intentionally a static, single-rollout replay; native
Codex supports discovery, families, replay, inspect, and live follow.

## Fold facts, not arrival order

The derived `SessionModel` must be idempotent and commutative: the final state is
a function of observed facts, not cross-file arrival order. Stable identities
join tool calls, results, children, and lifecycle evidence. Timestamp and revision
ordering decide latest evidence rather than whichever record happened to arrive
last. Shuffle and live-versus-bulk tests guard this property.

Derived state is reversible. A late child can reopen a workflow rollup, and a
later running lifecycle fact can resume an agent. Explicit lifecycle evidence
outranks silence heuristics. An uncertain tool result stays uncertain; a spawn
acknowledgement is completion evidence only when the adapter explicitly marks
that semantic meaning.

## Keep content time separate from presentation time

Content time comes from session events and determines folding, liveness, tool
state, and seeks. Presentation time advances while the user watches and governs
animation, camera motion, and afterglow. A seek rebuilds the model from the event
prefix and must produce the same state as playback at that point.

Live and replay share one timeline. Replay has a fixed right edge; live grows the
edge. The playhead paces behind it and pins when it catches up. The scrubber is
event-indexed so bursts remain visible instead of being flattened by long idle
gaps.

## Discovery is read-only and explicit files stay pinned

The native catalog searches Claude and Codex roots for the requested cwd using
bounded header reads and deterministic selection. It excludes auxiliary Codex
rollouts from automatic root selection, closes a chosen root over its known child
family, and never climbs from an explicitly opened child to an ancestor. A cwd
watch may switch to a newer eligible root after the quiet gate; a file watch does
not.

zoetrope never writes transcripts. The runtime adds no HTTP client, and Tokio is
built without networking. Browser-selected transcript bytes are parsed locally
and are not uploaded by the app. The hosted website itself still loads ordinary
site infrastructure such as analytics, so this privacy statement is deliberately
about transcript handling rather than all page traffic.

## Frontend boundary and toolchains

The root crate's portable core has no filesystem or async-runtime dependency;
native IO is behind the `native` feature. `web/wasm` is a separate unpublished
workspace that feeds selected bytes into the same portable decoder/model path.

The root crate's MSRV is Rust 1.88. The browser workspace follows stable Rust and
declares Rust 1.90 because the current renderer stack requires it. Keeping those
policies separate avoids raising the installed CLI's MSRV for a frontend-only
dependency; changing renderer or browser MSRV later remains a product choice.

See [DESIGN.md](DESIGN.md) for the ownership map and the
[usage guide](../web/src/content/docs/guides/usage.md) for the capability matrix.
