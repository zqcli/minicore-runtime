# Turn 执行模块与执行上下文架构设计

状态：当前权威架构（设计已冻结，生产实现待启动）
日期：2026-07-25

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
- WorkspaceSnapshot、SkillView、ToolSet、PromptSet和Model如何在一个Turn内稳定绑定；
- 每次模型调用如何只从 committed conversation 组装；
- Steer、FollowUp、retry、compaction 和 security revocation 如何影响 active Turn；
- crash recovery 可以安全恢复什么，何时必须 fail closed。

Turn、Item 与 Interaction 的领域语义以 [Turn、Item 与 Interaction 架构设计](turn-item-interaction.md) 为权威。

本文不重复定义：

- `SessionExecutor`、`SessionIngress`语义lane和异步operation的完整实现；这些以[Session Execution架构设计](session-execution.md)为权威；
- Runtime command、event和transport protocol；
- ModelGateway private `ProviderAdapter`的实现细节；首个production `RigProviderAdapter`只处理provider attempt映射与调用；
- 自研AgentLoop状态机的内部实现细节；其自研决策见[ADR 0115](../adr/0115-agent-loop-is-first-party-state-machine.md)。

## 决策摘要

- Turn execution 包含并驱动一个具体的 AgentLoop，但不与 AgentLoop 合并；
- SessionExecutor是active Turn mutable state的唯一owner；
- `TurnExecutionContext` 是 Turn-scoped、不可变的执行能力组合，不是领域 entity、Service 或通用 Resource owner；
- `TurnExecutionContext`固定exact AgentRevisionRef、SessionDefinitionRevision、WorkspaceSnapshot、captured SkillView、ToolSet、PromptSet和TurnModelSnapshot；
- active Turn 不重新读取 Workspace、Prompt、Tool、Skill 或 Model 的 future current value；
- 一次逻辑模型调用由committed conversation checkpoint、purpose、output contract、effective max_output_tokens和`AssembledModelContext`唯一确定；
- 不增加 `ModelStep` struct、ID、领域 entity 或公开协议对象；
- Session logical retry复用同一个不可变assembled context，不创建新的领域对象；
- PromptSet 是唯一产生 `AssembledModelContext` 的对象；
- PromptSet 在创建时绑定同一个 ToolSet 的 ToolPromptView，assembly 时不能再传入任意 Tool view；
- 模型可见动态事实必须来自 committed conversation，不建立未持久化的 dynamic contribution lane；
- Skill lazy load使用本Turn捕获的SkillView entry，并在实际读取时校验Workspace authorization；
- initiating UserMessage append 前不调用模型、不执行 Tool，也不发布领域 Turn；
- Steer 是 current Turn control input；成功 append 后才影响下一次逻辑模型调用；
- FollowUp 不属于 `TurnControl`，它在当前 Turn terminal 后开启新 Turn，并捕获新 Context；
- ordinary reload 只影响 future Turn；security-restricting update 撤销 lease 并中断 active Turn；
- same-Turn cold resume只有在Prompt/Model/Tool execution basis和Workspace reauthorization可重建时才允许；Skill旧正文不单独恢复，已经committed的Skill内容由conversation保留；否则保守中断。

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

一个 Turn execution 可以包含多个 AgentLoop segment：

- compaction entry append/apply 后必须从新的 committed conversation seed 重新建立 segment；
- Steer 默认由 `accept_committed_steer` 原地推进；实现也可以选择等价地重建 segment（ADR 0115）。

segment 重建不创建新 Turn，也不重新捕获 TurnExecutionContext。

## 对象关系

```text
SessionExecutor
└─ Active Turn execution
   ├─ Arc<TurnExecutionContext>       // Turn 内固定
   │  ├─ TurnModelSnapshot
   │  ├─ Arc<WorkspaceSnapshot>
   │  ├─ SkillViewContext
   │  ├─ Arc<SkillView>
   │  ├─ ToolSet
   │  └─ PromptSet
   ├─ CommittedConversationState      // 只消费成功 append receipts的trusted delta
   ├─ private AgentLoop state         // 底层 AgentLoop segment
   ├─ Turn control / cancellation     // 单调或可变执行状态
   └─ logical model-call state        // 串行局部值
```

领域对象保持简单：

```text
Turn domain
→ id、session_id、started_at、terminal-aware status

Turn start execution metadata
→ Agent/SessionDefinition/Workspace/Prompt/Tool/Skill view/TurnModel fingerprints and references

Turn Item collection
→ SessionStorage ordered projection，不内联 Turn head

Turn execution
→ Context、private AgentLoop state、conversation projection、control和logical retry
```

