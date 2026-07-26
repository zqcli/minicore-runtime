# MiniCore V1 → V2 版本迁移记录

状态：V1 → V2 版本迁移记录
日期：2026-07-25

## 目的

本文记录 MiniCore 从 V1 到 V2 的版本迁移，说明：

- V2 重构目标与总体原则；
- 各子系统的迁移顺序与完成情况；
- 每个迁移阶段的输入、输出和完成门槛；
- 新旧架构并存期间采取的控制方式；
- ADR、正式架构文档、研究文档和 V1 旧文档的收尾处理策略；
- 整体迁移完成的判定标准；
- V1 与 V2 的文档/模块对应关系。

本文不替代各子系统的架构设计。领域类型、interface、状态和不变量以 V2 的 [`docs/modules/`](../modules/) 各正式文档为权威；V1 归档见 [`docs/archive/v1/`](../archive/v1/)。

## V2 总体目标

迁移后的 MiniCore 采用以下领域关系：

```text
MiniCoreRuntime
└─ Agent*
   └─ Session*
      └─ Turn*
         └─ Item*
            └─ Interaction*
```

`MiniCoreRuntime` 是外部宿主接触 MiniCore 的唯一顶层门面，并在 Runtime 生命周期内拥有三个长生命周期深模块：

```text
PromptService
ToolService
SkillService
```

Turn 执行边界产生独立、不可变的有效执行对象：

```text
PromptService::for_turn(...) → PromptSet
ToolService::for_turn(...)   → ToolSet
SkillService::current_view(...) → SkillView
SkillService::load(...)      → Arc<LoadedSkill>
```

Prompt 是唯一负责模型实际可见上下文组装的 seam：

```text
PromptSet::compose_user_message(...)
→ CanonicalUserMessage
→ commit
→ committed conversation
→ PromptSet::assemble(...)
→ AssembledModelContext
→ ModelGateway
```

Prompt、Tool、Skill 未合并为通用 `Resource`。V1 的 `ResourceManager`、`ResourceSnapshotStore`、通用 Resource overlay 和四层 Resource snapshot 均未进入 V2 目标架构，其正确的不变量已分别下沉到 Workspace、Prompt、Tool、Skill 和 Turn 执行上下文。

## 总体迁移原则

### 目标模型优先

迁移以 V2 目标领域模型和目标 interface 为起点，不以 V1 模块名称、V1 调用链或 V1 持久化结构作为设计约束。

V1 实现只用于回答以下问题：

- 有哪些必须保留的用户行为；
- 哪些边界条件和失败场景已经被处理；
- 哪些测试可以作为回归保护；
- 哪些外部协议需要显式迁移。

V1 实现不因为已经存在，就自动成为 V2 目标架构的一部分。

### Replace，不 Layer

V2 模块替换 V1 职责，而不是长期包裹 V1 模块形成双层转发：

```text
采用：
caller → new deep module → implementation

避免：
caller → new facade → compatibility manager → old manager → implementation
```

只有在一个阶段无法一次切换全部调用方时，才允许增加临时 adapter。临时 adapter 必须：

- 有明确删除条件；
- 不拥有新的领域状态；
- 不成为新的事实来源；
- 不进入最终公开 interface；
- 在对应阶段完成时删除。

### 一个事实来源

同一份领域事实只有一个权威 owner：

- conversation durable truth 属于 Session storage；
- Agent definition 属于 Agent owner；
- Workspace definition 属于 Session；canonical roots、effective grants 和窄 view 由 WorkspaceResolver 原子解析；
- 全局 trust decision 和 managed policy 属于 WorkspaceAuthority adapter，不复制进 Prompt、Tool 或 Skill；
- Prompt definitions、解析和 PromptSet 属于 Prompt 子系统；
- Tool 注册、披露、执行和 ToolSet 属于 Tool 子系统；
- Skill discovery、Catalog、完整内容和 cache 属于 Skill 子系统；
- 最终模型可见上下文属于 PromptSet；
- provider-specific encoding 和调用属于 ModelGateway。

内存 projection、cache、snapshot 和 UI read model 只能由权威事实派生，不作为并列 source of truth。

### 深模块优先

每个子系统提供较小的 interface，并把 discovery、排序、校验、cache、错误分类、并发和 diagnostics 隐藏在实现内部。

设计每个模块时至少回答：

1. 它拥有什么事实和生命周期？
2. 哪些复杂性只有它能消除？
3. 调用方必须知道哪些最少信息？
4. 删除该模块后，复杂性是否会重新散落到多个调用方？
5. interface 是否可以同时作为生产调用和测试入口？

迁移中不为了类型对称而制造浅层 Manager、Registry、Coordinator 或 adapter。

### 领域快照独立

不同领域只冻结自己真正拥有的稳定值：

```text
PromptSet    → Prompt 的 Turn 有效快照
ToolSet      → ToolSpec 与 executor route 的原子快照
SkillView    → Skill metadata 的有效view
LoadedSkill  → 某个精确定义的不可变正文
```

