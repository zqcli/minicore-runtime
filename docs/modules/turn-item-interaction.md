# Turn、Item 与 Interaction 架构设计

状态：当前权威架构（ADR 0124后，生产实现待启动）
日期：2026-07-29

## 目的

本文定义MiniCore的Turn、Item和Interaction领域模型：

- Turn开始、结束与Steer/FollowUp边界；
- UserMessage、AgentMessage、Reasoning和Tool work的Item投影；
- ToolCall、ToolResult和provider/tool关联；
- approval与结构化UserQuestion；
- streaming/progress与durable Item的区别；
- 宽容replay下incomplete Tool exchange和crash recovery。

## 同类产品结论

| 项目 | Identity与形状 | MiniCore采用点 |
| --- | --- | --- |
| Codex | Thread→Turn→ThreadItem，每个Item稳定ID，Tool/command状态更新同一Item | TurnId、ItemId、operation-centric Item |
| pi | entry tree；ToolCall在assistant message，ToolResult按ToolCallId关联 | 简单transcript correlation，不增加ToolRound marker |
| Gemini CLI | message ID与callId；scheduler更新同一ToolCall状态 | ToolCallId关联和model-input sanitization |
| OpenHands | EventID/parent、ActionID、tool_call_id、observation action_id | event/item identity与结果关联分离 |
| Claude Code | entry UUID/parent UUID、tool_use.id/tool_result.tool_use_id | tree identity与provider call identity并存 |

MiniCore保留显式Turn/Item/Interaction公开模型，但持久化关系向同类产品的简单call/result关联收敛。

## 决策摘要

- Turn从`source = Input`的UserMessage成功append开始；
- Input UserMessage内联`StoredTurnStart`，不再引用独立TurnContext entry；
- Turn以Final AssistantMessage、TurnInterrupted或TurnFailed结束；
- Steer属于同一Running Turn，FollowUp开启下一Turn；
- 最小ItemContent为UserMessage、AgentMessage、Reasoning、ToolInvocation；
- ToolCall与ToolResult属于同一个ToolInvocation Item；
- ToolInvocation状态为Started、Completed或Abandoned；
- Tool side-effect start只属于current Runtime执行状态，不是durable Item/event；
- ToolResult以`TurnId + ItemId + ToolCallId`关联ToolInvocation；
- 不建立ToolRound entity或ToolRoundCompleted event；
- live路径中，同一assistant response的全部ToolCall得到exactly one matching ToolResult后，完整exchange自动进入模型conversation；cold replay duplicate result first valid wins；
- incomplete/orphan/identity-conflicting或abandoned-first exchange在cold replay中隔离并告警；
- Interaction是Item-owned durable request/resolution；
- 一个Item可以顺序拥有多个Interaction；
- request append后notify，resolution append后wake/side-effect；
- streaming delta、Tool progress、retry和execution phase不是durable Item；
- restart不恢复old waiter/Tool task，旧Running Turn中断。

## 领域关系

```text
Session
└─ Turn*
   ├─ ordered Item*
   │  └─ Interaction*
   └─ Input UserMessage内联TurnStart metadata
```

```text
Session 1 ── 0..* Turn
Turn    1 ── 1..* Item
Item    1 ── 0..* Interaction
```

Turn/Item/Interaction集合由SessionStorage projection提供，不要求parent struct内联完整children。

## Identity

```text
TurnId
→ 一次用户意图执行、Cancel和Steer target

ItemId
→ MiniCore稳定语义Item和UI/progress update target

ToolCallId
→ provider/tool协议call/result correlation

RequestId
→ Interaction request/resolution correlation

EntryId
→ SessionStorage tree node

CommandId
→ 当前Runtime command correlation，不持久化
```

规则：

