# Turn 执行模块与执行上下文架构设计

状态：目标架构已确定；Session execution、storage 和 protocol integration 待后续阶段完成
日期：2026-07-16

## 目的

本文定义 MiniCore Turn 执行模块的基础边界、执行上下文、AgentLoop 关系、模型调用步骤、创建顺序、pinning、reload、cancellation、fingerprint 和 recovery 规则。

本文以以下领域定义为前提：

```text
Turn 从 initiating UserMessage entry 成功 append 开始
到 final AssistantMessage / TurnInterrupted / TurnFailed entry 成功 append 结束
```

本文重点解决：

- Turn 执行是否包含具体 AgentLoop；
- Turn execution、领域 Turn 和底层 AgentLoop 的开始与结束边界；
- WorkspaceSnapshot、SkillCatalog、ToolSet、PromptSet 和 Model 如何在一个 Turn 内稳定绑定；
- 每次模型调用如何只从 committed conversation 组装；
- Steer、FollowUp、retry、compaction 和 security revocation 如何影响 active Turn；
- crash recovery 可以安全恢复什么，何时必须 fail closed。

Turn、Item 与 Interaction 的领域语义以 [Turn、Item 与 Interaction 架构设计](turn-item-interaction.md) 为权威。

本文不提前冻结：

- Session actor、task、mailbox 或锁的具体实现；
- Session execution 如何构造并append已定义的Session entries；
- Runtime command、event 和 transport protocol；
- 已定义 SessionStorage entry 在具体 Session execution actor/task 中的实现细节；
- ModelGateway provider adapter 的具体接口；
- AgentLoop 使用 Rig、自研状态机或其他 SDK 的具体实现；
- FollowUp queue 的持久化和调度策略。

## 决策摘要

已经确定：

- Turn execution 包含并驱动一个具体的 AgentLoop，但不与 AgentLoop 合并；
- Session execution 是 future design 中 active Turn mutable state 的唯一 owner；
- `TurnExecutionContext` 是 Turn-scoped、不可变的执行能力组合，不是领域 entity、Service 或通用 Resource owner；
- `TurnExecutionContext` pin exact AgentRevisionRef、SessionDefinitionRevision、WorkspaceSnapshot、SkillCatalog、ToolSet、PromptSet 和 TurnModelSnapshot；
- active Turn 不重新读取 Workspace、Prompt、Tool、Skill 或 Model 的 future current value；
- 一次逻辑模型调用由 committed conversation checkpoint、purpose、output contract 和 `AssembledModelContext` 唯一确定；
- 不增加 `ModelStep` struct、ID、领域 entity 或公开协议对象；
- provider retry 复用同一个不可变 assembled context，不创建新的领域对象；
- PromptSet 是唯一产生 `AssembledModelContext` 的对象；
- PromptSet 在创建时绑定同一个 ToolSet 的 ToolPromptView，assembly 时不能再传入任意 Tool view；
- 模型可见动态事实必须来自 committed conversation，不建立未持久化的 dynamic contribution lane；
- Skill lazy load 必须使用本 Turn pinned Catalog 的 `SkillCatalogEntryRef`；
- initiating UserMessage append 前不调用模型、不执行 Tool，也不发布领域 Turn；
- Steer 是 current Turn control input；成功 append 后才影响下一次逻辑模型调用；
- FollowUp 不属于 `TurnControl`，它在当前 Turn terminal 后开启新 Turn，并捕获新 Context；
- ordinary reload 只影响 future Turn；security-restricting update 撤销 lease 并中断 active Turn；
- exact cold recovery 只有在全部 definition reference、Tool executor identity 和 Workspace reauthorization 可重建时才允许；否则 fail closed。

## 三层边界

MiniCore 必须区分三个不同边界。

### 领域 Turn

领域 Turn 是 conversation 中的一段业务事实：

```text
initiating UserMessage entry appended
→ zero or more Message / Event / Compaction entries
→ final AssistantMessage | TurnInterrupted | TurnFailed appended
```

领域Turn的开始线性化点是initiating UserMessage entry成功append。

领域Turn的结束线性化点是以下任一terminal entry成功append：

```text
Message(role = assistant, phase = final)
TurnInterrupted
TurnFailed
```

candidate TurnId 的预留、Context capture 和 UserMessage 规范化发生在领域 Turn 正式开始之前。

### Turn Execution

Turn execution 是围绕一个领域 Turn 的完整运行过程：

```text
admission reservation
→ TurnExecutionContext capture
→ initiating UserMessage composition
→ TurnContext + initiating UserMessage append
→ AgentLoop drive
→ model / tool / append/apply loop
→ terminal entry append
→ release execution state
```

因此 Turn execution 的操作边界比领域 Turn 略宽：它包含 initiating UserMessage append 之前的 admission，但失败的 admission 不产生领域 Turn。

### AgentLoop

AgentLoop 是 Turn execution 内部的推理协议状态机：

```text
committed conversation seed
→ NeedModel
→ ModelOutput
→ NeedTools 或 Finished
→ committed delta
→ NeedModel ...
```

AgentLoop 不拥有 Turn admission、SessionStorage、Workspace、Prompt source、Tool permission、approval、Sandbox、Steer queue 或 terminal append。

一个 Turn execution 可以包含多个 AgentLoop segment。例如：

- compaction entry append/apply 后从新的 committed conversation seed 重新建立 segment；
- AgentLoop adapter 不支持原地注入 Steer 时，在同一 TurnContext 下重新建立 segment；
- provider 或 SDK rollover 要求重新建立内部 run state。

segment 重建不创建新 Turn，也不重新捕获 TurnExecutionContext。

## 对象关系

```text
Session execution owner
└─ Active Turn execution
   ├─ Arc<TurnExecutionContext>       // Turn 内固定
   │  ├─ TurnModelSnapshot
   │  ├─ Arc<WorkspaceSnapshot>
   │  ├─ pinned SkillCatalog context
   │  ├─ Arc<SkillCatalog>
   │  ├─ ToolSet
   │  └─ PromptSet
   ├─ CommittedConversationState      // 只消费成功 append receipts的trusted delta
   ├─ private AgentLoop state         // 底层 AgentLoop segment
   ├─ ModelTurnSession                // ModelGateway 内部连接状态
   ├─ Turn control / cancellation     // 单调或可变执行状态
   └─ logical model-call state        // 串行局部值
```