Session execution 把这些值组合成局部 `TurnExecutionContext`，但该对象不是新的通用资源 owner。

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

字段保持私有；模型调用和 Tool 执行通过窄操作完成，避免不同 Turn 的子快照被交叉组合。

### Transcript-First 与 conversation projection 更新顺序

所有模型可见 conversation fact 必须先形成规范化 entry、成功 append 并 apply trusted delta：

```text
UserMessage(source = Input | Steer)
ToolRoundCompleted 引用的完整 assistant/tool sequence
Compaction entry 的 Replace projection
AssistantMessage(phase = Final)
```

未 append 的 draft 不进入下一次模型调用；已 durable 但尚无 `tool_round_completed` 的 assistant/tool entries 同样不 model-visible。

SessionStorage 是 durable truth。热内存 conversation projection 只消费 `CommittedSessionEntry` 返回的 trusted delta，不在稳态重新扫描完整存储，也不与存储共同拥有同一事实。

### 先内部不变量，后公开协议

公开 command、query、event 和 snapshot interface 最后冻结。

在 Workspace、Turn、Item、Interaction、Session execution、storage 和 recovery 尚未稳定前，不先根据 V1 协议反推领域模型，也不为了 V1 payload 保留错误的内部 ownership。

## 迁移阶段与完成情况

### 阶段 0：基础领域和三个核心子系统

状态：目标架构已确定并落入 V2 正式文档。

对应 V2 文档：

- [MiniCore 领域模型](../architecture.md)
- [Prompt 子系统](../modules/prompt.md)
- [Tool 子系统](../modules/tools.md)
- [Skill 子系统](../modules/skills.md)

本阶段完成条件：

- Prompt、Tool、Skill 的 owner 和核心 interface 已确定；
- Prompt 是唯一模型上下文组装 seam；
- ToolSet 原子绑定模型可见 ToolSpec 和 executor route；
- SkillView与LoadedSkill分离，正文默认按需加载；
- 不再把三者合并为通用 Resource；
- 已记录尚未解决的 scope、identity、cache、reload 和 recovery 问题。

### 阶段 1：Workspace 子系统

状态：目标架构已确定；实现 integration 待后续阶段落地。

对应 V2 文档：[Workspace 子系统](../modules/workspace.md)

移除 V1 通用 ResourceManager 后，Workspace 模块统一解释 Session 的 roots、cwd、trust decision、source authorization 和 filesystem capability，但不建立 Runtime-global WorkspaceService 或 registry。

已定义：

- Workspace 是否需要独立 entity identity；结论是不定义 `WorkspaceId`；
- Session-owned Workspace definition、`WorkspaceRevision` 和 `WorkspaceFingerprint`；
- primary root、additional roots 和 cwd 合法域；
- Workspace 属于 Session 的精确语义；
- trust、source authorization 和 filesystem capability；
- Prompt、Tool、Skill 可以消费的窄只读 view；
- Workspace 更新对 active Turn 和 future Turn 的影响；
- 同根多 Session 的 mutable state 和 authorization 隔离；
- Workspace unavailable 和 reload 语义；
- ordinary reload 与 security-restricting revocation 的区别。

核心输出：

```text
WorkspacePromptContext
WorkspaceToolContext
WorkspaceSkillContext
WorkspaceAccessView
```

这些 view 由同一个不可变 `WorkspaceSnapshot` 投影，不由 PromptService、ToolService 或 SkillService 自行推断 trust 或 roots。

完成门槛：

- 三个 Service 不再接收模糊的 `WorkspaceContext` 占位类型；
- project source 不存在绕过 Workspace authorization 的加载路径；
- additional roots 必须进入 Tool filesystem ceiling，但默认不自动进入 Prompt/Skill source discovery；
- active Turn pin 一个 WorkspaceSnapshot，restrictive update 可以撤销 lease 并中断执行；
- 同根 Session 不共享 mutable Snapshot、authorization lease 或 Session-scoped grant。

### 阶段 2：Turn 执行上下文

状态：目标架构已确定；Session execution、storage 和 ModelGateway integration 待后续阶段落地。

对应 V2 文档：[Turn 执行模块与执行上下文架构设计](../modules/turn-execution-context.md)

已定义：

- 领域 Turn、Turn execution 和 AgentLoop 的开始与结束边界；
- `SkillView`、`ToolSet`、`PromptSet`的capture依赖图；
- 哪些对象在整个 Turn 内固定；
- 逻辑模型调用与 provider retry 的边界，不增加新的领域类型；
- PromptSet 如何绑定同一 ToolSet 的 `ToolPromptView`；
- Skill load 如何使用 pinned Catalog entry 的精确 identity；
- reload 与 Turn capture 的线性化语义；
- `ExecutionContextFingerprint`；
- cancellation、Steer、FollowUp、diagnostics 和释放时机；
- AgentLoop 与 Session execution 的职责分界；
- 崩溃恢复时需要持久化哪些 fingerprint 或 definition reference。

capture 依赖图：