- identities不要求相等，不通用合并；
- 一个assistant entry可以产生多个Item；
- 一个ToolInvocation Item跨assistant、Interaction、ToolResult和terminal等多条entry；
- 一个Item可以顺序产生多个RequestId；
- ToolCallId由ModelGateway adapter按provider contract归一化：保留原生ID，缺失时生成response-local opaque ID；只要求同一assistant response内唯一，durable关联使用`TurnId + ItemId + ToolCallId`；
- Fork保留历史TurnId、ItemId、RequestId、EntryId和ToolCallId；public route始终携带SessionId；
- ordered Vec和new-Item StateEvent顺序是展示排序契约，ID/timestamp/completion time不用于排序。

## Turn

```rust
pub struct Turn {
    pub id: TurnId,
    pub session_id: SessionId,
    pub started_at: Timestamp,
    pub status: TurnStatus,
}

pub enum TurnStatus {
    Running,
    Completed { completed_at: Timestamp },
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

Turn不内联：

```text
Vec<Item>
PromptSet / ToolSet / SkillView
WorkspaceSnapshot
provider session / AgentLoop
pending waiter
```

live TurnExecutionContext持有exact immutable执行对象；durable Input UserMessage只保存安全历史metadata，cold replay不重建旧execution environment。

## Turn 边界

```text
candidate admission
→ capture TurnExecutionContext
→ compose Input UserMessage + StoredTurnStart
→ append/apply
→ Turn Running
→ zero or more message/event entries
→ append one terminal fact
```

terminal：

```text
Assistant(Final)
TurnInterrupted
TurnFailed
```

live writer拒绝一个Turn多个terminal。cold replay采用first valid terminal wins，后续冲突entry从Turn projection忽略并报告diagnostic。

## UserMessage、Steer 与 FollowUp

```rust
pub struct UserMessageItem {
    pub message: CanonicalUserMessage,
}
```

- Input开始Turn并内联TurnStart；
- Steer绑定expected Running Turn，不重新capture Context；
- FollowUp在旧Turnterminal后重新capture并写新Input；
- Interaction answer不创建UserMessage；
- raw input、未commit Steer和FollowUp queue内容不是Item。

## Item

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

`ItemType`和`ItemStatus`从content/state派生，不作为第二份可变事实。

### UserMessage

PromptSet规范化后的Input/Steer在append/apply后成为Completed Item。

### AgentMessage

```rust
pub struct AgentMessageItem {
    pub content: AgentMessageContent,
}
```

- provider partial output不是durable Item；
- finalized assistant entry可以产生一个AgentMessage Item和多个Reasoning/ToolInvocation Items；
- 无ToolCall Intermediate是model-visible Continue，不结束Turn；
- 含ToolCall Intermediate创建pending Tool exchange；
- Final不含ToolCall并完成Turn。

### Reasoning

```rust
pub struct ReasoningItem {
    pub summary: ReasoningSummary,
    pub content: Option<Arc<str>>,
}
```

只投影provider实际返回且policy允许展示的finalized reasoning artifact。hidden chain-of-thought、stream delta和abandoned retry draft不持久化。

### ToolInvocation

```rust
pub struct ToolInvocationItem {
    pub call: ToolCall,
    pub state: ToolInvocationState,
}

