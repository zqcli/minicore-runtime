# Message Cycle 架构设计

状态：目标架构评审中；尚未开始迁移权威文档
日期：2026-07-15
分支：`refactor/codex-style-message-cycle`
基线：`d2babbd docs(progress): archive message execution lifecycle research`

## 目的

这是 `docs/refactor/` 下第一份目标架构设计文档。重构将从 MiniCore 的 Message Cycle 开始；随着评审推进，再以本文为入口，逐步扩展 Session、Item、Interaction、Storage、Prompt、Driver 和 Protocol 等周边架构。

本文首先是一份目标架构设计，其次才是迁移追踪文档。它定义并追踪 MiniCore 外层消息执行生命周期向 Codex App Server 模型的重构。

目标设计不受当前 `SessionPhase + CurrentRunState + RunEvent + MessageEvent + ToolCallEvent + RetryEvent + CompactionEvent` 结构约束。重构可以直接替换这些 interface，而不是在旧结构上继续增加兼容层。

在本文末尾的验收标准全部满足之前：

- 现有 ADR、`CONTEXT.md` 和模块文档仍然描述当前权威架构；
- 本文描述已经选定的目标架构和迁移策略；
- 历史 research/review 正文保持不变；
- 迁移到一半的术语不得被视为稳定公共契约。

## 决策摘要

MiniCore 完整采用 Codex 的外层生命周期语义，只做一个命名替换：

```text
Codex Thread → MiniCore Session
Codex Turn   → MiniCore Turn
Codex Item   → MiniCore Item
```

Approval 和其他用户交互采用 Codex 的 server request 模型，不再使用“事件 + 普通命令”的配对模型。

一个 fork tree 中所有 Session 共享的根身份统一命名为 `TreeId`。

目标层级：

```text
Tree
  └─ Session
      └─ Turn
          └─ Item
              └─ 可选 server request
```

## 目标术语

| 术语 | 含义 |
| --- | --- |
| `TreeId` | root Session 与从它 fork 出来的所有 Session 共享的稳定身份。对应 Codex 的 `thread.sessionId`。 |
| `SessionId` | 一条具体 conversation branch 的身份。对应 Codex 的 `thread.id`，不是 Codex 的 `thread.sessionId`。 |
| `Session` | 一条持久化 conversation branch，以及可选的 loaded runtime 状态。对应 Codex Thread。 |
| `TurnId` | 一个 Session 中一次 Turn 的身份。 |
| `Turn` | Session 内一次串行执行生命周期。Agent 工作、standalone compaction 和未来 review 可以使用不同的 Turn type。 |
| `ItemId` | Turn 中一项用户可观察输入或输出的身份，例如消息、工具执行、文件修改、reasoning、plan 或 compaction。 |
| `Item` | Turn 中可流式观察的一项输入或输出。每个 Item 只有一组 started/completed 生命周期，并可产生 typed delta。 |
| `RequestId` | 一次 server-initiated request 的关联身份；该 request 必须由客户端响应或由 Runtime 清除。 |
| `Interaction` | 临时阻塞或补充某个 Item 的 request/response 交互，例如 approval 或用户输入请求。 |
| `EntryId` | Runtime 内部的持久化 session-tree 坐标，不是公共 Item 生命周期身份。 |
| `ToolCallId` | provider/tool protocol 用来配对一次工具请求和结果的身份，可以出现在 ToolCall Item 内。 |

root 与 fork 身份规则：

```text
root Session:
  SessionId = S1
  TreeId    = S1  // 底层根身份相同，但类型角色不同

forked Session:
  SessionId = S2
  TreeId    = S1
  forked_from_session_id = S1
```

`TreeId` 不用于 Turn 路由、Item 路由、approval 决策、current-session selection 或 storage leaf navigation。

## 目标状态模型

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

状态含义：

