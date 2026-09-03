# Codex Sessions Choices Ledger

## Needs user

### The browser workspace follows stable Rust with a 1.90 floor

- **When:** Slice 05 (`codex-slice5`).
- **The choice:** Keep the installable root crate on its verified Rust 1.88 MSRV, while the separate unpublished browser workspace declares Rust 1.90 and is checked with stable Rust because its current renderer dependency requires that floor.
- **The gap:** The original acceptance text implied that every workspace shared Rust 1.88, but lowering or replacing the renderer is outside this slice and raising the CLI MSRV would couple unrelated products.
- **The reach:** Native users retain the existing MSRV; browser contributors and CI need stable Rust 1.90 or newer. A later renderer change can reunify the policies.
- **Verdict:** needs-user — the split is truthful and reversible, but maintaining two toolchain floors is a product/maintenance choice.
- **Confidence:** high.

### Equal-timestamp normalized facts share one scrubber position

- **When:** Slice 03 (`codex-slice3`).
- **The choice:** Several normalized facts emitted from one provider record at the same timestamp are assigned to the same scrubber column. Two tool starts in one Claude assistant turn therefore remain a visible two-call burst after the wire-to-event cutover instead of being split merely because the neutral stream has finer granularity.
- **The gap:** The plan preserves replay presentation but does not define whether scrubber geometry is based on normalized fact index or event time when one wire record expands.
- **The reach:** Only sparkline bin placement changes; timeline ordering, fold count, and event content remain fact-based.
- **Verdict:** needs-user — this is a presentation default. The provisional choice preserves the existing visual meaning and is reversible inside scrubber tallying.
- **Confidence:** medium.

### Unknown tool completion is shown as a separate uncertain count

- **When:** Slice 03 (`codex-slice3`).
- **The choice:** A `CompletedUnknown` call is excluded from success, failure, and pending counts, uses a neutral question-mark state in the UI, and adds an `uncertain tool results` line to inspect output when present. It is still included in the total tool-call count.
- **The gap:** The plan forbids presenting uncertain results as success but does not prescribe the user-facing wording or whether uncertainty belongs beside or inside the three existing tallies.
- **The reach:** Codex spawn acknowledgements no longer appear green; Claude inspect output is byte-for-byte unchanged because it has no uncertain results.
- **Verdict:** needs-user — the conservative state is required, while this exact presentation is a reversible product choice.
- **Confidence:** medium.

### Compact tool summaries use a small, ordered field vocabulary

- **When:** Slice 01 (`codex-slice1`).
- **The choice:** When a tool starts, the decoder keeps the call id and tool name exactly, then chooses the first useful string from `task_name`, `description`, `command`, `file_path`, `path`, or `query` as a short display summary. It flattens whitespace and limits the summary to 200 characters. For example, a spawn call with `task_name: "index-tests"` becomes a tool event labelled `index-tests`; an unfamiliar tool whose useful argument is under another key still has a correct id/name but no summary. The unbuilt alternative is to retain every argument as generic JSON and make a later UI layer decide what is safe and useful to show.
- **The gap:** The plan requires normalized tool starts and spawn labels, but does not specify the provider-neutral argument representation or summary policy.
- **The reach:** Later model and UI work may treat `ToolStart.summary` as the adapter's final display-ready description, so this establishes where argument summarization lives and which fields are visible.
- **Verdict:** needs-user — this is a presentation/product choice rather than a correctness requirement. The recommended provisional call is to keep the compact adapter-owned summary because it avoids exposing a provider wire object throughout the core; reversing it only requires replacing the optional summary field before Slice 03 removes the old path.
- **Confidence:** medium.

### Equal-mtime automatic discovery prefers Codex, then the greater path

- **When:** Slice 02 (`codex-slice2`).
- **The choice:** If two eligible root families for the same cwd have exactly the same newest filesystem modification time, discovery orders provider-qualified keys deterministically: Codex after Claude, then lexicographically greater paths. The ordinary case still chooses the newest family; explicit files bypass this choice entirely. The unbuilt alternatives are Claude-first or treating an exact tie as ambiguous.
- **The gap:** The plan requires deterministic cross-provider tie-breaking but deliberately does not choose which provider wins an otherwise indistinguishable tie.
- **The reach:** This affects only automatic cwd discovery at exact timestamp ties, which can occur with copied fixtures or coarse filesystems. It does not change family eligibility, explicit-file pinning, or normal newest-session behavior.
- **Verdict:** needs-user — the ordering is a product default rather than a correctness fact. The recommended provisional call is to keep it: it is stable, reversible, and gives the newly supported provider a predictable result without adding a prompt to a read-only launcher.
- **Confidence:** medium.