pub enum ToolInvocationState {
    Started,
    Completed { result: ToolResult },
    Abandoned { reason: ToolAbandonReason },
}
```

状态语义：

| 状态 | terminal | ToolResult | 可进入模型conversation |
| --- | --- | --- | --- |
| Started | 否 | 无 | 否 |
| Completed | 是 | 有 | 同一assistant exchange全部calls完成后 |
| Abandoned | 是 | 无 | 否 |

assistant ToolCall entry append/apply后Item成为Started。Tool执行、approval和current-Runtime start状态不创建额外Item。

## ToolResult

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

ToolResult保存为role=tool message并包含TurnId、ItemId和ToolCallId。

Completed可以来自：

```text
executor success
executor/schema/sandbox failure
policy/approval deny
cancelled-before-start
confirmed executor cancellation/timeout
```

Abandoned用于：

```text
side effect可能发生且outcome unknown
host restart后exact result不可恢复
required current-Runtime state丢失
```

Abandoned不生成synthetic ToolResult。

## Tool Side-Effect Start

**Canonical cross-module invariant: INV-401.** 索引见[架构总览](../architecture.md#跨模块不变量索引)。

Tool side-effect start是current Runtime的执行状态：

```rust
pub(crate) enum ToolOperationState {
    Prepared,
    Running,
    Settling,
    Terminal,
}
```

顺序：

```text
policy/approval/sandbox validation
→ observe EmergencyControl
→ owner-local start reservation
→ mark Running
→ call executor
→ exact ToolExecutionOutcome
```

不写`ToolExecutionStarted` ledger event。Cancel/SecurityRevoked在start reservation前获胜则Tool不执行；Running后获胜只能best-effort cancel并等待truthful result或Abandoned。

process crash可能遗漏side-effect start事实，MVP接受该限制；restart不会自动重放Tool。

## Complete Tool Exchange

一个assistant response内的ordered ToolCalls形成execution-local pending exchange：

```text
Assistant(intermediate, calls A/B/C) committed
→ ToolInvocation A/B/C Started
→ ToolResult按任意完成顺序append
→ A/B/C每个exactly one result
→ complete exchange进入conversation
```

不建立：

```text
ToolRound entity
ToolRoundId
ToolRoundCompleted event
assistant_entry_id/tool_entry_ids durable proof
```

conversation顺序固定为：

```text
assistant content原顺序
→ tool result A
→ tool result B
→ tool result C
```

物理结果append顺序和executor completion顺序不改变模型或UI的call order。

最后一个补齐集合的tool entry生成trusted `CommittedToolExchangeDelta`。SessionExecutor只把该delta交给AgentLoop；execution-local result vector不能直接推进模型conversation。

cold replay遇到missing、orphan或identity冲突时排除整个exchange并报告diagnostic；下一条合法User、Assistant、Compaction或Turn terminal会关闭尚未完成的exchange，之后到达的旧result视为orphan。duplicate result采用first valid wins并告警，不撤销已完成exchange。后续合法User/Assistant entry可以继续恢复。

## Item Lifecycle

```rust
pub enum ItemStatus {
    Started,
    Completed,
    Abandoned,
}
```

- UserMessage、AgentMessage和Reasoning创建时即Completed；
- ToolInvocation从Started到Completed或Abandoned；live writer拒绝Completed/Abandoned并存；
- cold replay对ToolResult与ToolAbandoned采用first valid terminal outcome wins并报告后续冲突；
- terminal Item不回到Started；
- live Turnterminal后不创建新Item；
- cold replay terminal后的冲突Item被忽略并告警；
- Turn terminal时未完成ToolInvocation可以在projection中显示Abandoned/Incomplete，不要求每个都存在durableAbandoned entry。

## Streaming 与 Observer Event

AgentMessage/Reasoning stream只属于process-local provisional view。

```rust
pub(crate) enum StreamingItem {
    AgentMessage { item_id: ItemId, text: String },
    Reasoning {
        item_id: ItemId,
        summary: Vec<String>,
        content: Vec<String>,
    },
}
```

- 首个visible delta分配稳定ItemId；
- started/delta走ProgressEvent；
- provider final产生同ItemId的FinalItemCandidate；
- append/apply后才发布ItemCompleted StateEvent；
- append失败、Cancel、retry或provider error丢弃provisional state；
- Host漏progress时由final StateEvent/Snapshot校正；
- Tool stdout/progress同样不持久化。

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

pub enum InteractionState {
    Pending,
    Resolved {
        resolution: InteractionResolution,
        resolved_at: Timestamp,
        resolution_key: IdempotencyKey,
    },
}
```

MiniCore拥有RequestId、durable request/resolution、Cancel、terminal cleanup和waiter resume。TUI/Web/GUI/RPC只是Presentation Adapter。

Presentation Adapter不能：

- 创建MiniCore没有请求的Pending Interaction；
- 用UserMessage代替resolution；
- 直接持有Tool future、SessionWriter或SessionExecutor；
- 自行推断timeout Deny。

## Interaction Family

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

规则：