Turn领域对象不持有`TurnExecutionContext`、AgentLoop state、逻辑模型调用状态、SkillView、ToolSet或PromptSet。

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
    skill_context: SkillViewContext,
    skill_view: Arc<SkillView>,

    tool_set: ToolSet,
    prompt_set: PromptSet,

    fingerprint: ExecutionContextFingerprint,
    diagnostics: Arc<[TurnContextDiagnostic]>,
}
```

字段保持私有，避免调用方取得PromptSet、ToolSet或SkillView后与其他Turn的对象交叉组合。

`skill_service`只用于读取captured view entry：

```text
SkillViewContext + SkillEntry
→ SkillService::load(...)
```

它不能重新查询reload后的current SkillView或重新解释Workspace。

`TurnExecutionContext` 的“不可变”指：

- model identity 和 capability projection 不变；
- WorkspaceSnapshot 和各 view 不变；
- captured SkillView entries和fingerprint不变；
- ToolSpec、Exposure、executor route 和 ToolSet fingerprint 不变；
- PromptProfile、ToolPromptView、SkillPromptView和Prompt fingerprint不变。

以下执行状态不进入 fingerprint，也不表示 Context 发生变化：

- cancellation token 是否已触发；
- Workspace authorization lease 是否已撤销；
- Tool approval waiter；
- provider attempt；
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
→ 从captured SkillView解析SkillIntent
→ SkillService::load(context + entry)
→ SkillInjector::build
→ 校验authorization lease和source stamp；正文由CanonicalUserMessage fingerprint覆盖
→ PromptSet::compose_user_message

assemble_model_context
→ 校验 committed conversation proof、authorization lease 和 Context binding
→ PromptSet::assemble
```

Tool execution 不在 Context 上复制一层公开转发方法。SessionExecutor 与 Context 位于同一内部模块，通过 Context 中 pinned ToolSet 执行调用；Tool 的 route、approval、Sandbox 和 executor 仍只由 ToolSet 处理。

Context 的 fingerprint 和 diagnostics 也保持内部值，只有 storage、diagnostics 或 recovery 出现真实调用方时才暴露窄 getter。

## Context Capture

不建立 `TurnContextFactory`、`TurnContextManager`、公开 capture DTO 或第四个 Runtime Service。Context capture 是 Session execution 内部的一次深操作。

capture 必须从 Session execution 的一个原子 admission basis 取得：

```text
Submit `CommandId` from the CommandRequest envelope
candidate TurnId
SessionId + exact SessionDefinitionRevision
SessionDefinition.agent = exact AgentRevisionRef
SessionDefinition.workspace / model / prompt selection
exact AgentDefinition prompt defaults
Turn-scoped ToolExecutionControl、cancellation和ProgressEventPublisher
```

SessionDefinitionRevision保证AgentRevisionRef、Workspace、SessionModelConfig和SessionPromptSelection来自同一个committed definition。Prompt/Skill adapter不得按AgentId或SessionId回查current heads。

presentation层的前台/后台（用户当前查看哪个Session）不是capture输入，也不进model resolution、tool for_turn或`ExecutionContextFingerprint`；runtime对所有loaded Session一视同仁，共享Model资源由明确配额协调。File mutation queue按Session独立，跨Session共享Workspace由host/user负责隔离。若将来出现「后台自主Turn必须自动处理审批」这类需求，应建成tool execution路径上一个窄的、不进fingerprint的approval disposition，而不是回到capture层的前后台标记。

`candidate TurnId`只表示已预留的execution identity。Submit `CommandId`由外层admission reservation持有，仅用于当前Runtime内定位同一in-flight Submit、合并重复请求和精确Cancel；它不是额外submission key，不进入TurnExecutionContext或fingerprint，也不承诺跨崩溃恢复。OutcomeUnknown不靠它reopen或replay-by-key，恢复统一读committed prefix加状态检查。capture成功不代表领域Turn已经创建。

## Capture 依赖图

Context capture 的逻辑依赖是 DAG，不要求建立跨 Service 的全局锁或 Resource generation：

```text
exact SessionDefinitionRevision
├─ exact AgentRevisionRef / AgentPromptSelection / SessionPromptSelection
├─ PromptService.current_view() → Arc<PromptResourceView>
├─ SessionDefinition.model
│  └─ ModelGateway.resolve_for_turn(...) → TurnModelSnapshot
└─ Arc<WorkspaceSnapshot>
       ├─ SkillService::current_view(SkillViewContext {
       │    agent, session_id, session_revision, workspace: workspace.skill_context()
       │  })
       │  └─ Arc<SkillView>
       └─ ToolService::for_turn(ToolTurnContext {
            agent, session_id, session_revision, turn_id,
            workspace: workspace.tool_context(),
            tool_calling: model.capabilities().tool_calling.clone(),
            execution_control, cancellation, progress_events
          })
          └─ ToolSet

SkillView.prompt_view()
+ ToolSet.prompt_view()
+ WorkspaceSnapshot.prompt_context()
+ PromptResourceView
+ Agent/Session Prompt selection
+ TurnModelSnapshot
→ PromptService::for_turn(...)
→ PromptSet

all child fingerprints
→ final validation
→ TurnExecutionContext
```

SkillView和ToolSet不互相依赖，实现可以并行捕获。PromptSet必须在PromptResourceView、SkillPromptView和ToolPromptView就绪后创建。

capture 完成前必须再次检查：

- Workspace authorization lease 未撤销；
- Turn cancellation 未触发；
- Session admission reservation 仍然有效；
- SkillViewContext / ToolTurnContext与captured AgentRevisionRef、SessionDefinitionRevision和candidate TurnId一致；
- ToolSet.prompt_view().tool_set_fingerprint 等于 parent ToolSet fingerprint；
- PromptSet记录的PromptResourceView、ToolSet、SkillView、Workspace和Model fingerprint与实际对象一致。

## Capture 线性化

Prompt、Tool 和 Skill 是独立领域，没有跨三者的 global publication instant。