领域对象保持简单：

```text
Turn domain
→ id、session_id、started_at、terminal-aware status

Turn start execution metadata
→ exact Agent/SessionDefinition/Workspace/Prompt/Tool/Skill/Model references

Turn Item collection
→ SessionStorage ordered projection，不内联 Turn head

Turn execution
→ Context、private AgentLoop state、conversation projection、control、retry 和 provider session
```

Turn 领域对象不持有 `TurnExecutionContext`、AgentLoop state、逻辑模型调用状态、SkillCatalog、ToolSet 或 PromptSet。

## TurnExecutionContext

`TurnExecutionContext` 组合本 Turn 的不可变有效执行值：

```rust
pub(crate) struct TurnExecutionContext {
    session_id: SessionId,
    session_revision: SessionDefinitionRevision,
    agent: AgentRevisionRef,
    model: TurnModelSnapshot,
    workspace: Arc<WorkspaceSnapshot>,

    skill_service: Arc<SkillService>,
    skill_context: SkillCatalogContext,
    skill_catalog: Arc<SkillCatalog>,

    tool_set: ToolSet,
    prompt_set: PromptSet,

    fingerprint: ExecutionContextFingerprint,
    diagnostics: Arc<[TurnContextDiagnostic]>,
}
```

字段保持私有，避免调用方取得 PromptSet、ToolSet 或 SkillCatalog 后与其他 Turn 的对象交叉组合。

`skill_service` 只用于执行 pinned exact-reference load：

```text
SkillCatalogContext + SkillCatalogEntryRef
→ SkillService::load(...)
```

它不能用于重新查询 current Catalog、重新解释 Workspace 或按 SkillId 漂移到新版本。

`TurnExecutionContext` 的“不可变”指：

- model identity 和 capability projection 不变；
- WorkspaceSnapshot 和各 view 不变；
- SkillCatalog entries 和 fingerprint 不变；
- ToolSpec、Exposure、executor route 和 ToolSet fingerprint 不变；
- PromptProfile、ToolPromptView、SkillCatalogView 和 Prompt fingerprint 不变。

以下执行状态不进入 fingerprint，也不表示 Context 发生变化：

- cancellation token 是否已触发；
- Workspace authorization lease 是否已撤销；
- Tool approval waiter；
- provider retry attempt；
- stream draft；
- diagnostics delivery 状态；
- cache hit、load state 或 telemetry。

## 最小执行 Interface

`TurnExecutionContext` 是 crate-internal invariant-bearing aggregate，不建立新的公开 Service interface。它只需要隐藏两类跨子系统复杂性：输入规范化和模型上下文组装。

```rust
impl TurnExecutionContext {
    async fn compose_message(
        &self,
        intent: PromptIntent,
    ) -> Result<CanonicalUserMessage, TurnExecutionError>;

    fn assemble_model_context(
        &self,
        input: PromptAssemblyInput<'_>,
    ) -> Result<AssembledModelContext, TurnExecutionError>;
}
```

```text
compose_message
→ 从 pinned Catalog 解析 SkillIntent
→ SkillService::load(exact request)
→ SkillInjector::build
→ 校验 authorization lease、contribution identity 和 content hash
→ PromptSet::compose_user_message

assemble_model_context
→ 校验 committed conversation proof、authorization lease 和 Context binding
→ PromptSet::assemble
```

Tool execution 不在 Context 上复制一层公开转发方法。future Session execution 与 Context 位于同一内部模块，通过 Context 中 pinned ToolSet 执行调用；Tool 的 route、approval、Sandbox 和 executor 仍只由 ToolSet 处理。

Context 的 fingerprint 和 diagnostics 也保持内部值，只有 storage、diagnostics 或 recovery 出现真实调用方时才暴露窄 getter。

## Context Capture

当前不建立 `TurnContextFactory`、`TurnContextManager`、公开 capture DTO 或第四个 Runtime Service。Context capture 是 Session execution 内部的一次深操作。

capture 必须从 Session execution 的一个原子 admission basis 取得：

```text
stable submission/idempotency key
candidate TurnId
SessionId + exact SessionDefinitionRevision
SessionDefinition.agent = exact AgentRevisionRef
SessionDefinition.workspace / model / prompts
exact AgentDefinition.prompts
execution mode、Turn-scoped ToolTurnPort、cancellation 和 ToolUpdate sink
```

SessionDefinitionRevision 保证 AgentRevisionRef、Workspace、Model 和 SessionPrompts 来自同一个 committed definition。Prompt/Skill adapter 不得按 AgentId 或 SessionId 回查 current heads。

`candidate TurnId` 只表示已预留的 execution identity。stable submission/idempotency key 由外层 admission reservation 持有，不进入 TurnExecutionContext 或其 fingerprint。capture 成功不代表领域 Turn 已经创建。

## Capture 依赖图

Context capture 的逻辑依赖是 DAG，不要求建立跨 Service 的全局锁或 Resource generation：

```text
exact SessionDefinitionRevision
├─ exact AgentRevisionRef / AgentPrompts / SessionPrompts / TurnModelSnapshot
└─ Arc<WorkspaceSnapshot>
       ├─ SkillService::catalog(SkillCatalogContext {
       │    agent, session_id, session_revision, workspace: workspace.skill_context()
       │  })
       │  └─ Arc<SkillCatalog>
       └─ ToolService::for_turn(ToolTurnContext {
            agent, session_id, session_revision, turn_id,
            workspace: workspace.tool_context(),
            provider: model.capabilities(),
            execution_mode, turn_port, cancellation, updates
          })
          └─ ToolSet

SkillCatalog.prompt_view()
+ ToolSet.prompt_view()
+ WorkspaceSnapshot.prompt_context()
+ Agent/Session Prompt config
+ TurnModelSnapshot
→ PromptService::for_turn(...)
→ PromptSet

all child fingerprints
→ final validation
→ TurnExecutionContext
```

SkillCatalog 和 ToolSet 不互相依赖，实现可以并行捕获。PromptSet 必须在二者之后创建，因为它要绑定两者的精确 view。

