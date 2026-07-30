# Turn、Item 与 Interaction 架构设计

状态：当前权威架构（ADR 0126后，生产实现待启动）
日期：2026-07-30

## 目的

本文定义live Turn、Item、ToolInvocation、complete Tool exchange、Interaction、streaming、terminal cleanup与cold replay projection。

## 领域关系

```text
Session
└─ Turn*
   └─ Item*
      └─ Interaction*
```

Turn/Item/Interaction首先存在于`LiveSessionState`，随后由SessionRecorder inline best-effort append。StateEvent表示current-process state，不表示flush、fsync或power-loss durability。

## Turn

```rust
pub enum TurnStatus {
    Running,
    Completed,
    Interrupted,
    Failed,
}
```

Turn开始：

```text
Input validated/composed
→ apply live UserMessage + Turn Running
→ await inline record Input/StoredTurnStart attempt
→ publish TurnStarted
→ ActiveTurnTask begins
```

Turn结束：

```text
apply live Final Assistant or terminal event
→ await inline record attempt
→ publish terminal StateEvent
→ ActiveTurnTask returns outcome
```

recording失败不改变live TurnStatus。一个Session同时最多一个Running Turn。

## Item

```rust
pub struct Item {
    pub item_id: ItemId,
    pub turn_id: TurnId,
    pub content: ItemContent,
    pub status: ItemStatus,
}
```

```rust
pub enum ItemContent {
    UserMessage(UserMessageView),
    AgentMessage(AgentMessageView),
    Reasoning(ReasoningView),
    ToolInvocation(ToolInvocationView),
}
```

```rust
pub enum ItemStatus {
    Started,
    Completed,
    Abandoned,
}
```

Item顺序由live conversation顺序和assistant content顺序决定；不增加DisplaySequence。

- UserMessage、final AgentMessage和Reasoning在live final mutation时Completed；
- ToolInvocation由assistant ToolCall创建为Started；
- truthful ToolResult使其Completed；
- unknown/cancelled settlement可以使其Abandoned；
- terminal Item不能回到Started。

## Streaming

Model stream只产生process-local provisional state：

```rust
pub(crate) enum StreamingItem {
    AgentMessage { item_id: ItemId, text: String },
    Reasoning { item_id: ItemId, summary: Vec<String>, content: Vec<String> },
}
```

- 首个visible delta分配stable ItemId；
- started/delta走ProgressEvent；
- provider final构造live final mutation；
- live mutation后完成inline record attempt，再发布ItemCompleted StateEvent；
- retry、Cancel或provider error丢弃provisional buffer。

Host可能先看到streaming，随后进程crash且没有任何recorded final Item。

## ToolCall Identity

```rust
pub struct ToolCall {
    pub item_id: ItemId,
    pub tool_call_id: ToolCallId,
    pub name: ToolName,
    pub arguments: JsonValue,
}
```

ToolCallId由ModelGateway adapter归一化：provider有native ID时保留，没有时生成response-local opaque ID。同一assistant response内必须唯一。

live与recorded correlation都使用：

```text
TurnId + ItemId + ToolCallId
```

ToolCallId不替代ItemId，也不要求Session-global唯一。

## Tool Side-Effect Start

**Canonical cross-module invariant: INV-401.**

```rust
pub(crate) enum ToolOperationState {
    Prepared,
    Running,
    Settling,
    Terminal,
}
```

```text
policy/approval/sandbox validation
→ observe EmergencyControl epoch
→ ToolStartGate owner-local reservation
→ state Running
→ invoke executor
→ exact ToolExecutionOutcome
```

start reservation前Cancel/SecurityRevoked获胜时Tool不执行。Running后获胜只能best-effort cancel并truthful settle或Abandoned。

SessionRecorder不参与Tool start，不记录`ToolExecutionStarted`。process crash可能丢失side-effect事实，restart不自动重跑Tool。

## Complete Tool Exchange

**Canonical cross-module invariant: INV-003.**

一个assistant response的ordered ToolCalls形成live pending exchange：

```text
Assistant(intermediate, calls A/B/C) applied live
→ await inline record assistant attempt
→ Tool A/B/C run
→ results arrive in arbitrary completion order
→ each result applied live + await inline record attempt
→ A/B/C each have first truthful ToolResult
→ expose ordered Assistant + Result A/B/C to LiveConversationView
→ next Model allowed
```

Live reducer拥有：

- expected ordered call set；
- first terminal outcome per call；
- duplicate/cross-Turn/identity mismatch rejection；
- complete/incomplete状态；
- provider-valid model message ordering。

Assistant response含ToolCall时，完整exchange形成前，assistant和results都不能进入下一次Model input。UI仍可以显示Started/Completed Tool Item。

以下exchange不能进入模型conversation：

- missing result；
- orphan result；
- abandoned-first；
- mismatched Turn/Item/ToolCall identity；
- duplicate/conflicting result造成的invalid live mutation。

live writer permit和`CommittedToolExchangeDelta`已删除。complete gate由async loop直接读取live reducer状态。

### Cold replay

