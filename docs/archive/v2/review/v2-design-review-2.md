# MiniCore V2 设计评审（第二轮）

状态：设计评审记录（发现待决议）
日期：2026-07-26
范围：`docs/architecture.md` + `CONTEXT.md` + `docs/modules/`（13 篇）+ `docs/adr/`（0100–0114）+ `docs/migration/v1-to-v2.md` + 当时仍存在的`docs/refactor/`
方式：在[第一轮评审](v2-design-review.md)全部A–F决议落盘后的整体复审。R1–R6已关闭；R7与第一轮C3/O1是同一条件性安全门禁。

> 2026-07-30：本文对SessionStorage durable truth、append/apply、single mutable owner和AgentLoop的评价属于ADR 0126之前的历史基线。当前执行/recording设计以ADR 0126和current modules为准；R6文档治理纪律与R7 Sandbox门禁继续有效。

> 2026-07-31：Prompt Q1/Q4已分别由[ADR 0128](../../../adr/0128-prompt-content-is-materialized-before-publication.md)和[ADR 0129](../../../adr/0129-user-message-contributions-use-part-level-safe-provenance.md)关闭：正文在candidate build期间materialize；用户body与SkillIntent正交；contribution使用safe part-level stamp。下文Q1/Q4门禁均已关闭。

> 2026-07-31：[ADR 0134](../../../adr/0134-public-and-conversation-wire-use-bounded-v1-schemas.md)、[Wire Schema](../../../modules/wire-schema.md)、[Format V1](../../../formats/conversation-jsonl-v1.md)与Wire V1 fixtures已关闭本文present-tense wire/schema、ID、Timestamp、Money与golden-vector门禁；internally-tagged等旧推荐被adjacent `type/data`取代。当前下一入口是Rust codec/reducer实现，非继续设计wire freeze。

## 总体判断

核心 seam 与不变量体系在决议回写后仍然自洽：append/apply 线性化、SessionStorage 唯一 durable truth、exact pin 纪律、PromptSet 唯一组装 seam、OutcomeUnknown 保守终结（A1 后语义）在全部文档中一致。第一轮的重大问题没有复发。

> 后续状态（2026-07-28）：本文中的ContentHash/fingerprint/golden-vector建议均为评审当时的历史问题记录。当前identity与reload决策已由[ADR 0123](../../../adr/0123-identity-uses-refs-and-explicit-reload.md)取代：不定义V2 `*Fingerprint`身份族，不新增generation/replacement ID；执行一致性使用exact refs、immutable `Arc`、explicit reload和structural validation。

当前最大风险已从"设计缺陷"转移为三类：

1. **设计冻结与零实现之间的验证缺口**——Rig 0.40.0 spike 尚未执行，而 spike 结论可能反推已冻结类型（`FinalizedAssistantResponse`、`StoredReasoning`、`content_index` 对齐）的修订；
2. **文档一致性负债已按R6收口**——8条高风险规则拥有stable ID与canonical owner，非owner副本已收窄，`docs/refactor/`已删除，横切ADR扫描纪律已写入migration；
3. **评审时识别的具体矛盾、未分配owner与文档治理问题**（R1–R6）均已关闭；当前剩余门禁见文末评审决议与handoff。

## 一、重大问题（编码前需定案或修复）

### R1 · Submit admission 语义自相矛盾（已关闭）

`session-execution.md` 的「Submit流程」写「普通Submit只在`Idle + Open + Loaded + Ready + accepting_requests`时接受」；但同文档 `SessionIngress` 定义了 bounded `TurnAdmissionQueue`（"只保存尚未开始的 Turn admission"），lane arbitration 第 6 条描述 Turn terminal 后在 FollowUpQueue 与 TurnAdmissionQueue 之间做公平 admission，`runtime-interface.md` 的 `SessionQueueView` 还暴露 `pending_submit_count`。两组表述冲突：**Running 期间到达的 Submit 到底排队等 terminal，还是立刻 SessionBusy？**「未选中且Session已被其他消息占用时及时返回SessionBusy」中"及时"的时点未定义。

- 影响：Executor 主循环第一个分支就要实现该语义；`pending_submit_count`、Cancel(Submit) 的 target 生命周期、Test Matrix 多条用例都依赖它。
- 建议：二选一并统一全部出处——(a) Submit 允许在 Running 期间进入 TurnAdmissionQueue，等待**至多一次** terminal 后的公平 admission decision，未选中即返回 SessionBusy；或 (b) 删除 TurnAdmissionQueue，Idle-only、非 Idle 立即 SessionBusy。同步修订 `SessionQueueView`、线性化表和 Test Matrix。
- 出处：`session-execution.md`（Submit流程 / SessionIngress / Lane arbitration）↔ `runtime-interface.md`（SessionSnapshot）。

### R2 · Token 估算器是无 owner 的横切依赖（已关闭）