capture 完成前必须再次检查：

- Workspace authorization lease 未撤销；
- Turn cancellation 未触发；
- Session admission reservation 仍然有效；
- SkillCatalogContext / ToolTurnContext 与 captured AgentRevisionRef、SessionDefinitionRevision 和 candidate TurnId 一致；
- ToolSet.prompt_view().tool_set_fingerprint 等于 parent ToolSet fingerprint；
- PromptSet 记录的 ToolSet、SkillCatalog、Workspace 和 Model fingerprint 与实际对象一致。

## Capture 线性化

Prompt、Tool 和 Skill 是独立领域，没有跨三者的 global publication instant。

ordinary reload 在 capture 期间发生时：

- 某子系统在自己的 capture 线性化点之前发布的新值，可以被本次 Context 捕获；
- 已经捕获的值不被后续 reload 原地替换；
- PromptSet 只绑定实际收到的 SkillCatalogView 和 ToolPromptView；
- 最终发布的是一个内部一致的组合，而不是“同一纳秒”的全局资源快照。

capture 完成后的 ordinary Skill invalidation 不允许 active Turn 回查新 Catalog。尚未 lazy-load 的 pinned entry 只能按旧 exact content hash 从授权 source 或 content-addressed cache 加载；旧内容不可得时返回 `Unavailable` 并 fail closed。

这不恢复通用 ResourceManager。

如果未来一个 versioned extension package 必须原子贡献 Prompt、Skill 和 Tool，应由独立 `ExtensionSet` 提供 package-level publication，而不是让 TurnExecutionContext 推断跨领域事务。

## Admission

initiating UserMessage 需要 PromptSet 规范化，而 PromptSet 又需要本次 admission 捕获的 ToolSet 和 SkillCatalog；其中 ToolTurnContext、cancellation 和 Turn-scoped grant 需要已预留的 candidate TurnId。因此 admission 必须使用未发布的 candidate，而不是先创建空 Turn。PromptSet 本身不保存 candidate TurnId。

推荐顺序：

```text
SessionLifecycle = Open
+ SessionLoadState = Loaded
+ SessionReadiness = Ready
+ SessionExecutionState = Idle
→ reserve admission slot + stable submission/idempotency key + candidate TurnId
→ SessionExecutionState = Starting
→ capture current exact SessionDefinitionRevision
→ check AgentStatus = Enabled and read exact AgentRevisionRef
→ capture TurnExecutionContext
→ Context.compose_message(PromptIntent)
   └─ 内部按需完成 pinned Skill load / injection
→ 获取短 Agent lifecycle gate
→ 在 gate 内最终检查 AgentStatus = Enabled
→ 在 gate 内append TurnContext entry → apply its AdvanceOnly delta
→ 在 gate 内append initiating UserMessage(source = Input, context_entry_id) → apply its Append delta
→ 确认两个append outcome并释放Agent lifecycle gate
→ 发布 SessionExecutionState = Running / TurnStatus = Running
→ 启动 private AgentLoop adapter
→ 第一次逻辑模型调用
```

initiating UserMessage entry是领域Turn的开始线性化点。Agent lifecycle gate只覆盖final Enabled check到该entry append outcome确认；TurnContext entry本身不创建Turn。disable/delete必须等待该gate释放。

在此之前：

- 不调用 ModelGateway；
- 不执行 Tool；
- 不创建 Tool approval Interaction；
- 不发布 committed Turn/UserMessage；
- context capture、source read 和 cache fill 不构成领域事实；
- `compose_message` 和 initiating UserMessage append 前都必须再次检查 Workspace authorization lease 与 Turn cancellation；
- revocation 后尚未 append 的 UserMessage、Steer 和 PromptContribution 全部丢弃。

capture、Skill load、UserMessage composition、Context append或UserMessage append失败时，释放candidate和局部Context，SessionExecutionState返回Idle；仅有orphan Context entry不创建空Turn或`Failed` Turn。

如果UserMessage append返回OutcomeUnknown，Session execution必须通过admission operation key和storage reload确认结果，不能直接分配另一个TurnId重试。

Session pin exact AgentRevisionRef：

- Agent current revision 更新不改变 candidate、active 或 future Turn；
- Session 显式升级会创建新的 SessionDefinitionRevision，只影响 update 后开始 admission 的 future Turn；
- Agent Disabled/Deleted 与 initiating UserMessage append通过同一个短Agent lifecycle gate线性化：status mutation先赢则start被拒绝，message append先赢则active Turn继续；
- recovery使用TurnContext entry中的exact AgentRevisionRef，不能替换为Agent current。

完整 lifecycle 线性化规则见 [Agent 与 Session 生命周期架构设计](agent-session-lifecycle.md)。

## Prompt 与 Transcript-First

模型可见输入只能来自三个来源：

```text
Turn 固定 baseline
→ PromptSet

执行中变化的事实
→ committed conversation

单次调用控制
→ ModelCallPurpose + OutputContract
```

不能存在第四类“调用方临时传入、模型可见但没有append/apply或pin”的动态字符串。

因此每次 assembly 的推荐输入为：

```rust
pub struct PromptAssemblyInput<'a> {
    pub conversation: &'a CommittedConversationView,
    pub output_contract: Option<&'a OutputContract>,
    pub purpose: ModelCallPurpose,
}
```

不再接收：

```text
任意 ToolPromptView
任意 PromptContribution[]
任意 current Workspace context
任意 current SkillCatalog
裸 Vec<MessageRecord>
```

ToolPromptView 已经被 PromptSet 固定。Workspace Prompt、Skill Catalog metadata 和其他 Turn-static baseline 也已经进入 PromptSet。

动态 PromptContribution 必须在模型可见前变成 committed fact：

- 用户显式 Skill invocation：`Context.compose_message()` 内完成 exact load/injection，并规范化进 `CanonicalUserMessage`，随 UserMessage append；
- Steer 中的 Skill invocation：使用同一个 `compose_message()` 规则，随 Steer append；
- 模型通过Tool调用Skill：结果随role=tool message持久化，并在`tool_round_completed`后进入conversation；
- compaction directive：使用 typed `ModelCallPurpose` 和 `OutputContract`，不伪装成普通 conversation text。