Replay对旧或损坏记录采用：

- duplicate result first valid wins并告警；
- ToolResult/ToolAbandoned conflict first terminal wins；
- 下一条合法User、Assistant、Compaction或Turn terminal关闭incomplete exchange；
- closure后迟到result视为orphan；
- incomplete exchange排除后，后续合法conversation仍可恢复。

## Tool Outcome

```rust
pub enum ToolExecutionOutcome {
    Completed(ToolResult),
    PreExecutionFailed(PreExecutionToolError),
    CancelledBeforeStart,
    Abandoned(ToolAbandonedReason),
}
```

- validation、policy deny、approval deny或sandbox deny产生truthful pre-execution result；
- 已可能发生副作用的Tool不自动retry；
- record failure不能触发Tool re-execution；
- Abandoned不生成synthetic ToolResult供模型继续。

## Interaction

```rust
pub struct Interaction {
    pub request_id: RequestId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub item_id: ItemId,
    pub request: InteractionRequest,
    pub state: InteractionState,
}
```

```rust
pub enum InteractionState {
    Pending,
    Resolved {
        resolution: InteractionResolution,
        resolved_at: Timestamp,
        resolution_key: IdempotencyKey,
    },
}
```

```rust
pub enum InteractionRequest {
    ToolApproval(ToolApprovalRequest),
    UserQuestion(UserQuestionRequest),
}

pub enum InteractionResolution {
    ToolApproval(ToolApprovalDecision),
    UserAnswer(UserQuestionAnswer),
    Cancelled(InteractionCancelReason),
}
```

MiniCore拥有RequestId、live state、Cancel、terminal cleanup和waiter routing。Presentation Adapter只展示并提交typed resolution。

## Interaction Ordering

**Canonical cross-module invariant: INV-301.**

```text
construct typed request
→ apply Pending Interaction live
→ await inline record InteractionRequested attempt
→ publish InteractionView
→ await oneshot
→ validate Turn/Item/Request/family/key
→ apply resolution live
→ await inline record InteractionResolved attempt
→ resume waiter / start protected side-effect
```

notify/resume等待当前inline append attempt返回；recording failure转为Degraded后仍继续Interaction。successful write不表示flush或fsync。

first live terminal resolution wins：

- same resolution_key在current run幂等；
- different key返回AlreadyResolved；
- transport断开和elapsed time不自动resolve；
- Cancel可以显式关闭任意family。

crash可能发生：

- request已展示但未record；
- resolution已生效但未record；
- request record存在但resolution缺失。

restart不恢复waiter；recorded Pending Interaction投影为cancelled/interrupted recovery view。

## Tool Approval

```text
Tool policy = Ask
→ InteractionRequested(ToolApproval)
→ await resolution
├─ AllowOnce / AllowWith → revalidate ToolStartGate → execute
├─ Deny → PreExecution ToolResult
└─ Cancelled → Cancelled/Abandoned settlement
```

approval只对当前call有效。MVP不持久化Session/Turn grant。

## UserQuestion

Ask-user Tool使用Interaction：

```text
assistant asks ask-user ToolCall
→ live ToolInvocation Started
→ InteractionRequested(UserQuestion)
→ await answer
→ answer converted to truthful ToolResult
→ complete exchange
→ next Model
```

UserAnswer不是UserMessage，不创建新Turn，也不进入Steer queue。

## Cancel与Terminal Cleanup

Cancel/SecurityRevoked后：

- Pending Interaction live-resolve为Cancelled；
- Prepared Tool不启动；
- Running Tooltruthful settle；
- incomplete exchange不进入model conversation；
- apply live TurnInterrupted；
- await inline record各terminal facts；
- publish final StateEvent；
- recording failure不阻止settlement，正常本地append latency由同Session settlement承担。

## Recording与Replay差异

| 行为 | Live execution | Cold replay |
| --- | --- | --- |
| duplicate ToolResult | reject mutation | first valid wins + diagnostic |
| invalid Interaction family | reject resolution | isolate invalid relation |
| recording failure | continue live | 未record tail不可见 |
| incomplete exchange | block next Model | exclude from model view |
| pending waiter | await current oneshot | never restore |
| terminal missing | live task仍可结束 | mark restart interruption |

## UI与Snapshot

SessionSnapshot包含live Turn、Items、Pending Interaction和phase。recording Degraded或process crash时，它可以领先可恢复的recorded prefix。StateEvent不作为flush/fsync receipt。

ProgressEvent可以丢弃；Snapshot和final StateEvent校正当前进程view。restart后的Snapshot只能基于recorded prefix重新构建。

## 测试要求

- Turn/Item lifecycle；
- streaming finalization和retry cleanup；
- ToolCall ID uniqueness/correlation；
- parallel result completion与canonical order；
- incomplete/abandoned/duplicate/conflicting exchange；
- ToolStartGate vs Cancel/SecurityRevoked；
- Interaction request/resolution ordering与idempotency；
- recording degraded时Tool/Interaction继续；
- crash造成request/result/terminal缺失后的replay；
- Cancel settlement与FollowUp handoff。
