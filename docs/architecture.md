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

MiniCore 的 AgentLoop 是自研的 crate-private 协议状态机（[ADR 0115](adr/0115-agent-loop-is-first-party-state-machine.md)）；Rig只用于实现`ModelGateway` private `ProviderAdapter`，保持为实现细节。`RigProviderAdapter`只编码并执行具体provider的单次attempt、桥接stream/cancellation并映射provider响应，SDK automatic retry固定为0；model resolution、request validation、auth policy、progress lifecycle、cache/continuation policy、错误分类和provider-neutral terminal result由`ModelGateway`拥有，logical retry由`SessionExecutor`拥有。下游 CLI、TUI、GUI 宿主只通过 `MiniCoreRuntime` 的 command、query、event 和 snapshot 交互，不依赖 Rig 类型、模型提供方类型或工具实现细节。pi、Codex 等项目可以作为设计参考，但除非文档明确标注兼容契约，否则其类型、调用方式和行为都不是 MiniCore 的兼容目标。

MiniCore 不重新实现 provider SDK 的底层 sampling/tool-call protocol，也不重新实现 provider HTTP client。模型返回 ToolCall 时，Session execution先append完整assistant/intermediate entry，再由ToolService执行工具治理；每个truthful result独立append为tool message。live路径中，同一assistant response的全部ToolCall都存在exactly one matching ToolResult时，Conversation projector自动形成完整Tool exchange并推进模型可见conversation，不再写独立`ToolRoundCompleted` marker；cold replay对duplicate result采用first-valid-wins并报告diagnostic。真实模型调用统一进入共享`ModelGateway`的深异步operation，再由其private `ProviderAdapter`执行具体provider attempt；首个production adapter使用Rig的provider client能力。

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
- Workspace属于Session（`SessionDefinition.workspace`），不属于Agent或Turn，也不是独立entity或Runtime-global registry；loaded Session只在Idle时接受Workspace definition update，active Turn pin immutable WorkspaceSnapshot；
- Prompt、Tool、Skill 是独立概念，不合并成通用 `Resource`；
- Turn领域对象不持有PromptSet、ToolSet、SkillView、Agent identity或Workspace；这些执行期对象由Turn执行上下文capture为immutable `Arc`/private objects，Input UserMessage只内联safe `StoredTurnStart`历史metadata，不作为restart execution checkpoint；
- durable lifecycle与loaded execution state分离：live append保持严格，cold replay可以跳过/隔离局部损坏并返回diagnostics；内存projection、cache和snapshot由可恢复的durable事实派生。

### Runtime-Owned共享深模块

`MiniCoreRuntime`在Runtime生命周期内拥有四个共享深模块；字段保持private，只通过顶层facade和crate-internal orchestration使用：

```rust
pub struct MiniCoreRuntime {
    prompt_service: Arc<PromptService>,
    tool_service: Arc<ToolService>,
    skill_service: Arc<SkillService>,
    model_gateway: Arc<ModelGateway>,
    shared_resources: RwLock<SharedResourceRoots>,
}

#[derive(Clone)]
struct SharedResourceRoots {
    prompt: Arc<PromptResourceView>,
    skills: Arc<SkillResourceView>,
    tools: Arc<ToolResourceView>,
    models: Arc<ModelCatalogView>,
}

impl MiniCoreRuntime {
    fn capture_shared_resources(&self) -> SharedResourceRoots;

    async fn reload_shared_resources(
        &self,
    ) -> Result<(), CommandError>;
}
```

`SharedResourceRoots`只是Runtime-private的四个current pointer原子publication value，不拥有source、definition、cache或reload逻辑，也没有ID/version/generation。各deep module独立build candidate；Runtime在短gate内整体替换该值，Turn admission在同一gate内整体clone该值，因此不存在per-service partial publish。

Turn 执行边界产生独立、不可变的有效执行对象：