| 状态 | 含义 | 拥有的运行时状态 |
| --- | --- | --- |
| `NotLoaded` | Session 存在于持久化 storage/catalog，但没有 loaded runtime。 | 没有 active Turn、pending server request 或 runtime actor state。 |
| `Idle` | Session runtime 已加载，可以启动 Turn。 | 没有 `InProgress` Turn，也没有 pending server request。 |
| `Active` | Session 恰好有一个 `InProgress` Turn。 | current Turn、live Item projection、零到多个 pending server request 和 aggregate active flags。 |
| `SystemError` | Session runtime 自身无法安全继续。 | diagnostics 和 recovery information。普通 Turn failure 不会产生该状态。 |

`active_flags` 是聚合投影：

```text
存在至少一个 pending approval request
  ↔ active_flags 包含 WaitingOnApproval

存在至少一个 pending user-input request
  ↔ active_flags 包含 WaitingOnUserInput
```

Retry、provider fallback、model call、tool execution 和 required compaction 都不是 Session 状态，也不是 active flag。

### Turn

```rust
pub enum TurnStatus {
    InProgress,
    Completed,
    Interrupted,
    Failed,
}
```

Turn terminal state 不可修改。每个已经 started 的 Turn 必须恰好到达一个 terminal state。

目标 Turn type，公共协议字段使用 `turnType`：

```rust
pub enum TurnType {
    Agent,
    Compaction,
    Review, // 为后续能力保留
}
```

| Turn type | 是否可 Steer | 含义 |
| --- | --- | --- |
| `Agent` | Active 时可以 | 一次用户请求及其后续所有 Agent 工作，包括 Steer、内部 retry 和 required overflow recovery。 |
| `Compaction` | 不可以 | Standalone/manual session compaction。Agent Turn 中的 required compaction 是该 Agent Turn 内的 Item，不创建新 Turn。 |
| `Review` | 默认不可以 | 未来 review workflow，遵循 Codex non-steerable Turn 模型。 |

### Item

公共 Item 生命周期：

```text
item/started
→ 零到多个 typed item delta
→ 可选 server request/response interaction
→ item/completed
```

不定义通用 `item/failed` notification。`item/completed` 携带的最终 Item payload 包含 item-specific status。

初始 Item type：

```rust
pub enum ItemType {
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

Item-specific terminal status：

| Item type | 最终 payload status |
| --- | --- |
| `UserMessage` | committed、failed 或 cancelled |
| `AgentMessage` | completed 或 cancelled |
| `Reasoning` / `Plan` | completed 或 cancelled |
| `CommandExecution` | completed、failed、declined 或 cancelled |
| `FileChange` | completed、failed、declined 或 cancelled |
| `ToolCall` | completed、failed、declined 或 cancelled |
| `ContextCompaction` | completed、failed、skipped 或 cancelled |

Typed delta family 可以像 Codex 一样保持 item-specific：

```text
item/agentMessage/delta
item/reasoning/delta
item/commandExecution/outputDelta
item/fileChange/delta
item/tool/outputDelta
```

公共生命周期仍然只有 `item/started` 和 `item/completed`。

### Interaction

Interaction 是双向 transport lifecycle，不是 Turn 或 Item 的 status enum：

```text
server request created
→ Pending
→ 可选 client response
→ Resolved
```

以下情况可以在没有 client response 时直接 resolve request：

- Turn 被 interrupt；
- Item 被 cancel；
- Session close；
- timeout 或 auto-resolution；
- system error 清除 waiter。

目标 request method：

```text
item/commandExecution/requestApproval
item/fileChange/requestApproval
item/permissions/requestApproval
item/tool/requestUserInput
```

`item/permissions/requestApproval` 是绑定到现有 Item 的 request namespace，通常绑定触发内置权限请求的 ToolCall/CommandExecution Item。它不会新增独立的 `Permissions` Item type。

每个 request 携带：

```text
RequestId
SessionId
TurnId
ItemId
typed request payload
available decisions（如适用）
```

Request 关闭后发布：

```text
serverRequest/resolved
```

`resolved` 只表示 waiter 已关闭，不表示 request 已被批准。

## 目标公共协议

### Session Method 与 Notification

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

协议迁移时再确定 MVP 的精确子集。上述名称保持 Codex 语义，只把 Thread 替换为 Session。

### Turn Method 与 Notification

```text
turn/start
turn/steer
turn/interrupt

