# ADR 0023: Driver Starts From One Committed Conversation Seed

Status: Accepted
Date: 2026-07-14

## Context

BR-055 started as a question about how `ResolvedPromptInput.parts` becomes persisted messages. The follow-up review found the larger issue: MiniCore had several message-like lanes (`parts`, current input messages, durable history, Rig history, model input projection, provider request messages) without one committed seed that every run starts from.

MiniCore now accepts the **Transcript-First** design. Session storage remains the durable truth; a driver run starts from one committed conversation seed; Driver/Rig may keep only a run-scoped working projection; Prompt is the only MiniCore seam that assembles model-visible context; ModelGateway only encodes that provider-neutral context and invokes the provider.

The MiniCore-owned public interfaces below are now decided. BR-049 / the Rig integration spike is still required, but its scope is limited to validating private adapter mapping in `driver/rig.rs` and `model_gateway/rig.rs`. The spike may change adapter internals or produce a targeted follow-up issue if Rig cannot support the mapping, but it no longer reverse-decides MiniCore public seams.

## Decisions

1. **Use Transcript-First.** A run starts from a single ordered committed transcript projection, not from long-lived `durable_history` / `current_input` lanes.
2. **Resource and tool capture names are fixed.** `ResourceManager.capture_turn_resources(...)` captures turn resources. `Tools.capture_turn_tools(...) -> TurnToolProfile` captures the prompt-facing tool view and the run executor in one operation.
3. **Prompt turn preparation names are fixed.** `Prompt.prepare_message_turn(...) -> PreparedMessageTurn -> ModelContextProfile` prepares the stable model-context profile for the work chain.
4. **Current input lowering is canonical.** `compose_user_message(...) -> CanonicalUserMessage` is the only accepted current-input lowering seam. MVP prompt-like input yields one canonical user message; future multi-message submissions require a separate decision.
5. **Model context assembly is centralized in Prompt.** `assemble_model_context(...) -> AssembledModelContext` is the only MiniCore seam that converts committed conversation state plus transient context into provider-neutral, model-visible context for both `AgentRun` and `CompactionSummary` purposes.
6. **Driver starts from a committed seed.** `ConversationSeed` is built after the initial `UserInput` and any pre-run `Compaction` commit succeed. It contains the current input exactly once and has already applied the latest compaction projection. Active Steer does not mutate the existing seed; it advances `CommittedConversationState` and `LiveConversation` through a `CommittedConversationDelta`.
7. **Committed conversation changes are explicit.** Runtime code distinguishes `CommittedConversationState` (a complete committed projection) from `CommittedConversationDelta` (the exact committed entries or equivalent finalized delta returned after a batch commit).
8. **Driver entry name is fixed.** `Driver.drive_conversation(...)` is the public driver operation for a run over a `ConversationSeed` and a narrow `DriverTurnInput`.
9. **ModelGateway entry name is fixed.** `ModelGateway.generate_model_turn(...)` is the model invocation seam. It receives a `ModelCallRequest` whose input is an `AssembledModelContext`.
10. **Tool and message commit helpers are fixed.** The runtime uses `execute_and_commit_tool_round`, `commit_pending_messages`, and `commit_final_assistant_message` as the named operations for the corresponding stable-batch boundaries.
11. **Compaction is not model-context assembly.** Compaction computes cut/protection/directive data. Protected `EntryId`s are excluded from summary targets. After a compaction commit, SessionRuntime applies/reloads the committed projection and builds a new `ConversationSeed` from `CommittedConversationState`.
12. **Tools fingerprint invariant is fixed.** The prompt view and executor inside one `TurnToolProfile` must share the same fingerprint. They are never fetched through separate getters.
13. **ModelGateway does not decide visibility.** Provider adapters may encode `AssembledModelContext` into provider DTOs and choose wire optimizations such as cache/continuation, but they must not reinterpret which session records are visible or how `MessageRecord` variants map to model messages.

## Interface Sketch

The exact Rust fields still belong to the pre-development contract closure review, but the seam vocabulary and ownership are fixed:

```rust
pub struct TurnToolProfile {
    pub prompt_view: ToolPromptView,
    pub executor: ToolBatchInvoker,
    pub fingerprint: ToolProfileFingerprint,
}

pub struct PreparedMessageTurn {
    pub model_context_profile: ModelContextProfile,
}

pub struct ConversationSeed {
    pub messages: Arc<[MessageRecord]>,
    pub fingerprint: ConversationFingerprint,
}

pub struct DriverTurnInput {
    pub model: ModelSelection,
    pub context_profile: ModelContextProfile,
    pub thinking_level: ThinkingLevel,
    pub stream_options: StreamOptions,
}

pub struct ModelCallRequest {
    pub purpose: ModelCallPurpose,
    pub model: ModelSelection,
    pub input: AssembledModelContext,
    pub thinking_level: ThinkingLevel,
    pub stream_options: StreamOptions,
    pub max_output_tokens: Option<u64>,
}
```

`PreparedMessageTurn` pins captured resource input and exposes `compose_user_message(...)`; it does not contain a precomposed user message and does not hold the run-only tool executor. `AssembledModelContext` is provider-neutral and model-facing. It may include system text, `ModelMessage`s, tool schemas, output contract, diagnostics and fingerprints, but it is produced by Prompt, not by ModelGateway. `ModelCallPurpose` remains the single top-level purpose on `ModelCallRequest`; it is an assembly input and fingerprint input, not a duplicate field inside `AssembledModelContext`.

## Invariants

- SessionStorage / SessionWriter owns durable facts.
- Driver/Rig working history is not durable truth and is discarded on commit failure, compaction recovery, or a new run seed.
- A committed current user message appears in the next `ConversationSeed` exactly once.
- Steady-state seed construction uses the committed in-memory projection updated from successful commit results; it does not require rereading session files after every commit.
- Pre-run compaction excludes protected `EntryId`s, especially the just-committed user input and any required retained suffix.
- Tool-call assistant output and matching tool results are committed as one complete stable batch before Driver feeds results back to Rig.
- Active steer is first composed as a canonical user message, committed, and then applied to the same `RunId` only through a committed delta / segment rollover.
- `Prompt.prepare_message_turn` and `assemble_model_context` are the only Prompt seams that turn resources/tools/history/transient context into model-facing material.
- `ModelGateway.generate_model_turn` never reads `SessionStorage`, resource snapshots, tools, or prompt templates, and never decides session-message visibility.
- Provider cache, continuation, `previous_response_id`, prompt cache, or native compact artifacts are wire optimizations; they must remain equivalent to the complete logical `AssembledModelContext` or fail closed / fall back to full request.

## Consequences

- BR-055 is closed as a design issue: current input has a canonical lowering seam and the run seed path is defined.
- Existing docs that still mention `PromptCallProfile`, `ModelInputProjection`, `capture_profile_baseline`, or `ModelGateway.call_model` are amended by this ADR when they describe the public seam; historical discussion may remain as background.
- BR-049 remains deferred only for private Rig/provider adapter mapping. The accepted MiniCore interface vocabulary is not reopened by the spike unless a concrete impossible mapping is found and escalated as a new architecture decision.
- Compaction, Prompt, Driver and ModelGateway have a stricter separation: cut/protection/directive, context assembly, run-state adaptation, and provider invocation respectively.
- The pre-development contract closure review still needs to fill exact fields and wire types, but it must use this vocabulary and must not reintroduce competing public seams.

## Amendments

- Amends [ADR 0013](0013-driver-receives-driver-turn-input.md): `DriverTurnInput` remains narrow, but `DriveRequest` now also carries an explicit `ConversationSeed`; this does not re-expand the seam to `TurnState`.
- Amends [ADR 0017](0017-prompt-uses-immutable-turn-assembly.md): the accepted Prompt vocabulary is now `prepare_message_turn`, `PreparedMessageTurn`, `ModelContextProfile`, `compose_user_message`, and `assemble_model_context -> AssembledModelContext`.
- Amends [ADR 0019](0019-session-writes-use-one-trusted-batch-writer.md): successful commits may return or construct a `CommittedConversationDelta` used to advance the run-scoped transcript; uncommitted drafts still never advance Driver/Rig.
- Amends [ADR 0021](0021-session-runtime-separates-actor-control-from-run-execution.md): `Tools.capture_profile_baseline` / `ToolProfileBaseline` are renamed to `Tools.capture_turn_tools` / `TurnToolProfile`, preserving the same fingerprint and actor/run-task separation invariant.