阶段 6–8 交付束的多个关键路径依赖token估算：soft trigger的上下文占用、`CompactionSummaryBudget.estimated_input_tokens`、`CompactionUnitRef.estimated_tokens`、PromptSet最终校验“token estimate不超过有效模型限制”、`CompactionSummaryAssemblyBasis.fixed_prompt_tokens`。没有任何文档定义估算器归谁所有、算法口径、跨 provider 如何规范化；估算值会影响CompactionPlan可行性和future Turn行为，因此估算算法版本本身需要明确owner。

- 影响：这是交付束的隐藏前置，会先于 Rig spike 卡住 Compaction 的 property tests 和 budget 派生实现。典型爆发场景：① 英文启发式（bytes÷4）对 UTF-8 中文低估 2–4 倍 → soft trigger 迟到 → 中文长会话系统性走 provider 硬 overflow → 撞上"同 basis 只一次 hard recovery + 单 Turn 次数上限" → TurnFailed，且英文测试集无法复现；② Compaction 与 PromptSet 各自实现估算且口径不一 → 压缩后仍判 overflow 而无新 frontier → `ContextStillTooLargeAfterCompaction`；③ 估算算法升级无版本承载 → 同内容在future Turn中得到不可解释的不同可行性判断。
- 出处：`compaction.md`（Context Budget / Compaction Summary Budget / CompactionUnitRef）↔ `prompt.md`（最终校验 / CompactionSummaryAssemblyBasis）↔ `model-gateway.md`（EffectiveModelLimits）。

**关闭方案（已回写权威文档）**：

同类产品共识（Codex / Claude Code / pi）均为"上次 provider usage 为主信号 + 保守本地启发式补增量"，多 provider 产品无一内置精确 tokenizer；MiniCore 既有原则文字（provider usage 优先、估算不冒充 fact、window 未知不触发）与共识一致，只补归属与口径：

- **Owner**：token估算率是per-model事实，归ModelGateway validated model definition——新增 `TokenEstimateRate { bytes_per_token: NonZeroU32, algorithm_version: u16 }`，进入 `ModelDefinitionVersion` 覆盖范围（不新增独立版本字段；rate 变化 = definition 变化 = 只影响 future Turn，与 exact-pin 语义一致）。ADR 0123后不再使用`TurnModelFingerprint`作为current架构术语；
- **分发**：经 `TurnModelSnapshot::token_estimator()` 以确定性纯值分发；PromptSet 最终校验、Compaction 的 unit/source/pressure 估算与 `CompactionSummaryAssemblyBasis.fixed_prompt_tokens` 共用同一 estimator，**禁止各调用点自行实现估算**；
- **算法（首版）**：字节保守启发式 `tokens = ceil(bytes / bytes_per_token)`；definition 未声明时 resolution 使用 Runtime 保守默认 `bytes_per_token = 3` 并记 diagnostics（对中文≈1 token/字，对英文高估约 25%——高估方向安全，只会提早压缩）；非文本 content 返回 typed unknown，不参与 soft trigger 估算；
- **红线不变**：估算不进 `ModelUsage`/durable usage fact；context window 未知不执行 soft trigger；权威判断仍是 PromptSet 最终校验 + provider 响应。

权威文档回写清单（已完成）：

| # | 文件 | 改动 |
| --- | --- | --- |
| M1 | `model-gateway.md` | `definition_version` 覆盖清单加入 `token estimate rate` |
| M2 | `model-gateway.md` | EffectiveModelLimits 节后新增「TokenEstimateRate」小节：struct、`TurnModelSnapshot::token_estimator()`、唯一估算来源与消费纪律；TurnModelSnapshot增加私有`token_estimate_rate`字段 |
| M3 | `model-gateway.md` | Test Matrix（Model Resolution）新增：rate 入 model definition version 且只影响 future Turn；未声明用保守默认并记 diagnostics；estimator 跨调用点确定性一致 |
| C1 | `compaction.md` | `CompactionPressureInput`与`CompactionPlanInput`携带Turn-pinned `TokenEstimator`（与EffectiveModelLimits并列）；unit/source/pressure估算全部来自该estimator，Compaction不自行实现 |
| C2 | `compaction.md` | Context Budget明确必须使用`TurnModelSnapshot::token_estimator()`，non-text unknown不触发Soft |
| C3 | `compaction.md` | Invariants新增“所有本地token estimate来自Turn-pinned TokenEstimator；估算rate与算法版本由model definition version覆盖” |
| P1 | `prompt.md` | 最终校验末条注明 token estimate 使用 `TurnModelSnapshot::token_estimator()` |
| P2 | `prompt.md` | `CompactionSummaryAssemblyBasis` 段注明 `fixed_prompt_tokens` 使用同一 estimator |

不改动：`CompactionUnitRef.estimated_tokens`等字段本体（来源由 C1 总括约束）、既有红线原文、`ModelUsage` 持久化规则；`TokenEstimator`除`TurnModelSnapshot::token_estimator()`外的具体方法签名粒度留给实现阶段。

### R3 · `MiniCoreRuntime` pub 字段与协议禁公开清单矛盾；共享模块口径不一（已关闭）

