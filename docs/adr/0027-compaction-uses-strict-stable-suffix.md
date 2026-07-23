# ADR 0027: Compaction Uses A Strict Stable Suffix

Status: Accepted
Date: 2026-07-24

## Context

MiniCore has established Transcript-First model input, by-entry JSONL, one authoritative SessionExecutor per loaded Session, one PromptSet assembly seam, and one provider-neutral ModelGateway operation. Compaction must reduce model-visible history without creating a second conversation truth, splitting a Tool protocol sequence, losing the current user request, or installing an in-memory replacement that cannot be recovered after a crash.

Comparable runtimes make different trade-offs. pi uses rolling summaries and recent history, but can split a long Turn into separate summaries and runs post-turn/manual compaction. Codex supports local and provider-native compaction, pre/mid-turn recovery, model fallback, and replacement history. Rig exposes load-time memory policies and rolling carry-over without owning MiniCore's durable Session execution order. Claude Code exposes automatic and manual compaction, but its internal cut and persistence rules are not public.

MiniCore's first implementation should prioritize durable truth, protocol-safe cuts, deterministic replay, and bounded recovery. It should not begin with split-turn history, provider-specific opaque artifacts, or a manual maintenance state.

## Decision

1. Compaction is a crate-internal planning and validation module. SessionExecutor owns trigger evaluation, asynchronous execution, control arbitration, writer calls, projection application, and Turn failure policy.
2. Automatic compaction is evaluated only at an active Turn `NeedModel` safe point. Triggers are soft context pressure, Prompt local context overflow, and provider context overflow. Final assistant completion does not trigger eager post-turn compaction.
3. A cut is selected over model-visible stable conversation units: one UserMessage, one complete ToolRound, one final AssistantMessage, or one existing Compaction summary. A cut may occur only between units.
4. The compacted range is one contiguous prefix and the exact retained range is one contiguous suffix. The projector does not remove arbitrary messages from the middle of history.
5. The active Turn initiating UserMessage is hard protected. Because the retained range is contiguous, all later active-Turn units are also retained exactly. If this protected suffix is too large, the first implementation reports `ProtectedSuffixTooLarge`; it does not split, summarize, or truncate the current Turn.
6. Repeated compaction is a portable rolling summary. The previous effective summary and newly evicted stable units are summarized into one new summary followed by the retained suffix.
7. Summary generation uses the active Turn's exact `TurnModelSnapshot`, `ModelCallPurpose::CompactionSummary`, and `OutputContract::NoToolCalls`. PromptSet remains the only model-context assembly seam, and ModelGateway remains the only model-call seam.
8. Compaction-summary assembly includes required Runtime safety policy, a typed compaction directive, and a trusted committed prefix. It omits ordinary Agent, Session, Workspace, Tool, and Skill instructions because the next AgentRun assembly re-injects the same Turn-pinned static inputs.
9. Large Tool results may be reduced only in the summary request representation. The representation records tool identity, outcome, head/tail content, original size, content hash, and omitted size. Durable Tool messages are never rewritten.
10. A successful SummaryModel call does not change conversation state. SessionExecutor revalidates SessionId, TurnId, execution version, source conversation checkpoint and fingerprint, Turn context, model, PromptSet, cancellation, and Workspace authorization before appending one `StoredCompaction` entry.
11. `StoredCompaction` contains the source checkpoint, stable boundaries, summary, protected EntryIds, and model-call metadata. The trusted storage projector derives `Replace([summary] + retained suffix)`; callers cannot provide an arbitrary replacement message vector.
12. The Compaction entry append/apply is the linearization point. Before it, the old conversation is authoritative. After it, SessionExecutor rebuilds ConversationSeed and the private AgentLoop segment while retaining the same TurnExecutionContext.
13. `TurnExecutionPhase::Compacting` is transient and keeps `TurnStatus = Running`. Steer queues, while Cancel and Workspace revocation can cancel the operation and win before append.
14. Soft-pressure failure may fall back to the original uncompressed ModelCallRequest only when its checkpoint, assembly fingerprint, execution version, and control state are still exact. Hard-overflow failure terminates the Turn.
15. One active Turn may perform at most one automatic overflow recovery. If the post-compaction AgentRun still overflows, the Turn fails instead of entering an unbounded compact-and-retry loop.
16. Restart replays a durable Compaction overlay but never resumes a summary call, retry timer, CompactionPlan, provider continuation, or old AgentLoop. Original entries remain append-only and are available on history branches before the Compaction entry.
17. The first implementation does not provide split-turn summaries, hierarchical summary trees, provider-native compaction, cross-model compaction fallback, standalone/manual compaction, or deterministic conversation truncation.

## Consequences

- ToolCall and ToolResult protocol ordering remains valid across every cut.
- The current user request remains exact, but a single active Turn can become too large to recover. That limitation is explicit rather than hidden behind lossy truncation.
- Summary calls are portable across restart, fork, and future provider changes because the durable result is provider-neutral text plus typed provenance.
- Static Prompt, Workspace, Tool, and Skill content is not duplicated into summaries and cannot become stale summary authority.
- Storage writer and projector validation are more complex than direct in-memory history replacement, but replay is deterministic and SessionStorage remains the only durable truth.
- Post-turn latency is not paid speculatively; the next NeedModel may pay compaction latency.
- Provider-native compaction can be added later only as an optimization that preserves an equivalent portable durable representation and exact request semantics.
- Manual compaction requires a future Runtime maintenance protocol rather than implicitly cancelling an active Turn.

## Supersedes And Amends

- Supersedes [ADR 0002](0002-compaction-is-session-runtime-owned.md) in terminology and orchestration shape. The retained principle is that session execution, not Driver or AgentLoop, owns compaction.
- Amends [ADR 0024](0024-session-storage-uses-by-entry-jsonl.md): Compaction Replace is derived from stable boundaries and trusted source projection; a caller cannot append an arbitrary replacement vector.
- Amends [ADR 0025](0025-loaded-session-uses-one-session-executor.md): `CompactConversation` is a cancellable RunningOperation coordinated by the same SessionExecutor and represented by transient `TurnExecutionPhase::Compacting`.
- Amends [ADR 0026](0026-model-gateway-uses-one-deep-async-operation.md): SummaryModel uses the same `generate_model_turn` operation with exact active-Turn model identity, `CompactionSummary` purpose, and `NoToolCalls` output contract.