## Sound

### Portable Claude identity uses recorded metadata, then the caller's selected filename

- **When:** Slice 05 (`codex-slice5`).
- **The choice:** Portable replay uses a non-whitespace Claude `sessionId` when present and otherwise requires the browser caller to supply the selected main filename stem. Codex always replaces that hint with the non-whitespace thread id from `session_meta`. Valid identifiers retain their recorded bytes; whitespace is a validity check, not a normalization step.
- **The gap:** Browser bytes have no native path manifest, while the app requires the same real provider-qualified identity used by event actors and stale-batch rejection.
- **The reach:** Browser loads no longer invent a shared `session` key, and explicit Codex children remain the logical root of their own view.
- **Verdict:** sound — identity comes from provider evidence or the user-selected file, never a format masquerade or constant.
- **Confidence:** high.

### Portable lifecycle dating never withholds a normalized fact

- **When:** Slice 05 (`codex-slice5`).
- **The choice:** The browser feed resolves `AtAgentStart` and `AtAgentEnd` immediately only when it already knows that actor's first or last dated event. Otherwise it delivers the relative-time event unchanged in the same batch; the timeline owns cross-batch redating.
- **The gap:** Buffering an unresolved journal result inside the feed could lose it forever when no later activity arrived, violating the one-line-to-all-events boundary.
- **The reach:** Snapshot/append convergence is judged through the timeline/model projection, while direct feed consumers receive every fact exactly once.
- **Verdict:** sound — parsing preserves evidence and the existing timeline remains the sole owner of deferred presentation dating.
- **Confidence:** high.

### Content detection waits for a provider-positive record

- **When:** Slice 05 (`codex-slice5`).
- **The choice:** Portable detection skips malformed and unknown JSONL and validates provider-specific structure: a Codex `session_meta` needs a non-whitespace payload id; a Claude record needs its recognized envelope field and any identity must be non-whitespace. Valid ids are preserved rather than trimmed. If no positive discriminator exists, the legacy fallback remains Claude so empty or metadata-poor Claude uploads still open under the caller-supplied filename.
- **The gap:** `type` spellings alone can collide with unrelated JSONL; for example, a wrong-shaped `user` record can precede a valid Codex header.
- **The reach:** Noise cannot steal a Codex thread's identity, while existing Claude browser uploads retain their permissive fallback.
- **Verdict:** sound — a provider claim needs positive evidence, and the fallback preserves established input behavior without fabricating Codex identity.
- **Confidence:** high.

### Replacement commits only after positive content and fresh family evidence

- **When:** Slice 04 (`codex-slice4`).
- **The choice:** A reset records every changed path and waits until each has a complete recognizable Claude record or Codex header. The subsequent full reload trusts the freshly resolved manifest; a known non-root file omitted from it is retained only while the path is missing or unreadable, never when readable content now belongs to another family.
- **The gap:** The live contract required both transient-file retention and family isolation but did not specify how to distinguish a temporarily unavailable child from a path reused by another rollout.
- **The reach:** Partial replacements emit no stale-key snapshot, restored files retry safely, and an atomically replaced child cannot import another session into the current graph.
- **Verdict:** sound — positive content proves readiness while the catalog remains the sole owner of family eligibility.
- **Confidence:** high.

### Codex family order is ancestry depth, then stable identity

- **When:** Slice 04 (`codex-slice4`).
- **The choice:** A manifest emits the root first, then spawned descendants by ancestry depth, with provider-qualified key and path as deterministic sibling tie-breakers.
- **The gap:** Path or key order is deterministic but not topological; a lexically early grandchild can otherwise be inserted before its parent and receive fallback graph placement.
- **The reach:** Snapshot and late-family synthetic discovery create every parent before its children without making the graph inspect provider metadata.
- **Verdict:** sound — topology is a manifest concern, while sibling order remains stable and reproducible.
- **Confidence:** high.

### Spawn-result references are exact, unique structural evidence

