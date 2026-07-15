# Message Cycle Architecture

Status: Target architecture under review; authoritative-doc migration not started  
Date: 2026-07-15  
Branch: `refactor/codex-style-message-cycle`  
Base: `d2babbd docs(progress): archive message execution lifecycle research`

## Purpose

This is the first target architecture document under `docs/refactor/`. It starts with MiniCore's Message Cycle and will act as the anchor for adjacent Session, Item, Interaction, storage, Prompt, Driver, and protocol refactor documents as those areas are reviewed.

This document defines and tracks the refactor of MiniCore's outer message execution lifecycle to the Codex App Server model. It is an architecture design document first and a migration tracker second.

The target deliberately ignores the current `SessionPhase + CurrentRunState + RunEvent + MessageEvent + ToolCallEvent + RetryEvent + CompactionEvent` shape. The refactor may replace those interfaces rather than adapt them incrementally.

Until the migration reaches the acceptance criteria at the end of this document:

- existing ADRs, `CONTEXT.md`, and module documents still describe the current authoritative architecture;
- this file describes the selected target architecture and migration strategy;
- historical research/review text remains unchanged;
- no partially migrated vocabulary should be treated as a stable public contract.

## Decision Summary

MiniCore will adopt the Codex outer lifecycle semantics, with one naming substitution:

```text
Codex Thread → MiniCore Session
Codex Turn   → MiniCore Turn
Codex Item   → MiniCore Item
```

Approval and other user interactions use the Codex server-request model rather than an event-plus-command pair.

The common identity shared by all Sessions in one fork tree is named `TreeId`.

The target hierarchy is:

```text
Tree
  └─ Session
      └─ Turn
          └─ Item
              └─ optional server request
```

## Target Vocabulary

| Term | Meaning |
| --- | --- |
| `TreeId` | Stable identity shared by the root Session and all Sessions forked from that root. It is the fork-tree identity that Codex exposes as `thread.sessionId`. |
| `SessionId` | Identity of one concrete conversation branch. It corresponds to Codex `thread.id`, not Codex `thread.sessionId`. |
| `Session` | Persistent conversation branch plus its optional loaded runtime state. Corresponds to Codex Thread. |
| `TurnId` | Identity of one Turn inside one Session. |
| `Turn` | One serialized execution lifecycle within a Session. Agent work, standalone compaction, and future review work can have different Turn types. |
| `ItemId` | Identity of one user-visible input, output, tool operation, file change, reasoning block, plan, or compaction item inside a Turn. |
| `Item` | A streamable unit of Turn input or output. Items have one started/completed lifecycle and optional typed deltas. |
| `RequestId` | Correlation identity for one server-initiated request that requires a client response. |
| `Interaction` | A request/response exchange that temporarily blocks or informs an Item, such as approval or user input. |
| `EntryId` | Internal durable session-tree coordinate. It is not the public Item lifecycle identity. |
| `ToolCallId` | Provider/tool-protocol identity used to pair one tool request and result. It can appear inside a ToolCall Item. |

Root and fork identity rules:

```text
root Session:
  SessionId = S1
  TreeId    = S1  // same underlying root identity, distinct typed role

forked Session:
  SessionId = S2
  TreeId    = S1
  forked_from_session_id = S1
```

`TreeId` is not used for Turn routing, Item routing, approval decisions, current-session selection, or storage leaf navigation.

## Target State Model

### Session

```rust
pub enum SessionStatus {
    NotLoaded,
    Idle,
    Active {
        active_flags: Vec<SessionActiveFlag>,
    },
    SystemError,
}

pub enum SessionActiveFlag {
    WaitingOnApproval,
    WaitingOnUserInput,
}
```

State meanings:

