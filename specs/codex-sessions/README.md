# Codex Sessions

Add first-class Codex session support without weakening Zoetrope's existing Claude Code behavior. A native launch follows the newest eligible session for the requested working directory across both providers; explicit files remain pinned; replay, live follow, inspect, and the portable browser core consume one provider-neutral event stream.

## Next Agent Prompt

Status: Slices 01–02 are integrated on `main`; Slice 03 is complete on
`codex-slice3` pending integration. Last updated 2026-09-04.

After integrating Slice 03, begin with
[Slice 04](slices/04-codex-families-and-live-follow.md). Preserve the snapshot's
per-file decoder state and byte identity, and preserve the original
`WatchTarget` on every reattach. Complete the remaining slices in order,
update the checklist and decision ledger after every pass, and refresh this
handoff after every two or three slices or immediately after a red gate,
rebase, compaction, or feature-area change.

Inherit these decisions:

- Preserve Claude Code behavior; do not migrate or rewrite transcripts.
- Treat Codex's resumable session unit as a thread/rollout. This is a local read-only visualizer, not a Codex SDK/app-server controller.
- Use one provider-neutral domain event model. Provider wire DTOs stay private to their adapters.
- Give every file one stateful decoder. Codex child files suppress copied ancestor history until their first `inter_agent_communication_metadata` record with `trigger_turn: true`.
- Prefer response items for assistant text/reasoning and tool call/result pairs. Use typed lifecycle events for spawned threads. Never count mirrored event views twice.
- Account Codex token usage from absolute cumulative observations scoped by thread, not by summing `total_token_usage` lines.
- Only `source.subagent.thread_spawn` is a visible child. Internal helpers such as `source.subagent.other` never win root discovery.
- An explicit child rollout is the logical root of its own pinned view and never silently climbs to an ancestor.
- Browser scope is one static Codex JSONL; browser directory families and live follow are out of scope.
- Runtime remains filesystem-read-only and network-free.

Verification uses an isolated Rust 1.88 toolchain at
`/private/tmp/zoetrope-rust188`; the system/Homebrew Rust remains 1.87 and is
not valid evidence for this repository's MSRV.

Global checklist:

- [x] [Slice 01](slices/01-normalized-events-and-codex-decoder.md): normalized events and stateful Codex decoder (`153706b`)
- [x] [Slice 02](slices/02-session-catalog-and-manifests.md): bounded cross-provider discovery and manifests (`5579a88`)
- [x] [Slice 03](slices/03-core-replay-and-inspect.md): cut the model, replay, and inspect over to normalized events (`codex-slice3`, pending integration)
- [ ] [Slice 04](slices/04-codex-families-and-live-follow.md): nested Codex families and live follow
- [ ] [Slice 05](slices/05-browser-docs-and-integration.md): browser floor, copy/docs, cleanup, and feature-wide review

## Outcome and observable behavior

- `zoe` and `zoe <working-directory>` find and follow the newest eligible top-level Claude Code or Codex session whose recorded cwd matches.
- `zoe <transcript-or-rollout.jsonl>` replays that file; `--follow` tails it; explicit targets never auto-switch.
- `zoe inspect <file>` uses the same decoding/loading path as the TUI.
- Codex prompts, assistant commentary/final output, available plaintext reasoning summaries, model, output-token usage, tools/results, and direct or nested spawned agents appear once.
- New Codex child rollouts join a live family without losing or duplicating activity.
- A single Codex rollout can be dragged into the browser build and replayed locally.
- Unknown, malformed, partial, oversized, truncated, or replaced input remains defensive and non-fatal.

## End-state ownership

The refactor-clean invariant is one owner per concept:

| Concept | Sole owner | Consumers |
| --- | --- | --- |
| Provider wire schemas and canonicalization | `formats::{claude,codex}` | portable decoder boundary |
| Provider-neutral activity contract | session/domain event module | model, timeline, info, inspect, browser |
| Byte framing and file identity | tailer byte state | per-file decoders |
| Native discovery and cwd matching | session catalog | CLI/tailer |
| Files belonging to one viewed session | session manifest loader | replay, inspect, live |
| Graph/liveness/token folding | existing session model | UI and inspect |

The finished repository must not retain a Codex-to-fake-Claude compatibility layer, a second Codex-specific model, discovery in both `main` and the tailer, or direct serde parsing in public entry points. Any bridge introduced while cutting over is removed by Slice 03, and all cleanup is audited in Slice 05.

## Verified context

