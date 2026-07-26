# MiniCore 架构（V2 当前权威）

本文档是 MiniCore 原生 Agent harness runtime core 的架构总入口。MiniCore 提供可嵌入的运行时核心、协议、会话、工具、事件和 harness 编排能力；CLI、TUI 和 GUI 产品在独立仓库中以 MiniCore 为核心接入。详细设计按可实现的运行时模块拆到 [`docs/modules/`](modules/README.md)。

## 版本状态

| 版本 | 状态 |
| --- | --- |
| **V1** | 已归档，仅用于历史参考。完整保存在 [`docs/archive/v1/`](archive/v1/README.md) 和 Git history 中，不得作为当前实现或新开发的设计依据。 |
| **V2** | 当前权威架构。目标设计已冻结，生产实现待启动；下一里程碑为阶段6–8模型调用协同交付束。 |

V1 → V2 的版本迁移记录见 [`docs/migration/v1-to-v2.md`](migration/v1-to-v2.md)。

> **权威顺序**：当前架构文档（本文与 `docs/modules/`）→ 当前 ADR（`docs/adr/`，0100+）→ `docs/research/` → `docs/archive/v1/`（非权威）。正式文档不再链接 V1 归档；只有迁移说明与新 ADR 的历史依据部分可以引用它。

## 设计定位

MiniCore 使用 Rig 作为原生 Agent SDK，但 Rig 保持为实现细节。下游 CLI、TUI、GUI 宿主只通过 `MiniCoreRuntime` 的 command、query、event 和 snapshot 交互，不依赖 Rig 类型、模型提供方类型或工具实现细节。pi、Codex 等项目可以作为设计参考，但除非文档明确标注兼容契约，否则其类型、调用方式和行为都不是 MiniCore 的兼容目标。

MiniCore 不重新实现 provider SDK 的底层 sampling/tool-call protocol，也不重新实现 provider HTTP client。模型返回 ToolCall 时，Session execution 先 append 完整 assistant/intermediate entry，再由 ToolService 执行工具治理；每个 truthful result 独立 append 为 tool message，最后以 `tool_round_completed` 推进模型可见 conversation。真实模型调用通过共享 `ModelGateway` 的一个深异步 operation 复用 Rig provider system。

## 领域模型

MiniCore 的基础领域关系：

```text
MiniCoreRuntime
└─ Agent*
   └─ Session*
      └─ Turn*
         └─ Item*
            └─ Interaction*
```

基数关系：

```text
MiniCoreRuntime 1 ── N Agent
Agent           1 ── N Session
Session         1 ── N Turn
Turn            1 ── N Item
Item            1 ── N Interaction
```

核心不变量：

- `MiniCoreRuntime` 是外部宿主接触 MiniCore 的唯一顶层门面；
- 一个 Agent 可被多个 Session 引用，一个 Session 只对应一个 Agent；
- Workspace 属于 Session（`SessionDefinition.workspace`），不属于 Agent 或 Turn，也不是独立 entity 或 Runtime-global registry；
- Prompt、Tool、Skill 是独立概念，不合并成通用 `Resource`；
- Turn领域对象不持有PromptSet、ToolSet、SkillView、Agent identity或Workspace；这些执行期对象由Turn执行上下文capture，fingerprints和必要references保存在TurnContext storage entry；
- durable lifecycle 与 loaded execution state 分离：内存 projection、cache、snapshot 只能由权威事实派生。

### 三个长生命周期深模块

`MiniCoreRuntime` 在 Runtime 生命周期内拥有三个长生命周期深模块：

```rust
pub struct MiniCoreRuntime {
    pub prompt_service: Arc<PromptService>,
    pub tools: Arc<ToolService>,
    pub skills: Arc<SkillService>,
}
```

Turn 执行边界产生独立、不可变的有效执行对象：

```text
PromptService::current_view() → Arc<PromptResourceView>
PromptService::for_turn(...)   → PromptSet
ToolService::for_turn(...)     → ToolSet
SkillService::current_view(...) → Arc<SkillView>
SkillService::load(...)        → Arc<LoadedSkill>
```

Prompt 是唯一负责模型实际可见上下文组装的 seam：

```text
PromptSet::compose_user_message(...) → CanonicalUserMessage
→ commit → committed conversation
→ PromptSet::assemble(...) → AssembledModelContext → ModelGateway
```

### 核心实体

