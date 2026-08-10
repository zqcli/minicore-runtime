# Turn Execution Context 架构设计

状态：当前权威架构（M7 capture及M10 validated settings、planning、CompactionSummary assembly façade与async orchestration已实现）
日期：2026-07-31

## 目的

本文定义一次Turn的immutable execution capture，以及ActiveTurnTask使用Prompt、Model、Tool、Interaction、Steer和Compaction时必须保持的exact binding。

```text
Turn
→ domain lifecycle，live并可best-effort record

TurnExecutionContext
→ 当前Runtime内一次Turn捕获的immutable execution objects
```

## 决策摘要

- Turn admission一次性capture exact Agent/Session/Workspace/Prompt/Skill/Tool/Model对象；
- PromptSet只包含candidate build期间已经materialize并由强Arc持有的PromptContent，不携带source resolver；
- active Turn不读取future current roots；
- private constructor阻止跨capture拼接；
- Input/Steer通过唯一async `resolve_user_message()`解析captured Skill contribution；
- ActiveTurnTask直接运行async Model→Tool→Model loop；
- 不再存在同步AgentLoop或committed delta interface；
- PromptSet从sanitized `LiveConversationView`组装模型输入；
- `ModelCallRequest`由ModelGateway唯一拥有，Context只调用其private constructor；
- `ConversationRevision`是current-process monotonic live-conversation operation basis，而非当前可见消息的hash/version；
- SessionRecorder不参与context correctness；
- Steer复用同一TurnExecutionContext；
- FollowUp创建新Turn和新Context；
- restart不恢复旧Context或ActiveTurnTask。

## Context Capture

**Canonical cross-module invariant: INV-201.**

```text
Session current definition
+ exact AgentRevision
+ current Workspace resolution
+ MiniCoreRuntime captured SharedResourceRoots
+ candidate TurnId

→ ModelGateway.resolve_for_turn
→ create Arc<SkillViewContext>
→ SkillService.for_turn(resources, context.clone())
→ ToolService.for_turn
→ PromptService.for_turn
→ Arc<TurnExecutionContext>
```

所有对象必须来自同一次admission capture。Runtime-global validated `CompactionSettings`也在该点clone为immutable `CompactionSettingsSnapshot`；MVP不在active Turn中读取current config、per-Session override或hot-reloaded policy。capture完成后：

- Agent新revision不影响active Turn；
- SessionDefinition update只在Idle并只影响future Turn；
- shared `/reload`只替换future capture roots；
- Workspace hard restriction通过SecurityRevoked中断Turn，不热替换Context；
- provider fallback不会在active Turn内重新resolve另一模型。

PromptService在该capture之前已经完成Prompt source读取、解析和materialization。TurnExecutionContext、PromptSet和ActiveTurnTask都不能持有path/URL/content key并在后续`resolve_user_message()`或`assemble_*()`中解析Prompt正文；Skill lazy parse只消费SkillView entry captured bytes。cache eviction不能使captured PromptSet失效。

## TurnExecutionContext

```rust
pub(crate) struct TurnExecutionContext {
    session_id: SessionId,
    turn_id: TurnId,
    session_revision: SessionDefinitionRevision,
    agent: AgentRevisionRef,
    model: Arc<TurnModelSnapshot>,
    workspace: Arc<WorkspaceSnapshot>,
    skill_service: Arc<SkillService>,
    skill_view_context: Arc<SkillViewContext>,
    skill_view: Arc<SkillView>,
    tool_set: Arc<ToolSet>,
    prompt_set: Arc<PromptSet>,
    compaction: CompactionSettingsSnapshot,
    diagnostics: Arc<[TurnContextDiagnostic]>,
}
```

字段private，只提供窄方法：

```rust
impl TurnExecutionContext {
    pub(crate) async fn resolve_user_message(
        &self,
        intent: PromptIntent,
    ) -> Result<CanonicalUserMessage, PromptError>;

    pub(crate) fn assemble_agent_run(
        &self,
        conversation: &LiveConversationView,
    ) -> Result<Arc<AssembledModelContext>, PromptError>;

}
```

M10 adds the following crate-private extension; it is not M4's Context surface:

```rust
impl TurnExecutionContext {
    pub(crate) fn compaction_pressure(
        &self,
        source: &LiveCompactionSourceView,
        trigger: CompactionTrigger,
        compactions_started: u8,
    ) -> CompactionPressure;

    pub(crate) fn plan_compaction(
        &self,
        source: Arc<LiveCompactionSourceView>,
        trigger: CompactionTrigger,
        compactions_started: u8,
    ) -> Result<Arc<CompactionPlan>, CompactionError>;

    pub(crate) fn assemble_compaction(
        &self,
        plan: &Arc<CompactionPlan>,
    ) -> Result<Arc<AssembledModelContext>, PromptError>;
}
```