`architecture.md`、`prompt.md`、`skills.md` 均展示 `pub struct MiniCoreRuntime { pub prompt_service: Arc<PromptService>, ... }`，而 `runtime-interface.md`「内部对象禁止公开」清单明确 `PromptService / ToolService / SkillService` 永远留在 crate 内部、外部宿主只依赖 facade 四能力。字段可见性应为 `pub(crate)`。同时该 struct 只列三个 Service，遗漏同为 Runtime-owned 的 ModelGateway；`architecture.md`「三个长生命周期深模块」与 `CONTEXT.md`「运行时共享模块」（四个，含 ModelGateway）、`session-execution.md` Runtime 关系图（四个 shared）口径不一致。

- 出处：`architecture.md`（三个长生命周期深模块）↔ `runtime-interface.md`（内部对象禁止公开）↔ `CONTEXT.md` ↔ `session-execution.md`。

**修复方案（机械修复，无决策成本）**：

统一口径为「MiniCoreRuntime 在 Runtime 生命周期内拥有**四个** Runtime-owned 共享深模块：PromptService、ToolService、SkillService、ModelGateway」。逐处修改：

| # | 文件 | 改动 |
| --- | --- | --- |
| 1 | `architecture.md` | 小节标题「三个长生命周期深模块」→「Runtime-owned 共享深模块」；struct 示例字段改 `pub(crate)` 并补 `model_gateway: Arc<ModelGateway>`；正文"三个"改"四个" |
| 2 | `prompt.md` | `MiniCoreRuntime` struct 示例同步（`pub(crate)` + 补 ModelGateway 字段或注明"其余字段省略"） |
| 3 | `skills.md` | 同上 |
| 4 | `runtime-interface.md` | 无需改（禁公开清单本身正确，是权威侧） |
| 5 | `CONTEXT.md` | 无需改（「运行时共享模块」已是四个，是正确侧） |

若认为 ModelGateway 与三个资源 Service 定位确有差异（它不产生 `for_turn` 快照对象），可保留"三个资源 Service + 一个模型网关"的表述，但必须在 architecture.md 一处写清，且 struct 字段可见性仍需修正——`pub` 字段是硬伤，与口径之争无关。

**关闭结果（2026-07-29）**：`architecture.md`已统一为四个Runtime-owned共享深模块，`MiniCoreRuntime`示例中的`prompt_service`、`tool_service`、`skill_service`、`model_gateway`和`shared_resources`均为private字段；`prompt.md`、`skills.md`和`modules/README.md`使用同一口径。外部host仍只通过MiniCoreRuntime facade交互，R3关闭。

### R4 · 迁移记录残留已被推翻的决议内容（已关闭）

`migration/v1-to-v2.md` 阶段 2 的 capture 依赖图仍写 `ToolTurnContext { ..., provider: model.capabilities(), execution_mode, ... }`：`execution_mode` 已被第一轮 A2 决议**移除**；完整 `ModelCapabilities` 传参已被非阻塞项决议改为 `tool_calling: ToolCallingCapabilities`。`turn-execution-context.md` 与 `workspace.md` 的同一张图是正确版本。

- 影响：这是"复述式同步"漏网的实证（见 R6）；迁移记录是实现者的排期入口，误导概率高。
- 出处：`migration/v1-to-v2.md`（阶段 2）↔ 评审一 A2 决议。

**修复方案（推荐链接替换，不再维护副本）**：

把 `migration/v1-to-v2.md` 阶段 2 中的整段 capture 依赖图代码块替换为：

