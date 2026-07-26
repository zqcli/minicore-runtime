# MiniCore V2 设计评审（第二轮）

状态：设计评审记录（发现待决议）
日期：2026-07-26
范围：`docs/architecture.md` + `CONTEXT.md` + `docs/modules/`（13 篇）+ `docs/adr/`（0100–0114）+ `docs/migration/v1-to-v2.md` + `docs/refactor/`
方式：在[第一轮评审](v2-design-review.md)全部 A–F 决议落盘后的整体复审。目标是超越第一轮已关闭项，检查残留矛盾、未分配 owner 的横切依赖、协议完备性与文档结构风险。本文所有发现均未形成决议；决议后按第一轮惯例回写并标注。

## 总体判断

核心 seam 与不变量体系在决议回写后仍然自洽：append/apply 线性化、SessionStorage 唯一 durable truth、exact pin 纪律、PromptSet 唯一组装 seam、OutcomeUnknown 保守终结（A1 后语义）在全部文档中一致。第一轮的重大问题没有复发。

当前最大风险已从"设计缺陷"转移为三类：

1. **设计冻结与零实现之间的验证缺口**——Rig 0.40.0 spike 尚未执行，而 spike 结论可能反推已冻结类型（`FinalizedAssistantResponse`、`StoredReasoning`、`content_index` 对齐）的修订；
2. **文档规模成为一致性负债**——同一不变量在 4–6 篇文档中全文复述，决议同步靠人肉，已出现漏网（见 R4）；
3. **少量残留矛盾与两个未分配 owner 的横切依赖**（Submit admission 语义、token 估算器）。

## 一、重大问题（编码前需定案或修复）

### R1 · Submit admission 语义自相矛盾

`session-execution.md` 的「Submit流程」写「普通Submit只在`Idle + Open + Loaded + Ready + accepting_requests`时接受」；但同文档 `SessionIngress` 定义了 bounded `TurnAdmissionQueue`（"只保存尚未开始的 Turn admission"），lane arbitration 第 6 条描述 Turn terminal 后在 FollowUpQueue 与 TurnAdmissionQueue 之间做公平 admission，`runtime-interface.md` 的 `SessionQueueView` 还暴露 `pending_submit_count`。两组表述冲突：**Running 期间到达的 Submit 到底排队等 terminal，还是立刻 SessionBusy？**「未选中且Session已被其他消息占用时及时返回SessionBusy」中"及时"的时点未定义。

- 影响：Executor 主循环第一个分支就要实现该语义；`pending_submit_count`、Cancel(Submit) 的 target 生命周期、Test Matrix 多条用例都依赖它。
- 建议：二选一并统一全部出处——(a) Submit 允许在 Running 期间进入 TurnAdmissionQueue，等待**至多一次** terminal 后的公平 admission decision，未选中即返回 SessionBusy；或 (b) 删除 TurnAdmissionQueue，Idle-only、非 Idle 立即 SessionBusy。同步修订 `SessionQueueView`、线性化表和 Test Matrix。
- 出处：`session-execution.md`（Submit流程 / SessionIngress / Lane arbitration）↔ `runtime-interface.md`（SessionSnapshot）。

### R2 · Token 估算器是无 owner 的横切依赖

阶段 6–8 交付束的多个关键路径依赖 token 估算：soft trigger 的上下文占用、`CompactionSummaryBudget.estimated_source_tokens`、`StableConversationUnit.estimated_tokens`、PromptSet 最终校验「token estimate 不超过有效模型限制」、`CompactionSummaryAssemblyBasis.fixed_prompt_tokens`。没有任何文档定义估算器归谁所有、算法口径、跨 provider 如何规范化；而估算值进入 `CompactionPlanFingerprint`，意味着估算算法版本本身需要 fingerprint 纪律。

