# ADR 0024: SessionStorage Uses By-Entry JSONL

Status: Accepted
Date: 2026-07-16

## Context

ADR 0019 established the important invariant that every created Session has one trusted runtime write seam and that only durable facts may advance runtime-owned projections. It also chose one atomic `StoredSessionBatch` per JSONL line so a complete ToolRound and terminal cleanup could be promoted as one physical transaction.

The later Transcript-First design made the actual semantic boundary clearer: physical durability and model visibility are different concerns. A ToolCall response, each truthful ToolResult, approval state, and side-effect barrier may need to survive a crash independently, while the corresponding assistant/tool sequence must remain hidden from the next model call until the round is complete. Encoding the whole business unit as one physical JSONL line obscures those independent facts, complicates replay and fork, and differs from the entry-oriented storage used by the comparable runtimes studied for this refactor.

MiniCore therefore retains one trusted writer and committed-only projection advancement, but replaces the physical batch protocol with by-entry append and an explicit conversation gate.

## Decision

1. A Session JSONL file contains one `SessionHeader` line followed by one `StoredSessionEntry` per physical line.
2. Every created Session has one runtime write seam: `SessionWriter::append(SessionEntryDraft) -> CommittedSessionEntry`.
3. `StoredSessionEntry` contains `entry_id`, `parent_id`, `timestamp`, `operation_key`, and `body`. The stable top-level body family is `TurnContext | Message | Event | Compaction`.
4. Messages use the standard roles `user | assistant | tool`. One finalized provider assistant response is one entry whose ordered `content[]` preserves returned reasoning, text, and tool calls. Usage and provider response metadata belong to that assistant entry. Each ToolCall result is a separate `role = tool` message.
5. The writer validates one draft, assigns storage-owned identity, enforces parent and operation-key rules, appends one line, and returns the committed entry plus projection delta. It does not schedule Turn execution or Tool work.
6. Operation-key idempotency is entry-scoped. An append acknowledgement with unknown outcome is resolved by reopen/replay or operation-key lookup before retry. Storage outcome unknown is not Tool side-effect outcome unknown.
7. Physical append does not by itself imply model visibility. Initiating and Steer user messages become visible when appended. Assistant tool-call responses and matching tool messages remain durable but hidden until a `tool_round_completed` event references the complete ordered set. A final assistant message becomes visible when appended and completes the Turn. Compaction replaces the conversation projection.
8. `tool_execution_started` remains the durable side-effect barrier: it must be appended and applied before an external Tool side effect is allowed. Interaction request must be appended before notification; resolution must be appended before wake or side effect.
9. Turn start linearizes at the initiating user message append. Normal completion linearizes at the final assistant message append. Interrupted and failed Turns end at their corresponding durable event. Cleanup before interruption/failure uses separate entries with stable operation keys; it is replay-idempotent, not a physical transaction.
10. `EntryId + parent_id` forms the Session history tree. Fork copies one committed parent path into staging storage and remaps target-local identities and nested references. Fork copies history, not AgentLoop, Tool task, approval waiter, provider session, Workspace lease, or other execution state.
11. Recovery may ignore only a final unterminated partial line. A complete malformed line, duplicate operation key with different content, invalid parent/reference, or impossible terminal state fails closed. Recovery never invents a ToolResult, automatically replays an outcome-unknown Tool, or automatically appends a missing `tool_round_completed` event.
12. Streaming deltas, partial assistant drafts, Tool progress, phase changes, heartbeats, and ordinary observer notifications are not authoritative JSONL entries. Public Runtime events are derived after committed entries are applied; they are not copied into a second durable event stream.
13. The storage protocol does not add `Begin`/`Commit` markers, `group_id`, batch fingerprints, `BatchId`, batch-result `LeafId`, or another physical transaction framing layer.

## Consequences

- ADR 0019 is superseded only in its physical batch protocol, atomic business-batch semantics, and batch-specific types. Its one trusted writer, commit-before-publication, and single durable truth principles remain in force.
- A crash may leave truthful but conversation-hidden assistant/tool entries. Replay preserves them, while the conversation projection advances only when the explicit completion gate exists.
- ToolInvocation operational completion and model-visible ToolRound completion are distinct: a truthful tool message completes the Item projection; `tool_round_completed` promotes the complete round.
- Terminal and recovery cleanup can be partially appended before a crash. Stable operation keys and replayed projection state allow deterministic continuation without pretending the sequence was atomic.
- Writer, replay, fork, lookup, and JSONL adapters become entry-oriented. Session execution owns the sequential `append -> apply -> gate` orchestration.
- `SessionStorage` remains the only durable truth. Hot projections, indexes, snapshots, sidecars, and observer streams remain rebuildable.

## Supersedes And Amends

- Supersedes the physical batch protocol in [ADR 0019](0019-session-writes-use-one-trusted-batch-writer.md). The single trusted writer principle is retained.
- Amends [ADR 0023](0023-driver-starts-from-one-committed-conversation-seed.md): a `CommittedConversationDelta` may result from one committed entry or from deterministic application of several entry receipts, but the Driver may consume only facts admitted by the conversation projection gate.
- Refines [ADR 0021](0021-session-runtime-separates-actor-control-from-run-execution.md): the authoritative loaded-Session owner remains responsible for writer/projection ordering, while external model and Tool work may run as cancellable futures. No execution task may own a second writer or projection.
