# ADR 0102: Prompt / Tool / Skill 是独立子系统

状态：Accepted
日期：2026-07-24

## 背景

模型可见上下文由三类材料构成：instructions（Prompt）、可调用工具（Tool）、按需披露的技能正文（Skill）。它们表面相似——都要发现来源、解析 scope、过滤、缓存、失效、投影给模型——因此容易被归并成一个通用 `Resource` 抽象，或沿 Runtime/Agent/Session/Turn 建立一套统一的领域分层。但三者的深层职责并不同构：Prompt 负责确定性组装模型上下文并做协议校验；Tool 把模型披露与真实 executor route、权限、审批、Sandbox 原子绑定；Skill 是渐进披露与正文按需加载。若强行合并，通用层只能取三者交集，把权限授权、executor 绑定、正文懒加载、协议组装等真正复杂的部分重新推回调用方，产生浅抽象与散落的不变量。需要一个决策固定三者的边界与协作 seam。

权威模型、完整字段与不变量见 [`../modules/prompt.md`](../modules/prompt.md)、[`../modules/tools.md`](../modules/tools.md)、[`../modules/skills.md`](../modules/skills.md)。

## 决策

- **三者是独立深模块，不合并为通用 Resource**：Prompt、Tool、Skill 各自封装其发现、解析、缓存、失效与投影；不建立 `Resource`/`ResourceManager` 通用外壳，因为通用层无法承载 executor 绑定、权限授权与正文懒加载等各自独有的深职责。
- **不建立Runtime/Agent/Session/Turn领域分层**：不引入按领域层复制的Prompt/Tool/Skill对象；Agent和Session只保存PromptId selection，领域Turn对象不持有PromptSet、ToolSet、SkillView或任何三者的字段。
- **各由 MiniCoreRuntime 初始化一个 `Arc<Service>`**：`MiniCoreRuntime` 创建并拥有 `Arc<PromptService>`、`Arc<ToolService>`、`Arc<SkillService>` 三个长生命周期 Service；durable Session/SessionDefinition 不持有 Service handle，也不复制 definitions 或正文。
- **Turn边界产出或捕获不可变有效对象**：candidate Turn admission期间、第一次模型调用前，Session execution取得`PromptResourceView`、`PromptSet`、`ToolSet`和`SkillView`（及按需得到的`LoadedSkill`）；同一Turn内所有模型/Tool循环复用同一组对象，Service不创建Turn、不改TurnStatus。
- **Prompt是唯一模型可见上下文组装seam**：`MessageRecord → ModelMessage`的唯一转换发生在`PromptSet::assemble()`；`AssembledModelContext`是进入ModelGateway的唯一provider-neutral Prompt输出。PromptService只消费`PromptResourceView`、`ToolPromptView`和`SkillPromptView`，不接收ToolService、ToolSet、SkillService或完整SkillView handle，也不主动调用它们。
- **ToolSet 原子绑定模型可见 ToolSpec 与 executor route**：Tool 定义与真实 executor 原子注册，ToolSet 在同一快照内同时保存模型披露 ToolSpec 与 `ToolName → Arc<dyn Tool>` route，使模型所见 schema 与 ToolCall 实际解析到的 executor 必然同源；注册全集、模型披露集、已授权执行集是三个不同集合。
- **SkillView与LoadedSkill分离、正文可按需解析**：SkillResourceView是shared reload publication root；SkillView是从captured shared root和WorkspaceSkillContext构建的轻量Turn-local metadata view。shared source在Runtime initialize/`/reload`时捕获，Workspace source在Session load、Idle definition update或`/reload workspace`时捕获；`SkillService::load()`可以lazy parse captured bytes，但不能在Turn内按path重新读取current file。`SkillInjector`只把已加载Skill转成`PromptContribution`，交由PromptSet规范化进committed MessageRecord。

## 后果

- 每个模块的价值来自它独有的深职责，deletion test 成立：删除任一模块都会把 executor 绑定、权限授权、协议组装或正文懒加载的复杂性重新散落到 Session execution；而合并三者只会得到无法承载这些职责的浅交集。
- cross-binding通过private immutable object收敛：PromptSet创建时固定同一次capture得到的PromptResourceView、由parent ToolSet私有投影的ToolPromptView与SkillPromptView；assembly不接受caller伪造或来自另一组capture的替代view。
- 依赖方向单一：Prompt 依赖 Tool/Skill 的窄 view，而非反向；三个 Service 之间无相互调用，Session execution 作为编排者按序取 view、组装、执行。
- 代价只保留必要的view投影、private constructors和explicit reload publication；不再引入Prompt多层override、Skill Catalog revision/exact hash恢复协议或跨子系统fingerprint链。
- 未来的动态 Context provider、Prompt hook、远程 source、插件协议应作为各自模块的 source adapter 接入，其输出必须经过同一 append/apply 与 conversation projection 规则，不得恢复 current-call 组装旁路，也不得据此重新引入通用 Resource 或领域分层。

## 历史

本 ADR 属 V2 决策集，取代 V1 的：

- ADR 0017（Prompt immutable turn assembly）——V2 保留 Turn 边界不可变组装，并进一步把 Tool/Skill 收敛为窄 view，明确 PromptSet 是唯一组装 seam。
- ADR 0011（Tools session-scoped）——V2 将 Tool 快照边界从 Session 收敛到 Turn（`ToolService::for_turn` 产出不可变 ToolSet），并原子绑定 ToolSpec 与 executor route。
- ADR 0016（command run policy 与 prompt delivery 分离）——V2 将该分离一般化：权限/审批/Sandbox 属 Tool 子系统，模型可见上下文组装属 Prompt 子系统，二者不互相越界。

三份 V1 原文见 [`../archive/v1/adr/`](../archive/v1/adr/)。

2026-07-28：[ADR 0123](0123-identity-uses-refs-and-explicit-reload.md)进一步删除Prompt/Tool/Skill fingerprint cross-binding术语，要求Prompt/Skill/Tool资源只在初始化或显式`/reload`后替换current immutable object；active Turn继续使用old captured objects。