`CommittedConversationView`只能从已验证CommittedConversationState借用；State由SessionStorage replay构造，或成功应用append receipt中的trusted delta后前进，不能从draft、stream buffer或任意message vector构造。ledger checkpoint可因`AdvanceOnly`entry推进；只有model-visible messages/`TranscriptFingerprint`改变才形成新的逻辑模型调用。

## 逻辑模型调用

一次逻辑模型调用由以下值共同确定：

```text
ExecutionContextFingerprint
+ ConversationCheckpoint / TranscriptFingerprint
+ ModelCallPurpose
+ OutputContract
+ AssembledModelContextFingerprint
```

Session execution 使用 `PromptAssemblyInput` 调用 Context，并保留返回的不可变 `AssembledModelContext` 供 provider retry 复用。当前不增加 `ModelStep`、`ModelStepId`、`ModelAttempt` 或额外 fingerprint 类型。

以下变化仍属于同一次逻辑模型调用：

- provider connection retry；
- rate-limit backoff；
- 尚未产生可提交结果的 transient retry；
- 同一 assembled request 的 provider attempt。

以下变化必须开始新的逻辑模型调用：

- model-visible `TranscriptFingerprint`改变；
- `tool_round_completed`、Steer或Compaction成功append/apply并改变conversation；
- ModelCallPurpose 改变；
- OutputContract 改变；
- PromptSet、ToolSet、SkillCatalog、Workspace 或 Model 改变。

后一个条件在 active Turn 中不应发生；发生时必须中断当前 Turn，而不是悄悄替换 Context。

## AgentLoop Contract

AgentLoop 应保持 sans-I/O。它在逻辑上产生三类动作：

```text
NeedModel { purpose, output_contract }
NeedTools { calls }
Finished { message draft }
```

具体 Rig/SDK adapter 可以使用自身最自然的 async stream、poll、callback 或 state enum 表达这些动作。当前不冻结 `AgentLoopFactory`、`AgentRun` 或 `AgentLoopAction` trait/enum；只有出现第二个真实 AgentLoop implementation 时才建立稳定 seam。

AgentLoop 不得：

- 从 SessionStorage 读取或写入 conversation；
- 直接调用 PromptService、ToolService 或 SkillService；
- 读取 current Workspace 或 Session config；
- 自行拼接 system prompt、messages 或 ToolSpec；
- 在 `tool_round_completed` 前把 ToolResult 加入模型 conversation；
- 处理 Tool approval、grant、Sandbox 或 filesystem authorization；
- 发布 Turn terminal fact。

## Turn Execution Loop

推荐逻辑循环：

```text
initiating UserMessage appended
→ private AgentLoop adapter 从 committed ConversationSeed 开始

loop:
  → 在 stable barrier 处理已接受的 Steer
  → 取得 AgentLoop 下一动作

  NeedModel
  → Context.assemble_model_context(committed conversation)
  → ModelGateway.generate(immutable assembled context)
  → 把 ModelOutput 交回 AgentLoop adapter

  NeedTools
  → append one assistant/intermediate message
     └─ ordered reasoning/text/tool_call content；每个ToolCall带ItemId
  → apply assistant entry delta（Started ToolInvocation，conversation AdvanceOnly）
  → control / revocation barrier
  → pinned ToolSet.execute(ToolExecutionRequest[])
     └─ approval 通过 durable Interaction request/resolution
     └─ ToolTurnPort append ToolExecutionStarted before side effect
  → ToolExecutionOutcome[]
     ├─ 每个Completed：append matching role=tool message → apply
     ├─ 全部Completed：append tool_round_completed → apply conversation delta
     └─ 任一 Abandoned：append ToolAbandoned → apply并进入terminal arbitration
  → 把model-visible committed conversation change交回AgentLoop adapter
  → continue

  Finished
  → 与 queued Steer / Cancel 仲裁
  → SessionExecutionState = Finishing
  → append assistant/final message → apply terminal delta
  → release Context
  → SessionExecutionState = Idle
```

ToolTurnPort内每次operational append也必须先通过storage-owned apply_committed应用全部projection deltas，再返回approval或允许side effect。下一次模型调用只能发生在`tool_round_completed`成功append并apply后。

以下内容不能进入下一次逻辑模型调用：

- streaming assistant draft；
- Started 或 Abandoned ToolInvocation；
- 尚无`tool_round_completed`引用的Tool message；
- pending Interaction；
- accepted 但未 append 的 Steer；
- compaction draft；
- provider retry 的 partial output。

## Waiting Approval

等待 Tool approval 时，Turn 没有结束：

```text
TurnStatus = Running
SessionExecutionState = Running
TurnExecutionPhase = WaitingApproval
InteractionState = Pending
ToolInvocationState = Started
```

InteractionRequested必须append后才发布approval request；InteractionResolved必须append后才进入ExecutingTools。Deny产生truthful denied Tool message，并在`tool_round_completed`后继续模型调用。

WaitingApproval不是Interrupted。默认情况下此时到达的Steer先排队，不自动当作approval resolution。若未来允许Steer preempt approval，必须先append `InteractionResolution::Cancelled(SteerPreempted)`、append truthful cancelled Tool message、append `tool_round_completed`，再append Steer UserMessage；同一Turn继续Running。

只有显式 cancel、runtime shutdown、security revocation 或不可恢复错误才使 Turn进入 terminal status。

## Steer

Steer 是 current Turn 的 control input，不是开启新 Turn 的普通 UserMessage。

语义：

```text
control(Steer)
→ 绑定 expected TurnId
→ 进入 active Turn control mailbox
→ 在 stable barrier 使用同一个 TurnExecutionContext 规范化
→ append Steer fact
→ apply delta
→ 下一次逻辑模型调用看见 Steer
```

Steer 可以在模型或 Tool 执行期间被接收，但不会直接修改 in-flight assembled context。

基础策略：