```text
exact SessionDefinitionRevision + AgentRevisionRef + candidate TurnId
+ exact AgentPrompts / SessionPrompts
+ WorkspaceSnapshot + TurnModelSnapshot
├─ SkillService::current_view(SkillViewContext {
│    agent, session_id, session_revision, workspace: workspace.skill_context()
│  }) → SkillView
└─ ToolService::for_turn(ToolTurnContext {
     agent, session_id, session_revision, turn_id,
     workspace: workspace.tool_context(),
     provider: model.capabilities(), execution_mode, execution_control, cancellation, progress_events
   }) → ToolSet

SkillView.prompt_view()
+ ToolSet.prompt_view()
+ WorkspacePromptContext
+ exact AgentPrompts / SessionPrompts
+ TurnModelSnapshot
→ PromptService::for_turn(...)
→ PromptSet
→ TurnExecutionContext
```

完成门槛：

- active Turn 不读取任何 Service 的 future current value；
- Skill metadata 和 LoadedSkill 正文不会跨版本漂移；
- 模型看到的 ToolSpec 必然对应同一个 ToolSet 的 executor；
- PromptSet assembly 不接受另一个任意 ToolPromptView；
- assembly 只从 committed conversation 和 typed call policy 构建，不存在任意 current-call contribution lane；
- 逻辑模型调用、provider retry、Steer 和 FollowUp 的边界无歧义；
- reload 只影响 future TurnExecutionContext。

### 阶段 3：Agent 与 Session 生命周期

对应 V2 文档：[Agent 与 Session 生命周期架构设计](../modules/agent-session-lifecycle.md)。

状态：目标架构已确定。

已明确：

- Agent 创建、更新、禁用、删除及 immutable `AgentRevision` 生成规则；
- Session pin exact `AgentRevisionRef`，Agent update 不自动改变已有 Session；
- Session 显式升级 Agent revision 的 CAS 和同 AgentId 约束；
- Session create、definition update、load/unload、archive/unarchive、delete；
- `SessionDefinitionRevision` 原子绑定 AgentRevisionRef、Workspace、SessionModelConfig 和 SessionPrompts；
- 一个 Agent 对多个 Session、一个 Session 只绑定一个 AgentId；
- Agent `Enabled / Disabled / Deleted` 和 Session `Open / Archived / Deleted`；
- transient `SessionLoadState / SessionReadiness / SessionExecutionState`；
- Session fork、Runtime restart 和 conservative recovery；
- Agent/Session lifecycle 与 active/future Turn 的 race；
- WaitingApproval、WaitingForUserInput、Steer 和 Turn terminal status 的关系。

完成门槛：

- [x] Agent definition 和 Session conversation 没有混合 ownership；
- [x] future Turn 使用哪个 Agent revision 有确定规则；
- [x] Session 不复制 Service-owned definitions、Catalog 或 cache；
- [x] durable lifecycle 与 loaded execution state 分离；
- [x] lifecycle error 和 terminal state 有明确分类；
- [x] 不引入 AgentManager、SessionManager 或 LifecycleService 领域对象。

### 阶段 4：Turn、Item 与 Interaction

对应 V2 文档：[Turn、Item 与 Interaction 架构设计](../modules/turn-item-interaction.md)。

状态：目标架构已确定。

已明确：

- Turn 从 committed initiating UserMessage entry 开始，到 final AssistantMessage、TurnInterrupted 或 TurnFailed entry 结束；
- Steer 只作用于 expected Running Turn，FollowUp 开启下一 Turn；
- `ItemContent = UserMessage | AgentMessage | Reasoning | ToolInvocation`；
- ItemType/ItemStatus 从 ItemContent 派生，不独立保存；
- ToolCall 与 ToolResult 合并为同一个 ToolInvocation Item；
- ToolInvocation `Started → Completed | Abandoned`；
- outcome unknown 不生成 synthetic ToolResult；
- Interaction request/resolution和cancellation family；用户沉默时保持Pending，不定义默认timeout或Deny；
- request-before-notify、resolution-before-resume/side-effect；
- Tool approval 与 UserQuestion 归属于 parent Item；
- UserQuestion由MiniCore producer seam发起，Presentation Adapter只负责presentation与resolution；
- pending Interaction reconnect/resend 和 abrupt transport loss；
- Turn/Item顺序由selected path、assistant content/call顺序和public ordered Vec表达，不增加DisplaySequence；
- `TurnStatus = Running | Completed | Interrupted | Failed` 与 typed terminal detail；
- WaitingApproval、WaitingForUserInput、Steer、terminal cleanup 和 conservative recovery。

完成门槛：

- [x] Item 与 transcript/storage entry 的关系可以被精确定义；
- [x] 每个 Interaction 可追溯到 Item、Turn 和 Session；
- [x] ToolCall、ToolResult 和 approval 使用同一个 ToolInvocation Item identity；
- [x] terminal Turn 不保留 Pending Interaction 或 Started Item；
- [x] AgentMessage/Reasoning started与delta使用稳定ItemId和process-local `StreamingItem`，只有append/apply后才产生Completed Item；
- [x] Snapshot/Query ordered Vec与new-Item StateEvent创建顺序稳定表达Turn/Item展示顺序，Tool逆序完成只原位更新；
- [x] 不引入 ItemManager、InteractionService、ModelStep 或 ToolRound entity。