> capture 依赖图以 [`../modules/turn-execution-context.md`](../../../modules/turn-execution-context.md#capture-依赖图) 为唯一权威版本，本文不维护副本。要点：exact SessionDefinitionRevision 展开 Agent/Prompt/Model/Workspace，SkillView 与 ToolSet 可并行捕获，PromptSet 在三个 view 就绪后创建，最终校验后组成 TurnExecutionContext。

理由：迁移记录是排期文档，不是技术规范；其中的技术图副本没有独立价值，只有漂移风险（本次 `execution_mode` / `provider: model.capabilities()` 残留即为实证）。这也是 R6 原则的第一个落地样例。若坚持保留内联图，则最低限度修正两处：删除 `execution_mode`，`provider: model.capabilities()` 改为 `tool_calling: model.capabilities().tool_calling`。

**关闭结果（2026-07-29）**：`migration/v1-to-v2.md`已删除capture依赖图副本，改为链接`turn-execution-context.md#capture-依赖图`，只保留排期所需摘要；`execution_mode`和完整`ModelCapabilities`旧传参不再出现，R4关闭。

### R5 · CONTEXT.md 混入未标注的 V1 条目（已关闭）

「资源快照」「资源摘要」「提示词素材」三个条目仍以现行语气描述 `ResourceManager` 的四层 snapshot，而其上方「运行时资源（pre-refactor aggregate term）」已声明目标架构删除 ResourceManager。三条既无 pre-refactor 标注又与 V2 冲突；CONTEXT.md 是每个会话加载的术语表，误导成本高。

- 出处：`CONTEXT.md`（资源快照 / 资源摘要 / 提示词素材）。

**修复方案（推荐直接删除三条 + 归属句合并）**：

1. 删除 `CONTEXT.md` 中「资源快照」「资源摘要」「提示词素材」三个条目全文；
2. 在其上方的「运行时资源（pre-refactor aggregate term）」条目末尾追加一句吸收说明：

   > 旧设计派生的「资源快照」（RuntimeResourceSnapshot/CwdResourceSnapshot/TurnResourceSnapshot）、「资源摘要」和「提示词素材」随 ResourceManager 一并废除：快照语义由 WorkspaceSnapshot、PromptSet、ToolSet、SkillView 各自的 Turn-pinned 不可变对象承接；UI 安全投影由各子系统 UI-safe view 承接；Prompt 输入由 PromptProfile 与 PromptContribution 承接。

不推荐"保留三条 + 加 pre-refactor 标注"的做法：这三条描述的四层 snapshot 结构在 V2 中没有一一对应物，保留详细旧结构描述只会持续占据术语表篇幅并诱导错误联想；术语演变说明由「运行时资源」一条承担即可。删除后全文检索确认无其他文档引用这三个词条名。

**关闭结果（2026-07-29）**：`CONTEXT.md`已删除“资源快照”“资源摘要”“提示词素材”三个独立现行条目，并在“运行时资源（pre-refactor aggregate term）”中统一说明其废除后的V2归属，R5关闭。

### R6 · 跨文档不变量复述造成一致性负债（已关闭）

R6关注的不是“相同概念在多篇文档出现”，而是**同一规范性规则在多个可独立编辑的位置保留完整副本**。每个副本都像权威定义，ADR修改其中一份后，其他副本仍可能继续描述旧接口、旧状态或旧错误语义。`modules/README.md`已经给出module权威归属，`architecture.md`也定义了文档权威顺序，但当前仍主要依靠全文搜索和人工判断完成同步。

**发生场景**：

1. **跨模块ADR改变同一规则**：ADR 0124同时改变Session replay、Tool exchange、Compaction、Fork与AgentLoop输入，曾需要同步三十余篇文档；复核时仍发现ADR 0117残留durable `ToolExecutionStarted`措辞。实现者若只阅读该段，会重新引入已删除的durable marker。
2. **删除一个横切机制**：ADR 0125删除ModelGateway四级permit，需要同步ModelGateway call flow、SessionExecutor并发说明、同步ADR、评审状态、handoff和历史refactor副本；第一次扫描后仍发现`generate_model_turn`会等待provider并发permit的残留描述。该残留会让实现者误建`concurrency.rs`或把共享Gateway错误串行化。
3. **协议或状态机字段冻结**：wire/schema freeze将同时影响ConversationStorage、Runtime Interface、Turn/Item/Interaction、migration示例和测试合同。若多个文档各自保存完整serde/ID规则，未来字段改名或casing变化可能只更新storage而漏掉公开协议。
4. **安全规则升级**：R7/O1关闭时，sandbox enforceability、Tool authorization、ToolStartPermit、PreExecution ToolResult和Runtime UI view会跨Tools、Workspace、Session Execution及Runtime Interface出现。任一旧副本仍写“approval后可执行”都会形成fail-open误导。
5. **跨机器或AI辅助恢复**：handoff、review、migration和module文档可能被不同工程师或agent作为入口。非权威副本若包含完整且过期的规则，读者可能在尚未进入权威module前就形成错误实现方案。

**可能结果**：

- 两个module按不同状态名、字段或错误语义实现，直到vertical slice集成时才暴露；
- 测试依据旧副本通过，但权威interface要求不同，形成“测试证明了错误合同”；
- 已关闭issue因旧措辞再次被评审提出，重复消耗设计时间；
- 安全或replay规则只部分更新，产生fail-open、history brick或模型输入污染风险；
- 每个ADR都需要大范围人工同步，文档越多，漏改概率越高。

**影响边界与优先级**：

- R6本身不会直接产生运行时错误，风险通过后续实现或维护读取错误合同间接发生；
- 当前权威顺序、ADR supersession说明和机械全文扫描已经降低风险，因此R6不阻塞AgentLoop L2、wire/schema freeze、Rig spike或ScriptedProviderAdapter vertical slice；
- 开始多人并行实现、production Tool/Sandbox adapter或继续新增大量横切ADR前，风险会明显上升；
- 修复前`docs/refactor/`仍存在完整历史副本，是最明显的额外误读面；该目录现已删除。

**MVP采用的收窄修复**：

原方案一次建立12条全局`INV-xxx`并大批重写文档，收益存在但改动面偏大。MVP先采用更轻的规则：

1. 每条跨模块规则只指定一个权威module保存完整定义；其他module、review、migration和handoff只保留一句摘要与链接。
2. 只有同时满足“至少三个消费module、错误会影响correctness/security、预计继续演进”的规则才分配稳定`INV-xxx`；不为所有普通约束建立编号体系。
3. 新ADR回写时执行“权威定义 → interface消费者 → review/handoff → 历史副本”的固定扫描，并用关键旧术语`rg`验证删除项无current性残留。
4. 删除`docs/refactor/`完整副本；仍需保留的历史只进入明确archive或Git history，避免形成第三份规范。（已完成）

**关闭结果（2026-07-29）**：

- `architecture.md`新增8条高风险跨模块不变量索引：INV-001/002/003、INV-101/102、INV-201、INV-301、INV-401，并链接唯一canonical owner section；
- canonical sections在ConversationStorage、Session Execution、Turn Execution Context和Turn/Item/Interaction中显式标注ID；`modules/README.md`冻结owner/link维护纪律；
- Architecture长篇执行副本、Workspace capture依赖图、Tools durable projection算法，以及Session/Lifecycle/Runtime消费者中的相关复述已收窄为本地职责摘要与canonical链接；
- ADR 0104/0105/0109/0111/0113/0118/0121/0123中仍像current规则的旧Tool marker、strict replay和ModelGateway capacity措辞已统一到ADR 0124/0125后的设计；
- `docs/refactor/`完整重复目录已删除，历史继续由Git与`docs/archive/v1/`保存；
- `migration/v1-to-v2.md`已写入横切ADR固定回写顺序与`rg`旧术语残留扫描要求；ADR 0125收口作为该流程的首个已完成验证，future wire/schema freeze按同一纪律执行；
- handoff已同步。R6关闭，不建设覆盖所有普通约束的全局不变量数据库。

### R7 · C3（sandbox 无法强制时的预执行拒绝；开放，已延后）

第一轮 C3 明确"保持开放，本轮不变"，是唯一未关闭的安全类重大项。当前排期决定将其延后：Tools不在阶段6–8交付束内，当前不实现会产生真实OS/网络/进程副作用的production Sandbox adapter；但开始首个production Tool/Sandbox adapter前，仍必须把"关闭 C3"作为条件性P0门禁，避免被遗忘。

- 出处：评审一 C3 ↔ `tools.md` ↔ `migration/v1-to-v2.md`。

**修复方案（低分歧，建议直接定案而非继续挂起）**：

C3的技术方案没有实质分歧；以下保留原推荐方案，等production Tool/Sandbox adapter进入计划时直接定案，不要求阶段6–8提前实现：

**1) 定案文本**（回写 `tools.md`「Policy、Approval 和 Sandbox」基础不变量清单，新增一条）：