| State | Meaning | Owned runtime state |
| --- | --- | --- |
| `NotLoaded` | Session exists in persistent storage/catalog but has no loaded runtime. | No active Turn, no pending server request, no runtime actor state. |
| `Idle` | Session runtime is loaded and can start a Turn. | No `InProgress` Turn and no pending server request. |
| `Active` | Session has exactly one `InProgress` Turn. | Current Turn, live Item projections, zero or more pending server requests, aggregate active flags. |
| `SystemError` | Session runtime itself cannot safely continue. | Diagnostic/recovery information. Ordinary Turn failure does not produce this state. |

`active_flags` is an aggregate projection:

```text
WaitingOnApproval exists
  iff at least one current pending request requires approval.

WaitingOnUserInput exists
  iff at least one current pending request requires user input.
```

Retry, provider fallback, model calls, tool execution, and required compaction are not Session states or active flags.

### Turn

```rust
pub enum TurnStatus {
    InProgress,
    Completed,
    Interrupted,
    Failed,
}
```

Turn terminal states are immutable. Every started Turn reaches exactly one terminal state.

Target Turn types (`turnType` on the public protocol):

```rust
pub enum TurnType {
    Agent,
    Compaction,
    Review, // reserved for later work
}
```

| Turn type | Steerable | Meaning |
| --- | --- | --- |
| `Agent` | Yes while active | A user request and all agent work that follows, including Steer, internal retries, and required overflow recovery. |
| `Compaction` | No | Standalone/manual session compaction. Required compaction during an Agent Turn is an Item in that Agent Turn, not a new Turn. |
| `Review` | No by default | Future review workflow following the Codex non-steerable Turn model. |

### Item

Public Item lifecycle:

```text
item/started
→ zero or more typed item deltas
→ optional server request/response interaction
→ item/completed
```

There is no separate generic `item/failed` notification. The final Item payload carried by `item/completed` contains the item-specific status.

Initial Item kinds:

```rust
pub enum ItemKind {
    UserMessage,
    AgentMessage,
    Reasoning,
    Plan,
    CommandExecution,
    FileChange,
    ToolCall,
    ContextCompaction,
}
```

Indicative item-specific terminal statuses:

| Item kind | Terminal payload status |
| --- | --- |
| `UserMessage` | committed, failed, or cancelled |
| `AgentMessage` | completed or cancelled |
| `Reasoning` / `Plan` | completed or cancelled |
| `CommandExecution` | completed, failed, declined, or cancelled |
| `FileChange` | completed, failed, declined, or cancelled |
| `ToolCall` | completed, failed, declined, or cancelled |
| `ContextCompaction` | completed, failed, skipped, or cancelled |

Typed delta families may remain item-specific, as in Codex:

```text
item/agentMessage/delta
item/reasoning/delta
item/commandExecution/outputDelta
item/fileChange/delta
item/tool/outputDelta
```

The common lifecycle remains `item/started` and `item/completed`.

### Interaction

Interaction is a bidirectional transport lifecycle, not a Turn or Item status enum:

```text
server request created
→ Pending
→ optional client response
→ Resolved
```

A request can resolve without a client response when its Turn is interrupted, its Item is cancelled, the Session closes, a timeout/auto-resolution fires, or a system error clears the waiter.

Target request methods:

```text
item/commandExecution/requestApproval
item/fileChange/requestApproval
item/permissions/requestApproval
item/tool/requestUserInput
```

`item/permissions/requestApproval` is a request namespace attached to the existing Item that invoked the built-in permission request, normally a ToolCall/CommandExecution Item. It does not introduce a separate `Permissions` Item kind.

Each request carries:

```text
RequestId
SessionId
TurnId
ItemId
typed request payload
available decisions, where applicable
```

Resolution is confirmed with:

```text
serverRequest/resolved
```

`resolved` means the waiter was closed. It does not mean the request was approved.

## Target Public Protocol

### Session Methods And Notifications

```text
session/start
session/resume
session/read
session/list
session/fork
session/archive
session/unarchive
session/delete
session/unsubscribe
session/compact/start

session/started
session/status/changed
session/closed
```

The exact MVP subset will be selected during protocol migration. Method names above preserve Codex semantics after replacing Thread with Session.

### Turn Methods And Notifications

