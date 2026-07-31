# Turn Execution Context 架构设计

状态：当前权威架构（ADR 0127后，生产实现待启动）
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
- active Turn不读取future current roots；
- private constructor阻止跨capture拼接；
- ActiveTurnTask直接运行async Model→Tool→Model loop；
- 不再存在同步AgentLoop或committed delta interface；
- PromptSet从sanitized `LiveConversationView`组装模型输入；
- `ConversationRevision`是current-process model-visible basis；
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
→ SkillService.for_turn
→ ToolService.for_turn
→ PromptService.for_turn
→ Arc<TurnExecutionContext>
```

所有对象必须来自同一次admission capture。capture完成后：

- Agent新revision不影响active Turn；
- SessionDefinition update只在Idle并只影响future Turn；
- shared `/reload`只替换future capture roots；
- Workspace hard restriction通过SecurityRevoked中断Turn，不热替换Context；
- provider fallback不会在active Turn内重新resolve另一模型。

## TurnExecutionContext

```rust
pub(crate) struct TurnExecutionContext {
    session_id: SessionId,
    turn_id: TurnId,
    session_revision: SessionDefinitionRevision,
    agent: AgentRevisionRef,
    model: Arc<TurnModelSnapshot>,
    workspace: Arc<WorkspaceSnapshot>,
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
    pub fn compose_input(
        &self,
        intent: PromptIntent,
    ) -> Result<CanonicalUserMessage, PromptError>;

    pub fn compose_steer(
        &self,
        intent: PromptIntent,
    ) -> Result<CanonicalUserMessage, PromptError>;

    pub fn assemble_agent_run(
        &self,
        conversation: &LiveConversationView,
    ) -> Result<Arc<AssembledModelContext>, PromptError>;

    pub fn assemble_compaction(
        &self,
        plan: &Arc<CompactionPlan>,
    ) -> Result<Arc<AssembledModelContext>, PromptError>;
}
```

Context不拥有：

- LiveSessionState或SessionRecorder；
- ActiveTurnTask/control actor；
- queue、waiter或CancellationToken；
- provider stream；
- mutable Workspace/Prompt/Tool/Skill root；
- public event publisher。

## Turn Admission

```text
SessionExecutor reserves candidate TurnId/control_generation
→ capture TurnExecutionContext
→ compose Input
→ LiveSessionState validates start + allocates EntryId + binds parent_id
→ apply start_turn(...) and increment ConversationRevision
→ await SessionRecorder.record(Input UserMessage)
→ publish TurnStarted
→ spawn ActiveTurnTask with same Arc<TurnExecutionContext>
```

recording failure不撤销已开始的live Turn。capture或live validation失败时不创建Turn。

## Live Conversation Basis

```rust
pub(crate) struct LiveConversationView {
    revision: ConversationRevision,
    messages: Arc<[ModelMessage]>,
}
```

`LiveConversationView`只能由live conversation reducer产生，保证：

- canonical User/Assistant/Tool role；
- complete Tool exchange；
- abandoned/incomplete/orphan exchange排除；
- Compaction Replace已经应用；
- deterministic ordering。

PromptSet不要求消息已经physical record。Model-visible mutation先更新LiveConversation并递增revision。

## ModelCallRequest

```rust
pub(crate) struct ModelCallRequest {
    model: Arc<TurnModelSnapshot>,
    purpose: ModelCallPurpose,
    context: Arc<AssembledModelContext>,
    source_revision: ConversationRevision,
    output_contract: OutputContract,
    effective_max_output_tokens: u32,
}
```

由private constructor验证：

- model/context来自同一TurnExecutionContext；
- purpose与OutputContract一致；
- token limits闭合；
- source revision等于assembly输入view；
- ToolSpec与captured ToolSet一致。

logical retry移动并复用同一个`Arc<ModelCallRequest>`，不重新assemble。Session record head或EntryId不参与retry proof。

## Async Loop Contract

ActiveTurnTask直接使用Context：

```text
LiveConversationView
→ PromptSet.assemble
→ ModelCallRequest::new
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
└─ execution route used by ActiveTurnTask
```

active Turn内禁止重新读取current Tool registry。Tool side-effect start由ToolStartGate/EmergencyControl保护，与Session recording无关。

## Steer

Steer复用当前Context，并按[INV-102](../architecture.md#跨模块不变量索引)只在完整step后的safe point消费：

```text
control actor accepts Steer for exact TurnId
→ ActiveTurnTask reaches safe point
→ context.compose_steer(intent)
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

ActiveTurnTask使用同一Context中的PromptSet、model和TokenEstimator：

```text
sanitized LiveConversationView
→ Compaction::plan(source_revision)
→ context.assemble_compaction(plan)
→ ModelCallRequest::new(CompactionSummary)
→ await ModelGateway
→ validate same control_generation/revision
→ apply live Replace
→ await inline record StoredCompaction attempt
```

recording marker不是Replace生效条件。Compaction后继续使用同一TurnExecutionContext，不需要rebuild AgentLoop。

## Cancel与SecurityRevoked

Context本身immutable且不可撤销。EmergencyControl由ActiveTurnTask观察：

- 不开始新Model、Tool、source load或Compaction；
- Running Tooltruthful settle；
- task terminal后SessionExecutor按current authority重新resolvefuture Workspace；
- 不修改已经capture的Arc对象。

## Reload

Shared Prompt/Skill/Tool/Model reload使用all-or-none candidate publication。Workspace reload只在Idle。active Turn的Context保持不变。

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
- Prompt ToolSpec与Tool execution route一致；
- ModelCallRequest source revision与assembly一致；
- retry复用exact request；
- Steer复用Context但递增conversation revision；
- FollowUp重新capture；
- Compaction Replace后同一Context继续运行；
- recording degraded不改变Context；
- restart不恢复Context。

## 开放问题

PromptContent inline/reference和contribution stamp字段仍需在wire/schema freeze关闭。Async loop/recording策略问题见[独立review](../review/async-loop-best-effort-recording-open-questions.md)。