> - 最终 `PermissionSet` 中含有当前 `ToolSandbox` 声明不能强制（enforce）的 capability class 时，必须在 executor 前拒绝，形成 `PreExecution` truthful ToolResult（disposition = Denied，reason 注明 capability 不可强制）；该规则与"Sandbox 执行失败不能静默回退到无 Sandbox 执行"并列为不变量。UI approval 不能替代 enforcement；不可强制 ≠ 用户同意后可裸跑。

**2) 配套接口**：`ToolSandbox` trait 需要能回答"我能强制什么"，否则预执行拒绝无判断依据。建议增加 capability 声明：

```rust
pub trait ToolSandbox: Send + Sync {
    fn enforceable(&self) -> SandboxEnforcementCapabilities;  // 按 filesystem/network/process/environment class 声明
    async fn execute(...) -> ToolResult;                       // 既有
}
```

`ToolAuthorization`在approval之后、SessionExecutor发放ToolStartPermit之前用`enforceable()`与最终PermissionSet求差：差集非空→PreExecution拒绝。未声明的class视为不可强制（fail closed默认）。`SandboxEnforcementCapabilities`影响ToolSet构造与execution routing，不进入durable identity或fingerprint。

**3) 挂门槛**：`migration/v1-to-v2.md` 中 Tool 子系统相关完成门槛加一条「[ ] C3 预执行拒绝不变量与 `SandboxEnforcementCapabilities` 已定案并落入 tools.md」；同时更新第一轮评审 `v2-design-review.md` 的 C3 状态（"保持开放" → 引用本决议）。

**4) Test Matrix 补充**（tools.md）：平台无法强制 network class 时，声明 network 限制的调用在 approval 通过后仍被 PreExecution 拒绝；`FullAccessWithApproval` 路径明确无 sandbox guarantee 的既有语义不变。

## 二、非阻塞 / 可延后问题

### 协议完备性

- `SessionSnapshot.queues` 只有计数，不含 queued Steer/FollowUp 的 `CommandId` 列表。同进程内重连的 UI 无法枚举队列内容来渲染或调用 `CancelQueuedMessage`——`queue_updated` 事件携带 CommandId 但 Snapshot 不带，snapshot-first 恢复模型在此不完整。要么 Snapshot 补 UI-safe queued message 摘要列表，要么显式声明"重连后放弃管理旧 queue"为有意取舍。（`runtime-interface.md` ↔ `session-execution.md`）
- `InteractionRequestView::ToolApproval(/* UI-safe approval fields */)` 等 payload 是占位注释；`resolution_key: IdempotencyKey` 的生成方（host）与规则（随机性/唯一性）未定义；`SessionMetadataRevision` 仅在 UpdateMetadata 出现一次、无定义处。阶段 9 冻结前补齐。（`runtime-interface.md`）
- Load/Unload/Archive/Delete 等 lifecycle command 无 expected-state CAS 字段（definition update 有），并发调用方靠 typed error 兜底；可接受但应点明为有意设计。（`runtime-interface.md`）
- `SessionExecutionError` 与公开 `CommandErrorCode` 缺映射表（如哪些内部错误映射为 `IngressLaneFull` / `SessionBusy`）。（`session-execution.md` ↔ `runtime-interface.md`）