```text
turn/start
turn/steer
turn/interrupt

turn/started
turn/completed
```

Turn invariants:

- only one Turn can be `InProgress` in one Session;
- `turn/steer` targets the active steerable Agent Turn and returns the same `TurnId`;
- `turn/interrupt` targets the active Turn;
- `turn/completed` is emitted exactly once;
- retry and required recovery do not create a new Turn;
- terminal Turn status is `Completed | Interrupted | Failed`;
- completed/failed/interrupted Turns are immutable history.

### Item Notifications

```text
item/started
item/<kind>/delta
item/completed
```

Every Item notification carries `SessionId`, `TurnId`, and `ItemId`.

### Server Requests

```text
item/<kind>/requestApproval
item/tool/requestUserInput
client response keyed by RequestId
serverRequest/resolved
```

Server requests are not notifications and are not answered with an ordinary session mutation command.

## Complete Lifecycles

### Session Load And Unload

```text
persistent Session exists
SessionStatus::NotLoaded

session/resume
→ load storage/runtime
→ session/started
→ session/status/changed(Idle)

last subscriber removed + unload policy fires
→ session/status/changed(NotLoaded)
→ session/closed
```

`session/read` does not load the Session or subscribe the caller. A read-only result can report `NotLoaded`.

### Normal Agent Turn

```text
Session: Idle

turn/start(input)
→ allocate TurnId
→ Session: Active { flags: [] }
→ Turn: InProgress
→ turn/started

→ item/started(UserMessage)
→ persist accepted user input
→ item/completed(UserMessage { committed })

→ item/started(AgentMessage)
→ item/agentMessage/delta*
→ persist completed agent message
→ item/completed(AgentMessage { completed })

→ Turn: Completed
→ turn/completed
→ Session: Idle
```

If user-input persistence fails after Turn admission:

```text
turn/started
→ item/started(UserMessage)
→ user input fails to become committed
→ item/completed(UserMessage { failed })
→ Turn: Failed
→ turn/completed
→ Session: Idle
```

### Active Steer

```text
Session: Active
Turn T1: InProgress and steerable

turn/steer(T1, input)
→ validate T1 is current and steerable
→ accept input into the same Turn
→ item/started(UserMessage)
→ persist user item at the valid model/tool boundary
→ item/completed(UserMessage { committed })
→ continue T1
```

Steer never silently becomes a future Turn. Invalid or non-steerable targets return a typed rejection.

### Command Approval

```text
Session: Active { flags: [] }
Turn T1: InProgress

item/started(CommandExecution I1 { status: inProgress })
→ create pending Request A1
→ Session: Active { flags: [WaitingOnApproval] }
→ item/commandExecution/requestApproval(A1, T1, I1)

client response(A1, Accept | AcceptForSession | Decline | Cancel)
→ validate Request/Session/Turn/Item and waiter generation
→ resolve exactly once
→ serverRequest/resolved(A1)
→ remove WaitingOnApproval when no approval request remains

Accept:
  execute command
  → item deltas*
  → item/completed(I1 { completed | failed })

AcceptForSession:
  update Session-scoped approval cache
  → execute as above

Decline:
  → item/completed(I1 { declined })
  → Turn normally continues

Cancel:
  → item/completed(I1 { cancelled })
  → Turn becomes Interrupted
```

The target follows Codex command/file approval semantics: `Decline` rejects the Item and allows the Turn to continue; `Cancel` rejects the Item and immediately interrupts the Turn. Other future request families must define their own typed decision semantics rather than inherit these outcomes implicitly.

The runtime owns pending approval state, frozen execution data, validation, policy/cache mutation, execution, and status projection. The client owns only presentation and collection of the user's response.

### User-Input Request

```text
item/started(ToolCall or elicitation Item)
→ create pending Request
→ Session: Active { WaitingOnUserInput }
→ item/tool/requestUserInput
→ client response or auto-resolution
→ serverRequest/resolved
→ remove active flag when no matching request remains
→ continue or complete Item
```

### Tool Item

