# Codex Sessions — shipped rationale

## Overview

Zoetrope reads Codex rollout JSONL as a first-class session format alongside
Claude Code. Native launches discover, replay, inspect, and live-follow a Codex
thread and its spawned-thread family. An explicitly opened rollout remains the
logical root of its own pinned view. The portable browser accepts one static
Codex rollout while retaining its existing multi-file Claude import.

The supported unit is a Codex thread/rollout, matching Codex's documented
start-and-resume session model. Zoetrope does not control Codex: authentication,
approvals, history ownership, and remote execution remain with Codex and its
app-server/SDK. The native transcript path is local and read-only; the hosted
browser's narrower trust boundary is recorded below.

## Why the boundary looks this way

Claude and Codex expose evolving private JSONL schemas with different record
shapes, overlapping views of the same fact, and different child-session
ownership rules. Translating Codex into pretend Claude records looked locally
cheap but moved provider ambiguity into every model and UI consumer. Instead,
each provider adapter interprets wire meaning once and emits the same semantic
event vocabulary.

Discovery has a stricter resource and privacy contract than an explicit open.
Codex history scans read a small fixed prefix from each candidate and accept
only positively identified top-level sessions for the requested cwd. Claude
automatic discovery retains its established canonical-directory and filename
convention; load-time validation still gates decoded facts. A user who pins a
file has already selected the resource, so that path streams chunks through a
classifier with a bound on an unfinished record and can recognize a valid late
header. Once accepted, a selected snapshot is read in full. This split keeps
ambient Codex discovery cheap without making unusual but valid files impossible
to inspect.

Each file keeps its decoder together with its byte cursor and filesystem
identity. Codex children can begin with copied ancestor history, and paths can
be replaced while a session is open; a stateless line parser or a
probe-then-reopen sequence cannot preserve ownership. Stable Unix file identity
also makes equal-or-longer atomic replacement detectable during live follow.

## Principles and invariants

- Session identity is provider-qualified. The same textual id from Claude and
  Codex never denotes the same session.
- Codex recognition and identity require positive structural evidence. Claude
  retains compatibility fallbacks for a caller-selected browser filename and
  an explicitly opened canonical session filename; those fallbacks never claim
  a Codex thread.
- Initial classification, identity validation, snapshot decoding, and tail
  handoff use one open file handle. Reattachments repeat that acceptance before
  exposing facts. Accepted identity and accepted bytes are atomic at those
  boundaries.
- Provider probing is strict UTF-8. Automatic discovery is prefix-bounded;
  pinned and replacement classifiers stream chunks with a bound on an
  unfinished record. These are classification bounds, not a limit on the total
  accepted snapshot or on complete newline-delimited records.
- Every tracked file owns one stateful decoder. Snapshot-to-tail handoff moves
  decoder state, offset, and file identity together.
- A spawned Codex rollout owns activity only after its exact
  `trigger_turn: true` marker. Copied ancestor history contributes no child
  prompts, tools, tokens, or descendants.
- Adapters choose one canonical source for mirrored provider facts and prefer a
  stable wire identity. When a Codex text item lacks one, its fallback combines
  the current turn (or `unknown-turn`) with content; prompt identity may fall
  through turn, item, and timestamp to content. Well-formed records keep turns
  distinct, while malformed/legacy records with none of those identities carry
  a documented repeated-text collision risk.
- Codex usage totals are absolute observations scoped to a thread. They are not
  additive deltas.
- Typed lifecycle evidence controls Codex child status. Silence, EOF, and an
  ambiguous spawn result do not fabricate success; uncertain tool completion
  remains visibly uncertain.
- Competing semantic facts such as usage revisions, lifecycle, and spawn
  provenance fold idempotently and independently of cross-file arrival order.
  Ordered text and first-model fields follow replay order or incoming live
  delivery order; the live model is updated before the timeline sorts its batch.
  They are not arbitrary-permutation reducers, and a later seek can refold them
  in timeline order.
- Automatic cwd discovery chooses only eligible roots and closes a selected
  root over known spawned descendants. Explicit children never climb to an
  ancestor, and explicit files never auto-switch.
