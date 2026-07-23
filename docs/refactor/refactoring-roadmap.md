# MiniCore 重构路线与总体策略

状态：重构过程说明
日期：2026-07-16

## 目的

本文是 MiniCore 后续重构工作的过程权威文档，规定：

- 重构目标与总体原则；
- 各子系统的设计和实现顺序；
- 每个阶段的输入、输出和完成门槛；
- 新旧架构并存期间的控制方式；
- ADR、正式架构文档、研究文档和旧文档的最终处理策略；
- 整体重构完成的判定标准。

本文不替代各子系统的架构设计。领域类型、interface、状态和不变量仍以对应的 `docs/refactor/*.md` 为权威。

## 总体目标

重构后的 MiniCore 使用以下领域关系：

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
SkillService::catalog(...)   → SkillCatalog
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

Prompt、Tool、Skill 不合并为通用 `Resource`。重构前的 `ResourceManager`、`ResourceSnapshotStore`、通用 Resource overlay 和四层 Resource snapshot 不进入目标架构。其正确的不变量分别下沉到 Workspace、Prompt、Tool、Skill 和 Turn 执行上下文。

## 总体策略

### 目标模型优先

重构以目标领域模型和目标 interface 为起点，不以旧模块名称、旧调用链或旧持久化结构为设计约束。

旧实现只用于回答以下问题：

- 当前有哪些必须保留的用户行为；
- 哪些边界条件和失败场景已经被处理；
- 哪些测试可以作为回归保护；
- 哪些外部协议需要显式迁移。

旧实现不能因为已经存在，就自动成为目标架构的一部分。

### Replace，不 Layer

新模块应替换旧职责，而不是长期包裹旧模块形成双层转发：