**推荐方案（按条对应）**：

1. *queued message 枚举*：`SessionSnapshot.queues` 增加 UI-safe 摘要列表——`queued_messages: Vec<QueuedMessageView { command_id: CommandId, kind: Steer | FollowUp, preview: String }>`（preview 为截断的 intent 文本，不含附件正文）。理由：这些信息 host 本来就通过 `SteerQueued`/`FollowUpQueued` response 和 `queue_updated` 事件获得过，放入 Snapshot 不扩大信息面，只补齐 snapshot-first 恢复模型的完整性；成本是 Snapshot 稍大，队列本身 bounded 所以有上界。若不采纳，必须在 `runtime-interface.md` 显式声明"重连后放弃管理既有 queue"为有意取舍。
2. *InteractionRequestView 占位*：阶段 9 冻结前定义 UI-safe 字段集。ToolApproval：`tool_name`、redacted arguments summary、`ToolRequirements` 摘要（按 filesystem/network/process/environment class 聚合展示）与`reason`；MVP decision只支持per-call `AllowOnce/AllowWith`或Deny，不提供grant suggestion。UserQuestion：`Vec<QuestionView { prompt, kind: FreeText | Choice { options } , required, secret: bool }>`，`secret = true` 的答案不进入普通 event/diagnostics。prepared private args、executor route、sandbox internals 维持禁入。
3. *resolution_key 生成*：host 为每次新 resolution 生成不可预测随机 128-bit key（同一 resolution 的重试复用同一 key），规则写入 `runtime-interface.md` InteractionCommand 节，与 CommandId 的生成措辞对齐（"随机、不可复用、不可预测"）。
4. *SessionMetadataRevision*：在 `agent-session-lifecycle.md` Session head 定义处补类型说明——metadata version 与 `SessionDefinitionRevision` 正交、单调，name/description 变化递增，definition 变化不递增；`AgentMetadataRevision` 同规则。
5. *lifecycle command 无 CAS*：确认为有意设计并在 `runtime-interface.md` Command Envelope 节点明一句："lifecycle transition command（Load/Unload/Archive/Delete）不携带 expected revision，以 durable lifecycle 状态机 + typed conflict error 兜底并发；definition/metadata mutation 才使用 expected revision CAS。"
6. *错误映射表*：在 `runtime-interface.md`「Error 分层」节补一张 `SessionExecutionError → CommandErrorCode` 映射表（如 `TurnAdmissionQueueFull → IngressLaneFull(TurnAdmission)`、`ExecutorStopping → SessionNotLoaded 或 RuntimeClosing`、`NoRunningTurn → TurnNotRunning`），作为 contract test 的断言依据。

### 类型 / 横切

- ~~全部 fingerprint（近 20 种）依赖 `ContentHash` 与规范化序列化，但哈希算法、canonical encoding、algorithm version 字段无统一定义处；第一轮「横切复用」建议的共享 authorization/pinning value type 也未落实。若各模块各写一套 canonical hash，golden vector 测试会互相冲突。~~ **已由ADR 0123取代**：V2不定义`*Fingerprint`身份族，不为Workspace/Prompt/Skill/Tool/Model/Turn/Compaction建立ContentHash freeze；实现可内部使用未公开Hash优化cache，但Hash不进入module interface、durable schema、authorization、retry、recovery或架构不变量。serde tags/casing和基础ID生成策略仍需在wire/schema freeze中单独处理。
- `EntryId`已定为随机（A1），但TurnId/ItemId/RequestId/SkillId的生成策略（随机/单调/长度）无处定义。ADR 0124后Fork保留历史ID，identity唯一性scope与wire格式仍需冻结。
- `Money`、`Timestamp`、`JsonSchema` 等基础类型未定义；format v1 冻结时必须定。
- `ProviderProtocol` 含 `Gemini`，但 Rig spike 验证范围只有 OpenAI Responses + Anthropic Messages；要么首版枚举去掉，要么标注 unimplemented。（`model-gateway.md`）

**推荐方案（合并为一个「wire & identity freeze」决议包，实现启动前一次冻结）**：