```text
item/started(ToolCall)
→ optional approval request
→ execute tool
→ item/tool/outputDelta*
→ persist the stable model-visible tool fact(s)
→ item/completed(ToolCall { completed | failed | declined | cancelled })
```

MiniCore may keep a stricter internal stable-batch contract than Codex. That internal contract must not expand the public outer lifecycle.

### Turn Interrupt

```text
Session: Active
Turn T1: InProgress

turn/interrupt(T1)
→ cancel active work
→ resolve/clear all pending requests in T1
→ serverRequest/resolved* for cleared requests
→ complete/cancel any publicly started Item lifecycle as required
→ Turn: Interrupted
→ turn/completed(T1)
→ Session: Idle
```

Committed history remains committed. Partial/uncommitted work follows the internal storage recovery contract.

### Turn Failure

```text
Turn T1: InProgress
→ unrecoverable execution error
→ resolve/clear pending requests
→ complete/cancel active Item lifecycle
→ Turn: Failed { error }
→ turn/completed(T1)
→ Session: Idle
```

Ordinary Turn failure does not make the Session `SystemError`.

### Required Compaction During Agent Turn

```text
Session: Active
Agent Turn T1: InProgress

→ item/started(ContextCompaction)
→ compact/rebuild internal model context
→ item/completed(ContextCompaction { completed | failed | skipped })

success:
  continue T1

skipped:
  continue T1 only when compaction was optional or the rebuilt context already fits;
  required recovery that remains over limit fails the Turn

required failure:
  Turn T1 → Failed
  → turn/completed
  → Session: Idle
```

No new Turn and no separate Session compaction phase are created.

### Standalone Manual Compaction

```text
Session: Idle

session/compact/start
→ create non-steerable Turn T2 { kind: Compaction, status: InProgress }
→ Session: Active
→ turn/started(T2)
→ item/started(ContextCompaction)
→ item/completed(ContextCompaction { completed | failed | skipped })
→ turn/completed(T2, Completed | Failed | Interrupted)
→ Session: Idle
```

### Retry

Retry is an internal attempt inside one active Agent Turn:

```text
Turn T1: InProgress
→ retryable failure
→ internal backoff/retry
→ T1 remains InProgress
```

Retry does not create:

- a new Turn;
- a new Session status;
- a Session active flag;
- a public retry lifecycle that competes with Turn lifecycle.

Retry diagnostics or progress may be exposed as item-specific progress or diagnostic notifications if a product surface requires it.

### Session System Error

```text
Session: NotLoaded | Idle | Active
→ unrecoverable session-runtime/storage/system failure
→ clear active requests safely
→ terminate active Turn as Failed or Interrupted where possible
→ Session: SystemError
```

Recovery/reload semantics must be specified before `SystemError` is exposed as a stable public state.

## Scheduling Semantics

Full Codex outer-loop adoption removes MiniCore's current `Steer / FollowUp / NextTurn` core queue taxonomy from the target public contract.

Target behavior:

- `turn/start` starts a new Turn only when the Session can start one;
- `turn/steer` modifies the current active steerable Turn;
- `turn/interrupt` interrupts the current Turn;
- a client that wants follow-up behavior waits for `turn/completed` and then calls `turn/start`;
- a client may keep unsent drafts locally;
- MiniCore core does not silently reinterpret start as steer, steer as follow-up, or follow-up as a local draft.

If server-side queued future Turns are later required, they must be designed as a deliberate extension with their own receipt/queue contract. They are not part of this refactor baseline.

## Runtime Ownership

| Concern | Owner |
| --- | --- |
| Session status and active flags | Runtime Session owner |
| Current Turn and terminal transition | Runtime Session owner |
| Item lifecycle projection | Runtime Turn/Item projector |
| Pending server request | Runtime interaction/request owner |
| Approval policy and frozen execution data | Tools/policy owner behind the runtime request |
| Client response presentation | Client/UI |
| Response validation and application | Runtime |
| Durable transcript | Session storage owner |
| Model-visible context assembly | Prompt owner |
| Provider invocation | Model gateway owner |