- The current parser/discovery module is Claude-shaped and exceeds 1,300 lines; Claude wire entries flow through tailer, model, info, replay, inspect, and WASM.
- Live delivery already owns valuable invariants: partial-line buffering, an 8 MiB cap, inode/truncation reset, snapshot offsets, session stamping, and an idle-gated newer-session switch.
- Local Codex 0.152.1 rollouts use calendar paths under `~/.codex/sessions`, with outer records including `session_meta`, `turn_context`, `response_item`, and `event_msg`.
- Root `session_meta` supplies thread id and cwd. Spawned child metadata supplies `parent_thread_id`, `agent_path`, and optional nickname under `source.subagent.thread_spawn`.
- Local child rollouts can contain thousands of copied ancestor records. The first owned turn is explicitly marked by `inter_agent_communication_metadata.payload.trigger_turn == true`.
- Response and event views overlap. `response_item.message` is the stable assistant-text source; function/custom call records pair by `call_id`; subagent activity exists in both current `item_completed/SubAgentActivity` and an older direct form.
- `token_count.info.total_token_usage` is cumulative. It must be treated as an absolute observation, not an additive delta.
- The local Codex tree is several GiB, so discovery may read bounded headers and metadata but never whole histories.
- Official Codex documentation describes threads as startable and resumable sessions and app-server as the higher-level owner of auth, history, approvals, and streamed events: <https://developers.openai.com/codex/sdk>.

Fixtures derived from local evidence must be compact, sanitized, and record the observed CLI version/schema family without copying real prompts, outputs, paths, identifiers, or secrets.

## Decisions and rejected alternatives

- Chosen: five independently rejectable slices. Rejected: two slices, because parser semantics and global live discovery would be mixed into unreviewable passes. Rejected: eight slices, because temporary parallel abstractions would live too long and several passes would lack useful user-visible progress.
- Chosen: provider-neutral events. Rejected: teach the domain model both Claude and Codex wire DTOs; that duplicates semantic decisions at every consumer.
- Chosen: bounded filesystem metadata catalog. Rejected: `session_index.jsonl`, because observed entries do not contain cwd or parentage. Rejected: persistent database/cache, because it adds mutation and invalidation to a read-only tool.
- Chosen: cumulative usage observations keyed by scope. Rejected: summing Codex totals, which inflates usage; rejected: provider conditionals in the model.
- Chosen: exact owned-turn marker for copied child prefixes. Rejected: timestamp/text/session-meta heuristics, which can plausibly misattribute ancestor activity.
- Chosen: explicit typed completion/interruption only. A tool result without a trustworthy status is `CompletedUnknown`, not fabricated success. Turn-level `task_complete` does not terminate an interactive root.
- Chosen: cwd canonicalization when paths exist, with deterministic lexical absolute fallback. Newest remains filesystem mtime with deterministic path/provider tie-breaking.

## Scope firewalls

- No Codex authentication, resume/control, SDK/app-server integration, subprocess, daemon, or network dependency.
- No transcript migration, rewrite, persistent index, or provider-directory sidecars.
- No UI redesign, graph-layout change, camera/control change, replay-speed change, or expansion into every token category.
- No decryption or display of encrypted reasoning.
- No promise to render unknown records; they must only fail safely and not poison valid records.
- No browser Codex directory import or live following.
- No archived-session auto-discovery in this feature; explicit archived files may still work by content sniffing.

## Review map

Slices 01–04 are provisionally high risk because each can silently create a plausible but false transcript, choose the wrong private session, or lose/duplicate live events. Slice 05 is medium on its own but closes with the repository's complete feature-wide review. Each implementation pass must reclassify from the actual diff.

The three planning drafts independently agreed on stateful per-file decoding, provider-neutral folding, bounded discovery, explicit-file pinning, and a single browser-file floor. A requested cross-model Claude draft could not run because the local Claude CLI was not authenticated; the third independent draft therefore used a differently biased Codex agent. This limitation changes confidence, not scope.

## Acceptance

- All existing Claude behavior stays green through characterization and the complete test suite.
- Every public entry point uses the same normalized path and reports the same facts.
- Prompt, text, reasoning, model, usage, tool, child-parent, and terminal facts match compact fixture oracles exactly once.
- Copied child prefixes contribute zero child activity.
- Cross-provider cwd discovery chooses only eligible roots and is deterministic.
- Replay-to-tail handoff, partial writes, rotation/truncation, backward seek, and live/bulk convergence remain exact.
- Native all-target tests, portable no-default-features tests, wasm check/build, clippy, rustdoc, formatting, and packaging gates pass on Rust 1.88+.
- Dependency inspection confirms no network runtime was added.