- 影响：这是交付束的隐藏前置，会先于 Rig spike 卡住 Compaction 的 property tests 和 budget 派生实现。
- 建议：明确 `TokenEstimator` 的 owner（PromptSet 或 ModelGateway 二选一）、估算口径（字节启发式 vs provider tokenizer）、算法版本进入哪些 fingerprint，并重申"本地估算不冒充 provider fact"在接口上的落点。
- 出处：`compaction.md`（Context Budget / Compaction Summary Budget / StableConversationUnit）↔ `prompt.md`（最终校验 / CompactionSummaryAssemblyBasis）↔ `model-gateway.md`（EffectiveModelLimits）。

### R3 · `MiniCoreRuntime` pub 字段与协议禁公开清单矛盾；共享模块口径不一

`architecture.md`、`prompt.md`、`skills.md` 均展示 `pub struct MiniCoreRuntime { pub prompt_service: Arc<PromptService>, ... }`，而 `runtime-interface.md`「内部对象禁止公开」清单明确 `PromptService / ToolService / SkillService` 永远留在 crate 内部、外部宿主只依赖 facade 四能力。字段可见性应为 `pub(crate)`。同时该 struct 只列三个 Service，遗漏同为 Runtime-owned 的 ModelGateway；`architecture.md`「三个长生命周期深模块」与 `CONTEXT.md`「运行时共享模块」（四个，含 ModelGateway）、`session-execution.md` Runtime 关系图（四个 shared）口径不一致。

- 建议：统一为「四个 Runtime-owned 共享深模块」或明确 ModelGateway 的差异定位；示例代码改 `pub(crate)` 或加注"字段对外不可见"。
- 出处：`architecture.md`（三个长生命周期深模块）↔ `runtime-interface.md`（内部对象禁止公开）↔ `CONTEXT.md` ↔ `session-execution.md`。

### R4 · 迁移记录残留已被推翻的决议内容

`migration/v1-to-v2.md` 阶段 2 的 capture 依赖图仍写 `ToolTurnContext { ..., provider: model.capabilities(), execution_mode, ... }`：`execution_mode` 已被第一轮 A2 决议**移除**；完整 `ModelCapabilities` 传参已被非阻塞项决议改为 `tool_calling: ToolCallingCapabilities`。`turn-execution-context.md` 与 `workspace.md` 的同一张图是正确版本。

- 影响：这是"复述式同步"漏网的实证（见 R6）；迁移记录是实现者的排期入口，误导概率高。
- 建议：直接修正该图；或将迁移记录中的 capture 依赖图改为指向 `turn-execution-context.md` 的链接，不再维护副本。
- 出处：`migration/v1-to-v2.md`（阶段 2）↔ 评审一 A2 决议。

### R5 · CONTEXT.md 混入未标注的 V1 条目

「资源快照」「资源摘要」「提示词素材」三个条目仍以现行语气描述 `ResourceManager` 的四层 snapshot，而其上方「运行时资源（pre-refactor aggregate term）」已声明目标架构删除 ResourceManager。三条既无 pre-refactor 标注又与 V2 冲突；CONTEXT.md 是每个会话加载的术语表，误导成本高。

- 建议：为三条补 pre-refactor 标注并改写为"已删除"口径，或直接删除并在「运行时资源」条目内一句话带过。
- 出处：`CONTEXT.md`（资源快照 / 资源摘要 / 提示词素材）。

### R6 · 不变量复述是最大的结构性负债

同一不变量普遍在 4–6 篇文档全文复写（OutcomeUnknown 保守终结 5 处；WaitingForUserInput 语义 5 处；Steer 消费时机 4 处；WorkspaceCommitAuthorization 竞态 3 处）。`modules/README.md` 的权威归属表方向正确，但同步机制是"重写 + 人肉对齐"：E3/D1 各需同步 8+ 文件，R4 即漏网实例。只有 `compaction.md` 使用了可引用的不变量 ID（COMP-001..020）。

- 建议：把跨模块不变量提升为全局编号清单（如 `architecture.md` 内 INV-xxx），非权威文档只引用编号加一句话概述，不复述细节。将未来每次决议的同步面从 8 个文件收敛到 2 个。
- 出处：全仓横切。