The client never mutates Session, Turn, Item, or request state directly. It sends methods or responses; the runtime validates them and publishes the resulting facts.

## Current-To-Target Mapping

| Current MiniCore | Target |
| --- | --- |
| `SessionPhase::Idle` | `SessionStatus::Idle` |
| `SessionPhase::Turn` | `SessionStatus::Active` + current `Turn` |
| `SessionPhase::Compaction` | Compaction Turn or ContextCompaction Item |
| `SessionPhase::RetryBackoff` | Internal attempt inside active Turn |
| `CurrentRunState` | Session active flags + current Turn/Item/request projections |
| `RunId` | `TurnId` |
| `RunView` | `Turn` / `TurnView` |
| `RunTerminalStatus` | `TurnStatus` |
| `AbortRun` | `turn/interrupt` |
| `SubmitPrompt` idle path | `turn/start` |
| `SubmitPrompt { Steer }` | `turn/steer` |
| FollowUp/NextTurn queues | Removed from baseline core contract |
| `RunEvent::Started/Finished` | `turn/started` / `turn/completed` |
| `MessageEvent` | UserMessage/AgentMessage Item lifecycle |
| `ToolCallEvent` | ToolCall/CommandExecution/FileChange Item lifecycle |
| `RetryEvent` | Removed from baseline public lifecycle |
| `CompactionEvent` | ContextCompaction Item lifecycle |
| `ApprovalRequested` event | Item-scoped server request |
| `DecideToolApproval` command | Client response to `RequestId` |
| `PendingToolApprovalView` | Pending server request view |
| `session_settled` | `session/status/changed(Idle)` |
| Codex `thread.id` equivalent | MiniCore `SessionId` |
| Codex `thread.sessionId` equivalent | MiniCore `TreeId` |

## Naming Migration

Expected renames include:

```text
RunId                       → TurnId
CurrentRun                  → CurrentTurn
RunView                     → TurnView
RunTerminalStatus           → TurnStatus
run_started                 → turn_started
run_finished                → turn_completed
AbortRun                    → turn/interrupt
```

The current model-call name `ModelTurn` conflicts with public Turn and must be removed or renamed:

```text
ModelTurn                   → ModelResponse
generate_model_turn         → generate_model_response
ConversationRunResult       → TurnExecutionOutcome or DriverOutcome
```

Private implementation terms may remain explicitly private:

```text
Turn attempt
Driver attempt
Rig segment
segment_index
execution_epoch
```

## Removal Candidates

The refactor should prefer deletion over compatibility layering. Candidates:

- `SessionPhase::{Turn, Compaction, RetryBackoff}`;
- `CurrentRunState::{Running, WaitingApproval, Suspended}` as the main public projection;
- public `RunId`, `RunView`, and `RunEvent` vocabulary;
- message-role-specific started/delta/finished event families;
- tool-specific public lifecycle duplication where Item lifecycle suffices;
- public retry lifecycle events;
- independent automatic-compaction Session phase;
- `ApprovalRequested` event plus `DecideToolApproval` command pairing;
- runtime-owned FollowUp/NextTurn queue semantics;
- public workflow types whose only purpose was coordinating the old phase model.

Compatibility aliases should only be added when a concrete external adapter requires a staged migration. This docs-only repository currently has no production protocol consumer that justifies carrying both models.

## Internal Architecture Freedom

The target outer model does not pre-decide the final internal implementation.

The implementation may retain or replace:

- per-session actor ownership;
- RunTask/TurnTask child execution;
- Rig adapter internals;
- Transcript-First storage;
- batch writer semantics;
- Prompt ownership;
- tool execution and approval internals;
- JSONL or future SQLite storage.

Internal modules must project exactly one target Session/Turn/Item/Interaction lifecycle and must not leak internal steps into the public interface.

## Migration Plan

### Phase 0: Contract Freeze

