# Tool, Policy, And Interaction Contract

Tool execution is injected through `Tool`, immutable `ToolSet`, and optional `ToolPolicy`. Approval and ToolInput suspension are typed, process-local, and actor-owned.

Source: [`src/tools/tool.rs`](../../src/tools/tool.rs), [`src/tools/set.rs`](../../src/tools/set.rs), [`src/tools/policy.rs`](../../src/tools/policy.rs), [`src/tools/input.rs`](../../src/tools/input.rs), [`src/interaction.rs`](../../src/interaction.rs), and private [`src/agent/tool_driver.rs`](../../src/agent/tool_driver.rs). Evidence: [`tool_set_contract.rs`](../../tests/tool_set_contract.rs), [`tool_policy_interaction_contract.rs`](../../tests/tool_policy_interaction_contract.rs), [`session_runtime_interaction_contract.rs`](../../tests/session_runtime_interaction_contract.rs), and the real-runtime failure/restart suites [`session_runtime_tool_policy_failure_evidence.rs`](../../tests/session_runtime_tool_policy_failure_evidence.rs) and [`session_runtime_restart_event_evidence.rs`](../../tests/session_runtime_restart_event_evidence.rs).

## Tool And ToolSet

`Tool` is `Send + Sync + 'static` and exposes one checked `ToolSpec` plus an async `execute(invocation, context)` future. `ToolInvocation` carries exact session/instance/turn/call/name identity and object-shaped bounded JSON arguments. Debug redacts arguments.

`ToolContext` contains only child cancellation, absolute deadline, and a nonblocking `ToolProgressSink`. It has no workspace, process, log, session owner, repository, or credential handle.

`ToolSet::builder` registers explicit tools, preserves the first duplicate/spec-panic/invalid-spec error, and freezes deterministic specs at build. The immutable set may be cloned and shared. SessionBindings validates enabled names and all frozen spec budgets before execution.

A Tool returns either `Completed(ToolOutput)` or `RequestInput(ToolInputRequest)`. Core installs no builtins.

## Policy

`ToolPolicy` is `Send + Sync + 'static` and receives an owned `ToolPolicyRequest` containing the exact frozen invocation/spec, cancellation token, and deadline.

`ToolDecision` is exactly:

- `Allow`;
- `Deny { reason }`;
- `RequireApproval { request }`.

Approval answers are exactly `ApprovalDecision::AllowOnce` or `ApprovalDecision::Deny`. Strings such as `"yes"`, `"allow"`, or session-wide allow rules are not accepted. Policy error, invalid decision, timeout, or panic fails closed; the Tool is not executed.

`AllowOnce` executes the already frozen exact invocation once. `Deny` produces a durable denied ToolResult without invoking the Tool.

## ToolDriver

The private ToolDriver owns policy evaluation, Tool execution, suspension, progress, deadline/cancellation, panic isolation, and semantic output validation. It does not append Conversation entries, allocate sequence/timestamp values, settle TurnHandle, or own SessionRuntime.

Tool calls execute sequentially in Assistant order. Configured Tool timeout/panic becomes a failed ToolResult while the actor remains available; an earlier Turn deadline or cancellation is Turn control and does not fabricate an ordinary result before settlement.

Progress is lossy. Full, closed, invalid, and no-op progress paths return immediately and never control Tool completion.

## Process-Local Interaction

`PendingInteraction` contains checked interaction/turn/tool-call/tool-name identity and one `InteractionKind`:

- `Approval(ApprovalRequest)`;
- `ToolInput(ToolInputRequest)`.

The public value carries no sender or continuation. SessionActor separately owns the sole one-shot resume sender and authenticates suspension against the first canonical unresolved durable ToolCall. WaitingForInput state is published before the best-effort event.

Answers must match both interaction identity and kind. Wrong kind, unknown ID, and repeated answer are typed errors. Answer/cancellation races consume the interaction at most once.

## Canonical Input Result JSON

A Tool future ends when it returns `RequestInput`; Core never resumes arbitrary Tool code. A valid answer directly becomes an `InputProvided` ToolResult with canonical compact JSON:

```json
{"answer":"typed text"}
```

or:

```json
{"choice_index":1,"choice":"the selected checked choice"}
```

JSON escaping is stable and bounds are revalidated. Approval has no string/JSON continuation protocol: `AllowOnce` executes the frozen invocation, while `Deny` creates a denied result.

## Restart

Interactions are memory-only. Restart does not recover approval prompts, ToolInput waits, resume senders, Tool futures, or answers. Load repairs the durable unfinished Turn by cancelling unresolved calls in canonical order and appending `CancelledByRestart`. A Host must present the repaired durable result rather than attempting to replay a stale answer.