ordinary reload 在 capture 期间发生时：

- 某子系统在自己的 capture 线性化点之前发布的新值，可以被本次 Context 捕获；
- 已经捕获的值不被后续 reload 原地替换；
- PromptSet只绑定实际捕获的PromptResourceView、SkillPromptView和ToolPromptView；
- 最终发布的是一个内部一致的组合，而不是“同一纳秒”的全局资源快照。

capture完成后的Skill reload不改变active Turn持有的SkillView。尚未lazy-load的entry按captured location读取当前文件内容；读取前仍必须校验authorization lease和source stamp。已经加载的`Arc<LoadedSkill>`保持不变。

这不引入通用 ResourceManager。

如果一个 versioned extension package 必须原子贡献 Prompt、Skill 和 Tool，应由独立 `ExtensionSet` 提供 package-level publication，而不是让 TurnExecutionContext 推断跨领域事务。

## Admission

initiating UserMessage需要PromptSet规范化，而PromptSet又需要本次admission捕获的ToolSet和SkillView；其中 ToolTurnContext、cancellation 和 Turn-scoped grant 需要已预留的 candidate TurnId。因此 admission 必须使用未发布的 candidate，而不是先创建空 Turn。PromptSet 本身不保存 candidate TurnId。

推荐顺序：

```text
SessionLifecycle = Open
+ SessionLoadState = Loaded
+ SessionReadiness = Ready
+ SessionExecutionState = Idle
→ reserve admission slot + Submit CommandId + candidate TurnId
→ SessionExecutionState = Starting
→ capture current exact SessionDefinitionRevision
→ check AgentStatus = Enabled and read exact AgentRevisionRef
→ capture TurnExecutionContext
→ Context.compose_message(PromptIntent)
   └─ 内部按需完成 pinned Skill load / injection
→ 与Agent status update串行化
→ 最终检查AgentStatus = Enabled
→ append TurnContext entry → apply its AdvanceOnly delta
→ append initiating UserMessage(source = Input, context_entry_id) → apply its Append delta
→ 确认两个append outcome并结束Agent status synchronization
→ 发布 SessionExecutionState = Running / TurnStatus = Running
→ 启动 private AgentLoop adapter
→ 第一次逻辑模型调用
```

initiating UserMessage entry是领域Turn的开始线性化点。Agent status synchronization只覆盖final Enabled check到该entry append outcome确认；TurnContext entry本身不创建Turn。disable/delete必须与该区间串行化。

在此之前：

- 不调用 ModelGateway；
- 不执行 Tool；
- 不创建 Tool approval Interaction；
- 不发布 committed Turn/UserMessage；
- context capture、source read 和 cache fill 不构成领域事实；
- `compose_message` 和 initiating UserMessage append 前都必须再次检查 Workspace authorization lease 与 Turn cancellation；
- revocation 后尚未 append 的 UserMessage、Steer 和 PromptContribution 全部丢弃。

capture、Skill load、UserMessage composition、Context append或UserMessage append失败时，释放candidate和局部Context，SessionExecutionState返回Idle；仅有orphan Context entry不创建空Turn或`Failed` Turn。

如果initiating UserMessage append返回NotCommitted（写入尚未开始，可安全重试同一 draft），可以重试同一 candidate 的同一 append。如果返回OutcomeUnknown（写入已开始但 ack 丢失），Session execution保守终结当前 admission、poison 该 writer、不在本 run 重试该 append、也不分配另一个 TurnId；OutcomeUnknown 不携带 operation_key，不在本 run reopen 或 replay-by-key。此时该 unacked initiating UserMessage 可能丢失，Turn 视为未开始，用户可重新提交。恢复靠下次 load 读 committed prefix 加状态检查，幂等性由状态判断保证，不依赖 operation key。

Session pin exact AgentRevisionRef：

- Agent current revision 更新不改变 candidate、active 或 future Turn；
- Session 显式升级会创建新的 SessionDefinitionRevision，只影响 update 后开始 admission 的 future Turn；
- Agent Disabled/Deleted与initiating UserMessage append通过同一个Agent status synchronization机制线性化：status mutation先完成则start被拒绝，message append先完成则active Turn继续；
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