- **When:** Slice 04 (`codex-slice4`).
- **The choice:** A Codex spawn result may name the child it launched using the observed `task_name` or `path` field. The adapter retains only that exact string, and the model links it to a child header only when exactly one spawning call under the same parent returned the same string. If two calls both return `/root/worker`, neither is guessed to be the child’s call; a later stable activity id can still make the exact join. An arbitrary message mentioning `/root/worker` is not treated as structural evidence.
- **The gap:** The plan orders path evidence below stable call ids but does not define which result fields count as an exact path or what happens when an exact value is not unique.
- **The reach:** Child provenance, prompt attribution, and spawn completion joins remain independent of file arrival order without exposing provider result JSON to the model or manufacturing context from a plausible string.
- **Verdict:** sound — a unique exact field is useful evidence, while ambiguity and prose both fail closed.
- **Confidence:** high.

### Spawn provenance merges by evidence strength rather than arrival order

- **When:** Slice 03 (`codex-slice3`).
- **The choice:** Exact call-site provenance from `ToolStarted` outranks lifecycle descriptors, which outrank metadata placeholders. Weaker evidence may fill a missing field but cannot overwrite a stronger one. Equal-strength conflicts keep the earliest timestamp and then the lexicographically smaller reasoning string, so arrival order cannot choose the winner.
- **The gap:** Parent, sidecar, and call records can arrive in either order, and the event contract did not prescribe how their overlapping provenance fields merge.
- **The reach:** Live discovery before transcript reading and timestamp-sorted snapshot replay now produce the same prompt era and preceding reasoning for a child.
- **Verdict:** sound — the record closest to the spawning call carries the strongest timestamp/context evidence while still allowing partial records to enrich absent fields.
- **Confidence:** high.

### Lifecycle reduction uses latest event time, with terminal evidence winning exact ties

- **When:** Slice 03 (`codex-slice3`).
- **The choice:** The model retains the latest typed lifecycle fact per agent by timestamp, so `completed → interacted → completed` reaches the same state regardless of file-delivery order. A timestamped `interacted` fact is also child activity. If two different states have the exact same or no timestamp, the deterministic order is failed, interrupted, completed, then running.
- **The gap:** Slice 01 normalized lifecycle events but did not define commutative reduction or an equal-time rule.
- **The reach:** Resumed Codex children cannot remain terminal because an older completion happened to arrive last, and immediate liveness recomputation cannot erase a real interaction.
- **Verdict:** sound — timestamps are the strongest available ordering evidence; the tie rule fails toward explicit terminal evidence.
- **Confidence:** high.

### Spawn-result completion eligibility is an adapter-owned semantic fact

- **When:** Slice 03 (`codex-slice3`).
- **The choice:** `ToolFinish` states whether it may also prove completion of a synchronously spawned child. Claude results retain the legacy completion heuristic; Codex results are ineligible because spawned-thread completion comes from typed lifecycle activity.
- **The gap:** A generic successful tool result and a child lifecycle completion are distinct facts, but the initial event contract did not encode the distinction.
- **The reach:** The provider-neutral model contains no provider-name branch and cannot turn a Codex `spawn_agent` acknowledgement into a false terminal child.
- **Verdict:** sound — the adapter interprets wire meaning once and consumers fold only normalized semantics.
- **Confidence:** high.

### Empty transcript records normalize to a silent activity fact

- **When:** Slice 03 (`codex-slice3`).
- **The choice:** A Claude user/assistant record that yields no richer prompt, text, reasoning, usage, or tool fact emits `Activity`. The event updates timestamps and cross-file joins but creates no log caption or tool marker.
- **The gap:** Dropping empty sidechain records lost the only evidence for child liveness and for dating untimed metadata/journal facts.
- **The reach:** Replay ordering and liveness preserve Claude behavior without leaking Claude wire entries into the model or inventing visible content.
- **Verdict:** sound — the event expresses exactly the surviving evidence: activity occurred at a time.
- **Confidence:** high.

### Snapshot handoff transfers decoder state and retryable metadata

- **When:** Slice 03 (`codex-slice3`).
- **The choice:** Each tracked file crosses snapshot-to-tail with its consumed byte offset, file identity, and mutated decoder. Snapshot bytes and identity come from the same open handle. A syntactically complete final JSON record is consumed even without a trailing newline, while an incomplete tail stays unread. Codex decoders are not finalized at a temporary EOF. Known transcripts that are temporarily unreadable remain tracked with their provider decoder at offset zero, and Claude sidecars that are unreadable or malformed remain pending, so live polling retries both after the writer finishes.
- **The gap:** Offsets alone suppress appended Codex child activity by losing the owned-turn gate; separate open/stat operations can seed mismatched identity; dropping an unreadable manifest file prevents later recovery; and treating a mid-write sidecar as consumed leaves an agent permanently unparented.
- **The reach:** Appends between bulk load and tail start are delivered once, complete last records are not lost, partial records are not consumed early, same-size replacement is detected on the first stat, and transient transcript or sidecar writes recover without a restart.
- **Verdict:** sound — all parsing state needed to continue a stream moves with the stream cursor.
- **Confidence:** high.