- sampling 期间可以 best-effort cancel 当前 draft；未 append draft 直接丢弃；
- AgentLoop 返回 NeedTools 后、任何 Tool side effect 前必须再次执行 control/revocation barrier；
- Tool 已经可能产生副作用且 exact outcome 可得时，默认等待该轮调用形成完整 Tool messages 后再 append `tool_round_completed`；executor/side-effect outcome unknown 时 ToolInvocation 进入 Abandoned 并 terminalize Turn；
- approval wait、sleep 或其他明确可中断 Tool 可以形成 cancelled ToolResult，再完成 ToolRound；
- final AssistantMessage append 与 Steer 由 Session execution 单一 owner 线性化；
- Steer 先获胜则旧 final draft 不作为 terminal entry；
- final terminal entry 先获胜则 Steer 不再属于该 Turn。

Steer 不重新捕获 WorkspaceSnapshot、SkillCatalog、ToolSet、PromptSet 或 Model。

## FollowUp

FollowUp 不属于 `TurnControl`，因为它不控制当前 Turn。

语义：

```text
active Turn 期间提交 FollowUp
→ Session execution/scheduler 排队
→ 当前 Turn terminal entry append
→ 取下一条 FollowUp
→ 作为普通 UserMessage admission
→ 捕获新的 TurnExecutionContext
→ 开启下一 Turn
```

因此 FollowUp：

- 不复用旧 TurnExecutionContext；
- 不复用旧 Workspace authorization lease；
- 不复用旧 ToolSet、PromptSet 或 SkillCatalog；
- 可以看到当前 Turn 期间完成的 ordinary reload；
- 失败或取消不会改变已经 terminal 的前一个 Turn。

FollowUp queue 的 ordering、deduplication、持久化和 transport acknowledgement 在 Session execution 与 storage 阶段定义，不在本文增加独立领域 entity。

## Retry

retry 分为两层：

```text
provider-internal attempt
→ ModelGateway 负责连接、认证刷新和 provider fallback

logical model-call retry
→ Session execution 负责是否复用同一个 immutable assembled context
```

同一次逻辑模型调用的 retry 必须满足：

- ExecutionContextFingerprint 不变；
- conversation checkpoint 不变；
- AssembledModelContextFingerprint 不变；
- purpose 和 output contract 不变；
- 没有新的 committed fact；
- Tool 没有因为该 retry 被重新执行。

partial stream 是 draft。retry 前关闭该 draft lifecycle，但不写入 conversation。

`tool_round_completed` 已成功 append/apply 后的下一次模型调用可以 retry，因为 Tool 不会重放。Tool executor outcome unknown 时不能自动重放 Tool；SessionWrite OutcomeUnknown 则必须按 operation key 解析原 durable append，不能混为 executor outcome unknown。

## Compaction

Compaction 改变 committed conversation，但不改变 TurnExecutionContext。

```text
PromptSet::assemble 或 provider 返回 context overflow
→ 丢弃当前未 append draft
→ Compaction 选择 cut/protection/directive
→ Context.assemble_model_context(
     purpose = CompactionSummary,
     output_contract = NoToolCalls
   )
→ summary model call
→ append Compaction entry
→ rebuild committed conversation projection
→ 必要时重建 AgentLoop segment
→ 新的逻辑模型调用
```

compaction model call 仍通过同一个 PromptSet 组装。`NoToolCalls` 保证本次输出不执行 Tool，但不建立第二个 Prompt assembly seam。

同一 work chain 的自动 overflow recovery 必须有明确上限；再次超限时 fail closed，不能形成无限 compact-and-retry loop。

## Cancellation 与 Security Revocation

普通 Turn cancellation 和 Workspace security revocation 都可以停止执行，但安全语义不同。

### Turn Cancellation

包括：

```text
explicit Turn cancel
fatal execution error
runtime shutdown
```

Cancellation：

- 阻止新的逻辑模型调用、Skill load 和 Tool execution；
- best-effort cancel in-flight provider/tool operation；
- 不撤销已经 committed conversation fact；
- 不伪装回滚已经完成的外部副作用；
- 通过一个 terminal entry 结束 Turn。

### Security Revocation

Workspace security-restricting update：

```text
WorkspaceAuthorizationControl.revoke()
→ authorization lease 失效
→ 通知 Session execution owner
→ cancel active Turn
```

revocation 后：

- 不开始新的模型调用；
- 不执行新的 Tool；
- 不加载新的 Skill 正文；
- 基于已撤销上下文产生但尚未append的模型结果直接丢弃；已append但conversation-hidden的assistant/tool entries不得再追加`tool_round_completed`，随后按terminal cleanup中断Turn；
- provider 已看到的内容无法撤回，只能 best-effort cancel；
- 已经发生的 Tool 外部副作用进入 audit/repair，不声称被回滚。

ordinary permissive update 不撤销 active lease，也不扩大 active Turn capability。

## Failure Atomicity

Context capture 的原子性指领域发布原子性，不要求回滚内部 cache：

- Skill discovery 或 Prompt source read 可以填充 content-addressed cache；
- ToolSet、SkillCatalog 或 PromptSet 局部值可以先后创建；
- 只有全部成功并完成最终 lease/fingerprint 校验后才发布完整 Context；
- 失败时 drop 局部值，不更新 Session current state；
- Context capture 本身不创建 Item、Interaction、grant 或 durable Turn；
- initiating UserMessage append 前不产生模型或 Tool 副作用。

Tool外部副作用与model-visible ToolRound completion无法形成通用分布式事务。assistant tool_call content先保存Started ToolInvocation；执行任何Tool side effect前，SessionStorage必须append非模型可见的`tool_execution_started`event；approval request和resolution也必须先durable append。它们不进入Prompt conversation，也不允许下一次逻辑模型调用看见半个ToolRound。

若exact ToolResult已知但role=tool message append返回NotCommitted，可以只重试同一entry；返回SessionWrite OutcomeUnknown时必须按operation key reopen/replay，已append则apply原receipt，已证明未append则重试同一draft。Tool message都已append后，`tool_round_completed`使用独立operation keyappend；其OutcomeUnknown同样只解析该entry，不能重放Tool。只有executor/side-effect outcome unknown时，同一个ToolInvocation Item才进入Abandoned且不生成synthetic ToolResult。完整schema见[Conversation 与 SessionStorage 架构设计](conversation-storage.md)。

## Fingerprint

至少需要：

