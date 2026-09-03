# Codex Sessions Choices Ledger

## Needs user

### Compact tool summaries use a small, ordered field vocabulary

- **When:** Slice 01 (`codex-slice1`).
- **The choice:** When a tool starts, the decoder keeps the call id and tool name exactly, then chooses the first useful string from `task_name`, `description`, `command`, `file_path`, `path`, or `query` as a short display summary. It flattens whitespace and limits the summary to 200 characters. For example, a spawn call with `task_name: "index-tests"` becomes a tool event labelled `index-tests`; an unfamiliar tool whose useful argument is under another key still has a correct id/name but no summary. The unbuilt alternative is to retain every argument as generic JSON and make a later UI layer decide what is safe and useful to show.
- **The gap:** The plan requires normalized tool starts and spawn labels, but does not specify the provider-neutral argument representation or summary policy.
- **The reach:** Later model and UI work may treat `ToolStart.summary` as the adapter's final display-ready description, so this establishes where argument summarization lives and which fields are visible.
- **Verdict:** needs-user — this is a presentation/product choice rather than a correctness requirement. The recommended provisional call is to keep the compact adapter-owned summary because it avoids exposing a provider wire object throughout the core; reversing it only requires replacing the optional summary field before Slice 03 removes the old path.
- **Confidence:** medium.

## Sound

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
