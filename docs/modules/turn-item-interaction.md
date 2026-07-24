# Turn、Item 与 Interaction 架构设计

状态：当前权威架构（设计已冻结，实现进行中）
日期：2026-07-16

## 目的

本文定义 MiniCore 的 Turn、Item 和 Interaction 领域模型，重点解决：

- 一个用户 Turn 的精确开始和结束边界；
- Turn durable status 与 transient execution phase 的区别；
- UserMessage、AgentMessage、Reasoning 和 Tool work 如何表达为 Item；
- ToolCall 与 ToolResult 是否应是两个 Item；
- approval 和结构化用户问题如何归属于 Item；
- Interaction request、resolution、timeout、reconnect 和 crash recovery；
- streaming delta、Tool progress、retry 和 diagnostics 是否属于 durable domain truth；
- Item 与 SessionStorage entry 的关系。

本文不定义以下内容（由对应文档或 Runtime protocol 权威定义）：

- SessionStorage entry 在具体文件 adapter、index 和 Session execution 中的实现细节；
- SessionExecutor/request queue/async operation 的具体实现，以[Session Execution架构设计](session-execution.md)为权威；
- Runtime command、query、event 和 snapshot payload；
- provider-specific message、tool-call 和 reasoning encoding；
- provider-specific reasoning 的最终编码；
- Question UI schema 的全部展示字段；
- compaction、review、background work 的最终 Item 投影。

## 同类产品结论

| 项目 | Turn / Item / Interaction 形状 | MiniCore 结论 |
| --- | --- | --- |
| Codex | 一个用户 Turn 包含异构 TurnItem；command、MCP、file change 等使用操作型 Item，调用、状态和结果属于同一个 Item；item started/completed 是稳定快照，delta 是通知；approval 和 user-input 使用 server request | 借鉴操作型 Item、stable ItemId、request/response 和 transient delta；不复制庞大的 Item variant 集合 |
| pi | `turn_start/turn_end` 更接近一次模型调用加 Tool execution round；ToolCall 在 AssistantMessage，ToolResult 是独立 Message；approval 是 runtime callback | loop 简洁，但“ToolCall 与结果分离 + ephemeral approval”不满足 MiniCore recovery 和 durable Interaction |
| Grok Build | conversation与TurnCompleted持久化；大部分pending interaction是内存waiter，PlanApproval另有独立持久化规则 | 特殊interaction单独持久化会产生例外；MiniCore应统一request/resolution durability |
| Claude Code | Permission 和 AskUserQuestion 围绕 Tool 调用；interrupt/checkpoint 是产品能力；stream-json partial output 是 observer event | Tool-centric Interaction 合理；permission policy 属于 Tool，Interaction 只表达外部回答 |
| Cursor | 可观察到 Plan、checkpoint 和 background agent，但内部 Turn/Item ownership 未公开 | 只参考产品体验，不推断其内部领域模型 |

MiniCore 采用：

```text
领域/public projection：operation-centric Item
storage implementation：immutable facts + projection fold
```

## 决策摘要

- Turn 从 initiating UserMessage entry 成功 append 开始；
- Turn 到 final AssistantMessage、TurnInterrupted 或 TurnFailed entry 成功 append 结束；
- `Interrupted` 是 terminal，不恢复为 Running；
- Steer 只作用于同一个 Running Turn，并携带 expected TurnId；
- FollowUp 在 current Turn terminal 后开启下一 Turn；
- Turn 逻辑上拥有有序 Item，但 durable Turn head 不内联 `Vec<Item>`；
- 最小 ItemContent 只有 UserMessage、AgentMessage、Reasoning、ToolInvocation；
- ToolCall 与 ToolResult 不建立两个 sibling Items；
- 一个 ToolInvocation Item 贯穿 call、approval、execution、result 和 recovery；
- ItemType 从 ItemContent discriminant 派生，不独立保存；
- UserMessage、AgentMessage 和 Reasoning 只在形成稳定值后成为 durable Item；
- streaming delta、Tool progress、provider retry 和 execution phase 不是 Item；
- 只保存 provider 实际返回的 finalized/replayable reasoning，不获取 hidden chain-of-thought；
- Interaction 是 Item-owned durable request/resolution；
- Interaction request append 后才向 host 发布；
- Interaction resolution append 后才唤醒 waiter或执行 Tool；
- transport disconnect 不自动关闭 Interaction；
- host restart baseline 关闭 pending Interaction并中断 Turn，不恢复旧 waiter；
- outcome-unknown ToolInvocation 进入 Abandoned，不生成 synthetic ToolResult；
- ToolRound 是 committed conversation unit，不是 entity、Manager 或公开 lifecycle object；
- ItemId、ToolCallId 和 storage entry identity 是不同 identity。

