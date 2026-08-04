# Turn、Item 与 Interaction 架构设计

状态：当前权威架构（ADR 0134后，生产实现待启动）
日期：2026-07-31

## 目的

本文定义live Turn、Item、ToolInvocation、complete Tool exchange、Interaction、streaming、terminal cleanup与cold replay projection。

## 领域关系

```text
Session
└─ Turn*
   └─ Item*
      └─ Interaction*
```

Turn/Item/Interaction首先存在于`LiveSessionState`。SessionRecorder只inline best-effort append稳定conversation facts；TurnStatus和terminal reason不进入JSONL。StateEvent表示current-process state，不表示flush、fsync或power-loss durability。

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
→ await inline record Input UserMessage attempt
→ publish TurnStarted
→ ActiveTurnTask begins
```

Turn结束：

```text
Completed: apply Final Assistant + live TurnCompleted
→ await inline record Final Assistant attempt
→ publish ItemCompleted + TurnCompleted

Interrupted/Failed: settle recordable Tool/Interaction facts
→ apply live terminal
→ publish TurnInterrupted / TurnFailed
→ ActiveTurnTask returns outcome
```

recording失败不改变live TurnStatus。一个Session同时最多一个Running Turn。restart后不恢复旧TurnStatus，recorded TurnId只用于conversation grouping。

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

公开`ItemView`是owner-produced UI-safe projection，不序列化private MessageRecord、Tool arguments或reasoning artifact：

```rust
pub struct ItemView {
    pub item_id: ItemId,
    pub turn_id: TurnId,
    pub status: ItemStatus,
    pub content: ItemContentView,
    pub created_at: Timestamp,
    pub completed_at: Option<Timestamp>,
}

pub enum ItemContentView {
    UserMessage(UserMessageView),
    AgentMessage(AgentMessageView),
    Reasoning(ReasoningView),
    ToolInvocation(ToolInvocationView),
}

pub struct UserMessageView {
    pub source: UserMessageSource,
    pub body: Option<String>,
    pub contributions: Arc<[PromptContributionOrigin]>,
}

pub enum UserMessageSource {
    Input,
    Steer,
}

pub struct AgentMessageView {
    pub disposition: AssistantDisposition,
    pub text: Arc<[String]>,
}

pub enum AssistantDisposition {
    Intermediate,
    Final,
}

pub struct ReasoningView {
    pub summaries: Arc<[String]>,
}

pub struct ToolInvocationView {
    pub tool_call_id: ToolCallId,
    pub tool_name: ToolName,
    pub arguments_summary: String,
    pub result: Option<ToolResultSummaryView>,
}

