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
Transcript-carried identity may appear in any syntactically accepted provider
record, but it is trusted as recorded evidence only after a positive format
discriminator. Familiar fields alone cannot supply Codex identity. Invalid
UTF-8 and malformed records fail closed or are skipped without changing an
already accepted identity. Codex identity always needs a positive header;
narrow Claude filename/empty-file compatibility fallbacks can choose a logical
view key and emit its session metadata even for an unknown record, but they do
not trust that record's content or transcript-carried identity. Classification
is bounded, defensive, and non-fatal.

## Keep decoder state with each file

Deduplication, copied-prefix suppression, and lifecycle joins depend on history.
Every tracked file therefore owns its decoder as well as its byte offset and file
identity. Snapshot-to-tail handoff moves all three together. Detectable
replacements and truncations re-establish ownership before new bytes are folded;
equal-or-longer same-path replacement relies on Unix device/inode identity and
cannot be proven on platforms without an equivalent stable identity.

Classification, identity validation, snapshot decoding, and tail handoff must
observe one opened file handle. A path may be replaced between operations, so a
probe followed by an independent reopen is not proof that the decoded bytes
belong to the accepted session. The same strict UTF-8 acceptance policy applies
on both sides of the handoff.

The portable browser feed follows the same rule. Claude directory imports keep a
decoder for the root, each subagent, and each workflow journal across appends.
Codex browser support is intentionally a static, single-rollout replay; native
Codex supports discovery, families, replay, inspect, and live follow.

## Fold competing facts by evidence

Reducers for facts that may race across files—usage revisions, lifecycle, and
spawn provenance—must be idempotent and commutative. Stable identities join tool
calls, results, children, and lifecycle evidence. Timestamp and revision ordering
decide latest evidence rather than whichever record happened to arrive last.
Ordered assistant/reasoning content and first model selection instead follow
replay order or incoming live delivery order; the live model is updated before
its timeline batch is sorted, and a later seek may refold that order. These are
not arbitrary-permutation reducers. Shuffle and live-versus-bulk tests guard the
specific projections they define.

Derived state is reversible. A late child can reopen a workflow rollup, and a
later running lifecycle fact can resume an agent. Explicit lifecycle evidence
outranks silence heuristics. An uncertain tool result stays uncertain; a spawn
acknowledgement is completion evidence only when the adapter explicitly marks
that semantic meaning.

Readable labels do not replace tool identity. Cards, detail panels, and chips
reuse the adapter's display summary; chips aggregate only matching operations,
not unrelated commands sharing a wrapper. Assigned tasks are separate from the
parent's reasoning. The model derives a missing agent description from its exact
spawn link, so late parent/child arrival cannot attach a neighboring task.
Explicit agent metadata remains authoritative.

Session information describes the selected root, not the whole agent family.
[`SessionInfo`](../src/state/info.rs) owns this boundary for snapshots and live
batches: child metadata never overwrites root settings, but a child opened
explicitly supplies its own settings. These are the latest recorded values,
outside the activity timeline; switching roots clears them. An unsupported
provider statistic is unknown rather than evidence of zero operations.

## Keep content time separate from presentation time

Content time comes from session events and determines folding, liveness, tool
state, and seeks. Presentation time advances while the user watches and governs
animation, camera motion, and afterglow. A seek makes the model represent the
timeline prefix: forward seeks fold the additional facts, while backward seeks
rebuild. Commutative projections match live playback at that point; ordered
fields are replayed in timeline order and can resolve a cross-source live
interleaving differently.

Live and replay share one timeline. Replay has a fixed right edge; live grows the
edge. The playhead paces behind it and pins when it catches up. The scrubber is
event-indexed so bursts remain visible instead of being flattened by long idle
gaps.

## Discovery is read-only and explicit files stay pinned

The native catalog searches Claude and Codex roots for the requested cwd with
deterministic selection. Claude retains its established canonical-directory and
filename discovery convention; Codex candidates require bounded positive header
evidence. The catalog excludes auxiliary Codex rollouts from automatic root
selection, closes a chosen root over its known child family, and never climbs
from an explicitly opened child to an ancestor. A cwd watch may switch to a newer
eligible root after the quiet gate; a file watch does not.

Broad automatic Codex scans inspect only a small bounded prefix so one hostile or
huge candidate cannot dominate discovery. Once the user pins a file—or a tracked
file is replaced—a streaming classifier bounds any unfinished probe record, so a
valid late header remains recognizable without turning Codex directory discovery
into an unbounded read. After acceptance, snapshot content and newly appended
regions are currently read in full; the probe bound is not a total-session memory
bound.

zoetrope never writes transcripts. The native runtime adds no HTTP client, and
Tokio is built without networking. The browser loader does not intentionally
upload selected transcript bytes, but the hosted page executes analytics,
including third-party JavaScript; it is not an offline or sandboxed isolation
boundary. This privacy statement describes first-party transcript handling, not
all page code or traffic.

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