## 领域关系

```text
Session
└─ Turn*
   ├─ ordered Item*
   │  └─ Interaction*
   └─ initiating UserMessage → exact TurnContext entry
```

基数：

```text
Session 1 ── 0..* Turn
Turn    1 ── 1..* Item
Item    1 ── 0..* Interaction
```

Item 与 Interaction 的集合由 SessionStorage projection 提供，不要求在 Turn/Item struct 内嵌完整 children。

## Turn

Turn 是一条用户意图的 durable execution boundary：

```rust
pub struct Turn {
    pub id: TurnId,
    pub session_id: SessionId,
    pub started_at: Timestamp,
    pub status: TurnStatus,
}

pub enum TurnStatus {
    Running,
    Completed {
        completed_at: Timestamp,
    },
    Interrupted {
        completed_at: Timestamp,
        reason: TurnInterruption,
    },
    Failed {
        completed_at: Timestamp,
        failure: TurnFailure,
    },
}
```

Turn 不内联：

```text
Vec<Item>
TurnModel
PromptSet / ToolSet / SkillView
WorkspaceSnapshot
AgentRevisionRef / SessionDefinition
provider session / AgentLoop
pending Interaction waiter
```

exact Agent、SessionDefinition、Workspace、Prompt、Tool、Skill和TurnModel references 属于 initiating UserMessage 引用的 TurnContext entry，不重复放入 Turn head。

### TurnInterruption

Interrupted 表示执行被外部控制或安全事件终止：

```rust
pub enum TurnInterruptionKind {
    UserCancelled,
    RuntimeShutdown,
    HostRestart,
    SecurityRevoked,
    RecoveryContextUnavailable,
}

pub struct TurnInterruption {
    pub kind: TurnInterruptionKind,
    pub message: Option<String>,
}
```

Interrupted 是 terminal：

- 不恢复为 Running；
- 不接受 Steer；
- 后续普通用户输入创建新 Turn；
- 已 committed conversation prefix 保留；
- 未 append draft 不进入 conversation。

### TurnFailure

Failed 表示 Turn 已经开始，但遇到不可恢复的执行错误：

```rust
pub enum TurnFailureKind {
    Model,
    Storage,
    Execution,
    Invariant,
}

pub struct TurnFailure {
    pub kind: TurnFailureKind,
    pub message: String,
    pub retryable: bool,
}
```

Tool 自身失败、schema error、policy deny 或 approval deny如果已经产生真实 ToolResult，通常只是 ToolInvocation outcome，Turn 可以继续 Running。

Admission 在 initiating UserMessage append 前失败不创建 Failed Turn。

## Turn 边界

```text
candidate admission
→ compose initiating UserMessage
→ append TurnContext entry
→ append initiating UserMessage Item
→ TurnStatus = Running
→ zero or more committed messages/events
→ append one terminal fact
```

terminal entry：

```text
Message(role = assistant, phase = final)

或

InteractionResolved*
ToolAbandoned*
TurnInterrupted | TurnFailed
```

一个 Turn 只能有一个 terminal fact。

## UserMessage、Steer 与 FollowUp

UserMessage Item 表达 durable、规范化后的用户可见消息：

```rust
pub struct UserMessageItem {
    pub message: CanonicalUserMessage,
}
```

不在 ItemContent 中增加第二份可变status。storage message entry的`source = Input | Steer`是authoritative classification：

- `Input` UserMessage 开启Turn并引用TurnContext entry；
- `Steer` UserMessage属于expected Running Turn；
- FollowUp在下一Turn写入新的`Input` UserMessage。

结构化Interaction answer不是UserMessage Item。它先关闭Interaction，随后成为对应ToolResult/tool message，并在`tool_round_completed`后进入conversation。

## Item

Item 是 Turn 内稳定、可观察的语义值或长生命周期操作：