```text
WorkspaceFingerprint 及各 view fingerprint
SkillCatalogFingerprint
ToolSetFingerprint
PromptFingerprint
TurnModelFingerprint
ExecutionContextFingerprint
AssembledModelContextFingerprint
```

### SkillCatalogFingerprint

覆盖：

- 稳定排序后的 Catalog entries；
- SkillCatalogEntryRef 中的 id、version、content hash、location identity 和 source stamp；
- 模型可见 metadata projection；
- Catalog filtering/ordering algorithm version。

只保存 `CatalogRevision` 不足以证明两个 Catalog 内容等价。

### ToolSetFingerprint

覆盖：

- 规范化 ToolSpec；
- Exposure 和 Deferred projection；
- executor route identity/version；
- WorkspaceToolFingerprint / WorkspaceAccessFingerprint；
- Tool policy revision；
- provider capability projection；
- ToolSet capture algorithm version。

随机 executor pointer、锁状态、approval waiter 和 cancellation 不进入 fingerprint。

### ExecutionContextFingerprint

由以下 child fingerprint 组合：

```text
AgentRevisionRef
SessionDefinitionRevision
TurnModelFingerprint
WorkspaceFingerprint 及相关 view fingerprint
SkillCatalogFingerprint
ToolSetFingerprint
PromptFingerprint
capture schema/algorithm version
```

`TurnId`和`SessionId`由TurnContext entry及其initiating UserMessage reference绑定，不建议仅为实例区分而加入内容fingerprint。

逻辑模型调用无需额外fingerprint类型。provider retry必须先证明`ConversationCheckpoint`不变；在此前提下，`ExecutionContextFingerprint + TranscriptFingerprint + purpose + output contract + AssembledModelContextFingerprint`足够判断是否仍是同一次调用。仅TranscriptFingerprint相同不足以忽略AdvanceOnly ledger变化。

Fingerprint 用于一致性、审计、cache 和 recovery 比对，不代替 secret redaction，也不是完整恢复数据。

## Execution Context Metadata

如果需要精确审计或未来cold recovery，TurnContext entry应保存或引用以下信息；哪些内容内嵌、哪些使用exact content reference仍由对应定义子系统闭合：

```text
capture schema/version
SessionId / TurnId
SessionDefinitionRevision
AgentRevisionRef
TurnModel exact reference
WorkspaceSnapshot / authority revision reference
SkillCatalog exact entry references
ToolSet spec/route implementation references
PromptSet definition/content references
ExecutionContextFingerprint
```

这些 metadata 是 execution/storage fact，不是领域 entity，不建立独立 CRUD、registry 或 `ExecutionContextManifest` struct。

PromptFingerprint 只存在于 Turn start execution metadata，用于 Prompt diagnostics，不能替代完整 execution context metadata；Turn head 不重复保存该字段。

metadata 不保存：

- provider credentials；
- 随机 lease token；
- Tool approval waiter；
- cancellation token；
- mutable cache state；
- 完整 Skill 正文副本；
- executor 内存地址。

## Crash Recovery

当前 baseline 采用保守恢复：

```text
SessionStorage reload
→ 重建 committed conversation
→ 检测没有 terminal fact 的 Turn
→ 检测 Pending Interaction 和 Started ToolInvocation
→ 不恢复旧 provider stream、AgentLoop state、approval waiter 或 Tool task
→ 不生成 synthetic ToolResult
→ 不自动重放 Tool
→ append idempotent recovery entries：
     resolve Pending Interaction
     preserve ToolInvocation with existing role=tool message as Completed
     remaining Started ToolInvocation → ToolAbandoned
     append TurnInterrupted(HostRestart / RecoveryContextUnavailable)
```

已 committed 的 UserMessage、`tool_round_completed` round、Steer、Compaction 和 AssistantMessage 保留。

只有同时满足以下条件，未来才允许 exact same-Turn resume：

- exact SessionDefinitionRevision 和 AgentRevisionRef 仍可读取；
- Workspace 可以重新授权，并且 capability ceiling 不比原 Context 更宽；
- PromptDefinition、Workspace Prompt source 和 content hash 可精确重建；
- SkillCatalogEntryRef 和旧 Skill 正文仍可按 exact hash 加载；
- ToolSpec 与 executor route 有稳定、可重建的 implementation version；
- Model identity、capability 和请求语义可重建；
- pending Tool side effect outcome 已知或有专用 repair protocol。

任一条件不满足时不能使用 Agent current revision、current SessionDefinition 或其他 current replacement 冒充旧 Context。

当前Tool子系统尚未定义稳定executor implementation identity，因此现阶段不能承诺透明cold resume。Pending Interaction必须在TurnInterrupted/Failed前以cancelled、expired或recovery reason持久关闭；Started ToolInvocation必须已有truthful role=tool message或ToolAbandoned，不能只删除内存waiter/task。

## Diagnostics 与释放

Context capture diagnostics 至少记录：

- captured AgentRevisionRef、SessionDefinitionRevision、PromptFingerprint、WorkspaceRevision 和 TurnModelFingerprint；
- Workspace、SkillCatalog、ToolSet、PromptSet 和 Model fingerprint；
- source unavailable、optional degradation 和 required failure；
- capture duration 和 cache hit；
- final lease validation；
- recovery mismatch 的具体 child reference。

绝对路径、Prompt 正文、Skill 正文、Tool arguments 和 credentials 默认不进入公开 diagnostics。

释放顺序：

```text
admission failure
→ drop candidate Context / draft

terminal path
→ 停止创建新的逻辑模型调用
→ cancel / close provider Turn session
→ 等待、取消或确认受控 Tool task outcome
→ append required InteractionResolved / ToolAbandoned entries
→ append assistant/final 或 TurnInterrupted / TurnFailed
→ apply each committed delta
→ drop private AgentLoop state
→ drop assembled context / draft
→ drop Arc<TurnExecutionContext>
```

Context drop 不 unregister Runtime-global Tool、不清空共享 content cache，也不 revoke 其他 Turn 的 lease。

## 与 Session Execution 的关系

本文确定 ownership，但不提前决定 actor/task 形状。

future Session execution 至少负责：