turn/started
turn/completed
```

Turn 不变量：

- 一个 Session 同时最多有一个 `InProgress` Turn；
- `turn/steer` 只能作用于 active、steerable 的 Agent Turn，并返回同一个 `TurnId`；
- `turn/interrupt` 作用于 active Turn；
- `turn/completed` 恰好发布一次；
- retry 和 required recovery 不创建新 Turn；
- terminal Turn status 只能是 `Completed | Interrupted | Failed`；
- completed、failed、interrupted Turn 都是不可修改的历史事实。

### Item Notification

```text
item/started
item/<type>/delta
item/completed
```

每个 Item notification 都携带 `SessionId`、`TurnId` 和 `ItemId`。

### Server Request

```text
item/<type>/requestApproval
item/tool/requestUserInput
client response keyed by RequestId
serverRequest/resolved
```

Server request 不是 notification，也不能通过普通 session mutation command 回答。

## 完整生命周期

### Session Load 与 Unload

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

`session/read` 不会加载 Session，也不会自动订阅 caller。Read-only result 可以报告 `NotLoaded`。

### 正常 Agent Turn

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

如果 Turn admission 后 user-input persistence 失败：

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
→ persist user item at a valid model/tool boundary
→ item/completed(UserMessage { committed })
→ continue T1
```

Steer 绝不能静默转换成 future Turn。目标无效或 Turn 不可 Steer 时返回 typed rejection。

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
  → item delta*
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

目标设计遵循 Codex command/file approval 语义：

- `Decline` 拒绝当前 Item，但允许 Turn 继续；
- `Cancel` 拒绝当前 Item，并立即 interrupt Turn；
- 未来其他 request family 必须定义自己的 typed decision semantics，不能隐式继承这组行为。

Runtime 拥有 pending approval state、frozen execution data、validation、policy/cache mutation、execution 和 status projection。客户端只负责展示并收集用户 response。

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
→ persist stable model-visible tool facts
→ item/completed(ToolCall { completed | failed | declined | cancelled })
```

MiniCore 内部可以保留比 Codex 更严格的 stable-batch contract，但该内部契约不得扩大公共外层生命周期。

### Turn Interrupt

```text
Session: Active
Turn T1: InProgress

turn/interrupt(T1)
→ cancel active work
→ resolve/clear all pending requests in T1
→ serverRequest/resolved* for cleared requests
→ complete/cancel publicly started Item lifecycle as required
→ Turn: Interrupted
→ turn/completed(T1)
→ Session: Idle
```

已经 committed 的历史继续保留。Partial/uncommitted work 遵循内部 storage recovery contract。

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

普通 Turn failure 不会令 Session 进入 `SystemError`。

### Agent Turn 内的 Required Compaction

```text
Session: Active
Agent Turn T1: InProgress

→ item/started(ContextCompaction)
→ compact/rebuild internal model context
→ item/completed(ContextCompaction { completed | failed | skipped })

completed:
  continue T1

skipped:
  optional compaction 或 rebuilt context 已经可容纳时继续 T1；
  required recovery 后仍超限时令 Turn Failed

failed:
  Turn T1 → Failed
  → turn/completed
  → Session: Idle
```

不会创建新 Turn，也不会创建独立 Session compaction phase。

### Standalone Manual Compaction

```text
Session: Idle

session/compact/start
→ create non-steerable Turn T2 {
      turnType: Compaction,
      status: InProgress
   }