```rust
pub struct Item {
    pub id: ItemId,
    pub turn_id: TurnId,
    pub content: ItemContent,
}

pub enum ItemContent {
    UserMessage(UserMessageItem),
    AgentMessage(AgentMessageItem),
    Reasoning(ReasoningItem),
    ToolInvocation(ToolInvocationItem),
}
```

不独立存储 `ItemType`：

```rust
pub enum ItemType {
    UserMessage,
    AgentMessage,
    Reasoning,
    ToolInvocation,
}
```

`Item::item_type()` 从 ItemContent discriminant 派生。

## Item 类型

### UserMessage

UserMessage 只能来自 PromptSet 规范化后的 durable value。

```text
raw host input
→ PromptSet.compose_user_message(...)
→ CanonicalUserMessage
→ append user message entry
→ UserMessage Item
```

raw input、accepted-but-uncommitted Steer 或 PromptContribution draft 不是 Item。

### AgentMessage

```rust
pub struct AgentMessageItem {
    pub content: AgentMessageContent,
}
```

storage assistant message使用稳定`phase = Intermediate | Final`：

- `Final` AssistantMessage 是Turn completed terminal fact；
- `Intermediate` AssistantMessage表示同一Turn仍需继续：包含ToolCall时等待完整ToolRound后model-visible；不含ToolCall时是Steer继续前保存的model-visible Assistant Continue step；
- streaming phase transition不是durable Item。

provider partial output、stream draft 和 abandoned retry attempt 不是 durable AgentMessage Item。

### Reasoning

```rust
pub struct ReasoningItem {
    pub summary: ReasoningSummary,
}
```

Reasoning Item是面向领域/UI的policy-filtered read projection：

- 可选；
- `summary`只表达允许展示的摘要，不是完整storage wire schema；
- authoritative assistant entry可以保存provider实际返回的finalized/replayable text、summary、encrypted payload、signature和provider item id；
- 不获取或伪造provider未返回的hidden chain-of-thought；
- provider reasoning delta是transient observer event；
- 是否对模型重新可见由Prompt/conversation policy决定，不能仅因它是Item就自动进入模型上下文。

### ToolInvocation

ToolInvocation 是 ToolCall、approval、execution 和结果的统一领域对象：

```rust
pub struct ToolInvocationItem {
    pub call: ToolCall,
    pub state: ToolInvocationState,
}

pub enum ToolInvocationState {
    Started,
    Completed {
        result: ToolResult,
    },
    Abandoned {
        reason: ToolAbandonReason,
    },
}
```

```rust
pub enum ToolAbandonReason {
    TurnTerminated,
    HostRestart,
    OutcomeUnknown,
    RecoveryContextUnavailable,
}
```

语义：

| 状态 | terminal | 有 ToolResult | 可进入 complete ToolRound conversation |
| --- | --- | --- | --- |
| Started | 否 | 否 | 否 |
| Completed | 是 | 是 | 只有 `tool_round_completed` 引用完整 round 后进入 |
| Abandoned | 是 | 否 | 否 |

`Abandoned` 是 truthful operational outcome：MiniCore 知道该操作不能继续，但无法诚实构造 ToolResult。

正常执行路径中，ToolResult 先是 execution-local candidate；`role = tool` message成功append后，ToolInvocation进入Completed operational state。Completed不等于已进入模型conversation；只有后续`tool_round_completed`event成功append后，完整assistant/tool sequence才一次性进入conversation。terminal/recovery保留已存在的truthful tool message，但不会隐式补做缺失的ToolRound completion event。

以下情况可以 Completed：

```text
executor success
executor/schema/sandbox failure
policy/approval deny
confirmed cancellation
confirmed timeout
```

以下情况必须 Abandoned：

```text
side effect 已可能开始但 outcome unknown
host restart 后没有 exact result
required recovery context 不可重建
```

## ToolResult Disposition

`Completed` 不等于 Tool 成功。ToolResult 使用 typed disposition：

```rust
pub struct ToolResult {
    pub call_id: ToolCallId,
    pub disposition: ToolResultDisposition,
    pub content: ToolResultContent,
    pub details: Option<Value>,
}

pub enum ToolResultDisposition {
    Succeeded,
    Failed,
    Denied,
    Cancelled,
}
```