pub struct ToolResultSummaryView {
    pub disposition: ToolResultDisposition,
    pub summary: String,
}
```

rules：

- `UserMessageSource::Input`表示创建Turn的initiating message；direct Submit和从FollowUp FIFO真正admit的新Turn都投影为Input。`Steer`只表示在same Running Turn safe point apply的message；
- `AssistantDisposition::Intermediate`表示same Turn仍需Tool exchange、queued Steer或下一次Model；`Final`只表示该assistant fact与live TurnCompleted同一decision形成；Interrupted/Failed不合成Final；

- Item read model的UserMessageSource/AssistantDisposition/ToolResultDisposition closed variants与live/storage projection一致；
- User body只返回用户显式body；Skill/Workspace contribution返回safe origin，不返回注入正文、absolute path或authorization；
- AgentMessage只返回user-visible finalized Text；opaque/hidden reasoning不混入；
- Reasoning只返回provider允许display的summary，不返回encrypted/signature/hidden chain-of-thought；
- Tool arguments/result由Tool-owned redaction policy产生bounded summary，不返回prepared args、raw details、credential或sandbox internals；
- public summary construction失败时使用typed redacted placeholder，不fallback raw payload；
- string/count/aggregate limits和serde shape由[Wire Schema](wire-schema.md#protocollimits-v10)拥有；

Item顺序由live conversation顺序和assistant content顺序决定；不增加DisplaySequence。

- UserMessage、final AgentMessage和Reasoning在live final mutation时Completed；
- ToolInvocation由assistant ToolCall创建为Started；
- truthful ToolResult使其Completed；
- possible side effect的exact outcome未知时使其Abandoned；cancel-before-start有truthful Cancelled ToolResult，因此使其Completed；
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

`ToolCall`与`ToolExecutionRequest`的唯一canonical shape由[Tools](tools.md#toolcallinvocation-和-result)拥有。canonical ToolCall保存`ToolCallId + ToolName + arguments + call_index`，不保存ItemId。ActiveTurnTask在assistant response live apply时为每个call分配ItemId，建立Item projection并构造携带`ItemId + Arc<ToolCall>`的execution request。

ToolCallId由ModelGateway adapter归一化：provider有native ID时保留，没有时生成response-local opaque ID。同一assistant response内必须唯一。

live与recorded correlation都使用：

```text
TurnId + ItemId + ToolCallId
```

ToolCallId不替代ItemId，也不要求Session-global唯一。

## Tool Side-Effect Start

**Canonical cross-module invariant: INV-401.**

唯一`ToolOperationSlot`类型及其`Prepared → Running → Settling → Terminal`状态由[Session Execution](session-execution.md#tool-operation-slot)拥有。本节只拥有INV-401的跨模块语义和Item投影，不定义第二个operation state enum。

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

`ConversationRevision`按[Conversation Storage的exact delta matrix](conversation-storage.md#conversationrevision)计数，而不按当前可见messages计数：accepted ToolCall Assistant在仍被complete gate隐藏时即`+1`；只有全部expected calls都有first truthful `Completed` result、整个exchange promotion时再`+1`。partial terminal、Abandoned或其他未进入模型的settlement均为`+0`。这使同一hidden exchange的先后accepted/projection operation不能被in-flight request误当成同一basis。

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
- 下一条合法User、Assistant或Compaction关闭incomplete exchange；EOF处仍未完成的exchange直接排除；
- closure后迟到result视为orphan；
- incomplete exchange排除后，后续合法conversation仍可恢复。

## Tool Outcome

`ToolExecutionOutcome`的唯一canonical enum由[Tools](tools.md#toolcallinvocation-和-result)拥有：`Completed { source = PreExecution | Executed, result } | Abandoned { ... }`。

- validation、policy deny、approval deny、sandbox deny或cancel-before-start都产生matching truthful pre-execution ToolResult；
- 已可能发生副作用的Tool不自动retry；
- record failure不能触发Tool re-execution；
- Abandoned不生成synthetic ToolResult供模型继续。

## Interaction

```rust
pub(crate) struct Interaction {
    request_id: RequestId,
    turn_id: TurnId,
    item_id: ItemId,
    request: InteractionRequest,
    state: InteractionState,
}
```

```rust
enum InteractionState {
    Pending,
    Resolved {
        resolution: ResolvedInteraction,
        resolution_key: Option<InteractionResolutionKey>,
    },
}

pub struct InteractionResolutionKey([u8; 16]);
```

`LiveSessionState`已绑定唯一SessionId，私有`Interaction`不重复保存该事实；resolution的exact timestamp属于同一条`StoredSessionEntry`，Interaction state只保留当前resolution与幂等key。

```rust
pub(crate) enum InteractionRequest {
    ToolApproval(ToolApprovalRequest),
    UserQuestion(UserQuestionRequest),
}

pub(crate) enum InteractionResolution {
    ToolApproval(ToolApprovalDecision),
    UserAnswer(UserQuestionAnswer),
    Cancelled(InteractionCancelReason),
}

pub(crate) struct ResolvedInteraction {
    live: InteractionResolution,
    view: InteractionResolutionView,
}

impl ResolvedInteraction {
    pub(crate) fn live(&self) -> &InteractionResolution;
    pub(crate) fn view(&self) -> &InteractionResolutionView;
}