Context不拥有：

- LiveSessionState或SessionRecorder；
- ActiveTurnTask/control actor；
- Session ingress/work queue、Interaction waiter或provider CancellationToken；Session-local file mutation queue只经captured ToolSet窄接口使用；
- provider stream；
- mutable Workspace/Prompt/Tool/Skill root；
- public event publisher。

`resolve_user_message()`是Input与Steer共享的唯一async composition seam。它在第一次await前拒绝duplicate SkillId，只从该Context捕获的SkillView按SkillId取得entry，调用同一次Runtime注入的SkillService解析entry captured bytes，再由SkillInjector生成ordered contributions；全部required Skill/Workspace contribution成功后才构造private `UserMessageCompositionInput`并同步调用PromptSet。缺失、stale、source mismatch、load/injection或required contribution失败统一映射为typed PromptError，不apply任何部分UserMessage。PromptSet成功后只保留safe part-level stamps；Context不把source authorization交给Recorder。该路径消费[INV-202](../architecture.md#跨模块不变量索引)。

SkillView私有绑定`skill_view_context`；TurnExecutionContext private constructor要求该binding与字段中的exact Arc相同。`skill_service`只用于`load(&skill_view, entry).await`，不能build future view、查询current root或按path重读正文。Context不自行判断candidate/active Turn是否仍current；Session Execution在await前后拥有control与revalidation。

## Turn Admission

```text
SessionExecutor reserves candidate TurnId/control_generation
→ install Submit CommandId emergency target
→ capture TurnExecutionContext
→ await context.resolve_user_message(Input intent) in Starting select loop
→ revalidate candidate/control/emergency/authority basis
→ LiveSessionState validates/projects start candidate + `ConversationRevision::checked_next()`
→ allocates EntryId + binds parent_id
→ infallibly construct exact Input entry Arc, bind prepared new-origin unit,
  commit start_turn, append same Arc to full path and install preflighted revision
→ await SessionRecorder.record(the same Input entry Arc)
→ publish TurnStarted
→ spawn ActiveTurnTask with same Arc<TurnExecutionContext>
```

recording failure不撤销已开始的live Turn。capture或live validation失败时不创建Turn。

## Live Conversation Basis

`LiveConversationView`的canonical crate-private definition、private fields、LiveSessionState-owned factory and `revision()/messages()` getters位于[Conversation Storage](conversation-storage.md#live与cold-conversation)。它只能由live conversation reducer产生，保证：

- canonical User/Assistant/Tool role；
- complete Tool exchange；
- abandoned/incomplete/orphan exchange排除；
- Compaction Replace已经应用；
- deterministic ordering。

PromptSet不要求消息已经physical record。revision遵守[Conversation Storage的exact delta matrix](conversation-storage.md#conversationrevision)：accepted ToolCall Assistant在尚未可见时已改变basis，complete exchange promotion再改变一次；Interaction、partial/abandoned settlement、progress/usage/recording不改变basis。所有`+1`的checked overflow在EntryId allocation与state mutation前失败。

M4 reader必须调用fallible `LiveSessionState::capture_conversation_views()`并持有返回的单一`CapturedConversationViews` aggregate；它在同一次live state/revision capture中提供此view、Compaction source、从full state path末项派生的selected head、`Arc<[ItemRelation]>`和safe `Arc<[PendingInteractionFact]>`。M4 aggregate不公开path；state保留full immutable entries给future Fork `LiveSnapshot`。它不是M8 public `SessionSnapshot`/Item/Interaction DTO或Fork `LiveSnapshot` activation。每Item至多一个Pending Interaction；terminal resolution后允许顺序later interaction，且这些Interaction mutation一律revision `+0`。

Compaction使用同一个aggregate中的`Arc<LiveCompactionSourceView>`。该view额外携带SessionId、revision和EntryId-bearing stable units，但不携带token estimate或settings；source/unit是immutable `Clone` handles。TurnExecutionContext不构造stable units；M4 reducer uses Compaction-owned `PreparedLiveCompactionUnit` to complete message/kind validation before any new origin allocation, then binds it infallibly after a new origin (or, for complete Tool exchange, with its existing Assistant origin before current Tool allocation). It creates a fresh current source and calls `source.has_same_stable_identity(&fresh_current_source)` before it accepts apply, then clones retained unit handles only from that fresh current source. After consuming `CompactionReplacement::into_parts()`, it may clone the prebuilt immutable rolling-summary message into the leading unit and flattened `LiveConversationView`; neither path reconstructs borrowed messages or a caller suffix. M10 adds only planning/model/control/plan/request arbitration around that reducer subset. Context only uses its captured source to form [Compaction完整private input](compaction.md#module-interface)。ordinary assembly仍只看到`LiveConversationView`。

## ModelCallRequest Binding

`ModelCallRequest`的唯一canonical owner、完整字段和private constructor位于[ModelGateway](model-gateway.md#modelcallrequest)。Turn Execution Context不复制第二份struct，也不保存独立OutputContract或effective output limit。

Context只负责把同一次capture产生的对象交给canonical constructor：

```text
Arc<TurnModelSnapshot>
+ ModelCallPurpose
+ Arc<AssembledModelContext>
+ source ConversationRevision
+ optional request max_output_tokens
→ ModelCallRequest::new(...)
```

cross-binding分工固定为：

- TurnExecutionContext private constructor保证model、PromptSet和ToolSet来自同一次capture；
- PromptSet保证ToolPromptView来自该ToolSet并生成crate-private assembly proof；
- ModelCallRequest constructor验证purpose、exact TurnModelRef、source revision、OutputContract proof、Compaction budget proof和request max output limit；
- ordinary AgentRun允许`input.output_contract = None`和`max_output_tokens = None`；
- CompactionSummary必须使用plan固定的`Some(NonZeroU32)` request max output。

logical retry移动并复用同一个`Arc<ModelCallRequest>`，不重新assemble或复制request字段。Session record head或EntryId不参与retry proof。

当前Structured contract foundation已闭合：`StructuredOutputContract` constructor绑定exact `TurnModelRef`并校验capability、schema byte cap与schema v1 subset，`ModelCallRequest::new`对Structured/NoToolCalls复验proof、empty tools与exact model绑定，Gateway terminal validation覆盖`Refused` bypass及`UnexpectedToolCall`/`IncompleteResponse`/`InvalidStructuredOutput` precedence，contract/gateway tests已通过；但Context/ActiveTurnTask尚无production requester——ordinary AgentRun仍以`output_contract=None`组装，toolful Turn在普通ToolRound后的第二次Structured call尚未激活。

## Async Loop Contract

ActiveTurnTask直接使用Context：

```text
LiveConversationView
→ PromptSet.assemble
→ ModelGateway-owned ModelCallRequest::new
→ await ModelGateway
→ validated FinalizedAssistantResponse
→ live Assistant mutation
→ optional await ToolSet
→ live ToolResult mutations
→ next safe point
```

Rig或其他provider SDK不驱动该loop。

ModelGateway validation负责finish reason、ToolCall presence和OutputContract矩阵；ActiveTurnTask只处理validated response。

## Tool Binding

PromptSet看到的ToolSpec和ActiveTurnTask执行的Tool route来自同一个`Arc<ToolSet>`。

```text
TurnExecutionContext.tool_set
├─ ToolPromptView used by PromptSet
├─ execution route used by ActiveTurnTask
├─ Turn-scoped ToolExecutionControl captured before task spawn
└─ Session-local mutation queue shared from SessionExecutor
```

active Turn内禁止重新读取current Tool registry。Tool side-effect start由ToolStartGate/EmergencyControl保护，与Session recording无关。

## Steer

Steer复用当前Context，并按[INV-102](../architecture.md#跨模块不变量索引)只在完整step后的safe point消费：

```text
control actor accepts Steer for exact TurnId
→ ActiveTurnTask reaches safe point
→ await context.resolve_user_message(Steer intent) while selecting EmergencyControl
→ revalidate active Turn/control_generation/source ConversationRevision
→ apply live UserMessage(source=Steer)
→ await inline record attempt
→ next assemble_agent_run
```

Steer不重新capturePrompt/Skill/Tool/Model/Workspace。recording失败不阻止Steer进入live conversation。

## FollowUp

FollowUp在旧ActiveTurnTask结束后创建新Turn，因此重新执行完整capture。旧Context、request、Tool state和ConversationRevision basis不能复用。

## Interaction

Interaction request绑定Context中的exact ToolSet和Workspace policy，但Pending/Resolved状态属于LiveSessionState。ActiveTurnTask通过Interaction router await resolution；Context不保存waiter。

## Compaction

M10 ActiveTurnTask从`LiveSessionState::capture_conversation_views()`取得revision-bound stable-unit source，但不直接拼装settings/model/Prompt scalar。它只把source、trigger和本Turn`compactions_started`交给同一个Context：

```text
LiveSessionState.capture_conversation_views()
→ CapturedConversationViews.compaction_source()
→ context.compaction_pressure(source, trigger, count)
→ context.plan_compaction(source, trigger, count)
   ├─ captured CompactionSettingsSnapshot
   ├─ same PromptSet AgentRun/Summary assembly bases
   └─ same TurnModelSnapshot model basis/TokenEstimator
→ context.assemble_compaction(exact plan)
→ ModelCallRequest::new(CompactionSummary)
→ await ModelGateway
→ validate same Turn/control/session/revision/plan/request
→ Compaction validates summary; M10 constructs sealed CompactionReplacement
→ final ActiveTurnTask arbitration, then live reducer applies plan-derived Replace
   with its unchanged M4 source/cut/marker/prepared-unit transaction
→ await inline record the same applied StoredCompaction entry Arc
```

`compaction_pressure()`和`plan_compaction()`是exact capture façade：内部构造Compaction canonical `CompactionPressureInput/CompactionPlanInput`，不向ActiveTurnTask暴露可跨Context拼接的basis fields。plan marker只能由stable-unit cut派生。recording marker不是Replace生效条件；Compaction后继续使用同一TurnExecutionContext，不需要rebuild loop。

上述pressure/plan/summary-request façade属于M10完整Compaction。M4只把source、nonzero cut、`CompactionReplacement`以及Session/Turn orchestration提供的`TurnId + Timestamp`交给live reducer，以验证derived marker并安装prebuilt summary；它不经TurnExecutionContext实现planner、budget、model request或orchestration。

## Cancel与SecurityRevoked

Context本身immutable且不可撤销。EmergencyControl由ActiveTurnTask观察：

- 不开始新Model、Tool、source load或Compaction；
- Running Tooltruthful settle；
- task terminal后SessionExecutor按current authority重新resolvefuture Workspace；
- 不修改已经capture的Arc对象。

SessionExecutor/ActiveTurnTask可以在EmergencyControl或Lifecycle先赢时drop正在等待的`resolve_user_message()` future。SkillService load/cache必须支持waiter cancellation；迟到parse result不能绕过owner revalidation进入live state。

## Reload

Shared Prompt/Skill/Tool/Model reload使用all-or-none candidate publication。Workspace reload只在Idle。Prompt candidate必须包含完整materialized PromptContent后才能publication；active Turn的Context保持不变。

不建立fingerprint、generation或hot replacement identity。`ConversationRevision`只描述live conversation变化，不描述resource reload。

## Restart

restart后：

- replay recorded Session prefix；
- 不恢复TurnExecutionContext、ActiveTurnTask、ModelCallRequest、provider continuation、Tool task或Interaction waiter；
- 不从recorded `TurnId`推断旧TurnStatus，`current_turn`为空；
- future Turn按current definition重新capture。

## 测试要求

- 同一次capture objects不能跨root拼接；
- reload不改变active Context；
- Prompt source变化或cache eviction不改变active PromptContent，assemble不执行正文I/O；
- SkillIntent只从captured SkillView解析；exact authorization在composition前验证，最终message只保留safe part-level stamp；
- Context捕获exact SkillService/SkillViewContext/SkillView binding，Input与Steer共用async resolve seam；
- resolve await前后不持有live/lifecycle guard，Cancel/SecurityRevoked先赢时迟到result被丢弃；
- reload-during-Steer只解析old captured bytes；
- Prompt ToolSpec与Tool execution route一致；
- ModelCallRequest constructor拒绝purpose/model/source revision/OutputContract proof不一致；
- ordinary AgentRun可用`output_contract = None`和`max_output_tokens = None`构造；
- Structured、NoToolCalls和CompactionSummary使用同一个canonical request type；
- retry复用exact same `Arc<ModelCallRequest>`；
- Steer复用Context但递增conversation revision；
- FollowUp重新capture；
- Compaction pressure/plan只能由Context用captured settings、Prompt bases和model basis构造，不能跨Turn拼接；
- duplicate message、ToolExchange和rolling summary source通过EntryId-bearing stable units形成exact marker；
- Compaction Replace后同一Context继续运行；
- recording degraded不改变Context；
- restart不恢复Context。

## 开放问题

Prompt Q1/Q4已分别由ADR 0128/0129关闭，async Skill composition由ADR 0130关闭。serde casing与public ID格式现由ADR 0134/Wire Schema冻结。Async loop/recording策略问题见[独立review](../review/async-loop-best-effort-recording-open-questions.md)。