provider adapter 可以从 disposition 派生 `is_error`，但领域层不使用一个 bool 压平 denied、cancelled 和 failed。

`ToolCallId` 是 provider/transcript correlation；`ItemId` 是 MiniCore Tool operation identity。二者不能互相替代。

## Item Lifecycle

Item read projection 可以派生：

```rust
pub enum ItemStatus {
    Started,
    Completed,
    Abandoned,
}
```

规则：

- UserMessage、AgentMessage 和 Reasoning durable Item 创建时即 Completed；
- ToolInvocation 从 Started 到 Completed 或 Abandoned；
- terminal Item 不回到 Started；
- Turn terminal 后不创建新 Item；
- Turn terminal entry 前必须关闭所有 Started Item；
- Completed 只表示该 Item 有真实、稳定的最终语义，不表示业务成功。

ItemStatus 是 projection，不作为与 ItemContent 独立更新的第二事实字段。

## Streaming 与 Observer Event

以下内容不是 durable Item：

```text
message_start / text_delta / reasoning_delta
Tool progress / stdout chunk
provider retry attempt
TurnExecutionPhase transition
approval dialog opened/closed notification
cache hit / token usage update
```

Runtime 可以发布：

```text
ItemStarted
ItemDelta
ItemCompleted
ItemAbandoned
```

公开 event shape 由 Runtime protocol 定义。completed/abandoned durable snapshot 是 replay truth；delta 可丢失、合并或不 replay。

## Complete ToolRound

ToolRound 是 conversation promotion unit，不是领域 entity：

```text
one stable model output
├─ optional AgentMessage / Reasoning Items
├─ ordered ToolInvocation Items
└─ exactly one truthful ToolResult per Completed invocation
```

执行顺序：

```text
parse stable ModelOutput
→ append one assistant/intermediate message with ordered reasoning/text/tool_call content
→ every tool_call creates a Started ToolInvocation projection
→ approval / execution durable events
→ all exact ToolResult candidates known
→ append one role=tool message per truthful result
→ append tool_round_completed event referencing assistant + all tool entries
→ storage-owned apply_committed applies the conversation promotion
→ next logical model call
```

Started/Completed Item可以是durable operational truth和UI projection，但`tool_round_completed`前对应assistant/tool messages不能进入模型conversation。

ToolRound completion 必须保持：

- 原始 ToolCall 顺序；
- 每个 call 恰好一个 truthful ToolResult；
- optional AgentMessage/Reasoning 与 calls 来自同一个 stable model output；
- ToolResult call_id 与 ToolCallId 匹配；
- 所有 ToolInvocation ItemId 属于同一个 Turn；
- incomplete、Abandoned 或 outcome-unknown invocation 不被纳入completion event。

不建立：

```text
ToolRoundId
ToolRound entity
ModelStep
ModelOutput entity
```

SessionStorage entry 身份只用 EntryId + parent_id；不再有独立的 durable operation key 字段，也不具有独立CRUD或lifecycle。

## Item Identity 与 Ordering

```text
ItemId
→ MiniCore semantic Item identity

ToolCallId
→ provider/tool protocol correlation

EntryId
→ fixed StoredSessionEntry.entry_id identity
```

基础规则：

- 三者不要求相等；
- fork remap child-local ItemId/EntryId 等 identity及nested references，preserve ToolCallId；完整规则由 Conversation/SessionStorage 定义；
- ToolCallId 必须按 provider contract 原样回显；
- Interaction 归属于 ItemId，不归属于裸 ToolCallId；
- 一个 ToolInvocation Item 可以对应多条 operational/conversation entries；
- Turn Item ordering由selected parent path上的entry sequence提供，不在Item内保存可变ordinal；
- 并发 Tool 执行可以乱序结束，但 complete ToolRound 按原始 call order 投影。

## Interaction

Interaction 是 Runtime 在 Running Turn 内发起、等待外部回答的 durable request：

```rust
// IdempotencyKey：调用方提供，仅用于活跃 run 内的 resolution 去重（防 host 重复发同一 resolution），
// 不是 durable crash key；storage 层 entry 不再有 operation_key 字段。
pub struct IdempotencyKey;

pub struct Interaction {
    pub request_id: RequestId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub item_id: ItemId,
    pub request: InteractionRequest,
    pub state: InteractionState,
}

pub enum InteractionState {
    Pending {
        requested_at: Timestamp,
        expires_at: Option<Timestamp>,
    },
    Resolved {
        resolution: InteractionResolution,
        resolution_key: IdempotencyKey,
        resolved_at: Timestamp,
    },
}
```