- 只为 `SessionLifecycle::Open + Loaded + Ready` 的 Session 接受 admission；
- 管理 `Idle → Starting → Running → Finishing → Idle` transient state；
- 一个 loaded Session 同时最多一个 active Turn；
- admission reservation；
- Context capture 和持有；
- committed conversation hot projection；
- AgentLoop drive；
- model → Tool → model 循环；
- ToolInvocation Item lifecycle；
- durable Interaction request/resolution、Steer、FollowUp 和 Cancel；
- retry、compaction、append/apply gate 和 terminal arbitration。

Turn control 至少包含三类语义：

```text
Steer { expected TurnId, PromptIntent }
Cancel { expected TurnId, reason }
ResolveInteraction { expected TurnId, RequestId, resolution }
```

这只是阶段 6 的行为约束，不冻结 `SessionExecution::control`、`TurnControl`、ack/error 或 transport Rust 类型。

Session execution 还需要 ordinary submission 和 FollowUp submission ingress，但其 request enum、ordering 和 acknowledgement 留到阶段 6 与 protocol 阶段统一设计，不在本文形成第二套 delivery 类型。语义上，ordinary submission 在 SessionExecutionState 为 Starting/Running/Finishing 时返回 conflict；FollowUp 排队并在 active Turn terminal 后重新进入普通 admission，不属于 current Turn control。

Session durable lifecycle、load/readiness/execution state 的完整定义见 [Agent 与 Session 生命周期架构设计](agent-session-lifecycle.md)。

## 与同类项目的关系

### Codex

Codex 区分 TurnContext 和每次 sampling request 的 StepContext。StepContext 会重新捕获 environment readiness、MCP runtime 和 AGENTS.md。

MiniCore 借鉴“用户 Turn 与模型 sampling step 分离”，但不在 active Turn 中重新读取 future Service state。MiniCore 的 sampling step 只重新组装 committed conversation，不重新捕获 Workspace、ToolSet、SkillCatalog 或 PromptSet。

### pi

pi 在一次 Agent run 内进行多次模型调用；其内部 turn 更接近 MiniCore 的一次逻辑模型调用，而不是 MiniCore 领域 Turn。pi 可以在 `prepareNextTurn` 时替换 system prompt、tools 和 model。

MiniCore 借鉴 immutable run snapshot 和 steering queue，但拒绝 active Turn 内替换 PromptSet、ToolSet 或 Model。

### Grok Build

Grok Build 在一个 prompt turn 中每轮重建 request，并注入 interjection、Skill reminder、MCP reminder 和 monitor event。

MiniCore 借鉴 persistence barrier 和 safe-point interjection，但不建立多个未提交的模型可见动态注入通道。

### Claude Code 与 Cursor

公开行为证明 interrupt/steer、Skill/MCP deferred loading、compaction、checkpoint 和 FollowUp 式连续任务有实际产品价值。两者内部实现并非完整公开，因此只作为行为参考，不用于推断 MiniCore 内部 ownership。

## 明确不建立的对象

当前不建立：

```text
TurnRunner 领域实体
TurnExecutionService
TurnContextFactory / TurnContextManager
ModelStepId / ModelAttemptId
ModelAttempt entity
TurnResources / RuntimeResources
通用 StepContext bag
FollowUp entity
AgentLoop registry
独立 ToolRunner / PromptRunner / SkillRunner
```

理由：

- Turn execution 的长期 owner 属于 Session execution；
- Context capture 当前只有一个真实调用方；
- 逻辑模型调用只需要 execution-local assembled context 和已有 fingerprints；
- provider attempt 属于 ModelGateway/telemetry；
- FollowUp 是下一 Turn 的调度请求；
- Prompt、Tool、Skill 已经由各自深模块拥有。

## 被否决的方案

### active Turn 每次模型调用重新捕获 Service state

否决原因：会让 PromptSet、ToolSet、SkillCatalog 和 Workspace authorization 在同一 Turn 内漂移，破坏 retry、ToolSpec/executor 一致性和 recovery。

### TurnExecutionContext 只是公开字段 bundle

否决原因：调用方可以取出 PromptSet、ToolSet 和 SkillCatalog 与其他 Turn 的对象交叉组合，跨模块校验重新散落。

### TurnExecutionContext 拥有完整 loop 和 storage

否决原因：会把 immutable execution binding 与 mutable Session ownership 混合，形成新的总控对象，并提前冻结阶段 5、6 的设计。

### AgentLoop 直接执行 Tool 和写 storage

否决原因：ToolRound conversation gate、approval、Sandbox、Session durable truth 和 Agent SDK 状态会混成一个不可测试循环。

### 任意 CurrentCall PromptContribution

否决原因：它既不属于固定 PromptSet，也不属于 committed conversation，形成不可审计、不可恢复的第三条模型可见状态通道。

### FollowUp 作为 TurnControl

否决原因：FollowUp 不改变 current Turn；它在 current Turn terminal 后开启新 Turn，并使用新的 Context。

## 基础不变量

- Turn execution 包含并驱动 AgentLoop，但 AgentLoop 不拥有领域事实和副作用；
- candidate admission 不是领域 Turn；
- initiating UserMessage append 是领域 Turn 开始线性化点；
- final assistant、TurnInterrupted或TurnFailed entry是领域Turn结束线性化点；
- TurnExecutionContext 不是领域 entity、Service 或通用 Resource owner；
- 同一 Turn pin exact AgentRevisionRef、SessionDefinitionRevision、WorkspaceSnapshot、SkillCatalog、ToolSet、PromptSet 和 Model；
- active Turn 不读取这些 Service 的 future current value；
- PromptSet 创建时绑定同一个 ToolSet 的 ToolPromptView；
- assembly 不接受任意 ToolPromptView；
- 所有模型可见动态事实来自 committed conversation；
- 下一次逻辑模型调用只从成功 append 并通过 conversation gate 的 facts 构建；
- Skill lazy load 使用 pinned SkillCatalogEntryRef；
- compose_message 和 initiating UserMessage append 前重新校验 authorization lease；
- ToolSpec 和 executor route 来自同一个 ToolSet；
- Tool side effect前append非模型可见tool_execution_started operational truth；
- Interaction request append-before-notify，resolution append-before-resume；
- Started/Abandoned ToolInvocation 和 incomplete ToolRound 不进入模型 conversation；
- provider retry 复用同一个 immutable assembled context；
- tool_round_completed append/apply后才开始下一次逻辑模型调用；
- WaitingApproval 时 Turn 仍是 Running；
- Steer append 后才影响下一次逻辑模型调用，不把 Turn 变为 Interrupted；
- FollowUp 创建新 Turn 和新 Context；
- ordinary reload 只影响 future Turn；
- security revocation 中断 active Turn，不替换其 Context；
- crash recovery 不自动重放 outcome unknown 的 Tool；
- exact Context 不可重建时 fail closed。