### A result is successful or failed only when its payload provides recognizable evidence

- **When:** Slice 01 (`codex-slice1`).
- **The choice:** A tool result reports success or failure when a structured payload contains `is_error`, `exit_code`, or a known terminal `status`, or when a text result starts with Codex's observed `Script completed`, `Script failed`, or `exec_command failed` envelope. Current custom-tool results wrap those envelopes in an array, so the decoder reads only blocks explicitly typed `input_text`; one recognized failure wins, otherwise at least one recognized success is required. A result such as `{"task_name":"child-a"}` proves only that a result exists, so it becomes `CompletedUnknown` instead of green success. The unbuilt alternative is to count every result as success unless it explicitly says error, which would make ambiguous spawn acknowledgements look successful.
- **The gap:** The plan fixes the three outcomes but does not enumerate which heterogeneous function/custom-tool result shapes are trustworthy evidence for each one.
- **The reach:** Failure counts, tool colors, and later agent liveness all inherit this conservative boundary; adding another success/failure spelling requires evidence and an adapter change rather than a consumer heuristic.
- **Verdict:** sound — it follows the plan's fail-closed meaning for uncertain completion while still recognizing the current Codex envelopes.
- **Confidence:** high.

### Missing response-item ids fall back to a turn-and-content identity

- **When:** Slice 01 (`codex-slice1`).
- **The choice:** Assistant and reasoning records normally de-duplicate by their response-item id. If a malformed or older record omits that id, the decoder uses the current turn plus the visible text as its identity. Thus two identical id-less mirrors in one turn appear once, while the same sentence in a later turn remains a new event. The unbuilt alternatives are to emit every id-less record, risking double counts, or drop every id-less record, losing visible content.
- **The gap:** Deterministic canonical de-duplication is required, but the no-id fallback key is unspecified.
- **The reach:** This determines replay behavior for incomplete and future-compatible records and prevents provider-specific de-duplication from leaking into the model.
- **Verdict:** sound — it preserves useful content and confines possible collapsing to exact repeated content within one turn.
- **Confidence:** medium.

### Tool finishes survive even when the matching start is absent

- **When:** Slice 01 (`codex-slice1`).
- **The choice:** A valid result with a call id becomes a `ToolFinished` event even if this decoder instance has not seen the matching start. For example, a truncated file beginning at a result still reports the known outcome; a later reducer can leave it orphaned or pair it if the start arrives through another loaded segment. The unbuilt alternative is to discard results until their starts have been observed, which makes parsing order and file completeness decide whether factual result evidence survives.
- **The gap:** The plan requires call/result identity and defensive truncated-input behavior but does not say whether pairing is enforced in the decoder or reducer.
- **The reach:** Later replay and live folding can own pairing without asking the provider adapter to buffer indefinitely, and partial input loses less information.
- **Verdict:** sound — normalization should preserve a trustworthy fact; graph pairing belongs to the state model.
- **Confidence:** high.

### Auxiliary rollouts decode normally but are distinguished in metadata

- **When:** Slice 01 (`codex-slice1`).
- **The choice:** A rollout whose header source is a non-thread-spawn subagent object gets `SessionOrigin::Auxiliary` and its own subsequent activity can still normalize. Discovery in Slice 02 can therefore exclude it from `zoe <dir>`, while a user who explicitly opens that JSONL can inspect what it contains. The unbuilt alternative is to suppress all auxiliary activity at the decoder, making even explicit-file replay metadata-only.
- **The gap:** The plan says auxiliary sessions cannot win root discovery, but does not specify explicit-file behavior for them.
- **The reach:** This keeps eligibility in the catalog owner rather than adding a second discovery policy inside the format decoder.
- **Verdict:** sound — it preserves the ownership seam: decoding answers what a file says; discovery answers whether the file is an automatic root candidate.
- **Confidence:** high.

### Direct activity timestamps outrank their enclosing record timestamp