不用 `InteractionStatus + Option<InteractionResolution>`，避免构造 `Resolved + None` 或 `Pending + Some(...)`。

read projection 可以派生：

```rust
pub enum InteractionStatus {
    Pending,
    Resolved,
}
```

## Interaction Family

最小 request family：

```rust
pub enum InteractionRequest {
    ToolApproval(ToolApprovalRequest),
    UserQuestion(UserQuestionRequest),
}
```

最小 resolution family：

```rust
pub enum InteractionResolution {
    ToolApproval(ToolApprovalDecision),
    UserAnswer(UserQuestionAnswer),
    Cancelled(InteractionCancelReason),
    Expired,
}
```

```rust
pub enum InteractionCancelReason {
    UserCancelled,
    TurnTerminated,
    TransportClosedByHost,
    HostRestart,
    Recovery,
}
```

规则：

- ToolApproval request 只能由 Tool-related Item 触发；
- UserQuestion 通常归属于 ask-user ToolInvocation，也允许归属于其他真正发起 request 的 Item；
- request/resolution family 必须匹配；
- Cancelled 和 Expired 可以关闭任意 family；
- auto-approved、auto-denied 的 Tool policy 不需要创建 Interaction；
- 一个 Item 可以顺序拥有多个 Interaction；
- domain 不强制一个 Turn 同时只能有一个 Pending Interaction，execution policy 可以进一步收紧。

## Interaction Append Order

```text
construct typed request
→ append InteractionRequested
→ apply durable projection
→ publish request notification
→ wait for response
→ validate expected TurnId / RequestId / family / resolution idempotency key
→ 由同一个SessionExecutor检查expires_at；deadline已到时只允许Expired/fail-closed closure
→ append InteractionResolved { resolution, resolution_key }
→ apply durable projection
→ wake waiter / continue Tool authorization
```

关键不变量：

- request 未 append 前不能通知 host；
- resolution 未 append 前不能执行受审批保护的副作用；
- Tool side effect前仍需重新检查Cancel state和current authorization；
- 相同 resolution_key 重试幂等返回当前结果；
- 不同 key 的第二次 resolution 返回 AlreadyResolved；
- `now >= expires_at` 后 late response 返回 InteractionExpired，即使 timeout worker 尚未先写入；
- first committed terminal resolution wins。

## Tool Approval

```text
ToolAuthorization = Ask
→ parent ToolInvocation 已有 ItemId
→ append ToolApproval Interaction
→ host Allow
→ append Interaction resolution
→ final Cancel state and authorization validation
→ Sandbox / Tool execute
```

Deny：

```text
append Deny resolution
→ 形成 truthful denied ToolResult candidate
→ append role=tool message
→ append tool_round_completed
```

Interaction 不决定 Tool permission。ToolRequirements、WorkspaceAccessView、ToolPolicy、grant 和 Sandbox 仍属于 Tool 子系统。

ToolService 只判断需要 approval；durable Interaction ownership 属于 Session execution。外部 TUI/RPC/Web host 通过[Runtime Interface](runtime-interface.md)的per-session StateEvent接收request，并通过`InteractionCommand::Resolve`提交resolution，不能直接持有Tool executor waiter。

## UserQuestion

```rust
pub struct UserQuestionRequest {
    pub questions: Vec<UserQuestion>,
}

pub struct UserQuestionAnswer {
    pub answers: Vec<UserAnswer>,
}
```

一个 request 可以包含多个相关问题，避免为同一个问卷创建多个 pending lifecycle。

answer：

```text
append UserAnswer resolution
→ ask-user Tool 生成 truthful ToolResult candidate
→ append role=tool message
→ append tool_round_completed
→ 下一次模型调用看见 answer
```

Interaction answer 本身不是 UserMessage，不开启新 Turn。

## Reconnect 与 Transport Loss

transport delivery 是 at-least-once；durable Interaction 是 truth：

```text
client disconnect
→ Interaction 保持 Pending
→ Turn 保持 Running / WaitingApproval 或 waiting question

client reconnect
→ query loaded Session snapshot / pending interactions
→ 使用相同 RequestId 重发 request
```