### 阶段 5：Conversation 与 SessionStorage

对应 V2 文档：[Conversation 与 SessionStorage 架构设计](../modules/conversation-storage.md)。

状态：目标架构已确定。

已明确：

- per-session append-only by-entry JSONL tree；
- `SessionWriter::append(SessionEntryDraft)` 是唯一 runtime write seam；
- SessionHeader 只由 create/fork staging 原子写入；
- Header 后一个物理 line 编码一个 StoredSessionEntry；
- `StoredEntryBody = TurnContext | Message | Event | Compaction`；
- standard message roles `user | assistant | tool`，assistant finalized response 按原始 content 顺序保存；
- operational facts 与 conversation promotion facts 位于同一个 durable log；
- initiating input、Interaction、ToolExecutionStarted 前置记录、tool messages、`tool_round_completed`、Steer、Compaction 和 terminal entries；
- ItemId、ToolCallId 和 EntryId 分离；
- `CommittedConversationState / View / Delta / Checkpoint`；
- trusted all-projection apply、AdvanceOnly 和 mismatch reload；
- `EntryId + parent_id` history tree、current entry 和 stable checkpoint；
- fork staging deep copy + target-local identity remap；
- append-only compaction overlay；
- partial tail、strict corruption、explicit repair 和 conservative recovery；
- projection snapshot/session index 只是 rebuildable cache。

完成门槛：

- [x] durable truth 只有 SessionStorage；
- [x] 任意模型调用只能从 committed transcript 构建 conversation；
- [x] 热内存 projection 可由 storage replay 和 committed entry delta 重建；
- [x] 不存在 current/previous input 等长期特殊消息 lane；
- [x] crash 后不会把半个 ToolRound 提升为模型 transcript；已 append 的 assistant/tool entries 保持 durable，但在 `tool_round_completed` 前不 model-visible；
- [x] 不引入 dual log、Branch entity、baseline SQLite 或 content-addressed DAG。

### 阶段 6-8 模型调用协同交付束

状态：三个模块的目标设计均已完成；生产实现、Rig spike和自动化测试尚未开始。阶段6、7、8继续作为职责索引，但实现不再按`6 → 7 → 8`独立串行验收。

三个模块共享同一条模型调用spine：

```text
SessionExecutor NeedModel
→ PromptSet.assemble(purpose)
→ ModelCallRequest::new(TurnModelSnapshot, purpose, AssembledModelContext)
→ ModelGateway.generate_model_turn(...)
→ AgentRun result或CompactionSummary result
→ SessionExecutor append/apply并继续同一Turn
```

普通调用和summary调用必须共用：

- 同一个`ModelCallRequest`构造和proof校验路径；
- 同一个exact `TurnModelSnapshot`、effective limits、cancellation、usage和error taxonomy；
- 同一个ModelGateway retry/provider adapter边界；
- 不同的typed purpose与output contract：`AgentRun`或`CompactionSummary + NoToolCalls`。

协同实现顺序：

1. 冻结最小共享spine：`ModelCallRequest::new`、purpose、proof、limits和terminal result/error；
2. 先实现`ScriptedProviderAdapter`，但它必须挂在真实ModelGateway private `ProviderAdapter` seam后，SessionExecutor不能直接调用fake model interface；
3. 在scripted adapter上闭环普通调用：Submit → NeedModel → AgentRun request → assistant/tool result → append/apply；
4. 在同一harness闭环overflow路径：AgentRun assembly overflow → Compaction plan → CompactionSummary request → StoredCompaction append/apply → reassemble → AgentRun继续；
5. 尽早并行执行Rig 0.40.0 spike；它不阻塞scripted vertical slice，但在冻结真实provider adapter前必须通过。AgentLoop按[ADR 0115](../adr/0115-agent-loop-is-first-party-state-machine.md)自研，spike范围只覆盖ModelGateway provider mapping，不评估Rig sans-I/O AgentRun；
6. 接入RigProviderAdapter和mock-server tests，证明真实provider mapping与scripted path使用同一ModelGateway contract。

共同完成门槛：

- [ ] scripted ordinary AgentRun vertical slice通过；
- [ ] scripted overflow → summary → append/apply → reassemble → AgentRun vertical slice通过；
- [ ] AgentRun与CompactionSummary都只能通过`ModelCallRequest::new`进入ModelGateway；
- [ ] cancellation、logical retry、usage和typed error在两种purpose下行为一致；
- [ ] Rig 0.40.0 spike验证OpenAI Responses与Anthropic Messages关键映射；
- [ ] production provider adapter与mock-server contract tests通过。

### 阶段 6：Session 执行子系统

对应 V2 文档：[Session Execution 架构设计](../modules/session-execution.md)。