- Treat this file as the target source during review.
- Do not add new fields to the old phase/run/event model unless required to document migration evidence.
- Resolve all open questions listed below.

### Phase 1: Domain And ADR Decision

- Add `TreeId`, Session, Turn, Item, and Interaction glossary definitions.
- Create a new Accepted ADR for Codex-style outer lifecycle adoption.
- Amend superseded ADRs, especially actor/run execution, prompt/history, and event protocol decisions.

### Phase 2: Protocol Shape

- Define Session/Turn/Item/request public types.
- Define method, notification, server-request, and response envelopes.
- Define ordering and terminal invariants.
- Define snapshot/read projections and pagination.

### Phase 3: Session Projection

- Replace `SessionPhase` with `SessionStatus`.
- Replace `CurrentRun` with current Turn and pending request projections.
- Make `NotLoaded` visible through session list/read without loading the runtime.
- Define `SystemError` recovery behavior.

### Phase 4: Turn Lifecycle

- Replace public Run vocabulary with Turn vocabulary.
- Make retry and required recovery internal to one Turn.
- Model standalone compaction as a non-steerable Turn.
- Remove server-side FollowUp/NextTurn baseline behavior.

### Phase 5: Item Lifecycle

- Introduce `ItemId` and typed Item payloads.
- Replace message/tool-specific public lifecycle events with Item lifecycle.
- Define `ItemId ↔ EntryId` or `ItemId ↔ committed entries` persistence mapping.
- Ensure public completed Item payloads can be rebuilt from persisted history where promised.

### Phase 6: Interaction Lane

- Introduce server-initiated requests and client responses.
- Replace approval event/command pairing.
- Add pending requests to Session snapshot/read projection.
- Define duplicate, stale, timeout, interrupt, close, and reconnect behavior.

### Phase 7: Runtime Projection

- Map internal model/tool/storage progress into the target lifecycle.
- Preserve actor responsiveness while requests are pending.
- Preserve stable commit and visibility guarantees internally without exposing commit phases.

### Phase 8: Remove Old Model

- Delete superseded phase/run/message/tool/retry/compaction public types.
- Remove compatibility prose from authoritative modules.
- Keep historical ADR/research/review text with explicit amendments rather than rewriting history.

### Phase 9: Verification And Handoff

- Run terminology and broken-link scans.
- Build protocol/state/item/request conformance matrices.
- Update progress/handoff documentation.
- Only then begin or resume production implementation.

## Conformance Matrix

### Session

- persisted unread Session reports `NotLoaded` without loading runtime;
- resume transitions `NotLoaded → Idle`;
- one Session has at most one `InProgress` Turn;
- Turn terminal returns Session to `Idle`;
- ordinary Turn failure does not produce `SystemError`;
- unload clears runtime-only requests and reports `NotLoaded`;
- root and fork Sessions share one `TreeId` and have distinct `SessionId`s.

### Turn

- `turn/start` produces one `turn/started` and one terminal `turn/completed`;
- `turn/steer` targets the same `TurnId`;
- steer is rejected for non-steerable Turns;
- `turn/interrupt` is idempotent/stale-safe;
- retry does not change `TurnId`;
- required compaction does not change `TurnId`;
- standalone compaction creates a separate non-steerable Turn;
- terminal Turn state is immutable.

### Item

- every `item/started` has exactly one `item/completed` or an explicitly documented Turn-terminal closure rule;
- deltas only occur after started and before completed;
- completed payload carries final item-specific status;
- UserMessage/AgentMessage/ToolCall/Compaction item histories can be projected consistently after reload;
- Item IDs are unique within their documented scope;
- Item-to-storage mapping survives fork/replay according to the selected policy.

### Interaction

- request is registered before it is sent to the client;
- Session active flag is present while matching requests are pending;
- duplicate response does not execute twice;
- stale Session/Turn/Item/request identity is rejected;
- interrupt/close/system error clears pending requests;
- every cleared request produces `serverRequest/resolved` where the connection contract permits;
- `AcceptForSession` updates runtime-owned session policy/cache, not UI state;
- client cannot replace frozen command/tool/file-change data in its response.