→ Session: Active
→ turn/started(T2)
→ item/started(ContextCompaction)
→ item/completed(ContextCompaction { completed | failed | skipped })
→ turn/completed(T2, Completed | Failed | Interrupted)
→ Session: Idle
```

### Retry

Retry 是 active Agent Turn 内部的一次 attempt：

```text
Turn T1: InProgress
→ retryable failure
→ internal backoff/retry
→ T1 remains InProgress
```

Retry 不会创建：

- 新 Turn；
- 新 Session status；
- Session active flag；
- 与 Turn lifecycle 竞争的公共 retry lifecycle。

如果产品界面确实需要，可以把 retry diagnostics/progress 投影成 item-specific progress 或 diagnostics notification。

### Session System Error

```text
Session: NotLoaded | Idle | Active
→ unrecoverable session-runtime/storage/system failure
→ clear active requests safely
→ terminate active Turn as Failed or Interrupted where possible
→ Session: SystemError
```

在 `SystemError` 成为稳定公共状态前，必须先定义 recovery/reload semantics。

## 调度语义

完整采用 Codex outer loop 后，MiniCore 当前 `Steer / FollowUp / NextTurn` core queue taxonomy 不再进入目标公共契约。

目标行为：

- `turn/start` 只在 Session 可以启动新 Turn 时启动；
- `turn/steer` 修改当前 active、steerable Turn；
- `turn/interrupt` interrupt 当前 Turn；
- 客户端若需要 follow-up 行为，应等待 `turn/completed`，再调用 `turn/start`；
- 客户端可以在本地保存未发送草稿；
- MiniCore core 不会把 start 静默解释为 steer，也不会把 steer 解释为 follow-up；
- baseline 架构不提供 server-owned FollowUp/NextTurn queue。

未来若确实需要 server-side queued future Turn，必须作为独立扩展设计自己的 receipt/queue contract，不属于本次重构 baseline。

## Runtime 职责

| 关注点 | Owner |
| --- | --- |
| Session status 和 active flags | Runtime Session owner |
| Current Turn 和 terminal transition | Runtime Session owner |
| Item lifecycle projection | Runtime Turn/Item projector |
| Pending server request | Runtime interaction/request owner |
| Approval policy 和 frozen execution data | Runtime request 背后的 Tools/policy owner |
| Client response 展示 | Client/UI |
| Response validation 和 application | Runtime |
| Durable transcript | Session storage owner |
| Model-visible context assembly | Prompt owner |
| Provider invocation | Model gateway owner |

客户端绝不能直接 mutate Session、Turn、Item 或 request state。客户端只能发送 method 或 response；Runtime 负责校验，并发布转换后的事实。

## 当前模型到目标模型的映射

| 当前 MiniCore | 目标模型 |
| --- | --- |
| `SessionPhase::Idle` | `SessionStatus::Idle` |
| `SessionPhase::Turn` | `SessionStatus::Active` + current `Turn` |
| `SessionPhase::Compaction` | Compaction Turn 或 ContextCompaction Item |
| `SessionPhase::RetryBackoff` | Active Turn 内部 attempt |
| `CurrentRunState` | Session active flags + current Turn/Item/request projection |
| `RunId` | `TurnId` |
| `RunView` | `Turn` / `TurnView` |
| `RunTerminalStatus` | `TurnStatus` |
| `AbortRun` | `turn/interrupt` |
| `SubmitPrompt` idle path | `turn/start` |
| `SubmitPrompt { Steer }` | `turn/steer` |
| FollowUp/NextTurn queues | 从 baseline core contract 删除 |
| `RunEvent::Started/Finished` | `turn/started` / `turn/completed` |
| `MessageEvent` | UserMessage/AgentMessage Item lifecycle |
| `ToolCallEvent` | ToolCall/CommandExecution/FileChange Item lifecycle |
| `RetryEvent` | 从 baseline public lifecycle 删除 |
| `CompactionEvent` | ContextCompaction Item lifecycle |
| `ApprovalRequested` event | Item-scoped server request |
| `DecideToolApproval` command | Client response to `RequestId` |
| `PendingToolApprovalView` | Pending server request view |
| `session_settled` | `session/status/changed(Idle)` |
| Codex `thread.id` equivalent | MiniCore `SessionId` |
| Codex `thread.sessionId` equivalent | MiniCore `TreeId` |

## 命名迁移

预计需要执行以下重命名：

```text
RunId                       → TurnId
CurrentRun                  → CurrentTurn
RunView                     → TurnView
RunTerminalStatus           → TurnStatus
run_started                 → turn_started
run_finished                → turn_completed
AbortRun                    → turn/interrupt
```

当前 model-call 名称 `ModelTurn` 会与公共 Turn 冲突，必须删除或重命名：

```text
ModelTurn                   → ModelResponse
generate_model_turn         → generate_model_response
ConversationRunResult       → TurnExecutionOutcome or DriverOutcome
```

以下实现术语可以保留，但必须保持 private：

```text
Turn attempt
Driver attempt
Rig segment
segment_index
execution_epoch
```

## 删除候选

重构应优先删除旧模型，而不是叠加 compatibility layer。候选项：

- `SessionPhase::{Turn, Compaction, RetryBackoff}`；
- 作为主要公共投影的 `CurrentRunState::{Running, WaitingApproval, Suspended}`；
- 公共 `RunId`、`RunView` 和 `RunEvent` 术语；
- 按 message role 复制的 started/delta/finished event family；
- 可以被 Item lifecycle 覆盖的 tool-specific public lifecycle；
- public retry lifecycle event；
- 独立 automatic-compaction Session phase；
- `ApprovalRequested` event + `DecideToolApproval` command 配对；
- runtime-owned FollowUp/NextTurn queue semantics；
- 仅用于协调旧 phase 模型的 public workflow type。

只有在存在真实外部 adapter、必须分阶段迁移时，才允许增加 compatibility alias。当前仓库仍是 docs-only，没有生产 protocol consumer 可以证明维护两套模型是合理的。

## 内部架构自由度

目标 outer model 不提前决定最终内部实现。

实现可以保留、替换或重构：

- per-session actor ownership；
- RunTask/TurnTask child execution；
- Rig adapter internals；
- Transcript-First storage；
- batch writer semantics；
- Prompt ownership；
- tool execution 和 approval internals；
- JSONL 或未来 SQLite storage。

内部模块必须只投影一套目标 Session/Turn/Item/Interaction 生命周期，不得把内部步骤泄漏进公共 interface。

## 迁移计划

### Phase 0：冻结旧契约扩张

- 评审期间以本文作为 target source。
- 除非为了记录迁移证据，否则不再给旧 phase/run/event 模型增加字段。
- 先解决本文列出的所有未决问题。

### Phase 1：Domain 与 ADR 决策

- 在 glossary 中增加 `TreeId`、Session、Turn、Item 和 Interaction 定义。
- 新增 Accepted ADR，正式接受 Codex-style outer lifecycle。
- 修订被 supersede 的 ADR，重点包括 actor/run execution、prompt/history 和 event protocol。

### Phase 2：Protocol Shape

- 定义 Session/Turn/Item/request 公共类型。
- 定义 method、notification、server request 和 response envelope。
- 定义 ordering 和 terminal invariants。
- 定义 snapshot/read projection 和 pagination。

### Phase 3：Session Projection

- 用 `SessionStatus` 替换 `SessionPhase`。
- 用 current Turn 和 pending request projection 替换 `CurrentRun`。
- Session list/read 可以在不加载 runtime 的情况下展示 `NotLoaded`。
- 定义 `SystemError` recovery behavior。

### Phase 4：Turn Lifecycle

- 用 Turn 术语替换公共 Run 术语。
- Retry 和 required recovery 保持在同一个 Turn 内。
- Standalone compaction 建模为 non-steerable Turn。
- 删除 baseline server-side FollowUp/NextTurn 行为。

### Phase 5：Item Lifecycle

- 引入 `ItemId` 和 typed Item payload。
- 用 Item lifecycle 替换 message/tool-specific public lifecycle event。
- 定义 `ItemId ↔ EntryId` 或 `ItemId ↔ committed entries` 持久化映射。
- 在承诺可恢复的范围内，确保 completed Item payload 可以从持久化历史重建。

### Phase 6：Interaction Lane

- 引入 server-initiated request 和 client response。
- 替换 approval event/command 配对。
- 将 pending request 加入 Session snapshot/read projection。
- 定义 duplicate、stale、timeout、interrupt、close 和 reconnect behavior。

### Phase 7：Runtime Projection

- 将内部 model/tool/storage progress 投影为目标生命周期。
- Request pending 期间继续保持 actor responsiveness。
- 内部保留 stable commit 和 visibility guarantees，但不暴露 commit phase。

### Phase 8：删除旧模型

- 删除被 supersede 的 phase/run/message/tool/retry/compaction 公共类型。
- 删除权威模块中的 compatibility prose。
- 历史 ADR/research/review 正文通过 amendment 保留，不倒改历史。

### Phase 9：验证与 Handoff

- 执行 terminology 和 broken-link scan。
- 建立 protocol/state/item/request conformance matrix。
- 更新 progress/handoff 文档。
- 完成后才开始或恢复生产实现。

## Conformance Matrix

### Session

- 已持久化但未加载的 Session 可以报告 `NotLoaded`，且不会因此加载 runtime；
- resume 产生 `NotLoaded → Idle`；
- 一个 Session 同时最多有一个 `InProgress` Turn；
- Turn terminal 后 Session 回到 `Idle`；
- 普通 Turn failure 不会产生 `SystemError`；
- unload 清除 runtime-only request，并报告 `NotLoaded`；
- root 与 fork Session 共享一个 `TreeId`，但拥有不同 `SessionId`。

### Turn

- `turn/start` 恰好产生一次 `turn/started` 和一次 terminal `turn/completed`；
- `turn/steer` 继续使用同一个 `TurnId`；
- non-steerable Turn 拒绝 steer；
- `turn/interrupt` 必须 idempotent/stale-safe；
- retry 不改变 `TurnId`；
- required compaction 不改变 `TurnId`；
- standalone compaction 创建独立 non-steerable Turn；
- terminal Turn state 不可修改。

### Item

- 每个 `item/started` 必须恰好对应一次 `item/completed`，或存在明确记录的 Turn-terminal closure rule；
- delta 只能发生在 started 之后、completed 之前；
- completed payload 携带最终 item-specific status；
- UserMessage/AgentMessage/ToolCall/Compaction Item history 在 reload 后可以一致投影；
- Item ID 在定义的 scope 内唯一；
- Item-to-storage mapping 按选定策略支持 fork/replay。

### Interaction

- Request 必须先在 Runtime 登记，再发送给客户端；
- 存在 matching pending request 时，Session 必须包含对应 active flag；
- duplicate response 不得重复执行；
- stale Session/Turn/Item/Request identity 必须被拒绝；
- interrupt/close/system error 必须清除 pending request；
- connection contract 允许时，每个被清除的 request 都发布 `serverRequest/resolved`；
- `AcceptForSession` 更新 Runtime-owned session policy/cache，不修改 UI-owned state；
- 客户端不得在 response 中替换 frozen command/tool/file-change data。

### Ordering

- Turn 必须先于其第一个 Item lifecycle 开始；
- Item completion 先于 Turn completion；
- Pending server request 必须在 Turn completion 前 resolved；
- Session 只有在 Turn completion 后才能进入 Idle；
- terminal/status notification 不得引用 private attempt/segment identity；
- 内部 writer 接受 stable fact 前，不得把它报告成 durable fact。

## 未决问题

Outer model 已经选定，以下细节仍需明确评审：

1. 所有看起来 durable 的 `item/completed` payload 是否必须等 stable storage commit 后才能发布，还是需要独立 persistence projection。
2. 一个 ToolCall Item 对应多条 stored message/entry 时，`ItemId ↔ EntryId` 如何精确映射。
3. 一个 Session fork 到同一 `TreeId` 下的另一个 Session 时，Item ID 是重新生成还是保留。
4. 同一 Runtime 仍存活、client connection 重建时，pending request 如何 resend/recover。
5. Request 因 abrupt transport loss 被清除时，是否保证发布 `serverRequest/resolved`。
6. `SystemError` 的精确进入与恢复 transition。
7. 哪些 Codex Item type 属于 MVP，哪些只保留名称。
8. MiniCore 使用一个通用 `item/<type>/delta` envelope，还是使用 Codex-style item-specific delta method。
9. Session list/read 与 Runtime snapshot 是否共用一个 Session view type，还是使用 summary/detail projection。
10. `TreeId` 和 `SessionId` 如何进入 JSONL header，以及 external import 时如何生成。
11. Client-side follow-up scheduling 是否足以满足所有 first-party adapter；server-side queued Turn 必须另行决策。
12. Permission request 与 `requestUserInput` interaction 的精确 decision set、timeout 和 auto-resolution semantics。

## Review 顺序

继续评审时按以下顺序推进：

1. Session 与 Tree identity：`TreeId`、`SessionId`、fork/read/resume behavior。
2. Turn admission 和 terminal ordering。
3. Item type、item-specific status 和 storage mapping。
4. Server-request transport、pending request snapshot 和 reconnect behavior。
5. Compaction/retry 如何投影进 Turn/Item。
6. 删除当前 queue 和 phase 模型。
7. Protocol migration 和权威文档更新计划。

## 进度清单

- [x] 创建专用重构分支。
- [x] 选择 Codex outer lifecycle model。
- [x] 将 Codex Thread 概念重命名为 MiniCore Session。
- [x] 选择 `TreeId` 作为 fork-tree identity。
- [x] 记录目标 Session/Turn/Item/Interaction 状态。
- [x] 记录 approval server-request ownership model。
- [x] 记录完整目标生命周期场景。
- [x] 记录当前到目标的映射和删除候选。
- [x] 记录迁移阶段和 conformance matrix。
- [ ] 解决未决问题。
- [ ] 创建并接受 lifecycle ADR。
- [ ] 更新 `CONTEXT.md` glossary。
- [ ] 更新 architecture 和 module authority documents。
- [ ] 更新 protocol 和 event contracts。
- [ ] 关闭或 supersede 受影响的 review issue。
- [ ] 完成 Rig/provider impact review。
- [ ] 增加 implementation 和 conformance tests。
- [ ] 删除被 supersede 的公共类型和文案。
- [ ] 迁移完成后更新 progress handoff。

## 验收标准

重构只有在以下条件全部满足后才算完成：

- `SessionStatus`、`TurnStatus`、Item lifecycle 和 server-request lifecycle 成为唯一公共外层执行状态模型；
- `TreeId` 和 `SessionId` 具有不同且明确的 fork 语义；
- 不再存在公共 `RunId`、`SessionPhase::Turn/Compaction/RetryBackoff` 或平行 retry workflow；
- message/tool public progress 使用 Item lifecycle；
- approval 使用 server request/client response/resolved semantics；
- 在承诺范围内，可以从 current Runtime projection 恢复 pending interaction；
- required compaction 和 retry 保持在同一个 Agent Turn 内；
- standalone compaction 是 non-steerable Turn；
- baseline 架构中不存在 public 或 server-owned FollowUp/NextTurn queue contract；
- 当前 storage 和 Prompt invariant 要么能隐藏在新 interface 后面干净映射，要么被新的 Accepted decision 明确替换；
- 权威文档、ADR amendment、review status、progress handoff 和 tests 与目标模型一致；
- 历史文档继续可用作历史背景，但不会被误认为当前契约。