状态：目标架构已确定；作为阶段6–8协同交付束的一部分实现，SessionExecutor与自动化测试尚未落地。研究与handoff记录（Session Execution研究进度，V1阶段文档已删除）不再作为权威。

已明确：

- 一个 loaded Session 由一个 `SessionExecutor` 拥有执行期 mutable state；
- 一个 Runtime 允许多个 SessionExecutor 同时 Running；
- per-session `SessionIngress` semantic lanes和typed request response；Submit/Steer/FollowUp/Interaction/Tool control各自bounded，Cancel/revocation与lifecycle使用sticky signal，Snapshot使用latest-wins mailbox；
- `Idle → Starting → Running → Finishing → Idle` 状态机；
- Context 构造、UserMessage composition、Model 和 Tool 使用异步 `RunningOperation`；
- operation result 使用 `SessionId + TurnId + execution_version + OperationType` 校验；
- private AgentLoop 只返回 `NeedModel | NeedTools | Finished`；
- `ToolExecutionControl` 负责 approval、UserQuestion 和 execution-start 的 required durable ordering；`request_user_question`只在pre-execution ask-user route使用，等待不持有资源锁；
- Submit、Steer、FollowUp、CancelQueuedMessage、ResolveInteraction、Cancel、Workspace revocation、PrepareForUnload 和 Snapshot 流程；
- FollowUp 使用 bounded process-local FIFO；
- progress event 与全部mutation/control ingress lane分离；
- restart 不恢复旧异步操作，unfinished Turn 保守 terminalize；
- multi-session 共享 Model/Tool 资源时使用明确并发限制和 canonical resource locks；WaitingForUserInput只暂停所属Session的逻辑Turn，不阻塞其他Session。
- lane只拆ingress、不拆SessionExecutor/SessionWriter owner；完整决策见[ADR 0111](../adr/0111-session-ingress-separates-control-and-work-lanes.md)。

完成门槛：

- [x] 每个 Session mutation 只有一个执行 owner；
- [x] 同一Turn复用一个TurnExecutionContext、PromptSet、ToolSet和captured SkillView；
- [x] SessionExecutor 驱动 AgentLoop，但 AgentLoop 不拥有 storage、Prompt assembly 或 Tool execution；
- [x] append、projection apply、模型可见性、side effect 和 UI event 顺序无歧义；
- [x] retry、Cancel 和 recovery 不产生重复 terminal fact；
- [x] 普通work lane满不会阻塞Cancel/revocation signal，Unload有有限grace deadline并最终fail closed；
- [x] crate-private interface 可以通过 synthetic request/operation result 集成测试；
- [ ] 通过协同交付束的ordinary/compaction scripted vertical slices；
- [ ] 实现SessionExecutor和自动化测试。

### 阶段 7：ModelGateway

对应 V2 文档：[ModelGateway 架构设计](../modules/model-gateway.md)。

状态：目标架构已确定；作为阶段6–8协同交付束的一部分实现，ModelGateway、provider adapter和测试尚未落地。

已明确：

- `ModelGateway::resolve_for_turn(...)` 固定 exact TurnModelSnapshot；
- `ModelGateway::generate_model_turn(...)` 是唯一真实模型调用 interface；
- `AssembledModelContext` 到 provider request 的 role/tool/output mapping；
- Model identity、capabilities、effective limits 和 generation policy；
- streaming progress 与 finalized result 分离；
- usage、finish reason、reasoning 和 allowlisted provider metadata 规范化；
- provider retry、transport fallback 与 Session logical retry 的边界；
- active Turn 内禁止 transparent cross-model fallback；
- authentication、secret redaction、custom provider 和 concurrency 治理；
- prompt cache、connection reuse 和 continuation 必须保持 full-request equivalence；
- Rig provider 差异只存在于 private adapter。

完成门槛：

- [x] ModelGateway 不重新组装 Prompt；
- [x] PromptSet 是模型上下文的唯一 producer；
- [x] provider adapter 差异不泄漏到 Session execution；
- [x] 错误分类足以驱动 retry、compaction 或 terminal failure；
- [x] provider cache/continuation 不是第二 conversation truth；
- [ ] 执行协同交付束的Rig 0.40.0 integration spike；
- [ ] 实现ModelGateway、ScriptedProviderAdapter、Rig adapter和mock-server tests。

### 阶段 8：Compaction

对应 V2 文档：[Compaction 架构设计](../modules/compaction.md)。

状态：目标架构已确定；作为阶段6–8协同交付束的一部分实现，Compaction module、summary model path和自动化测试尚未落地。

已定义：

- context window pressure 和触发条件；
- transcript cut、protection 和 stable ToolRound 边界；
- summary model call purpose 和 `OutputContract::NoToolCalls`；
- Compaction entry append/apply 与 conversation Replace projection；
- compaction 后 conversation seed；
- compaction 失败、retry 和 recovery；
- Skill、Workspace instructions 和动态 contribution 的保留策略。

完成门槛：