- Native transcript handling performs no writes, subprocess control, or network
  I/O. The browser loader itself does not upload selected bytes, but the hosted
  page loads analytics, including a third-party script, and is not an offline or
  sandboxed isolation boundary.
- Browser Codex support stays a static single-rollout floor. Native family
  discovery and live following are not implied browser capabilities.

## Decisions that remain product choices

The [choices ledger](choices.md) preserves the remaining reversible defaults
that need owner buy-in: the browser workspace's separate Rust floor, equal-time
scrubber binning, the presentation of uncertain tool results, and the
deterministic winner for an exact cross-provider mtime tie. None weakens the
correctness or privacy invariants above.

## Material divergences from the build plan

- The original five-pass plan acquired a dedicated review-remediation pass.
  Independent review exposed catalog-to-open replacement races, unvalidated
  synthetic child metadata, newline-at-EOF loss, display-text de-duplication,
  and a discovery bound incorrectly reused for pinned files. The resulting
  fixes strengthened the shared Claude path as well as Codex.
- A single Rust floor proved false. The installable native crate remains on Rust
  1.88; the separate unpublished browser workspace declares Rust 1.90 because
  its renderer dependency requires it. Raising the CLI floor or replacing the
  renderer was outside the feature's purpose.
- Replacement recovery needs a freshly validated manifest overlay before family
  closure. Reusing only the bounded discovery catalog can drop an already
  tracked child whose valid replacement header lies beyond the scan prefix.
- Complete JSON at physical EOF is meaningful even before its newline arrives.
  Framing therefore remembers that delivery through trailing whitespace and a
  later segmented CRLF, while incomplete JSON remains buffered.

## Known limits

- Accepted native snapshots and newly appended regions are currently allocated
  in full. The fixed limits protect discovery and unfinished-record buffering,
  not total selected-session memory use or every complete JSONL record.
- Unix device/inode identity detects same-path replacement even when the new
  file is not shorter. Platforms without a stable file identity still detect
  truncation, but cannot reliably distinguish an equal-or-longer replacement
  from an append during continuous tailing.
- Claude's metadata-poor portable fallback and unwritten canonical-file
  sentinel predate Codex support. Load-time validation prevents them from
  fabricating Codex identity, but “positive evidence only” is not a universal
  Claude rule.
- The hosted browser decodes through local WASM but shares a page with analytics
  JavaScript. Users requiring an isolation guarantee need a trusted local/offline
  host; the repository currently has no CSP or script sandbox that proves such
  isolation on the public site.
- Live text/reasoning append order and first-model selection follow incoming
  delivery order before timeline sorting. Cross-source permutation equivalence
  applies only to the explicitly commutative reducers above.

## Rejected paths and dead ends

- `session_index.jsonl` lacks the cwd and parentage needed for authoritative
  selection; it cannot replace rollout-header evidence.
- A persistent index or sidecar would add transcript-adjacent mutation and a
  cache-invalidation problem to a read-only tool.
- Filename-only provider detection and path-only family membership fail under
  replacement and can expose an unrelated private session.
- Summing Codex cumulative totals inflates token usage. Folding absolute scoped
  observations preserves revisions and arrival-order independence.
- Timestamp, text, or header heuristics cannot distinguish a child's copied
  ancestor prefix from its owned work. Only the provider's exact turn marker is
  accepted.
- Treating every tool result as success makes acknowledgements look terminal.
  Unknown completion is a first-class outcome instead.
- Text-only global de-duplication deletes legitimate repeated answers. Stable
  provider fact identities suppress mirrors without collapsing separate
  well-formed turns; the identity-poor fallback accepts the collision risk
  recorded above.

## Code and test map