1. *serde 惯例*：JSON field 使用 camelCase（storage 示例 `formatVersion`/`sessionId`/`parentId` 已是此风格，顺势冻结）；type/enum tag 值使用 snake_case（与事件类型名 `turn_started` 等既有约定一致）；tag 形态用 internally-tagged（`"type": "..."`）对齐现有示例。写入 `conversation-storage.md` 开放问题 1 的闭合处与 `runtime-interface.md` 实现顺序第 1 条。
2. *ID生成策略*：EntryId/TurnId/ItemId/RequestId/SkillId/CommandId统一使用128-bit不可预测随机值（UUIDv4或等价），字符串编码统一。Entry/Turn/Item/Request ID在Session scope内唯一；Fork复制历史ID并分配new SessionId，新append生成fresh随机ID。ID不作排序键。定义处：wire-freeze决议文档。
3. *ContentHash 规范化（历史建议，已废弃）*：ADR 0123删除O13/O14/O15中的ContentHash/fingerprint方案；不要把此处SHA-256/golden vector草案作为当前实现依据。
4. *共享pinning value type（历史建议，已废弃）*：ADR 0123关闭O13，不抽共享pinning/fingerprint value module；各deep module以private immutable interface和explicit reload保证一致性。
5. *基础类型*：`Timestamp` = UTC RFC3339 毫秒精度字符串（JSONL 示例已是此形态）；`Money` = `{ amount: String（decimal 原文）, currency: ISO4217 }`，只承载 provider 返回的 billed cost 原值，不做本地算术——与"reported_cost 只存 provider 明确返回值"原则一致。
6. *Gemini*：从首版 `ProviderProtocol` 删除，真实需求出现时随新 provider adapter 一起加回（与"不为未来需求预置枚举"的仓库纪律一致）。

### 措辞级冲突 / 陈旧残留

- ~~`turn-execution-context.md`需标注Skill lazy load弱一致例外。~~ **已由ADR 0123取代**：active Turn不读取future current value；Skill lazy load只解析shared或Workspace publication中captured bytes，不按location读取current file。
- ~~`refactor/README.md`写"ADR 0100–0113"且完整目录形成第三份副本。~~ **已由R6关闭**：`docs/refactor/`已删除，历史由Git保存。
- `compaction.md` 的 `CompactionSettingsSnapshot` 来源（Runtime config？Session 可配？）未说明，SessionDefinition 四字段中无它；建议加一句「首版为 Runtime-global config，per-session 配置留待未来（将产生新的 revision 语义）」。

**推荐方案（三处均为一句话级修改）**：

1. *Skill 弱一致例外标注（历史建议，已废弃）*：ADR 0123不再允许尚未lazy-load的Skill按location读取current file。shared source在Runtime initialize/`/reload`时capture，Workspace source在Session load、Idle definition update或`/reload workspace`时capture；lazy load只解析entry captured bytes。
2. *refactor/目录删除（已完成）*：正式文档无依赖后删除`docs/refactor/`；README/architecture导航无需变更。
3. *CompactionSettings 来源*：`compaction.md`「Context Budget」的 CompactionSettings 定义后加一句：「`CompactionSettings` 是 Runtime-global config；`CompactionSettingsSnapshot` 在 Turn admission 时从当前 Runtime config 捕获。per-session 压缩配置是未来扩展，届时须进入 `SessionDefinition` 并产生新的 revision 语义，本版不提供。」

### 复杂度观察（不要求修改，实现时警惕）

- ~~**同步原语无全局获取总序**~~：**已由ADR 0117关闭**。复核未发现当前可构造循环等待；single owner、non-blocking reservation、release-before-fan-out和typed permit取代全局lock-rank方案。
- **Tool 子系统对 MVP 偏重**：Deferred/search_tools/invoke_tool和完整Sandbox adapter在"MVP禁bash、内置工具为主"前提下首版收益有限。ADR 0123已删除grant store并把approval收窄为per-call decision；文件并发已由ADR 0116收窄为Session-local单文件FIFO queue与Serial批次降级。
- **每 Session 8 类 ingress lane/signal**：对 in-process 单用户 runtime 是重装备，但每条 lane 有 D1 失败模式论证支撑，属"有据可查的重"；建议首版容量给保守小值并靠 diagnostics 观察。
- ~~**Fork deep-copy + nested remap（含compaction boundary）**~~：**已由ADR 0124简化**。Fork保留selected path历史IDs与single Compaction marker，只分配new SessionId；测试重点改为copy/replay和future append ID collision。

**推荐方案（按条对应）**：

1. *异步同步纪律*：不建设全局锁序总表。普通Mutex/RwLock guard不得跨await、owner调用、event publication或fan-out；有意的bounded async serialization使用typed permit；controlled append由私有helper固定`TurnControl reservation → append/apply → release`；状态mutation释放gate后再通知Session。完整决策见[ADR 0117](../../../adr/0117-async-synchronization-uses-single-owner-and-typed-permits.md)与[ADR 0121](../../../adr/0121-workspace-updates-require-idle.md)。
2. *Tools 实现排期后置*：在`migration/v1-to-v2.md`补一句显式排期：Tool子系统完整实现（Deferred/search_tools/invoke_tool和完整Sandbox adapter）后置于阶段6–8交付束之后；交付束期间仅需ToolSet接口签名、Session-local mutation queue最小stub与ScriptedProviderAdapter联调。
3. *lane 容量默认值*：Runtime config 为各 lane 给出保守小默认（如 TurnAdmission=4、Steer=16、FollowUp=16、InteractionControl=8、ToolControl=32），随 diagnostics 的 lane depth 指标观察后调整；默认值本身不进入durable identity或execution consistency proof。
4. *Fork copy property test*：任意selected path复制到new Session后保留历史Entry/Turn/Item/Request/ToolCall IDs，tolerant replay得到等价conversation/history；future append生成fresh ID且不碰撞。该property test与Fork功能同PR交付。