- **Agent**：可被多个 Session 引用的 durable entity。head 保存 identity、current definition pointer、status 和 metadata；execution definition 使用 immutable `AgentDefinition`，identity 为 `(AgentId, AgentRevision)`，发布后不可原地修改。`AgentRevision` 只在 execution definition canonical content 改变时产生。
- **Session**：长期存在的对话对象。head保存identity、current `SessionDefinitionRevision`、durable lifecycle和metadata。`SessionDefinition`原子绑定`AgentRevisionRef`、Workspace、`SessionModelConfig`和`SessionPromptSelection`。Session 创建时 pin Agent 当时的 current revision；Agent 后续发布新 revision 不自动改变已有 Session，必须通过新 `SessionDefinitionRevision` 显式升级，且一个 Session 的全部 revision 只能引用同一个 AgentId。
- **Turn**：从 committed initiating UserMessage entry 到 terminal entry（final AssistantMessage / TurnInterrupted / TurnFailed）的用户意图执行过程。Turn head 不内联 `Vec<Item>`；Item 顺序由 SessionStorage projection 提供。active/recovered Turn 通过 initiating UserMessage 引用的 TurnContext entry 解析 exact execution metadata。
- **Item**：Turn 内稳定、可观察的语义值或长生命周期操作。`ItemContent = UserMessage | AgentMessage | Reasoning | ToolInvocation`；`ItemType`/`ItemStatus` 从 content 派生，不独立存储。ToolCall 与 ToolResult 属于同一个 `ToolInvocation` Item（`Started → Completed | Abandoned`）。AgentRun 的 AgentMessage/Reasoning started 与 delta 使用稳定 ItemId 和 process-local `StreamingItem`，但只有 final candidate append/apply 后才产生 Completed Item；provider retry、Tool progress 和 execution phase 同样不是 Item。
- **Interaction**：某个 Item 执行期间由 Runtime 发起并等待外部回答的 durable request/resolution（`ToolApproval | UserQuestion`）。归属固定为 `Interaction → Item → Turn → Session → Agent`；遵守 request-before-notify 与 resolution-before-resume/side-effect。MiniCore拥有交互协议和durable truth，Presentation Adapter只负责presentation与提交resolution。结构化 Interaction response 不是 UserMessage，不开启新 Turn。

### Turn 执行上下文

Turn execution 是领域 Turn 外围的执行过程，不增加新的领域层级：

```text
admission reservation
→ TurnExecutionContext capture
→ TurnContext + initiating UserMessage append
→ AgentLoop drive
→ model / Tool / append·apply loop
→ terminal entry append
```

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

`TurnExecutionContext` 是不可变 execution binding，不是 Service、领域 entity 或通用 Resource owner。字段保持私有；每次逻辑模型调用由 committed conversation checkpoint、purpose、output contract、effective max_output_tokens 与 `AssembledModelContext` 共同确定，不引入 `ModelStep` 领域类型、ID 或 registry。Turn execution 包含并驱动 AgentLoop；`NeedModel | NeedTools | Finished` 是 private AgentLoop action，Prompt assembly、Tool execution、append/apply、Steer、FollowUp、cancellation 和 terminal status 由 SessionExecutor 负责。

### 状态模型

- `AgentStatus = Enabled | Disabled | Deleted`；`SessionLifecycle = Open | Archived | Deleted`（`Open ↔ Archived → Deleted`）。`Deleted` 是不可恢复的逻辑删除，物理清除留给 `PurgeAgent` / `PurgeSession`。
- `SessionLoadState`、`SessionReadiness`、`SessionExecutionState` 是进程内 projection，不进入 durable Session；重启后所有 Session 视为 Unloaded。Loaded 不等于 Ready：Workspace、exact AgentRevision 或 conversation 不可用时 history 仍可读，但 Turn admission fail closed。
- `TurnStatus = Running | Completed | Interrupted | Failed`。initiating UserMessage entry append 后 Turn 首先 `Running`；terminal 不可恢复为 Running；一个 Session 同时最多一个 Running Turn。`WaitingApproval`、`WaitingForUserInput`、`Sampling`、`ExecutingTools`、`Compacting` 都只是 Running Turn 的 `TurnExecutionPhase`——等待审批、用户回答或 Compaction 时 TurnStatus 仍为 Running，Steer 默认排队到当前 operation 完成，不把 Turn 变为 Interrupted。

## Message Pipeline