```text
推荐：
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

同一份领域事实只能有一个权威 owner：

- conversation durable truth 属于 Session storage；
- Agent definition 属于 Agent owner；
- Workspace definition 属于 Session；canonical roots、effective grants 和窄 view 由 WorkspaceResolver 原子解析；
- 全局 trust decision 和 managed policy 属于 WorkspaceAuthority adapter，不复制进 Prompt、Tool 或 Skill；
- Prompt definitions、解析和 PromptSet 属于 Prompt 子系统；
- Tool 注册、披露、执行和 ToolSet 属于 Tool 子系统；
- Skill discovery、Catalog、完整内容和 cache 属于 Skill 子系统；
- 最终模型可见上下文属于 PromptSet；
- provider-specific encoding 和调用属于 ModelGateway。

内存 projection、cache、snapshot 和 UI read model 只能由权威事实派生，不能成为并列 source of truth。

### 深模块优先

每个子系统应提供较小的 interface，并把 discovery、排序、校验、cache、错误分类、并发和 diagnostics 隐藏在实现内部。

设计每个模块时至少回答：

1. 它拥有什么事实和生命周期？
2. 哪些复杂性只有它能消除？
3. 调用方必须知道哪些最少信息？
4. 删除该模块后，复杂性是否会重新散落到多个调用方？
5. interface 是否可以同时作为生产调用和测试入口？

不要为了类型对称而制造浅层 Manager、Registry、Coordinator 或 adapter。

### 领域快照独立

不同领域只冻结自己真正拥有的稳定值：

```text
PromptSet    → Prompt 的 Turn 有效快照
ToolSet      → ToolSpec 与 executor route 的原子快照
SkillCatalog → Skill metadata 的有效快照
LoadedSkill  → 某个精确定义的不可变正文
```

Session execution 可以把这些值组合成局部 `TurnExecutionContext`，但该对象不是新的通用资源 owner。

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

字段保持私有；模型调用和 Tool 执行通过窄操作完成，避免不同 Turn 的子快照被交叉组合。

### Transcript-First与conversation projection更新顺序

所有模型可见conversation fact必须先形成规范化entry、成功append并apply trusted delta：

```text
UserMessage(source = Input | Steer)
ToolRoundCompleted引用的完整assistant/tool sequence
Compaction entry的Replace projection
AssistantMessage(phase = Final)
```

未append的draft不能进入下一次模型调用；已durable但尚无`tool_round_completed`的assistant/tool entries同样不能model-visible。

SessionStorage是durable truth。热内存conversation projection只消费`CommittedSessionEntry`返回的trusted delta，不在稳态重新扫描完整存储，也不与存储共同拥有同一事实。

### 先内部不变量，后公开协议

公开 command、query、event 和 snapshot interface 最后冻结。

在 Workspace、Turn、Item、Interaction、Session execution、storage 和 recovery 尚未稳定前，不应先根据旧协议反推领域模型，也不应为了旧 payload 保留错误的内部 ownership。

## 重构阶段

### 阶段 0：基础领域和三个核心子系统

状态：设计进行中。

现有文档：

- [MiniCore 领域模型](minicore-domain-model.md)
- [Prompt 子系统](prompt-subsystem.md)
- [Tool 子系统](tool-subsystem.md)
- [Skill 子系统](skill-subsystem.md)

本阶段完成条件：

- Prompt、Tool、Skill 的 owner 和核心 interface 已确定；
- Prompt 是唯一模型上下文组装 seam；
- ToolSet 原子绑定模型可见 ToolSpec 和 executor route；
- SkillCatalog 与 LoadedSkill 分离，正文默认按需加载；
- 不再把三者合并为通用 Resource；
- 已记录尚未解决的 scope、identity、cache、reload 和 recovery 问题。

### 阶段 1：Workspace 子系统

状态：目标架构已确定；实现 integration 待后续阶段完成。

目标文档：[Workspace 子系统](workspace-subsystem.md)

删除通用 ResourceManager 后，Workspace 模块统一解释 Session 的 roots、cwd、trust decision、source authorization 和 filesystem capability，但不建立 Runtime-global WorkspaceService 或 registry。

必须定义：

- Workspace 是否需要独立 entity identity；当前结论是不定义 `WorkspaceId`；
- Session-owned Workspace definition、`WorkspaceRevision` 和 `WorkspaceFingerprint`；
- primary root、additional roots 和 cwd 合法域；
- Workspace 属于 Session 的精确语义；
- trust、source authorization 和 filesystem capability；
- Prompt、Tool、Skill 可以消费的窄只读 view；
- Workspace 更新对 active Turn 和 future Turn 的影响；
- 同根多 Session 的 mutable state 和 authorization 隔离；
- Workspace unavailable 和 reload 语义；
- ordinary reload 与 security-restricting revocation 的区别。

建议核心输出：

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

状态：目标架构已确定；Session execution、storage 和 ModelGateway integration 待后续阶段完成。

目标文档：[Turn 执行模块与执行上下文架构设计](turn-execution-context.md)

必须定义：

- 领域 Turn、Turn execution 和 AgentLoop 的开始与结束边界；
- `SkillCatalog`、`ToolSet`、`PromptSet` 的 capture 依赖图；
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
├─ SkillService::catalog(SkillCatalogContext {
│    agent, session_id, session_revision, workspace: workspace.skill_context()
│  }) → SkillCatalog
└─ ToolService::for_turn(ToolTurnContext {
     agent, session_id, session_revision, turn_id,
     workspace: workspace.tool_context(),
     provider: model.capabilities(), execution_mode, execution_control, cancellation, progress_events
   }) → ToolSet

SkillCatalog.prompt_view()
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

目标文档：[Agent 与 Session 生命周期架构设计](agent-session-lifecycle.md)。

状态：目标架构已确定。

已明确：

- Agent 创建、更新、禁用、删除及 immutable `AgentRevision` 生成规则；
- Session pin exact `AgentRevisionRef`，Agent update 不自动改变已有 Session；
- Session 显式升级 Agent revision 的 CAS 和同 AgentId 约束；
- Session create、definition update、load/unload、archive/unarchive、delete；
- `SessionDefinitionRevision`原子绑定AgentRevisionRef、Workspace、SessionModelConfig和SessionPrompts；
- 一个 Agent 对多个 Session、一个 Session 只绑定一个 AgentId；
- Agent `Enabled / Disabled / Deleted` 和 Session `Open / Archived / Deleted`；
- transient `SessionLoadState / SessionReadiness / SessionExecutionState`；
- Session fork、Runtime restart 和 conservative recovery；
- Agent/Session lifecycle 与 active/future Turn 的 race；
- WaitingApproval、Steer 和 Turn terminal status 的关系。

完成门槛：

- [x] Agent definition 和 Session conversation 没有混合 ownership；
- [x] future Turn 使用哪个 Agent revision 有确定规则；
- [x] Session 不复制 Service-owned definitions、Catalog 或 cache；
- [x] durable lifecycle 与 loaded execution state 分离；
- [x] lifecycle error 和 terminal state 有明确分类；
- [x] 不引入 AgentManager、SessionManager 或 LifecycleService 领域对象。

### 阶段 4：Turn、Item 与 Interaction

目标文档：[Turn、Item 与 Interaction 架构设计](turn-item-interaction.md)。

状态：目标架构已确定。

已明确：

- Turn 从 committed initiating UserMessage entry 开始，到 final AssistantMessage、TurnInterrupted 或 TurnFailed entry 结束；
- Steer 只作用于 expected Running Turn，FollowUp 开启下一 Turn；
- `ItemContent = UserMessage | AgentMessage | Reasoning | ToolInvocation`；
- ItemType/ItemStatus 从 ItemContent 派生，不独立保存；
- ToolCall 与 ToolResult 合并为同一个 ToolInvocation Item；
- ToolInvocation `Started → Completed | Abandoned`；
- outcome unknown 不生成 synthetic ToolResult；
- Interaction request/resolution、timeout 和 cancellation family；
- request-before-notify、resolution-before-resume/side-effect；
- Tool approval 与 UserQuestion 归属于 parent Item；
- pending Interaction reconnect/resend 和 abrupt transport loss；
- `TurnStatus = Running | Completed | Interrupted | Failed` 与 typed terminal detail；
- WaitingApproval、Steer、terminal cleanup 和 conservative recovery。

完成门槛：

- [x] Item 与 transcript/storage entry 的关系可以被精确定义；
- [x] 每个 Interaction 可追溯到 Item、Turn 和 Session；
- [x] ToolCall、ToolResult 和 approval 使用同一个 ToolInvocation Item identity；
- [x] terminal Turn 不保留 Pending Interaction 或 Started Item；
- [x] streaming delta/progress 与 durable Item truth 分离；
- [x] 不引入 ItemManager、InteractionService、ModelStep 或 ToolRound entity。

### 阶段 5：Conversation 与 SessionStorage

目标文档：[Conversation 与 SessionStorage 架构设计](conversation-storage.md)。

状态：目标架构已确定。

已明确：

- per-session append-only by-entry JSONL tree；
- `SessionWriter::append(SessionEntryDraft)` 是唯一 runtime write seam；
- SessionHeader 只由 create/fork staging 原子写入；
- Header 后一个物理 line 编码一个 StoredSessionEntry；
- `StoredEntryBody = TurnContext | Message | Event | Compaction`；
- standard message roles `user | assistant | tool`，assistant finalized response 按原始 content 顺序保存；
- operational facts 与 conversation promotion facts 位于同一个 durable log；
- initiating input、Interaction、ToolExecutionStarted前置记录、tool messages、`tool_round_completed`、Steer、Compaction和terminal entries；
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

### 阶段 6：Session 执行子系统

目标文档：

```text
docs/refactor/session-execution.md
```

状态：目标架构已确定。

研究与handoff：[Session Execution 研究进度](session-execution-progress.md)。

已明确：

- 一个loaded Session由一个`SessionExecutor`拥有执行期mutable state；
- 一个Runtime允许多个SessionExecutor同时Running；
- bounded FIFO `SessionRequestQueue`和typed request response；
- `Idle → Starting → Running → Finishing → Idle`状态机；
- Context构造、UserMessage composition、Model和Tool使用异步`RunningOperation`；
- operation result使用`SessionId + TurnId + execution_version + OperationType`校验；
- private AgentLoop只返回`NeedModel | NeedTools | Finished`；
- `ToolExecutionControl`负责approval和execution-start的required durable ordering；
- Submit、Steer、FollowUp、ResolveInteraction、Cancel、PrepareForUnload和Snapshot流程；
- FollowUp使用bounded process-local FIFO；
- progress event与request queue分离；
- restart不恢复旧异步操作，unfinished Turn保守terminalize；
- multi-session共享Model/Tool资源时使用明确并发限制和canonical resource locks。

完成门槛：

- [x] 每个Session mutation只有一个执行owner；
- [x] 同一Turn复用一个TurnExecutionContext、PromptSet、ToolSet和pinned SkillCatalog；
- [x] SessionExecutor驱动AgentLoop，但AgentLoop不拥有storage、Prompt assembly或Tool execution；
- [x] append、projection apply、模型可见性、side effect和UI event顺序无歧义；
- [x] retry、Cancel和recovery不产生重复terminal fact；
- [x] crate-private interface可以通过synthetic request/operation result集成测试；
- [ ] 完成Rig 0.40.0 adapter spike；
- [ ] 实现SessionExecutor和自动化测试。

### 阶段 7：ModelGateway

目标文档：

```text
docs/refactor/model-gateway.md
```

状态：目标架构已确定。

已明确：

- `ModelGateway::resolve_for_turn(...)`固定exact TurnModelSnapshot；
- `ModelGateway::generate_model_turn(...)`是唯一真实模型调用interface；
- `AssembledModelContext`到provider request的role/tool/output mapping；
- Model identity、capabilities、effective limits和generation policy；
- streaming progress与finalized result分离；
- usage、finish reason、reasoning和allowlisted provider metadata规范化；
- provider retry、transport fallback与Session logical retry的边界；
- active Turn内禁止transparent cross-model fallback；
- authentication、secret redaction、custom provider和concurrency治理；
- prompt cache、connection reuse和continuation必须保持full-request equivalence；
- Rig provider差异只存在于private adapter。

完成门槛：

- [x] ModelGateway不重新组装Prompt；
- [x] PromptSet是模型上下文的唯一producer；
- [x] provider adapter差异不泄漏到Session execution；
- [x] 错误分类足以驱动retry、compaction或terminal failure；
- [x] provider cache/continuation不是第二conversation truth；
- [ ] 执行Rig 0.40.0 ModelGateway integration spike；
- [ ] 实现ModelGateway、Rig adapter和mock-server tests。

### 阶段 8：Compaction

目标文档：

```text
docs/refactor/compaction.md
```

必须定义：

- context window pressure 和触发条件；
- transcript cut、protection 和 stable ToolRound 边界；
- summary model call purpose 和 `OutputContract::NoToolCalls`；
- Compaction entry append/apply与conversation Replace projection；
- compaction 后 conversation seed；
- compaction 失败、retry 和 recovery；
- Skill、Workspace instructions 和动态 contribution 的保留策略。

完成门槛：

- compaction 不直接改写未提交 conversation；
- Compaction entry成功append/apply前不替换模型可见历史；
- cut 不拆散 ToolCall/ToolResult 或其他协议稳定单元；
- compacted conversation 可从 storage 重建。

状态：目标设计已完成，见[Compaction架构设计](compaction.md)和[ADR 0027](../adr/0027-compaction-uses-strict-stable-suffix.md)。首版采用portable rolling summary、strict stable-unit cut、连续retained suffix和有界active-Turn recovery；不实现split-turn、manual、hierarchical或provider-native compaction。

### 阶段 9：Runtime interface 与公开协议

目标文档：

```text
docs/refactor/runtime-interface.md
```

必须定义：

- `MiniCoreRuntime` 的最小 command、query、event 和 snapshot interface；
- Agent、Session、Turn、Item、Interaction 的公开 payload；
- command acceptance 与业务完成事件；
- query consistency；
- snapshot 水位和重连；
- adapter capability 与权限；
- protocol versioning 和兼容策略；
- 哪些内部对象绝不进入公开协议。

完成门槛：

- 外部宿主只依赖 MiniCoreRuntime facade；
- 外部宿主不能直接操作内部 Service、provider、storage 或 Session execution state；
- command/query/event/snapshot 的职责不重叠；
- 所有 payload 都能从已稳定的领域模型和 read model 推导。

### 阶段 10：Extension / Plugin 子系统

目标文档：

```text
docs/refactor/extension-subsystem.md
```

该阶段不是核心重构的前置条件，只有出现真实扩展需求后再设计。

适用场景：一个 versioned package 需要同时贡献 Prompt、Skill、Tool、Hook、MCP 或其他能力，并要求统一 enable、trust、version 和 reload。

此时可以设计：

```text
ExtensionService::for_session(...) → ExtensionSet
```

ExtensionSet 管理 package lifecycle，不替代 PromptSet、ToolSet、SkillCatalog，也不恢复通用 ResourceManager。

完成门槛：

- 至少存在两个真实 adapter 或 package source；
- package identity、trust、enable 和 version 具有跨贡献的一致语义；
- 各子系统仍拥有自己的解析、执行和安全不变量。

## 每阶段执行流程

每个阶段按以下顺序推进：

```text
1. 阅读现有代码、旧文档、ADR 和测试
2. 明确领域术语、owner 和不变量
3. 设计目标 interface 和错误模型
4. 检查与已完成子系统的依赖方向
5. 编写 docs/refactor 目标文档
6. 对高风险 interface 进行至少两种方案比较
7. 冻结本阶段决策，必要时创建 ADR
8. 先增加目标行为测试或 contract test
9. 实现新模块
10. 迁移调用方
11. 删除临时 adapter 和旧实现路径
12. 更新正式文档、索引和 CONTEXT.md
```

不允许只完成新模块、却长期保留所有旧调用路径。阶段完成意味着调用方已经切换，旧 owner 已经失去职责。

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
- [ ] 旧术语和旧 ownership 已从正式入口清除；
- [ ] ADR 和文档状态已更新。

## 文档策略

### 重构期间

`docs/refactor/` 描述独立目标架构，不描述迁移过程，也不承诺兼容旧模块类型。

旧版 `CONTEXT.md`、`docs/architecture.md` 和 `docs/modules/` 在生产代码尚未切换前，仍可以描述当前实现，但不得被当作目标模型的依据。

出现冲突时必须明确区分：

```text
当前实现事实 → 旧正式文档
目标架构决策 → docs/refactor
历史原因     → ADR / research / review
```

不要把目标设计提前混入旧正式文档，制造一个既不描述当前实现、也不完整描述目标架构的中间版本。

### 子系统完成后

当某个子系统已经实现并完成调用方切换：

1. 将对应 `docs/refactor` 内容整理到正式 `docs/modules/` 或新的正式架构目录；
2. 把仍然正确的行为、不变量和运维说明合并到新正式文档；
3. 删除描述旧 owner、旧调用链和旧类型的模块文档；
4. 更新 `docs/architecture.md`、`docs/modules/README.md`、`README.md` 和 `CONTEXT.md`；
5. 清理所有指向旧文档的链接；
6. 为被替代的 ADR 增加 `Superseded by ADR-xxxx`。

### 整体完成后

最终仓库只保留一个正式事实来源：

```text
正式架构文档 → 描述当前系统
ADR          → 记录为什么选择和为什么替代
research     → 保存外部研究与事实依据
review       → 保存阶段性审查记录
Git history  → 保存已删除的旧实现文档
```

处理规则：

- 旧模块文档仍描述旧架构：提炼有效内容后删除；
- 旧文档部分仍正确：内容合并进新正式文档，不保留并列版本；
- Accepted ADR 已被替代：保留原文并标记 superseded，不重写历史；
- research/review/progress：可以保留或移动到 `docs/archive/`，但必须明确非权威；
- `docs/refactor/`：目标架构成为当前实现后，应移动到正式位置，不长期保留“refactor”副本。

Git 已经保存历史版本，因此不要仅为了“以后可能查看”而保留会误导维护者的旧正式文档。

## ADR 策略

以下情况必须创建或更新 ADR：

- 改变顶层 ownership；
- 删除或替代一个长期模块；
- 改变durable truth或conversation projection更新顺序；
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

当前仓库尚未进入完整生产实现阶段。开始实现后，测试范围按风险扩展：

- value type、排序、fingerprint：单元测试；
- Service interface、cache、reload、conflict：模块测试；
- TurnExecutionContext、ToolRound和conversation projection更新顺序：集成测试；
- storage reload、fork、crash recovery：持久化测试；
- provider mapping：adapter contract test；
- Runtime command/event/snapshot：端到端 protocol test。

关键不变量必须拥有测试：

```text
append/apply-before-model-visible
只有ToolRoundCompleted引用的完整round才可见
Prompt 是唯一上下文组装 seam
ToolSpec 与 executor route 同快照
active Turn 不受 future reload 影响
Skill metadata 与正文 identity 一致
SessionStorage 是 durable truth
Runtime facade 是唯一外部入口
```

## 整体重构完成标准

只有同时满足以下条件，才能宣布重构完成：

- [ ] 所有阶段 1–9 的目标文档已稳定；
- [ ] 目标 interface 已实现；
- [ ] 生产调用方不再依赖旧 ResourceManager、旧 Prompt、旧 Tools 或旧 SessionRuntime ownership；
- [ ] 所有模型调用只接收ModelCallRequest，且其唯一model-visible input是`AssembledModelContext`；
- [ ] 所有模型可见conversation fact遵守append/apply和conversation projection规则；
- [x] Agent/Session revision 与 durable/runtime lifecycle 语义已确定；
- [x] TurnExecutionContext 可以稳定绑定 AgentRevisionRef、SessionDefinitionRevision、WorkspaceSnapshot、PromptSet、ToolSet、SkillCatalog和TurnModelSnapshot；
- [x] Session load/reload/unload 与 fork lifecycle 有确定行为；
- [x] Turn/Item/Interaction 的 identity、lifecycle 和 terminal cleanup 已确定；
- [x] pending Interaction 的 request/resolution、reconnect 和 recovery 行为已确定；
- [x] Conversation/SessionStorage durable ownership、entry tree、fork 和 recovery 已确定；
- [x] compaction orchestration、stable cut、StoredCompaction和bounded recovery有确定定义；
- [ ] Runtime command/query/event/snapshot interface 已冻结；
- [ ] 关键不变量有自动化测试；
- [ ] 新文档已进入正式架构目录；
- [ ] 旧正式文档已删除或完成 supersede；
- [ ] README、CONTEXT.md、架构索引和所有内部链接只指向当前架构。

Extension / Plugin 子系统只有在产品确实需要可安装扩展包时才进入整体完成条件。

## 当前下一步

按照本文顺序，下一份目标设计文档是：

```text
docs/refactor/runtime-interface.md
```

Compaction已确定为“portable rolling summary + strict stable-unit cut + contiguous retained suffix + one StoredCompaction entry + bounded active-Turn recovery”。下一阶段冻结Runtime command/query/event/snapshot协议，包括manual `CompactSession`是否需要独立Session maintenance state；生产实现仍需Rig integration spike、SessionExecutor、ModelGateway provider adapter和Compaction测试共同验证。