> 2026-07-27后续决议：文件并发的权威范围见[ADR 0116](../../../adr/0116-file-mutations-use-session-local-queues.md)；异步同步纪律见[ADR 0117](../../../adr/0117-async-synchronization-uses-single-owner-and-typed-permits.md)；Workspace Idle-only update与SecurityRevoked见[ADR 0121](../../../adr/0121-workspace-updates-require-idle.md)。MVP不建设全局lock-rank系统或Workspace lease。

### 可实现性排序（直接开工的卡点顺序）

1. 「wire/schema freeze」（serde/casing + public ID策略 + 基础类型）——storage/protocol contract tests的前置；ContentHash/fingerprint freeze已由ADR 0123删除；
2. Rig 0.40.0 spike——唯一硬外部门槛；spike允许触发必要的ADR级修订；
3. `prompt.md`后续问题中Q1/Q4已由ADR 0128/0129关闭；CanonicalUserMessage使用safe part-level stamp且损坏stamp不丢conversation正文，剩余阻塞只属于通用wire/schema freeze；
4. R7继续延后到首个production Tool/Sandbox adapter前，届时升级为条件性P0门禁。R6已关闭，future横切决议遵守既定owner/link与扫描纪律。

## 三、结论摘要

| 维度 | 评价 |
| --- | --- |
| 核心 seam / 不变量 | 自洽且防御性强，第一轮重大问题无复发 |
| 已关闭具体问题 | R1 Submit admission、R2 token estimator owner、R3 Runtime字段/共享模块、R4 migration副本、R5 CONTEXT旧术语 |
| 横切缺口 | R2 token估算owner已关闭；ContentHash/fingerprint规范化已由ADR 0123删除，public ID/serde/basic types仍需wire/schema freeze |
| 安全开放项 | 1 个（R7 = C3，已延后；production Sandbox adapter前为条件性P0门禁） |
| 文档结构 | R6已关闭：8条高风险INV索引 + canonical owner/link纪律；`docs/refactor/`已删除 |
| 最大整体风险 | 零实现验证：冻结设计与 Rig 现实的首次碰撞 |

## 评审决议

- **R1（Submit admission语义）**：**已关闭**（2026-07-26）。运行中用户输入的解释归UI/CommandSurface层：Turn Running时默认路由为`Steer`，显式选择时`FollowUp`（与pi的调用方选择、Codex/Claude Code的默认注入一致）。协议层`Submit`收窄为Idle-decision-only：Executor仲裁时非Idle立即`SessionBusy`，`TurnAdmissionQueue`只是请求信箱而非跨Turn排队通道，与FollowUp的职责重叠消除；terminal decision窗口内到达的Submit参与该次公平admission。两条竞态回退为adapter约定：Steer遇terminal typed outcome后改发Submit；Submit遇`SessionBusy`后按新Snapshot重新路由。`Cancel(Submit)`生命周期覆盖信箱停留与Starting期间。已同步：`session-execution.md`（Submit流程、SessionIngress表、Admission规则、Lane arbitration、并发规则、Test Matrix）、`runtime-interface.md`（TurnCommand语义、CommandPromptDelivery、SessionQueueView）。

- **R2（token估算器owner）**：**已关闭**（2026-07-29）。估算率归ModelGateway validated model definition（`TokenEstimateRate`），经`TurnModelSnapshot::token_estimator()`分发为唯一本地估算来源；首版使用保守字节启发式，effective rate与algorithm version由`ModelDefinitionVersion`覆盖。已回写`model-gateway.md`、`compaction.md`和`prompt.md`，不使用`TurnModelFingerprint`。

- **R3（Runtime字段可见性与共享模块口径）**：**已关闭**（2026-07-29）。MiniCoreRuntime拥有四个private Runtime-owned共享深模块；architecture、Prompt、Skills、modules README和Runtime Interface口径一致。

- **R4（migration capture图陈旧副本）**：**已关闭**（2026-07-29）。migration删除内联capture图，链接Turn Execution Context权威版本并只保留排期摘要。

- **R5（CONTEXT混入V1资源术语）**：**已关闭**（2026-07-29）。三个旧条目已删除，其历史归属合并到pre-refactor aggregate term。

- **R6（跨文档不变量复述）**：**已关闭**（2026-07-29）。建立8条高风险INV索引与canonical owner/link纪律，收窄非owner副本，统一旧ADR current措辞，删除`docs/refactor/`并把横切ADR扫描流程写入migration；不建设覆盖所有普通约束的全局不变量数据库。

- **R7（Sandbox capability预执行拒绝）**：**开放但延后**。与第一轮C3/O1为同一问题；开始首个production Tool/Sandbox adapter前升级为条件性P0并关闭。

- **非阻塞项**：ContentHash/fingerprint、Skill lazy-load例外、全局锁序、Fork nested remap与`docs/refactor/`清理已被后续ADR或R6关闭；协议完备性、wire/schema freeze、Gemini枚举、CompactionSettings来源和复杂度观察中未明确接受的建议继续开放。

其余各项决议后按第一轮惯例在此登记并回写权威文档。