### Ordering

- Turn starts before its first Item lifecycle;
- Item completion precedes Turn completion;
- pending server requests are resolved before Turn completion;
- Session becomes Idle only after Turn completion;
- terminal and status notifications do not refer to private attempt/segment identities;
- committed facts are not reported as durable before the internal writer accepts them.

## Open Decisions

The outer model is selected. These details still require explicit review:

1. Whether all `item/completed` durable-looking payloads are emitted only after stable storage commit, or whether a separate persistence projection is needed.
2. Exact `ItemId ↔ EntryId` mapping for ToolCall Items that correspond to multiple stored messages/entries.
3. Whether Item IDs are regenerated or preserved when a Session is forked into another Session in the same `TreeId`.
4. Request resend/recovery behavior when a client connection is rebuilt while the same runtime remains alive.
5. Whether `serverRequest/resolved` is guaranteed for requests cleared during abrupt transport loss.
6. Exact `SystemError` entry and recovery transitions.
7. Which Codex Item kinds are MVP and which remain reserved.
8. Whether MiniCore exposes one generic `item/<kind>/delta` envelope or Codex-style item-specific delta method names.
9. Whether session list/read and runtime snapshot share one Session view type or use separate summary/detail projections.
10. How TreeId and SessionId are persisted in JSONL headers and regenerated during external import.
11. Whether client-side follow-up scheduling is sufficient for all first-party adapters; server-side queued Turns require a separate future decision.
12. Exact decision sets and timeout/auto-resolution semantics for permission requests and `requestUserInput` interactions.

## Review Order

Continue review in this order:

1. Session and Tree identity: `TreeId`, `SessionId`, fork/read/resume behavior.
2. Turn admission and terminal ordering.
3. Item kinds, item-specific statuses, and storage mapping.
4. Server-request transport, pending request snapshot, and reconnect behavior.
5. Compaction/retry projection into Turn/Item.
6. Removal of current queues and phase model.
7. Protocol migration and authoritative document update plan.

## Progress Checklist

- [x] Create dedicated refactor branch.
- [x] Select Codex outer lifecycle model.
- [x] Rename Codex Thread concept to MiniCore Session.
- [x] Select `TreeId` for fork-tree identity.
- [x] Record target Session/Turn/Item/Interaction states.
- [x] Record approval server-request ownership model.
- [x] Record full target lifecycle scenarios.
- [x] Record current-to-target mapping and removal candidates.
- [x] Record migration phases and conformance matrix.
- [ ] Resolve open decisions.
- [ ] Create/accept lifecycle ADR.
- [ ] Update `CONTEXT.md` glossary.
- [ ] Update architecture and module authority documents.
- [ ] Update protocol and event contracts.
- [ ] Close/supersede affected review issues.
- [ ] Complete Rig/provider impact review.
- [ ] Add implementation and conformance tests.
- [ ] Remove superseded public types and prose.
- [ ] Update progress handoff after migration.

## Acceptance Criteria

The refactor is complete only when:

- `SessionStatus`, `TurnStatus`, Item lifecycle, and server-request lifecycle are the only public outer execution state model;
- `TreeId` and `SessionId` have distinct, documented fork semantics;
- no public `RunId`, `SessionPhase::Turn/Compaction/RetryBackoff`, or parallel retry workflow remains;
- message/tool public progress uses Item lifecycle;
- approval uses server request/client response/resolved semantics;
- pending interactions are recoverable from the current runtime projection where promised;
- required compaction and retry remain inside one Agent Turn;
- standalone compaction is a non-steerable Turn;
- no public or server-owned FollowUp/NextTurn queue contract remains in the baseline architecture;
- current storage and Prompt invariants either map cleanly behind the new interface or are explicitly replaced by a new accepted decision;
- authoritative docs, ADR amendments, review status, progress handoff, and tests agree with the target model;
- historical documents remain available as historical context without being mistaken for the current contract.