- **When:** Slice 01 (`codex-slice1`).
- **The choice:** When a subagent activity payload includes `occurred_at_ms`, the normalized spawn/status uses that event time; otherwise it falls back to the outer JSONL timestamp. For example, a delayed event record still places the child's birth when Codex says it occurred. The unbuilt alternative always uses the write timestamp, which is simpler but can move births and completions later than the provider's explicit event time.
- **The gap:** Timestamp semantics were required, but the precedence between the two available Codex timestamps was not stated.
- **The reach:** Timeline ordering, durations, and nested-agent birth markers inherit this precedence.
- **Verdict:** sound — the most specific typed event timestamp is stronger evidence than its serialization time, with a defensive fallback when absent or invalid.
- **Confidence:** high.

### Discovery caps every candidate header read at 64 KiB

- **When:** Slice 02 (`codex-slice2`).
- **The choice:** The catalog reads no more than 64 KiB from a candidate rollout to obtain its first metadata record. A larger or malformed header is ineligible for automatic discovery but can still be opened explicitly and decoded defensively. The unbuilt alternative is to keep reading until a complete first line appears, which lets one hostile or corrupt file turn discovery into an unbounded history read.
- **The gap:** The plan requires bounded discovery but leaves the concrete ceiling to implementation.
- **The reach:** The bound protects every native refresh. Observed valid headers are substantially smaller, and tests pin both the bound and malformed-file behavior.
- **Verdict:** sound — a fixed generous ceiling enforces the read-only catalog's resource contract without changing valid observed inputs.
- **Confidence:** high.

### Refresh walks bounded calendar metadata but caches unchanged headers

- **When:** Slice 02 (`codex-slice2`).
- **The choice:** Each refresh examines the active Codex year/month/day hierarchy so a new bucket cannot be hidden by stale parent-directory metadata. Candidate headers are reread only when their cached file identity changes. The unbuilt alternative is a persistent index or trusting directory mtimes as a recursive change signal.
- **The gap:** The plan requires discovering newly created day buckets while avoiding full-history reads, but does not prescribe cache invalidation mechanics.
- **The reach:** Live discovery pays metadata-walk cost proportional to active candidate files, not JSONL body size, while remaining stateless across process launches.
- **Verdict:** sound — it keeps filesystem truth authoritative and avoids a writable cache with invalidation failure modes.
- **Confidence:** high.

### Unix replacement detection includes device and inode identity

- **When:** Slice 02 (`codex-slice2`).
- **The choice:** On Unix, the bounded-header cache key includes device and inode in addition to size and modification time, so replacing a file with the same size and timestamp still invalidates its metadata. Platforms without stable identity conservatively reread bounded headers. The unbuilt alternative is a size/mtime-only cache everywhere.
- **The gap:** The plan requires safe replacement handling but does not define which portable metadata can prove file continuity.
- **The reach:** This prevents automatic discovery from retaining another session's stale cwd or parentage after atomic replacement.
- **Verdict:** sound — it uses stronger evidence where available and fails toward bounded rereads elsewhere.
- **Confidence:** high.

### An explicit Claude file with no header cwd keeps cwd unknown

- **When:** Slice 02 (`codex-slice2`).
- **The choice:** Content-sniffed explicit Claude files that do not provide a cwd use `None`; the catalog does not invent the empty path or infer cwd from the filename. The unbuilt alternative is a sentinel path that downstream code could accidentally compare as real provenance.
- **The gap:** Automatic Claude discovery knows its sanitized project directory, but arbitrary explicit files may not carry equivalent provenance.
- **The reach:** Explicit replay remains available while cwd filtering and display distinguish unknown metadata from a real directory.
- **Verdict:** sound — absence stays absence and cannot silently become false provenance.
- **Confidence:** high.

### Duplicate Claude sidecar identities collapse deterministically

- **When:** Slice 02 (`codex-slice2`).
- **The choice:** When multiple Claude sidecars describe the same actor or workflow identity, the manifest retains one deterministic file assignment, while the explicitly selected root always remains the root. The unbuilt alternative is to expose duplicate logical actors based on directory iteration order.
- **The gap:** Existing transcript directories can contain overlapping metadata files, but the model requires stable actor identity independent of enumeration order.
- **The reach:** Replay and live loading receive one logical source per identity and cannot double-count merely because duplicate sidecars exist.
- **Verdict:** sound — deterministic de-duplication preserves the manifest's identity contract and explicit-file authority.
- **Confidence:** high.