### R7 · C3（sandbox 无法强制时的预执行拒绝）仍开放且无实现门槛挂钩

第一轮 C3 明确"保持开放，本轮不变"，是唯一未关闭的安全类重大项。MVP 禁 bash 缓解大部分风险，且 Tools 不在阶段 6–8 交付束内，时序可容忍；但迁移记录未把"关闭 C3"挂为 Tool 子系统实现的前置门槛，存在被遗忘的风险。

- 建议：在 `migration/v1-to-v2.md` 的 Tool 相关完成门槛中显式加入「C3 决议完成（最终 PermissionSet 含当前 ToolSandbox 不能强制的 capability class → executor 前拒绝）」。
- 出处：评审一 C3 ↔ `tools.md` ↔ `migration/v1-to-v2.md`。

## 二、非阻塞 / 可延后问题

### 协议完备性

- `SessionSnapshot.queues` 只有计数，不含 queued Steer/FollowUp 的 `CommandId` 列表。同进程内重连的 UI 无法枚举队列内容来渲染或调用 `CancelQueuedMessage`——`queue_updated` 事件携带 CommandId 但 Snapshot 不带，snapshot-first 恢复模型在此不完整。要么 Snapshot 补 UI-safe queued message 摘要列表，要么显式声明"重连后放弃管理旧 queue"为有意取舍。（`runtime-interface.md` ↔ `session-execution.md`）
- `InteractionRequestView::ToolApproval(/* UI-safe approval fields */)` 等 payload 是占位注释；`resolution_key: IdempotencyKey` 的生成方（host）与规则（随机性/唯一性）未定义；`SessionMetadataRevision` 仅在 UpdateMetadata 出现一次、无定义处。阶段 9 冻结前补齐。（`runtime-interface.md`）
- Load/Unload/Archive/Delete 等 lifecycle command 无 expected-state CAS 字段（definition update 有），并发调用方靠 typed error 兜底；可接受但应点明为有意设计。（`runtime-interface.md`）
- `SessionExecutionError` 与公开 `CommandErrorCode` 缺映射表（如哪些内部错误映射为 `IngressLaneFull` / `SessionBusy`）。（`session-execution.md` ↔ `runtime-interface.md`）

### 类型 / 横切

- 全部 fingerprint（近 20 种）依赖 `ContentHash` 与规范化序列化，但哈希算法、canonical encoding、algorithm version 字段无统一定义处；第一轮「横切复用」建议的共享 authorization/pinning value type 也未落实。若各模块各写一套 canonical hash，golden vector 测试会互相冲突。建议合并为一个「wire & identity freeze」决议：serde tags/casing（storage 开放问题 1）+ ID 生成策略 + ContentHash 规范化，一次冻结。
- `EntryId` 已定为随机（A1），但 TurnId/ItemId/RequestId/SkillId 的生成策略（随机/单调/长度）无处定义，而 fork remap、cross-session 拒绝依赖其唯一性假设。
- `Money`、`Timestamp`、`JsonSchema` 等基础类型未定义；format v1 冻结时必须定。
- `ProviderProtocol` 含 `Gemini`，但 Rig spike 验证范围只有 OpenAI Responses + Anthropic Messages；要么首版枚举去掉，要么标注 unimplemented。（`model-gateway.md`）

### 措辞级冲突 / 陈旧残留

- `turn-execution-context.md` 不变量「active Turn 不读取这些 Service 的 future current value」是绝对表述，但 Skill lazy load 弱一致性（C4 决议：未加载 entry 按 location 读**当前**文件内容）是有意例外；不变量句子本身未标注例外，易被实现者当 bug 报。建议在该不变量后加「（Skill 未加载正文按 C4 弱一致例外）」。
- `refactor/README.md` 写"ADR 0100–0113"（已有 0114）；该目录标注"review 后删除"，多轮 review 已完成，建议尽快删除，消除第三份可被误引用的副本。
- `compaction.md` 的 `CompactionSettingsSnapshot` 来源（Runtime config？Session 可配？）未说明，SessionDefinition 四字段中无它；建议加一句「首版为 Runtime-global config，per-session 配置留待未来（将产生新的 revision 语义）」。