规则：

- notification 丢失不丢 request；
- reconnect 不创建新 RequestId；
- lost response acknowledgement 使用同一个 resolution_key（in-run dedup）在活跃 run 内重试；
- abrupt client disconnect 默认不等于 Deny 或 Cancel；
- pending Interaction 使 Session execution 非 Idle，Unload 返回 Busy；
- host 可以显式关闭 transport 并提交 Cancelled(TransportClosedByHost)；
- 没有 subscriber 时可以继续等待，直到 timeout、cancel、shutdown 或 restart。

## Timeout 与 Auto Resolution

Interaction 可以保存可选绝对 `expires_at`，不建立通用 timeout policy entity。

到期：

```text
Pending + now >= expires_at
→ SessionExecutor拒绝新的host response
→ append Expired，或 request-specific fail-closed resolution
→ timeout worker 与 terminal cleanup 仍使用 first-wins CAS
```

Tool approval 默认 fail closed。是否把 timeout 映射为 denied ToolResult 由 Tool policy决定，但必须是明确、durable 的 resolution。

## WaitingApproval 与 Steer

等待审批时：

```text
TurnStatus = Running
TurnExecutionPhase = WaitingApproval
InteractionState = Pending
ToolInvocationState = Started
```

Steer：

```text
Steer(expected TurnId)
→ push_back进入普通FIFO
→ 等待当前ToolRound truthful completion
→ 下一次Model前pop_front一条
→ 不作为 approval decision
```

## Turn Terminal Cleanup

Turn terminal entry 前必须使领域projection闭合。Runtime-generated closure使用由terminal operation + RequestId派生的resolution_key做本轮去重；crash 后的重复 cleanup 幂等靠 committed prefix 状态判断（该 Interaction 已 resolved、该 Turn 已 terminal 则跳过），不依赖 durable operation key；exclusive lease 下恢复单跑，重复恢复靠状态跳过：

```text
all Pending Interaction → Resolved(Cancelled(reason)) 或 Resolved(Expired)
all Started ToolInvocation → Completed(existing exact durable result) 或 Abandoned
Turn Running → Completed / Interrupted / Failed
```

不能：

- terminal Turn 保留 Pending Interaction；
- terminal Turn 保留 Started Item；
- outcome unknown 时生成 fake error ToolResult；
- terminal 后接受 Interaction response、Steer 或新 Item。

response 与 terminal race：

```text
resolution append 先赢
→ execution 按 resolution 继续或参与 terminal cleanup

terminal cleanup 先赢
→ Interaction 已 Cancelled
→ late response返回AlreadyResolved / TurnNotRunning
```

## Crash Recovery

baseline：

```text
reload durable facts
→ 检测没有 terminal fact 的 Turn
→ 检测 Pending Interaction
→ 检测 Started ToolInvocation / operational execution records
→ append idempotent recovery entries：
     Pending Interaction → InteractionResolved(Cancelled)
     已有 role=tool message的invocation保持Completed
     其余Started invocation → ToolAbandoned
     Turn → TurnInterrupted
```

不恢复：

```text
provider stream
AgentLoop state
approval/question waiter
Tool task
streaming delta
```

不自动重放 outcome-unknown Tool，不生成 synthetic ToolResult，不把 Abandoned invocation 放入模型 conversation。

## Entries、Projection 与 Storage

Conversation 采用 per-session by-entry JSONL tree，完整定义见[Conversation 与 SessionStorage 架构设计](conversation-storage.md)。一个ToolInvocation由message/event entries投影：

```text
assistant content: tool_call
InteractionRequested*
InteractionResolved*
ToolExecutionStarted?
role=tool message?
ToolAbandoned?
ToolRoundCompleted?
```

这些是SessionStorage private durable entries，不是Runtime public event的序列化副本，也不形成独立entity/CRUD。

需要的 projection：

```text
Conversation projection
→ 只消费 initiating UserMessage、Steer、无ToolCallAssistant Continue、`tool_round_completed`引用的完整round、Compaction、final AssistantMessage

Turn/Item projection
→ 消费 semantic facts 和 operational Item lifecycle

Pending Interaction projection
→ Requested - Resolved

Recovery projection
→ started operation - known outcome/terminal entry
```

