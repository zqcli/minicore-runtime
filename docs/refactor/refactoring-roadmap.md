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

### Transcript-First 与 commit gate

所有模型可见 conversation fact 必须先形成规范化领域值并成功 commit：

```text
UserMessage
完整 ToolRound
Steer control input
Compaction result
final AgentMessage
```

未 commit 的 draft、局部 ToolCall、孤立 ToolResult 或临时内存状态不能进入下一次模型调用。

Session storage 是 durable truth。热内存 conversation projection 只消费成功 commit 返回的 delta，不在稳态重新扫描完整存储，也不与存储共同拥有同一事实。

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
     provider: model.capabilities(), execution_mode, cancellation, updates
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
- `SessionDefinitionRevision` 原子绑定 AgentRevisionRef、Workspace、Model 和 SessionPrompts；
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

目标文档：

```text
docs/refactor/turn-item-interaction.md
```

必须定义：

- Turn 从 UserMessage 开始、到下一条 UserMessage 前结束；
- Steer 作为 current Turn control input 的条件；
- `ItemType` 和 `ItemContent` 的最小封闭集合；
- UserMessage、AgentMessage、Reasoning、ToolCall、ToolResult 的 Item 表达；
- Interaction request、resolution、timeout 和 cancellation；
- Tool approval 与 ToolCall Item 的归属；
- pending Interaction 的 reconnect/resend 和 abrupt transport loss；
- `TurnStatus = Running | Completed | Interrupted | Failed`；
- WaitingApproval 等 execution phase 与 InteractionStatus 的关系；
- cancel、shutdown、Unavailable diagnostics 和 terminal semantics。

完成门槛：

- Item 与 transcript/storage entry 的关系可以被精确定义；
- 每个 Interaction 可追溯到 Item、Turn 和 Session；
- ToolCall、ToolResult 和 approval 不再使用并列、互不关联的 identity。

### 阶段 5：Conversation 与 SessionStorage

目标文档：

```text
docs/refactor/conversation-storage.md
```

必须定义：

- 单一有序 transcript；
- committed batch 和 commit-before-visible；
- SessionStorage 的 interface 与 durable ownership；
- UserInput、完整 ToolRound、Steer、Compaction 和 final AgentMessage 的 commit unit；
- `ItemId`、storage entry identity 和 fork identity；
- 一个 ToolCall Item 对多条持久化 entry 的映射；
- `CommittedConversationState` 的 delta apply；
- reload、repair、corruption 和 partial write；
- fork、branch、current leaf 和 stable navigation boundary。

完成门槛：

- durable truth 只有 SessionStorage；
- 任意模型调用只能从 committed transcript 构建 conversation；
- 热内存 projection 可由存储和 commit delta 重建；
- 不存在 current/previous input 等长期特殊消息 lane；
- crash 后不会把半个 ToolRound 暴露给模型或 UI。

### 阶段 6：Session 执行子系统

目标文档：

```text
docs/refactor/session-execution.md
```

必须定义：

- Turn admission 和 UserMessage 创建；
- TurnExecutionContext 创建和持有；
- model → Tool → model 循环；
- stable ToolRound；
- Steer 作为 current Turn control input、FollowUp 作为下一 Turn submission；
- retry、overflow recovery、cancellation 和 timeout；
- pending Interaction 的等待与恢复；
- commit、事件和状态变更的顺序；
- Session actor、run task 或其他 execution owner 是否必要。

只有在状态机和所有权明确后，才决定最终使用 manager、actor、task 或 registry 的具体形状。

完成门槛：

- 每个 mutation 只有一个执行 owner；
- 同一 Turn 复用同一个 TurnExecutionContext、PromptSet、ToolSet 和 pinned SkillCatalog；
- Session execution 驱动 AgentLoop，但 AgentLoop 不拥有 storage、Prompt assembly 或 Tool execution；
- commit、模型可见性和 UI 可见性顺序无歧义；
- abort/retry/recovery 不产生重复 terminal fact；
- session execution 可以通过公开 interface 进行集成测试。

### 阶段 7：ModelGateway

目标文档：

```text
docs/refactor/model-gateway.md
```

必须定义：

- `AssembledModelContext` 到 provider-neutral request；
- Model identity、capabilities 和 effective limits；
- system/developer/user role 映射；
- ToolSpec、tool choice 和 output contract 映射；
- stream event、usage、finish reason 和 provider error；
- provider retry 与 Session retry 的边界；
- authentication 和 secret redaction；
- provider cache-control 和 payload encoding。

完成门槛：

- ModelGateway 不重新组装 Prompt；
- PromptSet 是模型上下文的唯一 producer；
- provider adapter 差异不泄漏到 Session execution；
- 错误分类足以驱动 retry、compaction 或 terminal failure。

### 阶段 8：Compaction

目标文档：

```text
docs/refactor/compaction.md
```

必须定义：

- context window pressure 和触发条件；
- transcript cut、protection 和 stable ToolRound 边界；
- summary model call purpose 和 `OutputContract::NoToolCalls`；
- summary commit unit；
- compaction 后 conversation seed；
- compaction 失败、retry 和 recovery；
- Skill、Workspace instructions 和动态 contribution 的保留策略。

完成门槛：

- compaction 不直接改写未提交 conversation；
- summary 成功 commit 前不替换模型可见历史；
- cut 不拆散 ToolCall/ToolResult 或其他协议稳定单元；
- compacted conversation 可从 storage 重建。

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
- 改变 durable truth 或 commit gate；
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
- TurnExecutionContext、ToolRound、commit gate：集成测试；
- storage reload、fork、crash recovery：持久化测试；
- provider mapping：adapter contract test；
- Runtime command/event/snapshot：端到端 protocol test。

关键不变量必须拥有测试：

```text
commit-before-model-visible
完整 ToolRound 才可见
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
- [ ] 所有模型调用只接收 `AssembledModelContext`；
- [ ] 所有模型可见 conversation fact 遵守 commit gate；
- [x] Agent/Session revision 与 durable/runtime lifecycle 语义已确定；
- [x] TurnExecutionContext 可以稳定绑定 AgentRevisionRef、SessionDefinitionRevision、WorkspaceSnapshot、PromptSet、ToolSet、SkillCatalog 和 Model；
- [x] Session load/reload/unload 与 fork lifecycle 有确定行为；
- [ ] compaction 和 pending Interaction 的完整行为有确定定义；
- [ ] Runtime command/query/event/snapshot interface 已冻结；
- [ ] 关键不变量有自动化测试；
- [ ] 新文档已进入正式架构目录；
- [ ] 旧正式文档已删除或完成 supersede；
- [ ] README、CONTEXT.md、架构索引和所有内部链接只指向当前架构。

Extension / Plugin 子系统只有在产品确实需要可安装扩展包时才进入整体完成条件。

## 当前下一步

按照本文顺序，下一份目标设计文档是：

```text
docs/refactor/turn-item-interaction.md
```

Agent/Session lifecycle 已确定为“exact AgentRevisionRef pin + immutable SessionDefinitionRevision + durable/runtime 状态分离”。下一阶段需要完成 Turn、Item、Interaction 的封闭类型、pending request 和 terminal semantics。在 Turn/Item/Interaction 与 storage 稳定前，不冻结 Session execution 的 actor/task 形状或公开 protocol。