## Test Matrix

至少覆盖：

- Context capture 成功并绑定 exact AgentRevisionRef、SessionDefinitionRevision 和 WorkspaceSnapshot；
- SkillCatalog 与 ToolSet 并行 capture 后 PromptSet 绑定精确 fingerprints；
- capture 期间 ordinary Prompt/Tool/Skill reload；
- capture 最终 lease check 前发生 Workspace revocation；
- capture failure 不创建领域 Turn；
- initiating UserMessage append 前不调用模型或 Tool；
- initiating UserMessage append OutcomeUnknown 的幂等恢复；
- PromptSet assembly 无法接收另一个 ToolPromptView；
- compose_message 和 initiating UserMessage append 前发生 revocation 时丢弃未 append contribution；
- Skill lazy load 拒绝 current Catalog lookup 和 content hash 漂移；
- User Skill injection 进入 committed CanonicalUserMessage；
- 逻辑模型调用只接受 CommittedConversationView；
- provider retry 保持 AssembledModelContextFingerprint 不变；
- NeedTools 后、任何 side effect 前执行 control/revocation barrier；
- Tool side effect 前保存非模型可见 ToolInvocation Started/execution operational truth；
- InteractionRequested append-before-notify；
- InteractionResolved append-before-wake/side-effect；
- 每个ToolExecutionOutcome Completed时append role=tool message；全部matching messages存在后append tool_round_completed；
- 任一 ToolExecutionOutcome Abandoned 时不构造 ToolRound并进入 terminal arbitration；
- tool_round_completed前不开始下一次逻辑模型调用；
- tool_round_completed后AgentLoop adapter只消费committed delta；
- WaitingApproval 保持 TurnStatus Running / InteractionState Pending / ToolInvocation Started；
- Steer 在 sampling 或 WaitingApproval 期间入队并在 stable barrier append；
- Steer 与 final terminal append race；
- FollowUp 在前一 Turn terminal 后创建新 Context；
- compaction entry append/apply 后 conversation checkpoint 和 AssembledModelContextFingerprint 改变；
- ordinary reload 不影响 active Context；
- security revocation 阻止新的 model/tool/skill 操作；
- revoked result 未 append 时被丢弃；
- Tool side effect 已发起但 executor outcome unknown 时不自动重放，并把 ToolInvocation Abandoned；
- exact ToolResult 已知但 SessionWrite OutcomeUnknown 时先按 operation key reopen/replay：已 append 则 apply 原 receipt，已证明未 append 则只重试同一 tool message draft；未解析前不把它误判为 Abandoned；
- client disconnect 后 Interaction 保持 Pending，reconnect 使用相同 RequestId；
- duplicate/conflicting Interaction resolution；
- process restart后existing tool message保持Item Completed，但不补做缺失ToolRound completion；
- process restart 后 incomplete Turn 只 terminalize 一次；
- Turn terminal/restart 时 Pending Interaction 被持久关闭且不重复 resolution；
- Turn terminal/restart 时 Started ToolInvocation 被 truthful Completed 或 Abandoned；
- current Prompt/Tool/Skill 新版本不能替代 recovery manifest 中的旧版本；
- terminal 后释放 Context 且不清空 Runtime-global cache。

## 后续问题

1. AgentLoop 与 Rig 0.40.0 的具体 sans-I/O adapter 形状。
2. TurnModelSnapshot、ModelTurnSession 和 provider retry 的最终 interface。
3. Tool executor implementation identity/version 的注册和 recovery 规则。
4. PromptDefinition、SkillCatalog 和 ToolSet manifest reference 的 exact payload/reference 细节。
5. Steer 是否默认 preempt sampling，还是只在下一 stable barrier 消费。
6. FollowUp queue 的持久化、delivery acknowledgement 和多条消息 ordering。
7. Runtime pending Interaction query/event payload 与 ToolTurnPort 的 Session execution implementation。
8. standalone compaction、review 和 background work 是否使用 Turn execution。

## 设计进度

- [x] 区分领域 Turn、Turn execution 和 AgentLoop 边界。
- [x] 确定 Turn execution 包含并驱动 AgentLoop。
- [x] 确定 TurnExecutionContext 是不可变 execution binding，不是领域 entity 或 Service。
- [x] 固定 AgentRevisionRef、SessionDefinitionRevision、WorkspaceSnapshot、SkillCatalog、ToolSet、PromptSet 和 Model 的 Turn pinning。
- [x] 固定 Context capture DAG 和 reload 线性化。
- [x] 固定 PromptSet 与同一 ToolSet.prompt_view() 的绑定。
- [x] 固定 Skill lazy load 的 pinned identity。
- [x] 定义逻辑模型调用与 provider retry 边界，不增加 ModelStep struct/entity/ID。
- [x] 删除 arbitrary per-call ToolPromptView 和 dynamic contribution lane 的目标方向。
- [x] 固定 candidate admission、initiating UserMessage append 和 terminal entry 边界。
- [x] 区分 Steer 与 FollowUp。
- [x] 固定 ordinary reload 与 security revocation 语义。
- [x] 定义 fingerprint、manifest 和保守 crash recovery baseline。
- [x] 完成 Agent/Session lifecycle、exact revision pinning 和 transient state 分层。
- [x] 完成 operation-centric ToolInvocation Item、durable Interaction 和 terminal cleanup 语义。
- [x] 完成SessionWriter by-entry append、Message/Event layout、CommittedConversationDelta和recovery contract。
- [ ] 完成 Session execution owner 和并发状态机。
- [ ] 完成 ModelGateway 与 AgentLoop adapter。