```text
MiniCoreRuntime::capture_shared_resources()
→ SharedResourceRoots {
     Arc<PromptResourceView>,
     Arc<SkillResourceView>,
     Arc<ToolResourceView>,
     Arc<ModelCatalogView>
   }

captured roots + exact Turn context
├─ ModelGateway::resolve_for_turn(...) → Arc<TurnModelSnapshot>
├─ SkillService::for_turn(...)         → Arc<SkillView>
├─ ToolService::for_turn(...)          → Arc<ToolSet>
└─ PromptService::for_turn(...)        → Arc<PromptSet>
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
- **Turn**：从committed Input UserMessage entry到terminal entry（final AssistantMessage / TurnInterrupted / TurnFailed）的用户意图执行过程。Turn head不内联`Vec<Item>`；Item顺序由SessionStorage projection提供。Input UserMessage内联safe `StoredTurnStart`，cold replay不据此重建旧PromptSet、ToolSet、WorkspaceSnapshot或provider session。
- **Item**：Turn内稳定、可观察的语义值或长生命周期操作。`ItemContent = UserMessage | AgentMessage | Reasoning | ToolInvocation`；`ItemType`/`ItemStatus`从content派生，不独立存储。ToolCall与ToolResult属于同一个`ToolInvocation` Item（`Started → Completed | Abandoned`）。Tool side-effect start是current Runtime的owner-local状态，不是durable event。Turn/Item公开顺序由selected path entry顺序和assistant content/call顺序表达，有序Vec与new-Item StateEvent创建顺序就是契约，不增加DisplaySequence。AgentRun的AgentMessage/Reasoning started与delta使用稳定ItemId和process-local `StreamingItem`，但只有final candidate append/apply后才产生Completed Item；model logical retry、Tool progress和execution phase同样不是Item。
- **Interaction**：某个 Item 执行期间由 Runtime 发起并等待外部回答的 durable request/resolution（`ToolApproval | UserQuestion`）。归属固定为 `Interaction → Item → Turn → Session → Agent`；遵守 request-before-notify 与 resolution-before-resume/side-effect。MiniCore拥有交互协议和durable truth，Presentation Adapter只负责presentation与提交resolution。用户沉默、subscriber缺失或transport断开都保持Pending，不产生超时或默认Deny；Cancel、Turn terminal cleanup、Unload和recovery负责显式生命周期收口。结构化 Interaction response 不是 UserMessage，不开启新 Turn。

### Turn 执行上下文

Turn execution 是领域 Turn 外围的执行过程，不增加新的领域层级：

```text
admission reservation
→ TurnExecutionContext capture
→ Input UserMessage + StoredTurnStart append
→ AgentLoop drive
→ model / Tool / append·apply loop
→ terminal entry append
```

```rust
pub(crate) struct TurnExecutionContext {
    session_id: SessionId,
    session_revision: SessionDefinitionRevision,
    agent: AgentRevisionRef,
    model: Arc<TurnModelSnapshot>,
    workspace: Arc<WorkspaceSnapshot>,
    skill_service: Arc<SkillService>,
    skill_context: SkillViewContext,
    skill_view: Arc<SkillView>,
    tool_set: Arc<ToolSet>,
    prompt_set: Arc<PromptSet>,
    diagnostics: Arc<[TurnContextDiagnostic]>,
}
```

`TurnExecutionContext` 是不可变 execution binding，不是 Service、领域 entity 或通用 Resource owner。字段保持私有；private constructor只接受同一次capture得到的`Arc<WorkspaceSnapshot>`、`Arc<SkillView>`、`Arc<ToolSet>`、`Arc<PromptSet>`和`Arc<TurnModelSnapshot>`，调用方不能跨capture拼接任意view。PromptSet内部保留本次捕获的`Arc<PromptResourceView>`及Tool/Skill投影。每次逻辑模型调用由同一个immutable `Arc<ModelCallRequest>`、latest committed `ConversationCheckpoint.entry_id`、purpose、output contract与effective max_output_tokens共同确定，不引入 `ModelStep` 领域类型、ID、registry或额外派生身份值。Turn execution 包含并驱动 AgentLoop；`NeedModel | NeedTools | Finished` 是 private AgentLoop action，Prompt assembly、Tool execution、append/apply、Steer、FollowUp、cancellation 和 terminal status 由 SessionExecutor 负责。

### 状态模型

- `AgentStatus = Enabled | Disabled | Deleted`；`SessionLifecycle = Open | Archived | Deleted`（`Open ↔ Archived → Deleted`）。`Deleted` 是不可恢复的逻辑删除，物理清除留给 `PurgeAgent` / `PurgeSession`。
- `SessionLoadState`、`SessionReadiness`、`SessionExecutionState` 是进程内 projection，不进入 durable Session；重启后所有 Session 视为 Unloaded。Loaded 不等于 Ready：current Workspace或exact AgentRevision不可用于future execution，或replay无法得到安全admission basis时，history仍可读但Turn admission fail closed；局部conversation corruption本身只产生diagnostic。
- `TurnStatus = Running | Completed | Interrupted | Failed`。initiating UserMessage entry append 后 Turn 首先 `Running`；terminal 不可恢复为 Running；一个 Session 同时最多一个 Running Turn。`WaitingApproval`、`WaitingForUserInput`、`Sampling`、`ExecutingTools`、`Compacting` 都只是 Running Turn 的 `TurnExecutionPhase`——等待审批、用户回答或 Compaction 时 TurnStatus 仍为 Running，Steer 默认排队到当前 operation 完成，不把 Turn 变为 Interrupted。

## Message Pipeline

MiniCore 的 message pipeline 使用固定动词表达每个阶段，调用链只有一个方向：

```text
CommandSurface.parse_message_intent
  → SessionExecutor.Submit
  → capture exact SessionDefinition / AgentRevision / WorkspaceSnapshot
  → shared gate clones PromptResourceView / SkillResourceView / ToolResourceView / ModelCatalogView
  → ModelGateway.resolve_for_turn + SkillService.for_turn + ToolService.for_turn
  → PromptService.for_turn
  → TurnExecutionContext.compose_message
  → SessionWriter.append(UserMessage + StoredTurnStart) → apply
  → private AgentLoop.next_action
  → PromptSet.assemble(CommittedConversationView)
  → ModelGateway.generate_model_turn