- [x] compaction不直接改写未提交conversation；
- [x] Compaction entry成功append/apply前不替换模型可见历史；
- [x] cut不拆散ToolCall/ToolResult或其他协议稳定单元；
- [x] compacted conversation可从storage重建；
- [ ] 通过scripted overflow → summary → append/apply → reassemble集成测试；
- [ ] 实现Compaction module与storage/projector tests。

目标设计见[Compaction架构设计](../modules/compaction.md)和[ADR 0112](../adr/0112-compaction-supports-active-turn-checkpoints.md)。首版采用portable rolling summary、stable-unit safe cut、leading conversation summary、per-instruction-segment active-Turn checkpoint、model-aware summary budget和有界frontier advancement；不实现manual、hierarchical或provider-native compaction，也不通过强制新Turn代替checkpoint。

### 阶段 9：Runtime interface 与公开协议

对应 V2 文档：[Runtime interface 架构设计](../modules/runtime-interface.md)。

状态：目标架构已确定；protocol types、facade、event publisher、snapshot 和 contract tests 待实现。

已明确：

- `MiniCoreRuntime` 公开 `dispatch / query / snapshot / subscribe` 四类能力；
- 公开领域 identity 使用 `AgentId → SessionId → TurnId → ItemId → RequestId`，不定义 `RunId` 或 `WorkspaceId`；
- Command 在明确线性化点返回 typed outcome，Turn 长期完成通过 Event 发布；
- CommandSurface 是 Runtime 内部无状态命令解释模块，slash text 与 catalog selection 走同一 resolve 路径；
- Runtime和每个Session使用独立Snapshot与snapshot-first实时流，不建立runtime-global sequence、公开cursor/replay或all-loaded stop-the-world barrier；
- StateEvent与可合并/丢弃的ProgressEvent分离；message/reasoning started、delta与append/apply后的completed使用稳定ItemId，断线、背压或restart后重新订阅并获取新Snapshot；
- SessionStorage 拥有 message tree，Runtime 提供 history Query 和 message-anchor Fork command；
- 所有 Runtime mutation 经过 facade，UI selection、draft、scroll 和 layout 留在 adapter；
- 首版不公开 standalone/manual `CompactSession`。

已定义：

- `MiniCoreRuntime` 的最小 command、query、event 和 snapshot interface；
- Agent、Session、Turn、Item、Interaction 的公开 payload；
- command acceptance 与业务完成事件；
- query consistency；
- Snapshot-first原子订阅、stream关闭和重新Snapshot恢复；
- adapter capability 与权限；
- protocol versioning 和兼容策略；
- 哪些内部对象绝不进入公开协议。

完成门槛：

- 外部宿主只依赖 MiniCoreRuntime facade；
- 外部宿主不能直接操作内部 Service、provider、storage 或 Session execution state；
- command/query/event/snapshot 的职责不重叠；
- 所有 payload 都能从已稳定的领域模型和 read model 推导。

### 阶段 10：Extension / Plugin 子系统

对应 V2 文档：Extension 子系统（尚未设计）。

该阶段不是核心迁移的前置条件，只有出现真实扩展需求后再设计，因此在 V1 → V2 迁移中保持未启动状态。

适用场景：一个 versioned package 需要同时贡献 Prompt、Skill、Tool、Hook、MCP 或其他能力，并要求统一 enable、trust、version 和 reload。

此时可以设计：

```text
ExtensionService::for_session(...) → ExtensionSet
```

ExtensionSet管理package lifecycle，不替代PromptSet、ToolSet、SkillView，也不恢复V1通用ResourceManager。

完成门槛：

- 至少存在两个真实 adapter 或 package source；
- package identity、trust、enable 和 version 具有跨贡献的一致语义；
- 各子系统仍拥有自己的解析、执行和安全不变量。

## 每阶段迁移执行流程

各阶段按以下顺序推进：

```text
1. 阅读 V1 代码、V1 旧文档、ADR 和测试
2. 明确领域术语、owner 和不变量
3. 设计目标 interface 和错误模型
4. 检查与已完成子系统的依赖方向
5. 编写 docs/modules 目标文档
6. 对高风险 interface 进行至少两种方案比较
7. 冻结本阶段决策，必要时创建 ADR
8. 先增加目标行为测试或 contract test
9. 实现新模块
10. 迁移调用方
11. 删除临时 adapter 和 V1 实现路径
12. 更新正式文档、索引和 CONTEXT.md
```

迁移原则要求不允许只完成新模块、却长期保留所有 V1 调用路径。阶段完成意味着调用方已经切换，V1 owner 已经失去职责。

## 阶段完成检查表

每个阶段结束前必须确认：

- [ ] 领域 owner 唯一；
- [ ] interface 足够小，并隐藏实现复杂性；
- [ ] success、unavailable、conflict 和 failure 有 typed 表达；
- [ ] active execution 与 future reload 的关系明确；
- [ ] fingerprint/version/identity 覆盖必要的一致性事实；
- [ ] storage、cache 和 projection 没有形成双 source of truth；
- [ ] tests 通过公开 interface 验证核心不变量；
- [ ] 所有调用方已迁移到新 seam；
- [ ] 临时 compatibility adapter 已删除或记录明确删除阶段；
- [ ] V1 术语和 V1 ownership 已从正式入口清除；
- [ ] ADR 和文档状态已更新。

