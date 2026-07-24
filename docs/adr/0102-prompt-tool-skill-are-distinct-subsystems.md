# ADR 0102: Prompt / Tool / Skill 是独立子系统

状态：Accepted
日期：2026-07-24

## 背景

模型可见上下文由三类材料构成：instructions（Prompt）、可调用工具（Tool）、按需披露的技能正文（Skill）。它们表面相似——都要发现来源、解析 scope、过滤、缓存、失效、投影给模型——因此容易被归并成一个通用 `Resource` 抽象，或沿 Runtime/Agent/Session/Turn 建立一套统一的领域分层。但三者的深层职责并不同构：Prompt 负责确定性组装模型上下文并做协议校验；Tool 把模型披露与真实 executor route、权限、审批、Sandbox 原子绑定；Skill 是渐进披露与正文按需加载。若强行合并，通用层只能取三者交集，把权限授权、executor 绑定、正文懒加载、协议组装等真正复杂的部分重新推回调用方，产生浅抽象与散落的不变量。需要一个决策固定三者的边界与协作 seam。

权威模型、完整字段与不变量见 [`../modules/prompt.md`](../modules/prompt.md)、[`../modules/tools.md`](../modules/tools.md)、[`../modules/skills.md`](../modules/skills.md)。

## 决策

- **三者是独立深模块，不合并为通用 Resource**：Prompt、Tool、Skill 各自封装其发现、解析、缓存、失效与投影；不建立 `Resource`/`ResourceManager` 通用外壳，因为通用层无法承载 executor 绑定、权限授权与正文懒加载等各自独有的深职责。
- **不建立 Runtime/Agent/Session/Turn 领域分层**：不引入 `RuntimeTools/AgentTools/SessionTools/TurnTools` 等按领域层复制的对象；Runtime、Agent、Session 只作为配置 scope，领域 Turn 对象不持有 PromptSet、ToolSet、SkillCatalog 或任何三者的字段。
- **各由 MiniCoreRuntime 初始化一个 `Arc<Service>`**：`MiniCoreRuntime` 创建并拥有 `Arc<PromptService>`、`Arc<ToolService>`、`Arc<SkillService>` 三个长生命周期 Service；durable Session/SessionDefinition 不持有 Service handle，也不复制 definitions 或正文。
- **Turn 边界产出不可变有效对象**：candidate Turn admission 期间、第一次模型调用前，Session execution 分别取得本 Turn 的不可变快照——`PromptSet`、`ToolSet`、pinned `SkillCatalog`（及按需得到的 `LoadedSkill`）；同一 Turn 内所有模型/Tool 循环复用同一组快照，Service 不创建 Turn、不改 TurnStatus。
- **Prompt 是唯一模型可见上下文组装 seam**：`MessageRecord → ModelMessage` 的唯一转换发生在 `PromptSet::assemble()`；`AssembledModelContext` 是进入 ModelGateway 的唯一 provider-neutral Prompt 输出。PromptService 只消费窄只读 view `ToolPromptView` / `SkillCatalogView`，不接收 ToolService、ToolSet、SkillService 或完整 SkillCatalog handle，也不主动调用它们。
- **ToolSet 原子绑定模型可见 ToolSpec 与 executor route**：Tool 定义与真实 executor 原子注册，ToolSet 在同一快照内同时保存模型披露 ToolSpec 与 `ToolName → Arc<dyn Tool>` route，使模型所见 schema 与 ToolCall 实际解析到的 executor 必然同源；注册全集、模型披露集、已授权执行集是三个不同集合。
- **SkillCatalog 与 LoadedSkill 分离、正文按需加载**：Catalog 是轻量可重建的 metadata 快照，不含正文；完整正文只在 `SkillService::load()` 用 pinned `SkillCatalogEntryRef` 精确加载；`SkillInjector` 只把已加载 Skill 转成 `PromptContribution`，交由 PromptSet 规范化进 committed MessageRecord。

## 后果

- 每个模块的价值来自它独有的深职责，deletion test 成立：删除任一模块都会把 executor 绑定、权限授权、协议组装或正文懒加载的复杂性重新散落到 Session execution；而合并三者只会得到无法承载这些职责的浅交集。
- cross-binding 通过 fingerprint 收敛：PromptSet 创建时固定 `ToolPromptView.tool_set_fingerprint` 与 `SkillCatalogView` fingerprint，assembly 不接受任意替代 view，模型披露与 executor route、Catalog identity 的一致性可在单点验证。
- 依赖方向单一：Prompt 依赖 Tool/Skill 的窄 view，而非反向；三个 Service 之间无相互调用，Session execution 作为编排者按序取 view、组装、执行。
- 代价是引入 view 投影、fingerprint cross-binding 与 pinned entry 校验等额外类型层次，以及“注册/披露/授权”“Catalog/LoadedSkill”“scope/role”多组分离带来的概念开销。
- 未来的动态 Context provider、Prompt hook、远程 source、插件协议应作为各自模块的 source adapter 接入，其输出必须经过同一 append/apply 与 conversation projection 规则，不得恢复 current-call 组装旁路，也不得据此重新引入通用 Resource 或领域分层。

## 历史

本 ADR 属 V2 决策集，取代 V1 的：

- ADR 0017（Prompt immutable turn assembly）——V2 保留 Turn 边界不可变组装，并进一步把 Tool/Skill 收敛为窄 view，明确 PromptSet 是唯一组装 seam。
- ADR 0011（Tools session-scoped）——V2 将 Tool 快照边界从 Session 收敛到 Turn（`ToolService::for_turn` 产出不可变 ToolSet），并原子绑定 ToolSpec 与 executor route。
- ADR 0016（command run policy 与 prompt delivery 分离）——V2 将该分离一般化：权限/审批/Sandbox 属 Tool 子系统，模型可见上下文组装属 Prompt 子系统，二者不互相越界。

三份 V1 原文见 [`../archive/v1/adr/`](../archive/v1/adr/)。