```

Tool exchange和active Steer只通过committed typed delta推进同一个Turn：

```text
Tool-call response
  → SessionWriter.append(Assistant(intermediate)) → apply
  → ToolSet.execute under owner-local start control
  → SessionWriter.append(Tool message)* → apply each
  → final matching result completes assistant call set
  → CommittedToolExchangeDelta
  → AgentLoop.accept_committed_tool_results

Ask-user Tool-call
  → ToolExecutionControl.request_user_question
  → InteractionRequested append/apply → UI-safe StateEvent
  → InteractionResolved(UserAnswer) append/apply
  → PreExecution truthful Tool message
  → complete exchange后same Turn next model call

Steer after complete assistant/tool step
  → SteerQueue<TurnId>.pop_front()
  → TurnExecutionContext.compose_message
  → SessionWriter.append(UserMessage(source = Steer)) → apply
  → AgentLoop.accept_committed_steer(CommittedSteerDelta)
```

`SessionStorage`是已写入conversation/message/lifecycle history的durable truth；live writer严格校验，cold replay跳过或隔离局部损坏并返回diagnostics。`CommittedConversationState`在session open/recovery时从sanitized selected path建立，稳态应用成功append receipt返回的trusted delta。未append的draft不能进入模型调用；含ToolCall的assistant只有在全部matching ToolResult存在时才与ordered results一起进入conversation。无ToolCall Assistant Continue在append/apply时model-visible但不terminalize Turn；final AssistantMessage append/apply后才发布completed terminal。

## 模块文档地图

- [Runtime公开协议](modules/runtime-interface.md)：`MiniCoreRuntime`的`dispatch / query / snapshot / subscribe`、公开领域identity、snapshot-first实时流和协议边界。
- [Agent 与 Session 生命周期](modules/agent-session-lifecycle.md)：Agent/Session revision、create/load/unload/archive/fork、durable lifecycle 与 loaded execution state 分离。
- [Workspace](modules/workspace.md)：Session-owned Workspace definition、roots/cwd、trust、authorization、filesystem capability 与窄只读 view。
- [Prompt](modules/prompt.md)：共享PromptResourceView、各Turn独立PromptSet、CanonicalUserMessage、`assemble → AssembledModelContext`。
- [Skills](modules/skills.md)：SkillService、shared SkillResourceView、per-Turn SkillView与LoadedSkill、发现/解析/按需加载。
- [Tools](modules/tools.md)：ToolService `for_turn → ToolSet`、registry、policy、per-call approval、sandbox、executor。
- [Turn 执行上下文](modules/turn-execution-context.md)：capture 依赖图、exact refs、immutable Arc与explicit reload线性化、AgentLoop 分界。
- [Turn / Item / Interaction](modules/turn-item-interaction.md)：Turn 边界、ItemContent、ToolInvocation identity、Interaction、terminal cleanup。
- [Conversation 与 SessionStorage](modules/conversation-storage.md)：by-entry JSONL tree、唯一 append seam、entry tree、projection、fork、recovery。
- [Session 执行](modules/session-execution.md)：单 SessionExecutor、semantic SessionIngress lanes、RunningOperation、multi-session 并发与Session-local file mutation queue。
- [ModelGateway](modules/model-gateway.md)：`resolve_for_turn` / `generate_model_turn`、ModelGateway-owned single-attempt stream/auth/usage/cache，以及只负责provider attempt映射与调用的private `ProviderAdapter`（首个production实现为`RigProviderAdapter`）；logical retry归SessionExecutor。
- [Compaction](modules/compaction.md)：portable rolling summary、provider-valid prefix cut、single `first_kept_entry_id` marker、model-aware summary budget、`Compacting`阶段与宽容replay。

模块索引与权威归属见 [模块总览](modules/README.md)。

## 核心边界

- 下游 CLI/TUI/GUI 不能导入 Rig 类型，不能直接调用模型提供方、执行工具、读取凭据、扫描技能或读写会话文件；只依赖 `MiniCoreRuntime` facade。
- AgentLoop 是 MiniCore 自研的 crate-private sans-I/O 协议状态机（ADR 0115）；Rig只实现`ModelGateway` private `ProviderAdapter`中的provider协议映射与单次attempt调用，SDK automatic retry固定为0，不拥有AgentLoop、model resolution、Session logical retry、ModelGateway terminal语义、产品级工具治理、会话持久化或UI呈现。
- 同一份领域事实只有一个权威owner：已写入conversation/message/lifecycle history属于SessionStorage；current-Runtime Tool start/settlement属于SessionExecutor与ToolSet；Agent definition属于Agent owner；Workspace definition属于Session；PromptResourceView、ToolSet和SkillView属于各自子系统；最终模型可见上下文属于PromptSet；provider-specific encoding和调用属于ModelGateway。
- raw failure只存在于具体adapter implementation；发生事实的module负责typed、redacted分类，掌握Turn/control/durable state的owner负责恢复（ADR 0120）。不建立全局Error module或ErrorService；该规则首版只在ModelGateway response validation落地，其他module按真实需求逐步采用。
- 每个loaded Session由一个`SessionExecutor`拥有执行期mutable state、SessionWriter、committed projections、CurrentTurnExecution、唯一current RunningOperation、ToolOperationSlot和per-session `SessionIngress`；一个Runtime允许多个SessionExecutor同时Running。Submit、Steer、FollowUp、Interaction和Tool control使用独立bounded lane，Cancel/SecurityRevoked与lifecycle使用sticky signal，Snapshot读取immutable published view；持续订阅以原子Snapshot首帧开始。valid Cancel在sticky epoch发布后立即返回`CancelAccepted`并进入Finishing，已开始Tool结构化收口期间仍允许FollowUp排队，旧Turnterminal前不启动新Turn。Tool side-effect start由owner-local reservation线性化，不写durable marker；Authority hard restriction停止新operation并truthful settle，terminal后重新resolve Workspace，不承诺动态撤销open handle。Snapshot只作为live observer baseline；process restart或显式cold load顺序扫描JSONL，跳过/隔离局部损坏并返回diagnostics，不恢复process-local执行对象，保守关闭unfinished Turn后进入Idle或Unavailable。MVP有意接受O(n)成本，不实现ProjectionSnapshot/checkpoint index；已loaded Session切换不触发replay。WaitingForUserInput只暂停当前Turn的逻辑推进，不阻塞该SessionExecutor或其他Session。所有ledger mutation仍只由Executor通过`SessionWriter::append(SessionEntryDraft)`逐entry写入并立即应用trusted delta。
- `ModelGateway`通过`resolve_for_turn(...)`固定exact `TurnModelSnapshot`及validated `TokenEstimateRate`；PromptSet和Compaction只使用该Snapshot分发的同一个Turn-pinned estimator。RunningOperation只传`ModelCallRequest`；每个Gateway operation最多执行一个provider attempt，SessionExecutor对同一个AgentRun request最多logical retry 3次、CompactionSummary最多1次。Gateway隐藏provider、credential、endpoint、cache和continuation，在`ModelCallResult`前验证finish/content与OutputContract；它不判断session message visibility，也不在active Turn内执行transport/model fallback。
- Prompt/Skill/Tool/Model共享资源只在Runtime初始化或显式`/reload`后替换current immutable object；watcher最多标记dirty，不自动publish。`/reload`先完整build candidate并validate required resources，再在短publication gate下原子替换四个current `Arc`；Turn admission在同一gate下只克隆四个Arc，失败时保留全部old current values。shared Prompt/Skill filesystem source在Runtime initialize或shared reload时捕获；Workspace-bound Prompt/Skill source随Session load、Idle definition update或`/reload workspace` candidate捕获并与WorkspaceSnapshot一起发布。active/completed Turn不原地更新；future Turn capture new objects。`/reload workspace`沿用Idle-only规则，非Idle返回`SessionBusy`。
- 工具注册、per-call审批、路径授权、sandbox enforcement和真实副作用pipeline由`ToolService`统一治理；SessionExecutor在current Runtime内发放一次性ToolStartPermit并管理Running/Settling状态，ledger不保存start marker。每个loaded Session拥有独立file mutation queue，新Turn的`ToolSet`只持共享引用。同Session同文件mutation FIFO，跨Session共享Workspace不协调并由host/user负责隔离（ADR 0116）。MVP不启用通用`bash`；子进程限制无法强制时必须fail closed。
- 上下文压缩由SessionExecutor编排；`Compaction`只提供context budget、provider-valid prefix cut、single-marker plan、portable directive和结果校验，不构造`ModelCallRequest`，也不组装模型上下文。Compaction operation持有同一个`Arc<CompactionPlan>`及其source checkpoint、summary prefix、single `first_kept_entry_id` marker、budget和request；append前验证current checkpoint/Turn/version/control。StoredCompaction不保存scope、protected entries、previous checkpoint、boundary或coverage provenance；cold replay无法应用marker时忽略该Compaction并继续。首版使用active Turn captured model生成rolling summary，旧initiating/Steer UserMessage必要时可以进入summary，summary budget在plan阶段与pinned model limits求交。

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
- [ADR 0112：Compaction active-Turn checkpoint历史决策（已被0124取代）](adr/0112-compaction-supports-active-turn-checkpoints.md)
- [ADR 0113：UserQuestion使用Runtime交互协议与UI展示Adapter](adr/0113-user-question-uses-runtime-protocol-and-ui-presentation.md)
- [ADR 0114：Runtime观察协议使用Snapshot-First实时流](adr/0114-runtime-observation-uses-snapshot-first-streams.md)
- [ADR 0115：AgentLoop使用自研协议状态机](adr/0115-agent-loop-is-first-party-state-machine.md)
- [ADR 0116：文件mutation使用Session-local FIFO队列](adr/0116-file-mutations-use-session-local-queues.md)
- [ADR 0117：异步同步使用单Owner、短临界区与Typed Permit](adr/0117-async-synchronization-uses-single-owner-and-typed-permits.md)
- [ADR 0118：Cancel立即确认，FollowUp等待结构化收口后启动](adr/0118-cancel-acknowledges-immediately-and-followup-waits-for-settlement.md)
- [ADR 0119：模型调用使用Session逻辑重试](adr/0119-model-calls-use-session-logical-retries.md)
- [ADR 0120：失败由事实拥有模块分类，恢复由执行拥有者决定](adr/0120-failures-stay-with-owning-modules.md)
- [ADR 0121：Workspace定义只在Idle更新，安全撤权中断当前Turn](adr/0121-workspace-updates-require-idle.md)
- [ADR 0122：Workspace fingerprint历史决策（已被0123取代）](adr/0122-workspace-fingerprints-are-runtime-local.md)
- [ADR 0123：执行一致性使用Exact Ref、不可变快照与显式Reload（持久化条款部分被0124取代）](adr/0123-identity-uses-refs-and-explicit-reload.md)
- [ADR 0124：Session Replay宽容恢复并收窄持久化引用链](adr/0124-session-replay-is-tolerant-and-links-are-minimal.md)
