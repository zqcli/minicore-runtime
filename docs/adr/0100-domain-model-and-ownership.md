# ADR 0100: 领域模型与 ownership

状态：Accepted
日期：2026-07-24

## 背景

MiniCore 作为可嵌入的原生 Agent harness runtime core，需要一套稳定的领域层级与 ownership 约定，供 CLI、TUI、GUI 宿主统一理解。若不先冻结领域模型，就会反复出现的分歧包括：把 durable entity 与执行期 state 混为一谈、把 Prompt/Tool/Skill 合并成通用 `Resource`、把 Workspace 提升为 Runtime-global registry 或降为 Turn-bound 裸字段、以及让领域 Turn 直接持有执行期对象。这些歧义会让同一份事实出现多个并列 owner，破坏可恢复性与边界收敛。需要一个顶层决策确定实体层级、基数、ownership 与一个事实来源原则；下属专项决策（Workspace、Prompt/Tool/Skill、Turn/Item/Interaction、SessionStorage 等）在此基础上细化。

权威领域模型与全部字段、类型见 [`../architecture.md`](../architecture.md) 与 [`../modules/README.md`](../modules/README.md)。

## 决策

- **固定领域层级与基数**：`MiniCoreRuntime → Agent* → Session* → Turn* → Item* → Interaction*`，每层 1─N。`MiniCoreRuntime` 是外部宿主接触 MiniCore 的唯一顶层门面。
- **Agent 与 Session 的引用关系**：一个 Agent 可被多个 Session 引用，一个 Session 只对应一个 AgentId；Session 创建时 pin 当时的 current `AgentRevision`，Agent 后续发布新 revision 不自动改变已有 Session，只能通过新 `SessionDefinitionRevision` 显式升级。
- **Runtime 持有三个长生命周期深模块**：`PromptService`、`ToolService`、`SkillService` 在 Runtime 生命周期内长期存在；Turn执行边界从中捕获或产生独立、不可变的有效执行对象（`PromptResourceView` / `PromptSet` / `ToolSet` / `SkillView` / `LoadedSkill`），执行对象不回写 Service。
- **Prompt / Tool / Skill 是独立概念，不合并为通用 `Resource`**：三者有各自的定义、Set、授权与 lifecycle，由各自子系统治理；不引入统一 `ResourceManager` 或 per-cwd resource snapshot。详见 [ADR 0102](0102-prompt-tool-skill-are-distinct-subsystems.md)。
- **Workspace 属于 Session**：以 `SessionDefinition.workspace` 形式归 Session 所有，不属于 Agent 或 Turn，也不是独立 entity 或 Runtime-global registry。详见 [ADR 0101](0101-workspace-ownership.md)。
- **一个事实来源原则**：同一份领域事实只有一个权威 owner——conversation durable truth 属于 SessionStorage、Agent definition 属于 Agent owner、Workspace definition 属于 Session、PromptResourceView、Tool definitions/ToolSet和SkillView属于各自子系统、最终模型可见上下文属于 PromptSet、provider encoding 与调用属于 ModelGateway。durable lifecycle 与 loaded execution state 分离；内存 projection、cache、snapshot、UI read model 只能由权威事实派生，不能成为并列 source of truth。
- **Turn 领域对象不持有执行期对象**：领域Turn不内联PromptSet、ToolSet、SkillView、Agent identity或Workspace，也不内联`Vec<Item>`；这些执行期对象由TurnExecutionContext在admission时capture，fingerprints和必要references保存在initiating UserMessage引用的`TurnContext` storage entry；Item 顺序由 SessionStorage projection 提供。Turn execution 是领域 Turn 外围的执行过程，不新增领域层级。

## 后果

- 领域层级冻结后，下属专项 ADR 与模块文档只需在既定 ownership 内细化，不会重新协商顶层归属；边界收敛可在单一模块内验证。
- 一个事实来源原则让恢复与 reload 变得确定：重启后所有 Session 视为 Unloaded，所有 projection/cache 从 durable truth 重建，不存在需要对账的并列真相。
- 拒绝通用`Resource`合并的代价是三个子系统各自维护必要的定义、view、授权和有效对象，换取的是 Prompt/Tool/Skill 各自的授权硬边界不互相污染。
- Turn 领域对象与执行期对象分离带来 pin/fingerprint/reload 线性化的排序复杂度，但换取领域 Turn 的可持久化与可观察性不依赖任何执行期句柄。
- 未来能力（worktree、remote execution 等）应建立各自深模块并由既有 owner 引用其输出，不得塞回领域 entity 或复活通用 Resource。

## 历史

本 ADR 属 MiniCore V2 决策集，是领域模型与 ownership 的顶层决策，取代 V1 中相关的顶层 ownership 决策：

- ADR 0004（SessionManager 拥有 loaded session runtimes）——V1 由 SessionManager 集中持有 loaded session；V2 改为 durable lifecycle 与 loaded execution state 分离，loaded Session 由 SessionExecutor 拥有（见 [ADR 0105](0105-session-executor-owns-loaded-session.md)）。
- ADR 0020（Agent Runtime 无 current session）——V1 明确 Runtime 不持有 current session；V2 将该约束纳入更完整的 Runtime 门面与领域层级定义中。
- ADR 0005（ResourceManager 是 runtime-internal）与 ADR 0010（per-cwd 资源快照）——V1 的通用 ResourceManager 与 per-cwd resource snapshot 已废弃，V2 改为 Prompt/Tool/Skill 三个独立子系统，不恢复通用 Resource 抽象。

V1 原文见 [`../archive/v1/adr/`](../archive/v1/adr/)。