pub enum InteractionCancelReason {
    HostCancelled,
    TurnCancelled,
    SecurityRevoked,
    SessionUnloaded,
    RuntimeClosing,
    TurnTerminal,
}
```

`InteractionResolutionKey`的bytes由Presentation Adapter使用CSPRNG生成；field保持private，public constructor校验exact 16 bytes，Debug/Display不得输出raw value；wire使用[ADR 0134](../adr/0134-public-and-conversation-wire-use-bounded-v1-schemas.md)的`irk_<32 hex>`。

`InteractionCancelReason`是live/storage-safe closed taxonomy；subscriber disconnect、elapsed time和user silence没有variant，因为它们不自动resolve。public `InteractionResolutionInput::Cancelled`映射HostCancelled；其他variants只能由对应control/lifecycle owner产生。[Format V1](../formats/conversation-jsonl-v1.md#interaction-resolved)保存exact reason与optional host resolution key。

`resolution_key = Some`只用于host `InteractionCommand::Resolve`，包括public `Cancelled → HostCancelled`；Cancel/SecurityRevoked/Unload/terminal owner-driven closure使用`None`，只能产生对应的non-Host cancellation reason，并由single owner first-wins保证。key不授权Tool execution，只提供exact public resolution retry去重。

`Interaction`的fields和`InteractionState`都是`LiveSessionState` owner-module private state。只有`LiveSessionState::apply_interaction_request()`构造Pending Interaction，且只有`LiveSessionState::apply_interaction_resolution()`完成first-wins resolution/terminal transition；不存在crate-wide `Interaction` constructor、`state()` getter、mutable reference或可match的raw state enum。sibling module只能通过`CapturedConversationViews`中的`PendingInteractionFact`和public-safe event/storage view读取需要的事实，不能mutate或match raw Interaction state。

`InteractionRequest`和`ResolvedInteraction`是owner-private executable/live values：前者可含ToolApproval private option→PermissionSet map；后者是**opaque struct**，private `InteractionResolution`保留ToolApproval decision、UserAnswer或Cancelled reason，同时持有safe `InteractionResolutionView`。它不是enum，也不能由storage/public caller destructure或重建。它只经narrow safe `view()`与owner-only private-live `live()` access进入对应owner；它及其view的`Debug`都必须redact resolution payload、private permission与key material。live reducer从owner value导出safe `StoredInteractionRequest`/`StoredInteractionResolution`和event projection，但不接受caller预先投影的safe/durable body。resolved state保留exact private `ResolvedInteraction`，以便only owner routes the waiter；safe durable resolution不反向恢复private authorization.

MiniCore拥有RequestId、live state、Cancel、terminal cleanup和waiter routing。Presentation Adapter只展示并提交typed resolution。

每个Item最多同时有一个`Pending` Interaction；这不是“一生只能一个Interaction”：该request以terminal resolution结束后，同一Item可以顺序创建后续Interaction。request和resolution都是model-invisible live mutation，`ConversationRevision`一律`+0`；failed或same-key/same-payload idempotent resolution同样不改变revision。

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

- Presentation Adapter为每一次逻辑Resolve生成不可预测random 128-bit `InteractionResolutionKey`，retry exact same canonical resolution时复用；
- key scope固定为`SessionId + TurnId + ItemId + RequestId`，不能跨request/session复用；
- same key + same canonical resolution在current run幂等返回`InteractionResolved`，不产生第二live mutation、record或event；
- same key + different canonical resolution返回`CommandConflict`；
- different key在Interaction已经Resolved后返回`InteractionAlreadyResolved`；
- internal Cancel/SecurityRevoked/Lifecycle closure没有public key；
- transport断开和elapsed time不自动resolve；
- Cancel可以显式关闭任意family。

crash可能发生：

- request已展示但未record；
- resolution已生效但未record；
- request record存在但resolution缺失。

restart不恢复waiter，也不把recorded request投影为active Pending Interaction。缺少matching resolution的request保留为historical fact/diagnostic，不合成Cancelled resolution或Turn terminal。

## Tool Approval

```text
Tool policy = Ask
→ InteractionRequested(ToolApproval)
→ await resolution
├─ AllowOnce / AllowWith → revalidate ToolStartGate → execute
├─ Deny → PreExecution ToolResult
└─ Cancelled before start → PreExecution Cancelled ToolResult
```

approval只对当前call有效。MVP不持久化Session/Turn grant。

## UserQuestion

**Canonical cross-module invariant: INV-302.**

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

UserAnswer不是UserMessage，不创建新Turn，也不进入Steer queue。MVP UserQuestion只允许[Tools定义的non-secret Text/SingleChoice fields](tools.md#approval-and-question-types)。question/answer完整值可以进入live state、JSONL、Interaction event/history和PreExecution ToolResult，并可能发送给模型；不得用于credential/password/token收集。任何future secret input必须建立独立secure host port与non-recorded one-time reference，不能只给当前request增加`secret: true`。

## Cancel与Terminal Cleanup

Cancel/SecurityRevoked后：

- Pending Interaction live-resolve为Cancelled；
- Prepared Tool不启动并生成matching PreExecution Cancelled ToolResult；
- Running Tooltruthful settle；
- incomplete exchange不进入model conversation；
- apply live TurnInterrupted；
- 完成recordable Tool/Interaction settlement facts的inline record attempts；
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
| Turn terminal | publish current-process StateEvent | never record or reconstruct |

## UI与Snapshot

SessionSnapshot包含live Turn、Items、Pending Interaction和phase。recording Degraded或process crash时，它可以领先可恢复的recorded prefix。StateEvent不作为flush/fsync receipt。

M4 reader只使用`LiveSessionState::capture_conversation_views()`返回的crate-private `CapturedConversationViews`：同一revision的conversation/source/derived selected head/relations，以及`PendingInteractionFact { RequestId, TurnId, ItemId, safe InteractionRequestView }` array。aggregate不暴露完整path；state仍保留exact `Arc<StoredSessionEntry>` path给future LiveSnapshot/Fork。它不是M8 public Item/Interaction DTO、Runtime protocol activation或Fork LiveSnapshot。M4只需保持上述“一Item至多一个Pending、terminal后可顺序再请求”的live invariant，公开view、events与host interaction routing仍由M8关闭。

ProgressEvent可以丢弃；Snapshot和final StateEvent校正当前进程view。restart后的Snapshot只能基于recorded prefix重新构建。

## 测试要求

- Turn/Item lifecycle；
- ItemView只含user body/safe contribution origin、visible assistant text、reasoning summary和redacted Tool summary；
- direct Submit/FollowUp admission投影Input、same-Turn Steer投影Steer；Intermediate/Final只按live continuation/terminal decision产生；
- ToolResult Succeeded/Failed/Denied/Cancelled在Item、conversation和replay projection一致；
- streaming finalization和retry cleanup；
- ToolCall ID uniqueness/correlation；
- Tools-owned ToolCall/Outcome和Session-owned ToolOperationSlot没有第二份type definition；
- unknown/schema-invalid/approval-deny/Sandbox-unavailable/cancel-before-start均闭合为matching ToolResult；
- parallel result completion与canonical order；
- incomplete/abandoned/duplicate/conflicting exchange；
- ToolStartGate vs Cancel/SecurityRevoked；
- Interaction fields/raw state只可由LiveSessionState transition methods construct/resolve；sibling只能读取safe Pending/event/storage projection，不能mutate或match raw state；
- `InteractionResolutionCandidate::host`仅接受ToolApproval/UserAnswer/HostCancelled且seal Some key，`owner_cancellation`仅接受non-Host Cancelled且seal None；wrong origin在reducer/EntryId allocation前以redacted candidate error拒绝；
- 同一Item拒绝第二个concurrent Pending Interaction，但terminal resolution后允许顺序later interaction；Interaction mutation与idempotent/failing resolution均保持revision `+0`；
- StoredInteraction request/resolution format-v1 round-trip、family/reason/key relation和unknown variant isolation；
- InteractionCancelReason只允许Host/Turn/Security/Unload/Runtime/Terminal owner causes，silence/disconnect不产生reason；
- non-secret Text/SingleChoice validation，secret/password variant不可构造；
- recording degraded时Tool/Interaction继续；
- crash造成request/result缺失后的replay，且不合成Turn terminal；
- Cancel settlement与FollowUp handoff。