MiniCore 的 message pipeline 使用固定动词表达每个阶段，调用链只有一个方向：

```text
CommandSurface.parse_message_intent
  → SessionExecutor.Submit
  → capture exact SessionDefinition / AgentRevision / WorkspaceSnapshot
  → PromptService.current_view + SkillService.current_view + ToolService.for_turn
  → PromptService.for_turn
  → TurnExecutionContext.compose_message
  → SessionWriter.append(TurnContext) → apply
  → SessionWriter.append(UserMessage) → apply
  → private AgentLoop.next_action
  → PromptSet.assemble(CommittedConversationView)
  → ModelGateway.generate_model_turn
```

Tool round 和 active Steer 只通过 committed delta 推进同一个 Turn：

```text
Tool-call response
  → SessionWriter.append(Assistant(intermediate)) → apply
  → ToolSet.execute
  → SessionWriter.append(Tool message)* → apply each
  → SessionWriter.append(ToolRoundCompleted) → apply
  → CommittedConversationDelta → SessionExecutor applies committed conversation

Ask-user Tool-call
  → ToolExecutionControl.request_user_question
  → InteractionRequested append/apply → UI-safe StateEvent
  → InteractionResolved(UserAnswer) append/apply
  → PreExecution truthful Tool message → tool_round_completed
  → same Turn next model call

Steer after complete assistant/tool step
  → SteerQueue<TurnId>.pop_front()
  → TurnExecutionContext.compose_message
  → SessionWriter.append(UserMessage(source = Steer)) → apply
  → AgentLoop.accept_committed_steer or rebuild segment
```

`SessionStorage`是durable truth；`CommittedConversationState`在session open/recovery时从storage建立，稳态只应用成功append receipt返回的trusted delta。private AgentLoop从该热视图构造immutable conversation state，不要求每个Turn重新扫描session文件。未append的draft不能进入模型调用；含ToolCall的assistant和tool entry在没有`tool_round_completed`前不能进入模型conversation。无ToolCall Assistant Continue在append/apply时model-visible但不terminalize Turn；final AssistantMessage append/apply后才发布completed terminal。

## 模块文档地图

- [Runtime公开协议](modules/runtime-interface.md)：`MiniCoreRuntime`的`dispatch / query / snapshot / subscribe`、公开领域identity、snapshot-first实时流和协议边界。
- [Agent 与 Session 生命周期](modules/agent-session-lifecycle.md)：Agent/Session revision、create/load/unload/archive/fork、durable lifecycle 与 loaded execution state 分离。
- [Workspace](modules/workspace.md)：Session-owned Workspace definition、roots/cwd、trust、authorization、filesystem capability 与窄只读 view。
- [Prompt](modules/prompt.md)：共享PromptResourceView、各Turn独立PromptSet、CanonicalUserMessage、`assemble → AssembledModelContext`。
- [Skills](modules/skills.md)：SkillService、reloadable SkillView与LoadedSkill、发现/解析/按需加载。
- [Tools](modules/tools.md)：ToolService `for_turn → ToolSet`、registry、policy、approval、grants、sandbox、executor。
- [Turn 执行上下文](modules/turn-execution-context.md)：capture 依赖图、fingerprint、reload 线性化、AgentLoop 分界。
- [Turn / Item / Interaction](modules/turn-item-interaction.md)：Turn 边界、ItemContent、ToolInvocation identity、Interaction、terminal cleanup。
- [Conversation 与 SessionStorage](modules/conversation-storage.md)：by-entry JSONL tree、唯一 append seam、entry tree、projection、fork、recovery。
- [Session 执行](modules/session-execution.md)：单 SessionExecutor、semantic SessionIngress lanes、RunningOperation、multi-session 并发与资源锁。
- [ModelGateway](modules/model-gateway.md)：`resolve_for_turn` / `generate_model_turn`、private Rig adapter、stream/retry/auth/usage/cache。
- [Compaction](modules/compaction.md)：portable rolling summary、stable-unit cut、active-Turn checkpoint、model-aware summary budget、`Compacting` 阶段与StoredCompaction恢复。

模块索引与权威归属见 [模块总览](modules/README.md)。

## 核心边界