## 文档收尾策略

### 迁移期间

V2 目标文档描述独立目标架构，不描述迁移过程，也不承诺兼容 V1 模块类型。

V1 版 `CONTEXT.md`、`docs/architecture.md` 和 `docs/modules/` 在生产代码尚未切换前仍可以描述当前实现，但不得被当作目标模型的依据。

出现冲突时明确区分：

```text
当前实现事实 → V1 正式文档（归档至 docs/archive/v1/）
目标架构决策 → V2 docs/modules
历史原因     → ADR / research / review
```

不把目标设计提前混入 V1 正式文档，避免制造既不描述当前实现、也不完整描述目标架构的中间版本。

### 子系统完成后

当某个子系统已经实现并完成调用方切换：

1. 将对应目标文档内容整理到正式 [`docs/modules/`](../modules/) 或正式架构目录；
2. 把仍然正确的行为、不变量和运维说明合并到新正式文档；
3. 归档描述 V1 owner、V1 调用链和 V1 类型的模块文档；
4. 更新 [`docs/architecture.md`](../architecture.md)、`docs/modules/README.md`、`README.md` 和 `CONTEXT.md`；
5. 清理所有指向 V1 文档的链接；
6. 为被替代的 ADR 增加 `Superseded by ADR-xxxx`。

### 整体完成后

迁移完成后仓库只保留一个正式事实来源：

```text
正式架构文档 → 描述当前系统（V2）
ADR          → 记录为什么选择和为什么替代
research     → 保存外部研究与事实依据
review       → 保存阶段性审查记录
docs/archive/v1/ → 保存已替代的 V1 正式文档
Git history  → 保存已删除的旧实现文档
```

处理规则：

- V1 模块文档仍描述 V1 架构：提炼有效内容后归档到 [`docs/archive/v1/`](../archive/v1/)；
- V1 文档部分仍正确：内容合并进 V2 正式文档，不保留并列版本；
- Accepted ADR 已被替代：保留原文并标记 superseded，不重写历史；
- research/review/progress：保留或移动到 [`docs/archive/`](../archive/)，但明确非权威；
- V2 目标文档：目标架构成为当前实现后已移动到 [`docs/modules/`](../modules/) 正式位置，不长期保留独立“重构”副本。

Git 已经保存历史版本，因此不为了“以后可能查看”而保留会误导维护者的 V1 正式文档。

## ADR 策略

以下情况必须创建或更新 ADR：

- 改变顶层 ownership；
- 删除或替代一个长期模块；
- 改变 durable truth 或 conversation projection 更新顺序；
- 改变公开 protocol contract；
- 引入新的跨子系统 lifecycle；
- 推翻已有 Accepted ADR。

ADR 只记录具有长期影响的决策，不记录每个字段和实现步骤。

被替代 ADR 的处理方式：

```text
旧 ADR：保留原始 Context / Decision / Consequences
状态：Superseded
增加：Superseded by ADR-00xx

新 ADR：描述新的背景、决定和后果
状态：Accepted
```

## 实现与测试策略

迁移过程中，测试范围按风险扩展：

- value type、排序、fingerprint：单元测试；
- Service interface、cache、reload、conflict：模块测试；
- TurnExecutionContext、ToolRound 和 conversation projection 更新顺序：集成测试；
- storage reload、fork、crash recovery：持久化测试；
- provider mapping：adapter contract test；
- Runtime command/event/snapshot：端到端 protocol test。

阶段6–8优先建立一个共享的scripted vertical-slice harness。该harness必须经过真实`PromptSet → ModelCallRequest::new → ModelGateway → ProviderAdapter`路径，不能为SessionExecutor或Compaction建立第二个fake request seam。Rig spike与该harness并行推进，并在production provider adapter冻结前完成。

关键不变量必须拥有测试：

```text
append/apply-before-model-visible
只有 ToolRoundCompleted 引用的完整 round 才可见
Prompt 是唯一上下文组装 seam
ToolSpec 与 executor route 同快照
active Turn 不受 future reload 影响
Skill metadata 与正文 identity 一致
SessionStorage 是 durable truth
Runtime facade 是唯一外部入口
```

## 整体迁移完成标准

只有同时满足以下条件，才能宣布 V1 → V2 迁移完成：