因此assembly input按purpose使用[Prompt子系统定义的closed variants](prompt.md#模型上下文组装)：`AgentRun`只接收trusted committed conversation与output contract，`CompactionSummary`只接收trusted scope-aware source与fingerprinted directive。variant确定ModelCallPurpose；Compaction source同样来自CommittedConversationState的trusted view，不形成第四类临时model-visible input。

不接收：

```text
任意 ToolPromptView
任意 PromptContribution[]
任意 current Workspace context
任意current SkillView
裸 Vec<MessageRecord>
```

ToolPromptView已经被PromptSet固定。Workspace Prompt、SkillPromptView metadata和其他Turn-static baseline也已经进入PromptSet。

动态 PromptContribution 必须在模型可见前变成 committed fact：

- 用户显式Skill invocation：`Context.compose_message()`内完成load/injection，并规范化进`CanonicalUserMessage`，随UserMessage append；
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
+ effective max_output_tokens
+ AssembledModelContextFingerprint
```

SessionExecutor使用`PromptAssemblyInput`调用Context，再通过validated constructor形成并保留完整immutable `ModelCallRequest`供Session logical retry复用。不增加`ModelStep`、`ModelStepId`、`ModelAttempt`或额外fingerprint类型。

以下行为不改变逻辑模型调用identity：

- SessionExecutor等待logical retry backoff；
- delivery-safe terminal error后，再次调用Gateway并复用同一个immutable `ModelCallRequest`。

每次Gateway operation仍是独立single provider attempt；provider connection retry、401 resend和transport fallback不属于MVP。

以下变化必须开始新的逻辑模型调用：

- model-visible `TranscriptFingerprint`改变；
- `tool_round_completed`、Steer或Compaction成功append/apply并改变conversation；
- ModelCallPurpose 改变；
- OutputContract改变；
- effective max_output_tokens改变；
- PromptSet、ToolSet、SkillView、Workspace或TurnModelSnapshot改变。

后一个条件在 active Turn 中不应发生；发生时必须中断当前 Turn，而不是悄悄替换 Context。

## AgentLoop Contract

AgentLoop 应保持 sans-I/O。它在逻辑上产生三类动作：

```text
NeedModel { output_contract }
NeedTools { calls }
Finished { message draft }
```

AgentLoop 是自研的同步协议状态机（ADR 0115），以直接方法调用表达这些动作，不由 Rig 或其他 SDK 驱动。不冻结 `AgentLoopFactory`、`AgentRun` 或 public `AgentLoopAction` trait/enum；只有出现第二个真实 AgentLoop implementation 时才建立稳定 seam。

NeedModel只表示普通`AgentRun`需要模型输出；CompactionSummary由SessionExecutor在AgentLoop之外启动，AgentLoop不能选择ModelCallPurpose。`Finished`只表示candidate final：Steer FIFO为空时保存为Assistant Final；FIFO非空时保存为Assistant Continue并重建AgentLoop segment。

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
  → 取得 AgentLoop 下一动作

  NeedModel
  → steer FIFO非空时pop_front一条并append/apply Steer
  → Context.assemble_model_context(committed conversation)
  → ModelGateway.generate_model_turn(immutable ModelCallRequest)
  → 把 ModelOutput 交回 AgentLoop adapter

  NeedTools
  → observe EmergencyControl Cancel/revocation epoch
  → validate current authorization
     └─ Cancel/revocation wins：进入Interrupted cleanup
  → 取得WorkspaceCommitAuthorization
  → append one assistant/intermediate message
     └─ ordered reasoning/text/tool_call content；每个ToolCall带ItemId
  → apply assistant entry delta（Started ToolInvocation，conversation AdvanceOnly）
  → release WorkspaceCommitAuthorization
  → pinned ToolSet.execute(ToolExecutionRequest[])
     └─ approval 通过 durable Interaction request/resolution
     └─ ask-user route通过durable UserQuestion等待typed answer，并产生PreExecution outcome
     └─ ToolExecutionControl record ToolExecutionStarted before side effect
  → ToolExecutionOutcome[]
     ├─ 每个Completed：append matching role=tool message → apply
     ├─ 全部Completed：append tool_round_completed → apply conversation delta
     └─ 任一Abandoned：append ToolAbandoned → apply并确定Turn terminal result
  → 把model-visible committed conversation change交回AgentLoop adapter
  → steer FIFO非空时pop_front一条并append/apply Steer
  → continue

  Finished
  → 与Cancel/revocation仲裁
  → steer FIFO非空：append model-visible Assistant Continue
     → pop_front一条Steer并append/apply
     → rebuild AgentLoop segment并continue
  → steer FIFO为空：SessionExecutionState = Finishing
     → append assistant/final message → apply terminal delta
     → release Context
     → SessionExecutionState = Idle
```

ToolExecutionControl要求的每次durable append都必须先通过storage-owned apply_committed更新全部projection，再返回approval/UserAnswer或允许side effect。下一次模型调用只能发生在`tool_round_completed`成功append并apply后。

以下内容不能进入下一次逻辑模型调用：

- streaming assistant draft；
- Started 或 Abandoned ToolInvocation；
- 尚无`tool_round_completed`引用的Tool message；
- pending Interaction；
- accepted 但未 append 的 Steer；
- compaction draft；
- failed provider attempt的partial output。

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

WaitingApproval不是Interrupted。此时到达的Steer只进入current Turn的bounded FIFO，不作为approval resolution，也不preempt当前Interaction/ToolRound。

只有显式 cancel、runtime shutdown、security revocation 或不可恢复错误才使 Turn进入 terminal status。

## WaitingForUserInput

等待UserQuestion时，Turn也没有结束：

```text
TurnStatus = Running
SessionExecutionState = Running
TurnExecutionPhase = WaitingForUserInput
InteractionState = Pending
ToolInvocationState = Started
```

首版ask-user route在`ToolExecutionStarted`、file mutation ticket reservation和外部副作用之前调用`ToolExecutionControl::request_user_question`。等待期间不预留mutation ticket，也不持有`WorkspaceCommitAuthorization`，同一assistant step的sibling ToolCall尚未启动；SessionExecutor继续处理Interaction resolution、Cancel、Unload和Snapshot；用户沉默时保持Pending，不产生默认Deny。答案形成`PreExecution` ToolResult后，同一ToolRound恢复普通调度；其他Session始终由各自Executor独立推进。

WaitingForUserInput不是Interrupted。此时到达的Steer只进入current Turn的bounded FIFO，不作为UserAnswer，也不preempt当前Interaction。

## Steer

Steer是current Turn的queued input，不是开启新Turn的普通UserMessage。它使用`SteerQueue<TurnId>`中的bounded per-Turn FIFO，不取消当前Model/Tool operation。

语义：

```text
Steer(expected TurnId)
→ 进入该Session的`SteerQueue<expected TurnId>`
→ 验证后push_back到该Turn FIFO
→ 当前assistant/tool step完整结束
→ 下一次Model调用前pop_front一条并append/apply Steer
→ 下一次逻辑模型调用看见Steer
```

Steer可以在Model或Tool执行期间被接收，但不会修改已经发送给provider的AssembledModelContext。

基础策略：

- Sampling期间不cancel、不推进execution_version；已完成response必须先保存为ToolCall step或无ToolCallAssistant Continue step；
- AgentLoop返回NeedTools后、任何Tool side effect前必须重新检查Cancel state和current authorization；
- Tool 已经可能产生副作用且 exact outcome 可得时，默认等待该轮调用形成完整 Tool messages 后再 append `tool_round_completed`；executor/side-effect outcome unknown 时 ToolInvocation 进入 Abandoned 并 terminalize Turn；
- approval/UserQuestion wait、sleep 或其他明确可中断 Tool 可以形成 cancelled ToolResult，再完成 ToolRound；
- candidate final与Steer FIFO由SessionExecutor线性化；已有queued Steer时candidate final保存为non-terminal Continue，queue为空时才append Final；
- final terminal entry先完成后到达的Steer不再属于该Turn。

Steer不重新捕获WorkspaceSnapshot、SkillView、ToolSet、PromptSet或Model。

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
- 不复用旧ToolSet、PromptSet或SkillView；
- 可以看到当前 Turn 期间完成的 ordinary reload；
- 失败或取消不会改变已经 terminal 的前一个 Turn。

FollowUp使用`FollowUpQueue` bounded FIFO；它最多获得一次连续admission优先，若上一Turn由FollowUp启动且下一次Idle decision有external Submit，则先选Submit。Submit不是隐式FollowUp，未选中且Session再次Busy时明确返回。该lane不是独立领域entity，也不拥有Session状态。

## Retry

模型调用失败恢复分为两个边界：

```text
single provider attempt
→ ModelGateway负责一次request/stream和typed terminal mapping

logical model-call retry
→ Session execution负责是否复用同一个immutable ModelCallRequest
```

MVP不执行provider transparent retry、401 refresh-and-resend或transport fallback；Rig和底层provider SDK automatic retry固定为0。AgentRun默认最多3次logical retry，CompactionSummary最多1次，完整policy见[ADR 0119](../adr/0119-model-calls-use-session-logical-retries.md)。

同一次逻辑模型调用的retry只能在旧Model RunningOperation已经terminal/remove或被安全drop并关闭结果路径后启动，并且必须满足：

- ExecutionContextFingerprint 不变；
- conversation checkpoint 不变；
- AssembledModelContextFingerprint 不变；
- purpose、output contract和effective max_output_tokens不变；
- 没有新的 committed fact；
- Tool 没有因为该 retry 被重新执行。

partial stream 是 draft。retry 前关闭该 draft lifecycle，但不写入 conversation。

`tool_round_completed` 已成功 append/apply 后的下一次模型调用可以 retry，因为 Tool 不会重放。Tool executor outcome unknown 时不能自动重放 Tool；SessionWrite OutcomeUnknown 时保守终结当前 round、恢复靠 committed prefix，不重放 Tool，也不能混为 executor outcome unknown。

## Compaction

Compaction改变committed conversation，但不改变TurnExecutionContext。完整规则见[Compaction架构设计](compaction.md)。

```text
AgentLoop NeedModel安全点
→ PromptSet assembly显示soft pressure/local overflow，或provider返回ContextOverflow
→ Compaction选择ConversationPrefix或ActiveTurnCompletedPrefix stable-unit cut
→ 保留exact initiating/Steer UserMessage anchors、真实protected region和recent exact tail
→ Context.assemble_model_context(
     PromptAssemblyInput::CompactionSummary {
       source: trusted_scope_aware_view,
       directive,
     }
   )
→ SummaryModel call
→ revalidate source checkpoint/control/authorization
→ append/apply StoredCompaction
→ Replace committed conversation projection
→ rebuild ConversationSeed和AgentLoop segment
→ 新的逻辑模型调用
```

Compaction summary call仍通过同一个PromptSet和exact TurnModelSnapshot。普通Agent/Session/Workspace/Tool/Skill静态instructions不进入summary；下一次AgentRun assembly从同一个TurnExecutionContext重新注入。

`TurnExecutionPhase = Compacting`期间TurnStatus保持Running，Steer排队，Cancel和security revocation可以在append前获胜。每个active instruction segment的completed coverage frontier可在完整ToolRound安全点滚动推进；滚动checkpoint用`previous_checkpoint`指向当前effective checkpoint，并从backing compaction派生covered-through provenance。exact active UserMessage anchors或真实protected region自身过大时返回`ProtectedRegionTooLarge`。同一source/scope-frontier的hard recovery只尝试一次，但成功推进后允许在`max_compactions_per_turn`内再次compact。

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
- best-effort cancel当前provider/tool operation；
- 不撤销已经 committed conversation fact；
- 不伪装回滚已经完成的外部副作用；
- 通过一个 terminal entry 结束 Turn。

### Security Revocation

Workspace security-restricting update：

```text
WorkspaceAuthorizationControl.revoke()
→ authorization lease 失效
→ 通知 SessionExecutor
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
- ToolSet、SkillView或PromptSet局部值可以先后创建；
- 只有全部成功并完成最终 lease/fingerprint 校验后才发布完整 Context；
- 失败时 drop 局部值，不更新 Session current state；
- Context capture 本身不创建 Item、Interaction、grant 或 durable Turn；
- initiating UserMessage append 前不产生模型或 Tool 副作用。

Tool外部副作用与model-visible ToolRound completion无法形成通用分布式事务。assistant tool_call content先保存Started ToolInvocation；执行任何Tool side effect前，SessionStorage必须append非模型可见的`tool_execution_started`event；approval request和resolution也必须先durable append。它们不进入Prompt conversation，也不允许下一次逻辑模型调用看见半个ToolRound。

若exact ToolResult已知但role=tool message append返回NotCommitted（写入尚未开始，可安全重试），可以只重试同一entry；返回SessionWrite OutcomeUnknown时保守终结当前round，不在本run重试该append，该tool message视为未持久化，下次load按committed prefix恢复（round不完整则Abandon，模型重跑工具），不reopen/replay-by-key。Tool message都已append后，`tool_round_completed`同样按此规则append；其OutcomeUnknown也保守终结、恢复靠committed prefix，不重放Tool。只有executor/side-effect outcome unknown时，同一个ToolInvocation Item才进入Abandoned且不生成synthetic ToolResult。完整schema见[Conversation 与 SessionStorage 架构设计](conversation-storage.md)。

## Fingerprint

至少需要：

```text
WorkspaceFingerprint 及各 view fingerprint
SkillViewFingerprint
ToolSetFingerprint
PromptFingerprint
TurnModelFingerprint
ExecutionContextFingerprint
AssembledModelContextFingerprint
```

### SkillViewFingerprint

覆盖：

- 稳定排序后的SkillView entries；
- SkillId、location identity和source stamp；
- 模型可见metadata projection；
- view filtering/ordering algorithm version。

该fingerprint用于capture一致性、cache和diagnostics，不是Skill版本或旧正文恢复协议。

### ToolSetFingerprint

覆盖：

- 规范化 ToolSpec；
- Exposure 和 Deferred projection；
- executor route identity/version；
- WorkspaceToolFingerprint / WorkspaceAccessFingerprint；
- Tool policy revision；
- ToolCallingCapabilities projection；
- ToolSet capture algorithm version。

随机 executor pointer、锁状态、approval waiter 和 cancellation 不进入 fingerprint。

### ExecutionContextFingerprint

由以下 child fingerprint 组合：

```text
AgentRevisionRef
SessionDefinitionRevision
TurnModelFingerprint
WorkspaceFingerprint 及相关 view fingerprint
SkillViewFingerprint
ToolSetFingerprint
PromptFingerprint
capture schema/algorithm version
```

`TurnId`和`SessionId`由TurnContext entry及其initiating UserMessage reference绑定，不建议仅为实例区分而加入内容fingerprint。

逻辑模型调用无需额外fingerprint类型。Session logical retry必须先证明`ConversationCheckpoint`不变；在此前提下，`ExecutionContextFingerprint + TranscriptFingerprint + purpose + output contract + effective max_output_tokens + AssembledModelContextFingerprint`足够判断是否仍是同一次调用。仅TranscriptFingerprint相同不足以忽略AdvanceOnly ledger变化。

Fingerprint 用于一致性、审计、cache 和 recovery 比对，不代替 secret redaction，也不是完整恢复数据。

## Execution Context Metadata

如果需要精确审计或 cold recovery，TurnContext entry应保存或引用以下信息；哪些内容内嵌、哪些使用exact content reference由对应定义子系统闭合：

```text
capture schema/version
SessionId / TurnId
SessionDefinitionRevision
AgentRevisionRef
TurnModel exact reference
WorkspaceSnapshot / authority revision reference
SkillView fingerprint and selected entry source references
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

baseline 采用保守恢复：

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

只有同时满足以下条件，才允许 exact same-Turn resume：

- exact SessionDefinitionRevision 和 AgentRevisionRef 仍可读取；
- Workspace 可以重新授权，并且 capability ceiling 不比原 Context 更宽；
- PromptResourceView与Prompt fingerprint仍匹配，或旧Turn按recovery policy中断；
- SkillView不要求恢复旧正文；已经committed的Skill contribution仍由conversation保存；
- ToolSpec 与 executor route 有稳定、可重建的 implementation version；
- Model identity、capability 和请求语义可重建；
- pending Tool side effect outcome 已知或有专用 repair protocol。

任一条件不满足时不能使用 Agent current revision、current SessionDefinition 或其他 current replacement 冒充旧 Context。

Tool子系统尚未定义稳定executor implementation identity，因此不承诺透明cold resume。Pending Interaction必须在TurnInterrupted/Failed前以cancelled、expired或recovery reason持久关闭；Started ToolInvocation必须已有truthful role=tool message或ToolAbandoned，不能只删除内存waiter/task。

## Diagnostics 与释放

Context capture diagnostics 至少记录：

- captured AgentRevisionRef、SessionDefinitionRevision、PromptFingerprint、WorkspaceRevision 和 TurnModelFingerprint；
- Workspace、SkillView、ToolSet、PromptSet和Model fingerprint；
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

[Session Execution架构设计](session-execution.md)确定：

- 每个loaded Session由一个`SessionExecutor`拥有执行期mutable state；
- 一个Runtime允许多个SessionExecutor同时Running；
- `SessionIngress`按语义分为TurnAdmission、per-Turn Steer、FollowUp、InteractionControl、ToolControl、EmergencyControl、LifecycleControl和SnapshotMailbox；
- Cancel/revocation不等待普通work lane，GetSnapshot从immutable published view读取，持续观察使用snapshot-first subscription；
- Context、Model和Tool使用cancellable `RunningOperation`；
- operation result使用`SessionId + TurnId + execution_version + OperationType`校验；
- private AgentLoop只返回NeedModel、NeedTools或Finished；
- FollowUp使用bounded process-local FIFO；
- progress通过独立`ProgressEventPublisher`发布；
- AgentLoop是自研同步状态机，由主循环直接调用，不存在monolithic adapter task（ADR 0115）。

普通Submit在Starting/Running/Finishing时返回SessionBusy；FollowUp在Running或Finishing期间可进入bounded FIFO，在active Turn terminal后重新进入普通admission，不属于current Turn control。CancelAccepted只停止current Turn推进，不清除FollowUp。

Session durable lifecycle、load/readiness/execution state 的完整定义见 [Agent 与 Session 生命周期架构设计](agent-session-lifecycle.md)。

## 与同类项目的关系

### Codex

Codex 区分 TurnContext 和每次 sampling request 的 StepContext。StepContext 会重新捕获 environment readiness、MCP runtime 和 AGENTS.md。

MiniCore借鉴“用户Turn与模型sampling step分离”，但不在active Turn中重新读取future Service state。MiniCore的sampling step只重新组装committed conversation，不重新捕获Workspace、ToolSet、SkillView或PromptSet。

### pi

pi 在一次 Agent run 内进行多次模型调用；其内部 turn 更接近 MiniCore 的一次逻辑模型调用，而不是 MiniCore 领域 Turn。pi 可以在 `prepareNextTurn` 时替换 system prompt、tools 和 model。

MiniCore 借鉴 immutable run snapshot 和 steering queue，但拒绝 active Turn 内替换 PromptSet、ToolSet 或 Model。

### Grok Build

Grok Build 在一个 prompt turn 中每轮重建 request，并注入 interjection、Skill reminder、MCP reminder 和 monitor event。

MiniCore借鉴durable update ordering和operation完成后的interjection，但不建立多个未提交的模型可见动态注入通道。

### Claude Code 与 Cursor

公开行为证明 interrupt/steer、Skill/MCP deferred loading、compaction、checkpoint 和 FollowUp 式连续任务有实际产品价值。两者内部实现并非完整公开，因此只作为行为参考，不用于推断 MiniCore 内部 ownership。

## 明确不建立的对象

不建立：

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
- Context capture 只有一个真实调用方；
- 逻辑模型调用只需要 execution-local assembled context 和已有 fingerprints；
- provider attempt 属于 ModelGateway/telemetry；
- FollowUp 是下一 Turn 的调度请求；
- Prompt、Tool、Skill 已经由各自深模块拥有。

## 被否决的方案

### active Turn 每次模型调用重新捕获 Service state

否决原因：会让PromptSet、ToolSet、SkillView和Workspace authorization在同一Turn内漂移，破坏retry、ToolSpec/executor一致性。

### TurnExecutionContext 只是公开字段 bundle

否决原因：调用方可以取出PromptSet、ToolSet和SkillView与其他Turn的对象交叉组合，跨模块校验重新散落。

### TurnExecutionContext 拥有完整 loop 和 storage

否决原因：会把 immutable execution binding 与 mutable Session ownership 混合，形成新的总控对象，并越界冻结 Session execution 与 ModelGateway 的设计。

### AgentLoop 直接执行 Tool 和写 storage

否决原因：ToolRound模型可见性规则、approval、Sandbox、Session durable truth和Agent SDK状态会混成一个不可测试循环。

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
- 同一Turn固定exact AgentRevisionRef、SessionDefinitionRevision、WorkspaceSnapshot、captured SkillView、ToolSet、PromptSet和TurnModelSnapshot；
- active Turn 不读取这些 Service 的 future current value；
- PromptSet 创建时绑定同一个 ToolSet 的 ToolPromptView；
- assembly 不接受任意 ToolPromptView；
- 所有模型可见动态事实来自 committed conversation；
- 下一次逻辑模型调用只从成功append并已进入conversation projection的facts构建；
- Skill lazy load使用captured SkillEntry并重新校验读取授权；
- compose_message 和 initiating UserMessage append 前重新校验 authorization lease；
- ToolSpec 和 executor route 来自同一个 ToolSet；
- Tool side effect前append非模型可见tool_execution_started operational truth；
- Interaction request append-before-notify，resolution append-before-resume；
- Started/Abandoned ToolInvocation 和 incomplete ToolRound 不进入模型 conversation；
- Session logical retry复用同一个immutable assembled context；
- tool_round_completed append/apply后才开始下一次逻辑模型调用；
- WaitingApproval和WaitingForUserInput时Turn仍是Running；
- Steer在完整assistant/tool step后FIFO出队，append后才影响下一次逻辑模型调用，不把Turn变为Interrupted；
- FollowUp 创建新 Turn 和新 Context；
- ordinary reload 只影响 future Turn；
- security revocation 中断 active Turn，不替换其 Context；
- crash recovery 不自动重放 outcome unknown 的 Tool；
- same-Turn execution basis不可重建时保守中断，不使用current resource冒充旧Context。

## Test Matrix

至少覆盖：

- Context capture 成功并绑定 exact AgentRevisionRef、SessionDefinitionRevision 和 WorkspaceSnapshot；
- PromptResourceView、SkillView与ToolSet capture后，PromptSet绑定对应fingerprints；
- capture 期间 ordinary Prompt/Tool/Skill reload；
- capture 最终 lease check 前发生 Workspace revocation；
- capture failure 不创建领域 Turn；
- initiating UserMessage append 前不调用模型或 Tool；
- initiating UserMessage append NotCommitted 时重试同一 draft；OutcomeUnknown 时保守终结、不在本 run 重试该 append、不分配另一个 TurnId，Turn 视为未开始，用户可重新提交；
- PromptSet assembly 无法接收另一个 ToolPromptView；
- compose_message 和 initiating UserMessage append 前发生 revocation 时丢弃未 append contribution；
- Skill lazy load不查询reload后的current SkillView；未加载entry允许读取location当前正文；
- User Skill injection 进入 committed CanonicalUserMessage；
- 逻辑模型调用只接受 CommittedConversationView；
- Session logical retry保持AssembledModelContextFingerprint不变；
- NeedTools后、任何side effect前重新检查Cancel state和current authorization；
- Tool side effect 前保存非模型可见 ToolInvocation Started/execution operational truth；
- InteractionRequested append-before-notify；
- InteractionResolved append-before-wake/side-effect；
- `request_user_question`在ToolExecutionStarted和file mutation ticket reservation之前创建UserQuestion；
- WaitingForUserInput不预留mutation ticket，sibling ToolCall尚未启动，其他Session可继续运行；
- 每个ToolExecutionOutcome Completed时append role=tool message；全部matching messages存在后append tool_round_completed；
- 任一 ToolExecutionOutcome Abandoned 时不构造 ToolRound并进入 terminal arbitration；
- tool_round_completed前不开始下一次逻辑模型调用；
- tool_round_completed后AgentLoop adapter只消费committed delta；
- WaitingApproval 保持 TurnStatus Running / InteractionState Pending / ToolInvocation Started；
- WaitingForUserInput保持TurnStatus/SessionExecutionState Running，UserAnswer恢复同一Turn而不是创建UserMessage；
- Sampling Steer不取消旧Model；WaitingApproval/WaitingForUserInput/ExecutingTools Steer在current ToolRound完成后FIFO出队；
- Steer 与 final terminal append race；
- FollowUp 在前一 Turn terminal 后创建新 Context；
- compaction entry append/apply 后 conversation checkpoint 和 AssembledModelContextFingerprint 改变；
- ordinary reload 不影响 active Context；
- security revocation 阻止新的 model/tool/skill 操作；
- revoked result 未 append 时被丢弃；
- Tool side effect 已发起但 executor outcome unknown 时不自动重放，并把 ToolInvocation Abandoned；
- exact ToolResult 已知但 role=tool message append NotCommitted 时只重试同一 draft；SessionWrite OutcomeUnknown 时保守终结当前 round、不在本 run 重试该 append，该 tool message 视为未持久化，下次 load 按 committed prefix 恢复（round 不完整则 Abandon）；
- client disconnect 后 Interaction 保持 Pending，reconnect 使用相同 RequestId；
- duplicate/conflicting Interaction resolution；
- process restart后existing tool message保持Item Completed，但不补做缺失ToolRound completion；
- process restart 后 incomplete Turn 只 terminalize 一次；
- Turn terminal/restart 时 Pending Interaction 被持久关闭且不重复 resolution；
- Turn terminal/restart 时 Started ToolInvocation 被 truthful Completed 或 Abandoned；
- active Turn不原地替换PromptSet、ToolSet或SkillView；进程重启后旧Prompt fingerprint无法重建时保守中断Turn；
- terminal 后释放 Context 且不清空 Runtime-global cache。

## 后续问题

1. ~~AgentLoop 与 Rig 0.40.0 的具体 sans-I/O adapter 形状~~（已由 ADR 0115 关闭：AgentLoop 自研，Rig 不参与 loop）。
2. Rig 0.40.0对TurnModelSnapshot、finish reason、reasoning、single-attempt error/delivery state和SDK retry=0的ModelGateway private adapter映射。
3. Tool executor implementation identity/version 的注册和 recovery 规则。
4. PromptResourceView、SkillView和ToolSet fingerprint/reference的最终持久化细节。
5. Runtime pending Interaction公开query/event payload。
6. standalone compaction、review和background work是否使用Turn execution。