| Concern | Entry points and executable evidence |
| --- | --- |
| Neutral identity and facts | [`Provider`, `SessionKey`, and `SessionEvent`](../../../src/event.rs) |
| Provider interpretation | [`ClaudeDecoder`](../../../src/formats/claude.rs) and [`CodexDecoder`](../../../src/formats/codex.rs); Codex fixture anchors include `root_fixture_normalizes_each_canonical_fact_once` and `child_prefix_is_suppressed_until_the_exact_owned_turn_marker` |
| Positive, bounded format evidence | [`SessionProber`](../../../src/formats/mod.rs); `invalid_utf8_after_a_valid_header_does_not_poison_the_probe` pins the byte-policy boundary |
| Discovery and family membership | [`SessionCatalog` and `SessionManifest`](../../../src/session_catalog.rs); `manifest_closes_over_nested_children_and_pins_an_explicit_child` and `explicit_and_replacement_probe_past_the_automatic_discovery_budget` pin the two trust levels |
| Atomic snapshot ownership | [`load_snapshot`](../../../src/session_loader.rs); `snapshot_identity_comes_from_the_handle_that_supplied_bytes` and `manifest_to_first_open_replacement_never_mixes_old_key_with_new_content` pin same-handle validation, while `claude_snapshot_and_probe_both_skip_invalid_utf8_records` pins probe/decoder byte-policy parity |
| Framing and live families | [`tailer::bytes`](../../../src/tailer/bytes.rs) and [`LiveSession`](../../../src/tailer/live.rs); `late_child_replacement_between_catalog_and_open_never_leaks` and `tracked_child_replacement_keeps_a_large_header_in_the_family` cover the dangerous transitions |
| Order-independent reducers | [`SessionModel::apply_event`](../../../src/state/session.rs); `final_state_is_arrival_order_invariant`, `distinct_codex_turns_keep_identical_text_once_in_every_arrival_order`, and `unknown_tool_completion_is_never_presented_as_success` pin the tested reducer projections and text-identity behavior |
| Native entry points | [`run_inspect` and `run_tui`](../../../src/main.rs) share catalog/loader paths with [`tailer::run`](../../../src/tailer/mod.rs) |
| Portable entry points | [`replay_from_session` and `SessionFeed`](../../../src/tailer/item.rs) feed [`zoetrope_load` and `zoetrope_append`](../../../web/wasm/src/main.rs); `replay_from_jsonl` is their single-file wrapper, and portable Codex root/child fixture tests sit beside the feed |

## Visual provenance

No external reference image or redesign target was supplied; the explicit
visual requirement was parity with Zoetrope's existing terminal renderer. A
[same-build browser comparison](assets/claude-vs-codex-browser.png) preserves
the acceptance evidence at its original height: the left half is the bundled
Claude demo and the right half is
[`tests/fixtures/codex/root-current.jsonl`](../../../tests/fixtures/codex/root-current.jsonl),
both rendered at 2560×1289 in the same Chrome viewport, production WASM bundle,
theme, and end-of-session state. Temporary full-viewport harnesses imported
`web/dist/wasm/web.js`; the Codex harness passed the sanitized fixture to
`zoetrope_load`, while the Claude harness used the bundled demo.

The independent visual-review session `codex_browser_visual_critique` first
inspected the Codex result, then received the two captures as unlabeled A/B
inputs. It judged connector visibility, secondary-text contrast, minimap
treatment, and footer fit a high-confidence tie: Codex added no material visual
regression. The shared low-contrast edges and crowded final footer hint remain
baseline design debt outside this feature's scope. The A/B composite is the
durable report; generated diff telemetry was treated only as navigation aid
because the two fixtures intentionally have different graph geometry.

### Overview and metadata readability

The sanitized [readability fixture](../../../tests/fixtures/codex/readability.jsonl)
drives the final [overview](assets/readability-graph.png),
[agent details](assets/readability-details.png), and
[session information](assets/readability-info.png) captures. These are production
Ratatui buffers rendered at a fixed playhead and rasterized with a monospace font,
not OS-terminal screenshots. Same-fixture captures against the pre-change build
were byte-different on all three surfaces; full images and enlarged crops were
checked independently.

The first critique found insufficient contrast in operation summaries, assigned
tasks, and session values. These use body-text contrast in the final captures;
the final critique confirmed their readability without new overlap or glyph
defects. Existing faint timeline/provenance text, connector gaps behind chips,
and deliberate ellipses in the narrow split-view graph remain shared UI debt.
The [architecture](../../../docs/ARCHITECTURE.md#fold-competing-facts-by-evidence)
owns the summary, task-link, and root-only metadata contracts behind these views.