### 复杂度观察（不要求修改，实现时警惕）

- **同步原语无全局获取总序**：initiating append 同时涉及 Agent lifecycle gate、TurnControlGate reservation、WorkspaceCommitAuthorization 三层；文档分散给出了两两顺序（"先 reservation 再 authorization"、"Agent lifecycle → Session lifecycle"），但没有一处列出全部同步原语的全局锁序总表。建议在 `session-execution.md` 增补一节，把死锁分析从实现者脑中移到文档里。
- **Tool 子系统对 MVP 偏重**：Deferred/search_tools/invoke_tool、跨 Session canonical resource locks、grant store 在"MVP 禁 bash、内置工具为主"前提下首版收益有限。设计冻结无碍，但实现排期应显式后置（当前迁移记录未给 Tools 排期）。
- **每 Session 8 类 ingress lane/signal**：对 in-process 单用户 runtime 是重装备，但每条 lane 有 D1 失败模式论证支撑，属"有据可查的重"；建议首版容量给保守小值并靠 diagnostics 观察。
- **Fork deep-copy + nested remap（含 compaction boundary）**：已知取舍；remap 校验是最易写错的角落，property test 优先级应排最高一档。

### 可实现性排序（直接开工的卡点顺序）

1. R1（Submit admission 语义）——主循环第一周即触碰；
2. 「wire & identity freeze」（serde/casing + ID 策略 + ContentHash 规范化）——golden vector 测试的前置；
3. R2（token 估算器 owner）——Compaction plan/property tests 的前置；
4. Rig 0.40.0 spike——唯一硬外部门槛；建议在迁移记录中写明「spike 允许触发 ADR 级修订」，避免"设计已冻结"话语惯性阻碍必要修改；
5. `prompt.md` 后续问题中 Q1（PromptContent inline vs reference）与 Q4（stamp 精确字段）阻塞 CanonicalUserMessage 存储 schema，属交付束内必须闭合；其余可延后。

## 三、结论摘要

| 维度 | 评价 |
| --- | --- |
| 核心 seam / 不变量 | 自洽且防御性强，第一轮重大问题无复发 |
| 残留矛盾 | 3 处具体（R1 语义歧义、R3 可见性矛盾、R4/R5 陈旧残留） |
| 横切缺口 | 2 个（R2 token 估算 owner、ContentHash/ID 规范化） |
| 安全开放项 | 1 个（R7 = C3，时序可容忍，需挂实现门槛） |
| 文档结构 | 权威归属表方向对，复述式同步是负债（R6）；refactor/ 应删除 |
| 最大整体风险 | 零实现验证：冻结设计与 Rig 现实的首次碰撞 |

## 评审决议

- **R1（Submit admission语义）**：**已关闭**（2026-07-26）。运行中用户输入的解释归UI/CommandSurface层：Turn Running时默认路由为`Steer`，显式选择时`FollowUp`（与pi的调用方选择、Codex/Claude Code的默认注入一致）。协议层`Submit`收窄为Idle-decision-only：Executor仲裁时非Idle立即`SessionBusy`，`TurnAdmissionQueue`只是请求信箱而非跨Turn排队通道，与FollowUp的职责重叠消除；terminal decision窗口内到达的Submit参与该次公平admission。两条竞态回退为adapter约定：Steer遇terminal typed outcome后改发Submit；Submit遇`SessionBusy`后按新Snapshot重新路由。`Cancel(Submit)`生命周期覆盖信箱停留与Starting期间。已同步：`session-execution.md`（Submit流程、SessionIngress表、Admission规则、Lane arbitration、并发规则、Test Matrix）、`runtime-interface.md`（TurnCommand语义、CommandPromptDelivery、SessionQueueView）。

其余各项决议后按第一轮惯例在此登记并回写权威文档。