SessionStorage 仍是唯一 durable truth；projection 和 event stream 都不是第二事实来源。

## Ownership

| 对象/行为 | Owner |
| --- | --- |
| Turn durable facts | SessionStorage；Session execution 负责 append |
| Turn active execution | SessionExecutor |
| Item identity 和 lifecycle transition | Session execution + trusted entry writer |
| Item content value | 对应 producer；Session execution 规范化后 append |
| ToolCall/ToolResult execution semantics | ToolService / ToolSet |
| ToolInvocation domain projection | Session execution / storage projector |
| Interaction request/resolution | Session execution + SessionStorage |
| approval/question external delivery | Runtime Interface的per-session StateEvent与InteractionCommand |
| pending waiter | loaded Session execution，transient |
| streaming delta/progress | observer event pipeline |
| model-visible conversation | committed conversation projector + PromptSet |

不建立：

```text
TurnManager
ItemManager / ItemService
InteractionManager / InteractionService
ToolResult entity
ToolRound entity
ModelStep entity
PendingRequest registry as durable truth
```

loaded Session 可以维护 waiter map，但 durable pending state 必须从 SessionStorage 重建。

## Error 与 Race 分类

本节固定语义；公开 error enum 由 Runtime protocol 定义：

```text
TurnNotRunning
ExpectedTurnMismatch
ItemNotFound
ItemAlreadyTerminal
InteractionNotFound
InteractionAlreadyResolved
InteractionFamilyMismatch
InteractionExpired
ParentItemMismatch
ToolCallMismatch
OutcomeUnknown
TerminalAppendConflict
StaleProjection
```

关键 race：

- Steer vs terminal append；
- Interaction response vs timeout；
- Interaction response vs Turn terminal cleanup；
- Tool result vs security revocation；
- tool_round_completed append vs cancel；
- initiating/terminal append outcome unknown；
- reconnect resend vs original response acknowledgement。

全部race由per-session SessionExecutor、expected TurnId和in-run idempotency（resolution/submission dedup）线性化。

## 被否决的方案

### ToolCall 和 ToolResult 是 sibling Items

否决原因：

- approval 不知道应该归属于 call、result 还是二者之间的隐式 operation；
- 容易产生 orphan ToolResult 或一个 call 多个 result；
- UI、replay 和 recovery 必须反复 correlation；
- outcome unknown 时 call 永久悬空；
- 同一用户可观察操作被拆成两个 identity。

Transcript 可以保存独立 tool-call/tool-result records，但领域 Item 不需要复制这种 provider encoding。

### 所有事件都成为 Item variant

否决原因：stream delta、retry、cache、token update 和 phase 都是 observer/execution facts，不值得获得领域 identity 与 lifecycle。

### 公开纯事实模型

否决原因：immutable facts 适合作为 SessionStorage implementation，但直接暴露会把 SessionStorage schema、projection fold 和 migration complexity 泄漏给领域调用方。

### Ephemeral Interaction

否决原因：request notification 丢失、client reconnect、response acknowledgement 丢失和 host restart 都无法从 durable truth判断 request 状态。

### Interrupted 恢复为 Running

否决原因：破坏 terminal status 和 exact execution context语义。继续工作应创建新 Turn，或在 terminal 前使用 Steer。

## 基础不变量

- Turn 由 committed initiating UserMessage 开始；
- final assistant、TurnInterrupted或TurnFailed entry是Turn唯一结束线性化点；
- Interrupted 和 Failed 都是 terminal；
- Steer 只属于 expected Running Turn；
- FollowUp 开启新 Turn；
- Turn head 不内联完整 Items；
- ItemType 从 ItemContent 派生；
- 最小 ItemContent 只有 UserMessage、AgentMessage、Reasoning、ToolInvocation；
- ToolCall 与 ToolResult 属于同一个 ToolInvocation Item；
- ItemId、ToolCallId、EntryId 不混用；
- streaming delta 和 progress 不是 durable Item；
- 只持久化provider实际返回的finalized/replayable reasoning，不获取hidden chain-of-thought；
- Started ToolInvocation 不进入模型 conversation；
- Completed ToolInvocation 必须拥有 truthful ToolResult；
- Abandoned ToolInvocation 不拥有 ToolResult，也不进入 conversation；
- `tool_round_completed`前不开始下一次模型调用；
- Interaction request append-before-notify；
- Interaction resolution append-before-resume/side-effect；
- Interaction response 不是 UserMessage；
- transport disconnect 不自动 resolution；
- reconnect 使用相同 RequestId；
- Turn terminal 后没有 Pending Interaction 或 Started Item；
- outcome unknown 不生成 synthetic ToolResult；
- SessionStorage 是 durable truth；
- 不建立新的 Manager、Service、ModelStep 或 ToolRound entity。