- [x] 所有阶段 1–9 的目标文档已稳定；
- [ ] 目标 interface 已实现；
- [ ] 生产调用方不再依赖 V1 ResourceManager、V1 Prompt、V1 Tools 或 V1 SessionRuntime ownership；
- [ ] 所有模型调用只接收 ModelCallRequest，且其唯一 model-visible input 是 `AssembledModelContext`；
- [ ] 所有模型可见 conversation fact 遵守 append/apply 和 conversation projection 规则；
- [x] Agent/Session revision 与 durable/runtime lifecycle 语义已确定；
- [x] TurnExecutionContext可以稳定绑定AgentRevisionRef、SessionDefinitionRevision、WorkspaceSnapshot、PromptSet、ToolSet、SkillView和TurnModelSnapshot；
- [x] Session load/reload/unload 与 fork lifecycle 有确定行为；
- [x] Turn/Item/Interaction 的 identity、lifecycle 和 terminal cleanup 已确定；
- [x] pending Interaction 的 request/resolution、reconnect 和 recovery 行为已确定；
- [x] Conversation/SessionStorage durable ownership、entry tree、fork 和 recovery 已确定；
- [x] compaction orchestration、stable cut、active-Turn checkpoint、model-aware summary budget、StoredCompaction 和 bounded recovery 有确定定义；
- [x] Runtime command/query/event/snapshot interface 已冻结；
- [ ] 关键不变量有自动化测试；
- [x] 新文档已进入正式架构目录 [`docs/modules/`](../modules/)；
- [ ] V1 正式文档已归档或完成 supersede；
- [ ] README、CONTEXT.md、架构索引和所有内部链接只指向 V2 架构。

Extension / Plugin 子系统只有在产品确实需要可安装扩展包时才进入整体完成条件。

## V1 → V2 文档对应

本节记录 V1 与 V2 的模块/ADR 对应关系与归档位置。

- V1 旧模块文档 `docs/modules/*` 与 V1 ADR `docs/adr/0001`–`docs/adr/0028` 已归档到 [`docs/archive/v1/`](../archive/v1/)，仅作历史参考，非权威。
- V2 新架构由 [`docs/architecture.md`](../architecture.md) + [`docs/modules/`](../modules/)（12 篇模块文档）+ [`docs/adr/`](../adr/)（0100–0114）构成，是当前唯一权威事实来源。

子系统文档对应：

| V2 目标文档 | V1 对应内容（归档于 `docs/archive/v1/`） |
| --- | --- |
| [`../architecture.md`](../architecture.md) | V1 领域模型（minicore-domain-model） |
| [`../modules/prompt.md`](../modules/prompt.md) | V1 Prompt 子系统 |
| [`../modules/tools.md`](../modules/tools.md) | V1 Tool 子系统 |
| [`../modules/skills.md`](../modules/skills.md) | V1 Skill 子系统 |
| [`../modules/workspace.md`](../modules/workspace.md) | V1 Workspace / ResourceManager 相关模块 |
| [`../modules/turn-execution-context.md`](../modules/turn-execution-context.md) | V1 Turn 执行相关模块 |
| [`../modules/agent-session-lifecycle.md`](../modules/agent-session-lifecycle.md) | V1 Agent / Session 生命周期 |
| [`../modules/turn-item-interaction.md`](../modules/turn-item-interaction.md) | V1 Turn / Item / Interaction |
| [`../modules/conversation-storage.md`](../modules/conversation-storage.md) | V1 Conversation / SessionStorage |
| [`../modules/session-execution.md`](../modules/session-execution.md) | V1 Session 执行子系统（Session Execution 研究进度已删除） |
| [`../modules/model-gateway.md`](../modules/model-gateway.md) | V1 模型调用路径 |
| [`../modules/compaction.md`](../modules/compaction.md) | V1 无独立 compaction 设计 |
| [`../modules/runtime-interface.md`](../modules/runtime-interface.md) | V1 Runtime 公开协议 |

ADR 对应：

- V1 ADR `0001`–`0028` 归档于 [`docs/archive/v1/`](../archive/v1/)。
- V2 ADR采用`0100`–`0114`编号，位于[`docs/adr/`](../adr/)。Compaction当前决策由[ADR 0112](../adr/0112-compaction-supports-active-turn-checkpoints.md)记录并取代ADR 0107；Session ingress控制/工作lane决策由[ADR 0111](../adr/0111-session-ingress-separates-control-and-work-lanes.md)记录；UserQuestion producer与UI presentation决策由[ADR 0113](../adr/0113-user-question-uses-runtime-protocol-and-ui-presentation.md)记录；snapshot-first观察协议由[ADR 0114](../adr/0114-runtime-observation-uses-snapshot-first-streams.md)记录。

## 当前迁移状态

阶段1–9目标设计已完成并进入V2正式文档。仓库当前仍为文档阶段，没有`Cargo.toml`、`src/`或`tests/`；production interface和自动化测试尚未实现。

下一实现里程碑是阶段6–8模型调用协同交付束：先建立ScriptedProviderAdapter vertical slice并尽早完成Rig integration spike，再共同落地SessionExecutor、ModelGateway provider adapter和Compaction闭环。该交付束通过后，才进入Runtime protocol types、facade routing、snapshot-first publisher和CommandSurface target architecture实现。

Extension / Plugin 阶段仍不是核心实现前置条件；只有出现至少两个真实 package/adapter source 后再启动阶段 10 设计。
