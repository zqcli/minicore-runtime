# Conversation Contract

Conversation is the only durable execution history owned by Core. It is append-only from the Core perspective and validated as one canonical sequence.

Source: [`src/conversation/entry.rs`](../../src/conversation/entry.rs), [`src/conversation/validator.rs`](../../src/conversation/validator.rs), [`src/conversation/log.rs`](../../src/conversation/log.rs), [`src/conversation/recovery.rs`](../../src/conversation/recovery.rs), and [`src/conversation/settlement.rs`](../../src/conversation/settlement.rs). Evidence lives in the conversation log/validator tests, [`session_runtime_turn_contract.rs`](../../tests/session_runtime_turn_contract.rs), and the atomic multi-result settlement/restart cases in [`session_runtime_restart_event_evidence.rs`](../../tests/session_runtime_restart_event_evidence.rs).

## Five Durable Entries

`ConversationEntry` has exactly five variants:

1. `UserMessage`: Turn identity, checked user input, frozen model/reasoning/tool-round execution record, timestamp.
2. `AssistantMessage`: model output, reasoning, ordered tool calls, usage, finish reason, timestamp.
3. `ToolResult`: exact Turn/call/name identity, typed outcome, bounded content, timestamp.
4. `Summary`: a canonical completed boundary, bounded summary text, timestamp.
5. `TurnTerminal`: exact Turn, terminal classification, conservative usage, timestamp.

There are no durable interaction, pending approval, continuation, job, event, workspace, provider, or actor-state entries.

## Validator Invariants

The validator requires contiguous `ConversationSeq` values and applies a batch to a cloned candidate before mutating confirmed state. It enforces:

- one active Turn at a time;
- User execution matches the durable SessionSpec;
- Assistant shape, model, text/reasoning limits, finish reason, and enabled tools;
- zero-based ordered ToolCall indices and globally unique ToolCall IDs;
- ToolResults match the next pending call exactly and occur once;
- no terminal while calls remain unresolved;
- `Completed` requires a final Assistant response;
- at most one terminal per Turn;
- Summary boundaries are authenticated prior terminal boundaries and advance monotonically.

Invalid batches do not call the adapter and do not mutate the confirmed projection.

## Active-Turn Summary

A Summary may be appended while a newer Turn is active only when `through` identifies an authenticated terminal boundary before that active Turn. The Summary replaces compacted prior history for prompt projection but must preserve the active User entry, execution record, phase, pending ToolCalls, seen call IDs, and terminal eligibility.

A Summary cannot cross a nonterminal boundary, summarize the active Turn, forge a boundary, or alter the durable head it was proposed against. The actor requires exact stale-head equality before append.

## Durable-First Mutation

Only `ConversationLog` assigns sequence numbers and timestamps. Runners submit unsequenced drafts; SessionActor submits them to ConversationLog. Confirmed in-memory state advances only after the adapter returns an exact `AppendReceipt` proving one canonical prefix extension.

Known append failure leaves confirmed memory unchanged. Unknown outcome, timeout, panic, or invalid receipt never commits memory and latches durability uncertainty. Public state, ToolFinished, TurnHandle completion, and TurnFinished obey the durable-first order defined by their contracts.

## Settlement

Settlement derives from the confirmed Conversation. If calls remain unresolved, Core synthesizes one bounded ToolResult per pending call in canonical order, then appends the `TurnTerminal` in the same atomic batch. Terminal append is first-wins and exactly once.

A critical Assistant, ToolResult, Summary, or settlement append failure degrades the Session, cancels the active Turn, and forbids a fallback terminal append. Core reports durability unavailable/unknown instead of fabricating completion.

## Restart Repair

Load replays and validates all pages before readiness. An unfinished Turn is repaired once:

- unresolved ToolCalls receive deterministic `Cancelled` ToolResults in call order;
- one `CancelledByRestart` terminal follows;
- the repair is one atomic append batch.

Already terminal history receives no repair. Pending approvals, ToolInput requests, Tool futures, Model continuations, and in-memory cancellation state are not restored. An unknown repair outcome fails load without spawning a ready actor.

## Transcript

`SessionHandle::transcript(after, limit)` returns only adapter-confirmed entries in a checked `TranscriptPage`. Page sequence, cursor, observed head, and slice contents are validated against the confirmed projection. If adapter transcript I/O becomes uncertain, Core falls back to confirmed memory where defined and never exposes a speculative entry.