## Test Matrix

至少覆盖：

- initiating UserMessage append创建Running Turn和对应Item；
- admission failure 不创建 Turn；
- completed/interrupted/failed terminal exclusivity；
- terminal Turn 拒绝 Steer、Item append 和 Interaction resolution；
- expected TurnId mismatch；
- UserMessage/AgentMessage/Reasoning 创建即 Completed projection；
- ItemType 从 ItemContent 派生；
- ToolInvocation Started → Completed(success)；
- ToolInvocation Started → Completed(failed/denied/cancelled)；
- ToolInvocation Started → Abandoned(outcome unknown)；
- Completed invocation 必须有 matching ToolResult；
- Abandoned invocation 不允许 ToolResult；
- ItemId 与 ToolCallId 独立；
- parallel Tool completion后按source call order append tool messages和completion references；
- incomplete ToolRound 不进入 conversation；
- streaming delta 丢失不影响 replay；
- finalized provider reasoning artifact 按 retention/redaction policy replay；
- InteractionRequested append-before-notify；
- InteractionResolved append-before-wake；
- Tool approval allow/deny；
- UserQuestion 多问题单 request；
- Interaction family mismatch；
- duplicate same resolution_key 幂等；
- conflicting second resolution key；
- response before deadline vs timeout first-wins；
- response after expires_at 被拒绝，即使 timeout worker 尚未 append；
- response vs terminal cleanup first-wins；
- disconnect 后 pending 保留；
- reconnect 使用相同 RequestId resend；
- lost acknowledgement 使用 in-run resolution_key retry；
- WaitingApproval 中 Steer 排队；
- WaitingApproval Steer只排队，不preempt Interaction；
- Tool side effect前完成resolution并重新检查Cancel state和current authorization；
- restart recovery使用幂等entries逐步关闭pending、abandon unknown并interrupt Turn；
- recovery 不生成 synthetic ToolResult；
- terminal projection 不含 Pending Interaction 或 Started Item；
- Tool-level failure 不自动把 Turn 标为 Failed；
- tool_round_completed、terminal和recovery entry 幂等（靠 committed prefix 状态判断——已 resolved/已 terminal 则跳过，不依赖 durable operation key）。

## 后续问题

1. Runtime pending Interaction query/snapshot/event payload。
2. Question option、secret answer 和 validation 的最终 wire schema。
3. Tool approval timeout 的 product default。
4. Reasoning summary retention、redaction 和 user visibility policy。
5. standalone compaction、review和background work是否产生Item。

## 设计进度

- [x] 固定 Turn initiating/terminal entry 边界。
- [x] 固定 Running/Completed/Interrupted/Failed semantics。
- [x] 固定 Steer、FollowUp 和 Interaction response 的区别。
- [x] 选择 operation-centric ToolInvocation Item。
- [x] 拒绝 sibling ToolCall/ToolResult Items。
- [x] 定义最小 ItemContent 和派生 ItemType。
- [x] 定义 ToolInvocation Started/Completed/Abandoned。
- [x] 定义 typed ToolResultDisposition。
- [x] 区分 Item、ToolCall 和 storage identity。
- [x] 定义 Interaction request/resolution family。
- [x] 定义 request-before-notify 和 resolution-before-resume。
- [x] 定义 reconnect/resend、timeout 和 transport loss。
- [x] 定义 WaitingApproval 与 Steer。
- [x] 定义 terminal cleanup 和 conservative recovery。
- [x] 区分 durable Item 与 transient observer delta。
- [x] 完成by-entry JSONL tree、Message/Event layout、EntryId、ToolRoundCompleted模型可见性规则和fork identity schema。
- [x] 完成SessionExecutor与private ToolExecutionControl/Interaction request处理流程。
- [ ] 完成 public Runtime protocol projection。