- 下游 CLI/TUI/GUI 不能导入 Rig 类型，不能直接调用模型提供方、执行工具、读取凭据、扫描技能或读写会话文件；只依赖 `MiniCoreRuntime` facade。
- Rig 拥有 agent loop 的协议级状态机，不拥有产品级工具治理、会话持久化或 UI 呈现。
- 同一份领域事实只有一个权威owner：conversation durable truth属于SessionStorage；Agent definition属于Agent owner；Workspace definition属于Session；PromptResourceView、ToolSet和SkillView属于各自子系统；最终模型可见上下文属于PromptSet；provider-specific encoding和调用属于ModelGateway。
- 每个loaded Session由一个`SessionExecutor`拥有执行期mutable state、SessionWriter、committed projections、CurrentTurnExecution、唯一current RunningOperation和per-session `SessionIngress`；一个Runtime允许多个SessionExecutor同时Running。Submit、Steer、FollowUp、Interaction和Tool control使用独立bounded lane，Cancel/revocation与lifecycle使用sticky signal，Snapshot读取immutable published view；持续订阅以原子Snapshot首帧开始。WaitingForUserInput只暂停当前Turn的逻辑推进，不阻塞该SessionExecutor或其他Session。所有ledger mutation仍只由Executor通过`SessionWriter::append(SessionEntryDraft)`逐entry写入并立即应用trusted delta。
- `ModelGateway` 通过 `resolve_for_turn(...)` 固定 exact `TurnModelSnapshot`，RunningOperation 只传 `ModelCallRequest`；Gateway 隐藏 provider、credential、endpoint、transport retry、cache 和 continuation，不判断 session message visibility，也不在 active Turn 内替换 model identity。
- 工具注册、审批、授权记忆、路径授权、sandbox enforcement、资源锁和真实副作用由 `ToolService` 统一治理；新 Turn 通过 `ToolService::for_turn(...) -> ToolSet` 原子绑定模型可见 `ToolPromptView` 与 executor snapshot。MVP 不启用通用 `bash`；子进程限制无法强制时必须 fail closed。
- 上下文压缩由 SessionExecutor 编排；`Compaction` 只提供 context budget、stable-unit projection、scope/frontier planning、protected `EntryId`、portable directive 和结果校验，不构造 `ModelCallRequest`，也不组装模型上下文。首版使用active Turn exact model生成leading rolling summary或anchored active-Turn segment checkpoint；initiating与Steer UserMessage保持原文，每个instruction segment内已完成的早期ToolRound可在安全边界摘要，summary budget在plan阶段与pinned model limits求交。

## 相关决策

长期架构决策记录在 [`docs/adr/`](adr/)（0100+）：

- [ADR 0100：领域模型与 ownership](adr/0100-domain-model-and-ownership.md)
- [ADR 0101：Workspace 属于 Session](adr/0101-workspace-ownership.md)
- [ADR 0102：Prompt / Tool / Skill 是独立子系统](adr/0102-prompt-tool-skill-are-distinct-subsystems.md)
- [ADR 0103：Turn / Item / Interaction 模型](adr/0103-turn-item-interaction-model.md)
- [ADR 0104：SessionStorage 是 durable truth](adr/0104-session-storage-is-durable-truth.md)
- [ADR 0105：SessionExecutor 拥有 loaded Session](adr/0105-session-executor-owns-loaded-session.md)
- [ADR 0106：ModelGateway 是单一深异步 operation](adr/0106-model-gateway-is-single-deep-operation.md)
- [ADR 0107：Compaction 使用严格 stable suffix（已被0112取代）](adr/0107-compaction-uses-strict-stable-suffix.md)
- [ADR 0108：Runtime 公开协议](adr/0108-runtime-public-protocol.md)
- [ADR 0109：Prompt、Projection 与 Session Operation 使用确定性规则](adr/0109-review-b-determinism-and-serialized-operations.md)
- [ADR 0110：Prompt 与 Skill 使用共享、可替换 View](adr/0110-prompt-and-skill-use-shared-reloadable-views.md)
- [ADR 0111：SessionIngress 分离控制与工作 lane](adr/0111-session-ingress-separates-control-and-work-lanes.md)
- [ADR 0112：Compaction支持active-Turn checkpoint与模型感知预算](adr/0112-compaction-supports-active-turn-checkpoints.md)
- [ADR 0113：UserQuestion使用Runtime交互协议与UI展示Adapter](adr/0113-user-question-uses-runtime-protocol-and-ui-presentation.md)
- [ADR 0114：Runtime观察协议使用Snapshot-First实时流](adr/0114-runtime-observation-uses-snapshot-first-streams.md)