- ToolApproval只由Tool-related Item触发；
- UserQuestion通常归属ask-user ToolInvocation；
- request/resolution family必须匹配；
- Cancelled可以关闭任意family；
- auto-allow/deny policy不创建Interaction；
- 一个Item可以顺序拥有多个Interaction；
- 一个request可以包含多个相关问题；
- domain允许多个Pending Interaction，MVP execution policy可以收紧为同一assistant step独占ask-user等待。

## Interaction Append Order

**Canonical cross-module invariant: INV-301.** 索引见[架构总览](../architecture.md#跨模块不变量索引)。

```text
construct typed request
→ append InteractionRequested
→ apply projection
→ notify host
→ wait
→ validate Session/Turn/Item/Request/family/resolution key
→ append InteractionResolved
→ apply projection
→ wake waiter / continue authorization
```

first committed terminal resolution wins：

- 相同resolution_key在当前run重试幂等返回；
- 不同key的第二次resolution返回AlreadyResolved；
- elapsed time不改变Pending；
- transport断开不产生默认resolution。

## Tool Approval

```text
Tool policy = Ask
→ ToolInvocation Item已Started
→ append approval request
→ host Allow/Deny
→ append resolution
→ re-observe Cancel/SecurityRevoked
→ Allow: start reservation + executor
→ Deny: PreExecution ToolResult
```

approval表示用户意愿，不替代Sandbox enforcement。

## UserQuestion

```rust
pub struct UserQuestionRequest {
    pub questions: Vec<UserQuestion>,
}

pub struct UserQuestionAnswer {
    pub answers: Vec<UserAnswer>,
}
```

首版由`ToolExecutionControl::request_user_question(item_id, request)`发起：

- 发生在file mutation ticket reservation和外部副作用前；
- 等待期间不持有mutation permit或TurnControl reservation；
- 同一assistant step的sibling ToolCall尚未启动；
- answer形成PreExecution ToolResult；
- matching results完整后exchange进入conversation；
- 其他Session继续独立执行。

## Reconnect 与 Transport Loss

```text
client disconnect
→ Interaction保持Pending

client reconnect
→ Snapshot返回相同RequestId
→ host重新展示
→ Resolve提交相同RequestId
```

- notification丢失不丢request；
- reconnect不创建新RequestId；
- 没有subscriber仍保持Pending；
- PrepareForUnload使用有限grace，deadline后Cancel Turn并关闭Pending；
- restart不恢复waiter，recovery best-effort写Cancelled resolution并中断Turn。

## Steer

Steer属于current Running Turn：

```text
WaitingApproval/WaitingForUserInput/ExecutingTools
→ Steer进入bounded FIFO
→ 不作为Interaction answer
→ current operation或complete Tool exchange结束
→ append one Steer
→ next model call
```

ToolCall step始终先等待complete exchange；无需额外ToolRound marker。

## Turn Terminal Cleanup

live terminal前SessionExecutor尽力关闭：

```text
Pending Interaction → Resolved(Cancelled)
Running Tool operation → exact result或Abandoned
Started但未运行ToolInvocation → Cancelled ToolResult或Abandoned
Turn → Final / Interrupted / Failed
```

宽容durable模型不要求这些事实形成事务。任一append OutcomeUnknown时writer poison，当前run停止继续写；下次load按实际可见prefix/suffix恢复。

## Crash Recovery

restart：

- 不恢复provider stream、AgentLoop、Tool task或Interaction waiter；
- replay保留existing User/Assistant/Tool/Interaction事实；
- complete Tool exchange继续model-visible；
- incomplete exchange从模型conversation排除；
- Pending Interaction best-effort Cancelled；
- Running Turn best-effort appendTurnInterrupted；
- 未完成ToolInvocation显示Abandoned/Incomplete；
- 不自动重放Tool、不合成ToolResult；
- 单条损坏不brick整个Session。

## Entries 与 Projection

一个ToolInvocation可以由以下entries投影：

```text
Assistant content: ToolCall
InteractionRequested*
InteractionResolved*
Tool message?
ToolAbandoned?
Turn terminal?
```

Tool side-effect start不进入ledger。完整exchange由assistant call集合与matching tool messages动态识别。

History/UI projection可以显示incomplete/orphan事实；model conversation只消费provider-valid complete exchange。这是同一storage的两个有意不同投影。

## Ownership

- SessionExecutor：Turn/Item/Interaction sequencing、start reservation、terminal arbitration；
- SessionStorage：strict append、tolerant replay和typed projections；
- ToolSet：validation/policy/approval/sandbox/executor；
- PromptSet：只消费sanitized committed conversation；
- Presentation Adapter：展示Interaction并提交resolution；
- AgentLoop：只推进NeedModel/NeedTools/Finished协议状态。

不建立InteractionService、ToolRoundService、ItemManager或第二writer。

## Error 与 Race 分类

### Append

- NotCommitted：可retry同一draft；
- OutcomeUnknown：poison writer，停止当前run继续写；
- cold replay按实际文件恢复并报告diagnostic。

### Tool start vs Cancel

- emergency先被owner观察：Tool不启动；
- start reservation先完成：Tool可以运行，Cancel只best-effort；
- 不依赖durableToolExecutionStarted判定该race。

### Tool result

- exact result已知但append失败：不重新执行Tool；
- duplicate result live append拒绝；
- duplicate result cold replay采用first valid wins；后续duplicate忽略并产生diagnostic；
- outcome unknown：Abandoned，无synthetic result；cold replay若同时存在result，first valid terminal outcome wins。

### Interaction

- response与terminal cleanup first committed wins；
- stale Turn/Item/Request返回typed error；
- wrong family拒绝；
- reconnect重复相同resolution_key幂等。

## 基础不变量

- Session→Turn→Item→Interaction关系保持；
- TurnId、ItemId、RequestId、EntryId、ToolCallId职责分离；
- Fork保留历史ID；
- Input开始Turn，Final/Interrupted/Failed结束Turn；
- Steer不开始新Turn，FollowUp开始新Turn；
- ToolCall与ToolResult属于同一ToolInvocation Item；
- side-effect start是current-Runtime状态；
- complete exchange才进入模型conversation；
- incomplete exchange隔离并告警；
- Interaction request-before-notify、resolution-before-wake；
- transport断开不自动Deny；
- streaming/progress不是durable Item；
- recovery不重放Tool、不合成ToolResult；
- cold replay局部损坏不brick Session。

## Test Matrix

至少覆盖：

- Input/Steer/FollowUp边界；
- Turn first terminal wins；
- AgentMessage/Reasoning streaming ItemId稳定；
- ToolInvocation Started→Completed/Abandoned；
- ItemId与ToolCallId独立；
- 多ToolCall结果逆序完成但UI/model顺序保持call order；
- 最后一个matching ToolResult产生CommittedToolExchangeDelta；
- missing/orphan ToolResult隔离，duplicate first valid wins；
- 下一条合法User、Assistant、Compaction或Turn terminal关闭incomplete exchange，迟到result成为orphan；
- incomplete exchange后的后续conversation可恢复；
- start reservation与Cancel双向race；
- start后exact outcome/Abandoned settlement；
- ToolApproval allow/deny；
- UserQuestion多问题单request；
- 一个Item顺序多个Interaction；
- family mismatch与stale identity；
- disconnect/reconnect相同RequestId；
- response与terminal cleanup first-wins；
- restart Pending/Running/incomplete Tool recovery；
- Fork保留全部历史ID；
- malformed中段entry只影响局部projection。

## 明确不建立

```text
ToolRound entity / ToolRoundId / ToolRoundCompleted
ToolExecutionStarted durable event
ToolCall和ToolResult sibling Items
InteractionService
ItemManager
StartedItem/DeltaItem durable variants
DisplaySequence
RunId / ModelStepId
```

## 开放问题

实现阶段冻结：

1. public ID UUID格式；
2. Item/Interaction wire casing；
3. replay中`Incomplete`与`Abandoned` UI文案；
4. simultaneous Pending Interaction的MVP execution上限。
