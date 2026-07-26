# MiniCore V2 设计评审

状态：设计评审记录
日期：2026-07-25
范围：`docs/architecture.md` + `docs/modules/`（12 篇）+ `docs/adr/`（0100–0113）
方式：初始发现来自按设计切面的只读评审；A、B、C1/C2/C4、D1与E3已形成决议并同步到权威文档，未关闭项继续保留为评审输入。

## 总体判断

核心 seam 在原则层自洽且防御性强：单 SessionExecutor + 单 mutation 点、append/apply 线性化、exact model pin、PromptSet 唯一模型上下文组装、trusted projector 构造、lease-based revocation。`TurnExecutionContext` 字段 owner、`SessionExecutor`/`AgentLoop` 分界、四类 Workspace/Prompt/Tool/Skill view 契约在各文档表述一致——这是设计的强项。

风险集中在两类：

1. 多处「留待后续」的**共享类型 / 全序规则**实际会阻塞首版实现；
2. 模块交界处的**归属与失败分类**。

以下问题按「重大（编码前需定案）」与「非阻塞 / 可延后」分列。严重度经二次核对有调整：并发资源锁一项在复核 `tools.md` 执行链后从重大降为非阻塞（见 D 组说明）。

---

## 一、重大问题（影响实施，需在编码前定案）

### A. 跨模块共享契约缺失（最优先）

**A1 · `operation_key` 无全域命名空间语法 + payload normalization 未定义**
幂等语义（同 key 同 payload 返回原 receipt、不同 payload 报 `OperationConflict`）、`OutcomeUnknown` reopen-by-key、recovery「由 TurnId/ItemId/RequestId + reason 派生稳定 key」、fork regenerate 全部把正确性押在 key 唯一性与 normalized fingerprint 上，但文档只给示例串 `turn:t1:user`，缺少覆盖所有 entry kind 的带命名空间前缀的无碰撞 key 语法，以及 normalized payload 的规范化函数定义。
- 影响：两个不同 intent 撞 key → 静默别名（返回错误 receipt）或误报 conflict；normalization 不定 → 同一 logical retry fingerprint 不一致，退化为 conflict 或写第二条 log（= corruption）。幂等层无法正确实现。
- 建议：先冻结 key grammar（kind 前缀 + 稳定业务坐标）与 normalization 白名单，作为 durable 契约而非 helper 惯例，供所有 append 调用点唯一引用。
- 出处：`conversation-storage.md`，跨模块。

**A2 · `ExecutionMode` 是跨模块使用却从未定义的共享类型**
`ResolveTurnModelRequest.execution_mode`、`ToolTurnContext.execution_mode`、TurnExecutionContext capture metadata 均引用它并进入 fingerprint，但无枚举/语义；`ModelCallPurpose` 又明确「不是 foreground/background」，故 `ExecutionMode` 是独立概念。
- 影响：model resolve 与 tool for_turn 两条 capture 关键路径的输入未定义，capture DAG 无法实现，不同实现者臆测取值域破坏 fingerprint 一致性。
- 建议：定义封闭枚举与语义（如何影响 model resolution 与 tool 披露/授权），或从 context 中移除直到有真实需求。
- 出处：`model-gateway.md`、`tools.md`、`turn-execution-context.md`。

**A3 · Session `Create` 的 Agent 绑定语义矛盾**
`runtime-interface.md` 为 `Create { agent: AgentRevisionRef, ... }`（caller 传 exact 修订）；`agent-session-lifecycle.md` 的 create 流程说「在同步内读取 Agent current AgentRevisionRef 来 pin」。若 caller 传 A1 而 current 已是 A2，pin 谁未定。
- 影响：Session↔Agent 绑定第一因果点自相矛盾，直接决定「一个 Session 只引用一个 AgentId」「create pin current」两条基础不变量能否成立。
- 建议：二选一并统一。坚持「pin current」则公开命令改为 `Create { agent_id: AgentId, expected_current_revision?: AgentRevisionRef }`；要支持按指定 revision 创建则改 lifecycle 为「校验传入 ref 属该 Agent 且 Enabled，直接 pin」，删除「读取 current」表述。
- 出处：`runtime-interface.md` ↔ `agent-session-lifecycle.md`。

### B. 确定性与恢复正确性（前置不变量）

> 以下B1–B3正文保留评审时的原始发现和当时建议，仅用于说明问题来源；当前决议已由[ADR 0109](../adr/0109-review-b-determinism-and-serialized-operations.md)替代，权威结果见文末“评审决议”。

**B1 · Prompt scope 内 priority / 冲突排序未定（已关闭）**
基础不变量要求「相同输入产生相同排序与稳定 `PromptFingerprint`」，但同 scope 多 definition 的全序、同 key 冲突解析留到「后续问题」。`PromptMergeMode::Append` 已引用「priority」，而 `PromptDefinition` 无该字段。
- 影响：无 scope 内全序则 `PromptSet::assemble` 不确定、`PromptFingerprint` 不稳定，而后者是 cache/continuation、retry 一致性、recovery 比对的基础——Prompt 子系统能否进入实现的前置。
- 建议：先冻结 scope 内 priority key 与同 key 冲突（diagnostic vs 拒绝）规则，至少给出一个可确定的 total order。
- 出处：`prompt.md`，跨模块复核确认。

**B2 · 缺「append 校验 ⊇ replay 校验」+「committed entry 必然可 project」不变量（已关闭）**
`apply 失败不回滚已 append entry → 丢弃 hot projection → 从 durable current entry replay`，replay 用同一批 projector 重放同一条已落盘 entry。若 apply 失败是确定性的（projector 语义拒绝而非 OOM 等瞬时故障），replay 会再次失败 → 永久 replay-fail brick，而非承诺的「恢复」；append-time 校验若弱于 reload-time 校验，一条已 commit 的 entry 会在冷 open 时 fail closed，整个 Session 不可打开。
- 影响：恢复路径能否成立的前提，却未写成不变量。
- 建议：明确 (a) append 校验集合等价于/强于 reload 校验；(b) 对已通过校验的 committed entry，projector 只能瞬时失败、不得语义拒绝；据此把「apply 失败→replay」限定为瞬时故障重试。
- 出处：`conversation-storage.md`。

**B3 · `execution_version` 在 logical retry 不递增，四元组失去唯一性（已关闭）**
校验四元组 `SessionId+TurnId+execution_version+OperationType` 在「被判失败的旧 attempt」与「retry attempt」间完全相同（retry 只递增 `retry_count`，不在校验元组内）。retry 最有价值的场景（`RequestOutcomeUnknown`/`StreamInterrupted`/客户端 timeout）恰是旧 future 可能仍 in-flight 时，会出现两个同元组 future，executor 无法区分，可能 accept stale/duplicate model response 或 double-drive AgentLoop。
- 影响：并发正确性核心。当前隐含不变量「同一 version 至多一个 in-flight future 且新 op 严格晚于同 version 旧 op 返回」只在 retry 由已返回的 retryable failure 触发时成立，对 hang/unknown 场景不成立。
- 建议：每次新 model attempt（含 logical retry）递增 `execution_version`，或给 `RunningOperation`/`OperationResult` 增加单调 `operation_instance_id`(attempt nonce) 并纳入校验；同时把上述不变量写成显式约束。
- 出处：`session-execution.md`。

### C. 安全 / fail-closed 缺口

> 状态说明（2026-07-24）：C1、C2、C4已按[ADR 0110](../adr/0110-prompt-and-skill-use-shared-reloadable-views.md)关闭；以下原始问题正文仅作评审历史记录。C3本轮不变，仍保持开放。

**C1 · scope override 单调性只是散文约束**
`DefinitionOverrides.enabled/model_visible/user_visible` 是裸 `Option<bool>`，`PromptMergeMode` 只有 `Required`（强制 present）无对称的 `Forbidden`/sealed。解析为 Runtime→Agent→Session last-wins，resolver 无法区分「default disable（可被下层翻回）」与「required disable（锁定）」，Session override `enabled=true` 规范上可翻回 Runtime 的 disable。
- 影响：scope resolution 与 PromptPolicy 核心路径，数据模型无法表达「禁止/密封」一侧——安全相关。
- 建议：给 override 增加 sealed/lock 一等标记，或由 `PromptPolicy` 承载 per-scope allow/deny；resolver 遇下层解除密封项返回 typed diagnostic 并 fail-closed，使「禁止」与 `Required` 对称。
- 出处：`prompt.md`。

**C2 · role×scope 特权未约束**
scope 与 role 正交，但类型上任意 scope 可用 `role=System`。Workspace prompt 属 Session scope、来自「已授权但不可信」的项目文件，若以 `role=System`/`Developer` 注入即进入特权指令块——现实注入向量（文档示例是 Workspace→Developer，但无强制规则）。
- 影响：`for_turn`/`assemble` 的 required-policy 校验需要该规则才能封住旁路。
- 建议：定义 role claim 策略（`System` 保留 Runtime，`Developer` 上限到 Agent，Workspace/Session 仅 `Developer/User` 且受白名单），在 for_turn 解析与 assemble 最终校验双点 fail-closed。
- 出处：`prompt.md`。

**C3 · sandbox 无法强制某 capability class 时无预执行拒绝**
文档覆盖「sandbox 执行失败不静默回退」，但未覆盖「adapter 在本平台根本无法约束 process/network（正是无 bash / 子进程限制不可强制场景）」。存在「spec 上有 sandbox、实际未受限执行」的缝隙。
- 影响：fail-closed 安全基线，须先于 Sandbox adapter 实现确定。
- 建议：明确「最终 `PermissionSet` 含当前 `ToolSandbox` 不能强制的 capability class → executor 前拒绝（PreExecution ToolResult）」，与「不静默回退无 Sandbox」并列为不变量。
- 出处：`tools.md`。

**C4 · skills pinned content-hash 再校验表述含糊**
`SkillCatalogEntryRef` 携 `location + content_hash`；anti-drift 要求对按 `location` 实际读入的 bytes 重算 hash 与 pinned 值比对，但文档只说「校验 content hash」，未明确是校验 request 字段还是重算读入内容——「pinned entry 长 Turn/reload 不漂移」头号属性的落点。
- 影响：漂移 / TOCTOU 的正确性完全压在这一步。
- 建议：显式规定 read 后 recompute-and-compare，mismatch → `SkillLoadError`（NotFound/ContentParse），不得用新版本正文替代。
- 出处：`skills.md`。

### D. 并发控制面

> 复核修正：原「canonical resource lock」重大项经复核 `tools.md` 执行链（`approval → ToolResourceLocks → record_execution_start → Sandbox → execute`）后**降为非阻塞**——资源锁在 approval 之后才获取，不会跨人工审批持有，此前担心的 priority inversion 不成立；残留问题见非阻塞组。原评审本组只保留D1，现已关闭。

**D1 · 控制面与数据面共享 bounded FIFO，无优先通道（已关闭）**
`Cancel`/`ResolveInteraction`/`WorkspaceAuthorizationRevoked` 与 `Submit`/`Steer`/`GetSnapshot`/`ToolControl` 走同一队列；队列满时 Cancel 排队等待，其间 Turn 持续调用 model / 执行 tool / 计费。Revocation 的安全性已由 out-of-band lease（`authorize_commit` vs `revoke` 同一同步原语）保住，但 Revocation 的 terminalize/资源释放/`TurnInterrupted` 事件、以及 Cancel 的全部语义仍受队列排空速度支配，Cancel 无 out-of-band backstop。
- 影响：控制面被数据面 backpressure 饿死，与文档「Cancel/ResolveInteraction 不被阻塞」的自述矛盾；队列类型选择应在实现前定。
- 决议：采用per-session `SessionIngress` semantic lanes，不再建立跨lane全局FIFO。Submit、per-Turn Steer、FollowUp、InteractionControl和ToolControl各自bounded；Cancel/revocation使用sticky `EmergencyControl`；PrepareForUnload使用sticky lifecycle signal和有限grace deadline；Snapshot读取immutable published view。保留单一SessionExecutor/Writer owner。权威决策见[ADR 0111](../adr/0111-session-ingress-separates-control-and-work-lanes.md)。
- 出处：`session-execution.md`。

### E. 能力缺口 / 可能需重定范围

**E1 · Compaction 对长 agentic Turn 是主路径失败，不是边缘（已关闭）**
initiating UserMessage 之后全部 committed model-visible history 被 hard-protect，且 retained 必须是连续 suffix → active Turn 内所有 ToolRound 都不可摘要。大量 / 大体积 tool round（编码 agent 常态）使 protected suffix 单调增长 → `ProtectedSuffixTooLarge` → hard overflow → `TurnFailed`，且「同一 Turn 最多一次 overflow recovery」使其不可挽救；轮间 soft compaction 只能回收 pre-turn 历史，headroom 有限。
- 影响：这是目标用例（长 agentic Turn）的主路径，而非文档定位的「单个超大 Turn 边缘情形」，决定 v1 是否必须支持 turn 内 tool-round 级压缩/分段。
- 决议：保留initiating与Steer UserMessage原文，新增`ActiveTurnCompletedPrefix` scope；每个exact UserMessage开启一个instruction segment，在完整ToolRound安全边界把该segment早期已完成work滚动为至多一个`ActiveTurnCheckpoint`。Pending/Started/incomplete ToolRound、explicit protected entries和recent exact tail不进入coverage。每个segment使用单调coverage frontier；滚动时用`previous_checkpoint`指向当前effective checkpoint，并从backing compaction派生covered-through provenance，不能把checkpoint boundary误当成原始frontier。successful compaction推进后可在单Turn有界次数内再次compact；同一source/frontier hard recovery不重复。权威决策见[ADR 0112](../adr/0112-compaction-supports-active-turn-checkpoints.md)。
- 出处：`compaction.md`、ADR 0112（取代ADR 0107）。

**E2 · `summary_max_output_tokens` 与 pinned model `EffectiveModelLimits` 未 reconcile（已关闭）**
`CompactionSettings.summary_max_output_tokens` 是单一全局 `NonZeroU32`，直接进入 plan 与 directive；但 `ModelCallRequest::new` 校验 `max_output_tokens ≤ TurnModelSnapshot` effective limit。当 pinned model 上限更小时，每次 compaction 请求构造 `InvalidRequest` → 小 context 模型 compaction 永久失效，且误分类为 InvalidRequest/TurnFailed。
- 影响：plan → request 是必经路径，文档无 clamp/校验规则，而 model-gateway 又禁止静默 clamp（clamp 会改变已 fingerprint 的 policy）。
- 决议：plan阶段派生`CompactionSummaryBudget`，对全局上限、pinned known model output limit、summary source、Prompt固定开销、context window与safety reserve求交；最终值进入plan/directive/fingerprint。低于`summary_min_output_tokens`时返回`NoFeasibleSummaryBudget`，ModelGateway继续strict validate且不静默clamp。plan/Prompt proof/ModelCallRequest/SessionExecutor append gate负责临时budget一致性，SessionStorage冷重放只验证entry可重建的scope、boundary、hash、checkpoint和provenance关系。权威决策见[ADR 0112](../adr/0112-compaction-supports-active-turn-checkpoints.md)。
- 出处：`compaction.md` ↔ `model-gateway.md`。

**E3 · `UserQuestion` Interaction 没有发起 seam（已关闭）**
`InteractionRequest` 冻结为 `ToolApproval | UserQuestion`，公开协议也含 UserAnswer resolution，但 Tool↔SessionExecutor 唯一 crate-internal seam `ToolExecutionControl` 只有 `request_approval`/`record_execution_start`，`Tool::execute` 只有 `ToolUpdateSink` + 窄 context，无法发起 durable Interaction，也无内建 ask-user Tool。
- 影响：领域与公开协议承诺 UserQuestion，但执行层无生产者，任何 ask-user 能力无法落地。
- 决议：在 `ToolExecutionControl` 增加 `request_user_question(item_id, request)` crate-internal producer seam；首版由独占的pre-execution ask-user route调用，在`ToolExecutionStarted`、资源锁和外部副作用之前创建durable UserQuestion Interaction。等待阶段使用`WaitingForUserInput`，不持有跨Session资源；答案恢复原Tool future并形成`PreExecution` truthful ToolResult。Presentation Adapter负责展示和提交`InteractionCommand::Resolve`，MiniCore负责协议、durability、校验、无限等待、Cancel、Unload、幂等和recovery。权威决策见[ADR 0113](../adr/0113-user-question-uses-runtime-protocol-and-ui-presentation.md)。
- 出处：`turn-item-interaction.md` ↔ `tools.md` ↔ `session-execution.md`。

### F. 实现顺序

> 状态说明（2026-07-26）：F1已关闭。阶段6、7、8保留职责编号，但改为一个模型调用协同交付束；以下原始问题正文仅作评审历史记录。

**F1 · 路线图低估 SessionExecutor(6) / ModelGateway(7) / Compaction(8) 的耦合**
三者在「逻辑模型调用」强耦合：compaction planning 内联进 SessionExecutor 的 `NeedModel` 安全点；`ModelCallRequest::new` 用 `PromptAssemblyProof` 校验 `TurnModelFingerprint`/purpose，Prompt 与 ModelGateway 互持类型契约。按 6→7→8 串行独立交付会返工。真正的硬门是仍标 `[ ]` 的 Rig 0.40.0 spike。
- 建议：把三者作为协同交付束；先落地 `DeterministicProviderAdapter`/`ScriptedProviderAdapter`，让 SessionExecutor + Compaction 在 fake adapter 上闭环，Rig spike 只 gate 真实 provider；把该依赖显式写入迁移记录。
- 出处：`migration/v1-to-v2.md`（原 roadmap 阶段依赖）。

---

## 二、非阻塞 / 可延后问题

### 类型 / 命名一致性（低成本）

- ~~`ProviderCapabilities`（`tools.md`）vs `ModelCapabilities`命名不一致~~：**已关闭**。删除未定义的`ProviderCapabilities`；`ToolTurnContext.tool_calling`直接接收selected model现有的`ToolCallingCapabilities`，不传完整`ModelCapabilities`，也不增加新projection类型。
- ~~`CurrentTurnExecution.model_attempt: ModelAttemptState`与“不建立ModelAttempt entity”矛盾~~：**已关闭**。删除该字段；`current_operation: Option<RunningOperation>`是当前逻辑模型工作的唯一execution-local状态，provider attempt和transparent retry留在ModelGateway内部。
- ~~`AgentLoop::accept_committed_tool_round(round: CommittedToolRound)`引用未定义类型~~：**已关闭**。不新增ToolRound表示；方法直接接收`tool_round_completed`成功append/apply后由SessionStorage生成的trusted `CommittedConversationDelta`。
- ~~`TurnExecutionPhase::Committing`无驱动路径~~：**已关闭**。删除该variant；append/apply保持在当前业务phase内，terminal写入使用`SessionExecutionState::Finishing`，写入延迟通过ProgressEvent/diagnostics观察。
- ~~`PromptMergeMode::Append`引用未定义priority~~：已随B1关闭，当前使用固定层级和stable identity顺序，不增加priority字段。

### 协议 / 事件语义标注（点明即可）

- ~~`CommandResponse`无cursor watermark~~：**已关闭**。ADR 0114删除公开cursor/replay；CommandResponse只返回typed outcome，持续观察使用原子snapshot-first subscription。
- ~~cursor跨load-epoch/restart连续性未定义~~：**已关闭**。首版无公开cursor或跨restart续订；disconnect、背压或restart后重新subscribe并获取新Snapshot。
- ~~Interaction `expires_at`到期后由谁推进未定~~：**已关闭**。MVP删除Interaction级`expires_at`、`Expired` resolution和timeout worker；用户沉默、subscriber缺失或transport断开时保持Pending，不推断Deny。Cancel、Turn terminal、PrepareForUnload和restart recovery使用明确Cancelled closure收口。
- ~~`CommandId`与`SubmissionId`双correlation id~~：**已关闭**。删除独立`SubmissionId`；Submit envelope的随机、不可复用`CommandId`同时作为Turn创建前的process-local admission/cancel target。duplicate in-flight Submit加入原completion；CommandId不持久化，restart后不重放，Turn创建后使用`TurnId`。
- ~~StateEvent混载durable-derived与process-local状态的可靠性范围不清~~：**已关闭**。StateEvent本身统一为非durable observer record；committed-derived与readiness/queue/phase都只在当前subscription lifetime内按序交付。restart后前者从projection重建，未append Steer/FollowUp和旧phase可以消失，host以新Snapshot为准。
- ~~message/reasoning Item只有`item_completed`，流式临时view和ItemId语义不清~~：**已关闭**。SessionExecutor只为AgentRun维护process-local `StreamingItem`；message/reasoning在首个streamed content update分配稳定ItemId，started与delta走ProgressEvent，provider final生成`FinalItemCandidate`，append/apply后才发布同ItemId的`item_completed` StateEvent。Host漏掉started时可由首个delta构造临时view；logical retry清理上一operation的临时view，Turn terminal或新Snapshot提供最终校正。
- ~~Turn/Item公开排序键未定义~~：**已关闭**。不增加DisplaySequence。Turn/Item顺序由selected history path、assistant content/call顺序、Snapshot/Query ordered Vec和new-Item StateEvent顺序表达；并发Tool逆序完成只按ItemId更新原位置。Snapshot是live observer baseline，restart从JSONL replay/conservative recovery开始；MVP只为长期Session/Turn历史和大型catalog分页。
- Runtime scope 与 Session scope 无跨流顺序保证 → 给 host 一句 reducer 指引（两 scope 皆可作为 Session 首次出现来源）。
- 公开 history 读模型 vs 模型可见 conversation 是同一 storage 两投影（durable 但未 `tool_round_completed` 的 tool entry 对 UI 可见、对模型不可见）→ 点明为有意投影差异，避免误判一致性 bug。
- Agent→Session 是 reference-grouping 而非 containment（删 Agent 不级联、history 仍可读）→ ADR 0100 一句话点明。
- ~~`QueryResponse.stamp`与`SessionSnapshot`定位重叠~~：**已关闭**。删除cursor-based ReadStamp；Query只返回typed data与可选领域revision，Snapshot或snapshot-first subscription负责完整恢复读模型。

### 存储 scale / 恢复兜底（correctness 不阻塞，scale 需规划）

- 无持久 checkpoint/index：冷 open、`OutcomeUnknown` reopen、apply-mismatch reload 全 O(n) 全量 replay（含 O(n) 跨 entry 引用/ancestry 校验）；compaction 只 append overlay、物理文件永不收缩，replay 成本随会话寿命单调增长、compaction 后也不下降 → 补 rebuildable 已校验 projection snapshot + byte-offset/checkpoint index，并给物理 segmentation/vacuum 方案（与 fork anchor 引用旧 entry 相互制约，需尽早留位）。
- 「同时只有一个 Running Turn」不在 corruption/replay 校验清单，只靠 executor 纪律 → 提升为 writer 追加校验 + replay fold 不变量（fail closed）。
- 无 explicit repair 工具：中段坏行（delayed-alloc 掉电常见）即 brick 整个 Session 历史，只有 partial-tail 能自动截断 → 补受控 last-valid-prefix 修复 utility（需 exclusive lease）。
- host restart 跨会话非幂等 Tool 重复副作用（Started-but-no-result → Abandoned → 下一 Turn 模型重新请求并再次执行）→ 点明代价，引入 tool 级副作用幂等 key 缓解。
- fork = deep copy 无内容共享（已显式否决 DAG），大会话近 tip 反复 fork 成倍复制 → 记为已知取舍；fork 只复制 path 不复制 sibling branch 应显式声明以免被当缺陷。

### 并发（承 D 组复核）

- 多资源 Tool 无全序获取仍可跨 Session 反序死锁，但操作响应 Turn cancellation，故非永久 → 对单次 execute 的多个 canonical `ToolResourceKey` 采用稳定总序获取，避免依赖 cancellation 兜底。
- ~~某 Tool 在 execute 内部临时发起 UserQuestion（持锁后）会跨该交互持锁~~：**已随E3关闭**。ADR 0113禁止普通Tool在`ToolExecutionStarted`或持有资源锁后调用`request_user_question`；若未来需要，必须另行定义不持锁的producer protocol。
- ~~`PrepareForUnload` graceful unload 不自动 Cancel，长期Pending Interaction可能阻止卸载~~：**已随D1关闭**。LifecycleControl立即stop admission；有限grace deadline属于Unload lifecycle，到期后Cancel active Turn并以Cancelled关闭Pending Interaction。
- 共享 ModelGateway 配额下无前台/后台公平性，大量后台 Session 可饿死交互 Session → 为交互 Session 预留配额/优先级。
- Cancel 需等待越过 `ToolExecutionStarted` 的不可取消 Tool 确认 outcome 后才能 append `TurnInterrupted`，延迟受最慢在途副作用约束（truthfulness 换速度）→ 暴露「Cancelling」中间可观察状态，避免 UI 误判无响应。
- Agent status synchronization 与 WorkspaceCommitAuthorization 两个跨切面同步原语嵌套包裹 initiating append，共享同一 Agent 的多 Session 在 status 原语上跨 Session 串行 turn-start → 文档化二者全局加锁总序，记录同 Agent 多 Session turn-start 串行化吞吐影响。
- ~~queued FollowUp（process-local FIFO）与队首新到 external Submit 的处理优先级未定义~~：**已随D1关闭**。terminal后已accepted FollowUp最多获得一次连续优先；若上一Turn由FollowUp启动且external Submit待决，则下一次Idle decision先选Submit。Submit不会被当作隐式FollowUp跨整个Turn等待。
- `assemble_model_context` 为同步 fn，大 context 组装/tokenize 在同步段内阻塞该 executor 控制面 → 随规模评估 offload，或标注为控制面 stall 风险点。
- Cancel/revocation 路径产生的 Completed（有 truthful tool message 但无 `tool_round_completed`）永久 conversation-hidden，后续 FollowUp/Steer 模型不可见 → 属预期语义，文档显式点明以免实现者误加补偿逻辑。

### ModelGateway / Workspace 弹性与取舍

- gateway 有界 retry 与 executor logical retry 无全局预算，RateLimited/Timeout 下 backoff 复合 → 定义单 Turn 级总 attempt/时间上限并扣除 gateway 已消耗 backoff。
- continuation 要求 new full input prefix 逐段等价于 cache 的 previous input + finalized response，但 finalized assistant（encrypted/signature reasoning、provider item id、空白规范化）经持久化重组后难逐字节还原，优化几乎不触发 → 给 canonical 等价精确定义 + round-trip golden vectors，接受「full request 是常态」为基线。
- `resolve_for_turn` 无 availability probe + 禁 active-Turn cross-model fallback → 首消息命中宕机 model 直接 TurnFailed → 增加「下一 Turn 自动 fallback 到显式配置备用 model」策略（保持 exact pin、不在 turn 内静默替换）。
- 正常 AgentRun 下 provider 返回越权 ToolCall / 结构化输出违约应映射的 `ModelCallErrorKind`（ProtocolViolation? InvalidRequest?）与是否 retryable 未明确 → 显式规定。
- ~~compaction summary 输入预算基准不一致~~：**已随E2关闭**。AgentRun pressure budget与`CompactionSummaryBudget`分离，summary feasibility使用自身effective output reserve。
- 无 manual/proactive compaction（不公开 `CompactSession`）→ 至少预留未来 maintenance 协议位，文档标注为有意 v1 缺口。
- 无 `WorkspaceId`：历史 session 项目归属靠 primary root canonical path 相等，目录移动/路径复用致分组漂移或跨项目误并（授权侧安全，UI 分组会错）→ 明确 grouping 为 cosmetic，规定 path 复用/失效行为，或引入非授权可持久化 project label。
- restrictive definition update 若 durable commit 失败，repair 按 durable-current 旧（较宽松）definition 重解析，收紧静默未生效仅中断 active Turn → 在 SessionReadiness/diagnostics 显式标记「上次收紧未持久化，需重试」。
- lease 在 `authorize()` 时检查，对已 open 的文件 handle 后续写入不再校验，长 handle tool 存在 revocation 窗口 → 补 handle-relative open + 周期 lease recheck 收敛窗口。
- additional roots 进入 Tool ceiling 但默认不进 Prompt/Skill discovery → monorepo「加目录=期望带上项目指令/skills」直觉会落空 → diagnostics/UI 提示「该 root 未授权为 Prompt/Skill source」，属取舍成本而非缺陷。
- crash recovery 是否持久化 `WorkspaceFingerprint`/view fingerprint 仍开放，而 Test Matrix 的「ToolGrantKey 绑定 WorkspaceAccessFingerprint」「fork/resume 后 grant 一致」依赖它 → 实现 storage 前定案。

### 横切复用

- Prompt/Tool/Skill 各自复制同一套 pinning + authorization 纪律（exact revision、lease 校验、source stamp、不回查 current head、content-addressed cache、Workspace*Context 三份平行投影）→ 抽出共享的 authorization/pinning **value type**（非领域分层、非 Resource 外壳），让该安全不变量只定义一次；不合并子系统（deletion test 成立）是对的。
- `prompt_set.tools.tool_set_fingerprint == tool_set.fingerprint()` 的相等性应在 `TurnExecutionContext` 构造处有单一、命名、fail-closed 的断言点。
- `ToolPromptView` 现仅 `specs + fingerprint`，Prompt 组装引用的「guidelines」未定义（Q7）→ 若 system prompt 需 per-tool 指南，窄 view 不足，确认后再决定是否加 `guidelines` 字段。
- 「PromptSet 是唯一组装 seam」依赖 ModelGateway 只做 role lowering 与 cache-control 编码、不新增任何模型可见语义内容 → 在 `model-gateway.md` 显式写成不变量，否则 seam 从 provider 侧泄漏。
- `AssembledModelContextFingerprint` 覆盖 committed conversation + output_contract + purpose，但 `CompactionSummaryDirective` 正文似未入 fingerprint → 纳入 directive hash，保证 summary 请求可复现/可审计。
- prompt `PromptFingerprint` 只覆盖 definition identity/version；若 `PromptContent` inline 且同 version 可变（Q2 未定），content 变更不被察觉（Workspace prompt 有 WorkspacePromptFingerprint，Runtime/Agent/Session 无）→ 保证 content 变更必 bump version，或 fingerprint 纳入 content hash。
- prompt「检测不存在未提交的 current-call model-visible contribution」实为 by-construction（assemble 只接受 `CommittedConversationView`），非运行时检查 → 措辞改为 by-construction。

---

## 复核说明

- 严重度经二次核对调整一处：`session-execution` 的 canonical resource lock 项，在复核 `tools.md` 执行链（approval 先于取锁）后，从重大降为非阻塞，残留「多资源全序获取」建议见并发非阻塞组。
- 初始问题正文保留为历史依据；已关闭项以本页“评审决议”和对应ADR为准，开放项仍是待决输入。核心 seam 划分（deep module deletion test、exact model pin、trusted projector 构造 Replace、lease-based revocation、append/apply-before-model-visible）判定为自洽。

---

## 评审决议（更新至2026-07-26）

针对 A 组已作决定并落盘：

- **A1（operation_key）**：**放弃「溯源重建恢复」要求**，key 机制参考 Claude Code / pi——单写者 append + 随机 per-entry `EntryId` + `parent_id` 树 + partial-tail 截断，不做确定性可重建 key、不做 `OperationConflict` 冲突检测索引、不做 payload normalization。已落盘：`conversation-storage.md` 删除 `operation_key`/`IdempotencyKey` storage 字段、`OperationConflict`、operation-key index、normalized payload fingerprint、fork key-regenerate 与 reload/corruption 的 key 校验；`OutcomeUnknown` 改为 poison writer + 保守终结、恢复靠 committed prefix 状态判断（不 in-run replay-by-key）；恢复终结改为**状态驱动**（已 terminal/已 resolved 则跳过），exclusive lease 下单跑。消费方文档（session-execution / turn-execution-context / tools / turn-item-interaction / agent-session-lifecycle / compaction / runtime-interface / model-gateway）与 ADR 0103/0104 同步；`resolution_key` / `CommandId`保留为in-run去重，Submit CommandId同时承担pre-Turn admission定位，均不承诺跨崩溃durable重建。**B2（committed entry 必可 project、append 校验⊇replay）因恢复完全依赖重放 committed prefix 而更关键**。
- **A2（ExecutionMode）**：**移除**。已从 `ResolveTurnModelRequest`、`ToolTurnContext`、Turn capture DAG 与 fingerprint 删除，并在 `turn-execution-context.md` 记录「前台/后台是 presentation 概念、不进 capture/fingerprint」。若将来需要，改为 tool execution 路径上不进 fingerprint 的窄 approval disposition。
- **A3（Session↔Agent 绑定）**：**采用方案2（snapshot-current + 显式 reload）**。`SessionCommand::Create` 改收 `agent_id`（创建时快照 current 并钉成 exact ref）；`UpgradeAgentRevision.target` 改为 `Option<AgentRevisionRef>`（`None`=重钉 current 的常规升级，给出 exact ref=钉指定/旧版）。存储层始终保存 exact `AgentRevisionRef`。理由：exact pin让Agent selection、Workspace和Model配置稳定；显式Prompt resource reload另行只影响future Turn。

针对B组已作决定并落盘，长期决策见[ADR 0109](../adr/0109-review-b-determinism-and-serialized-operations.md)：

- **B1（Prompt顺序）**：**已关闭**。不增加priority；当前固定Runtime required System → Runtime base System → Agent System → Session User → Workspace User → Tool → Skill层级。PromptDefinition层按PromptKey、PromptId、DefinitionVersion和stable provenance identity排序；Workspace/Tool/Skill分别按relative path、ToolName、SkillId排序；PromptDefinition层内重复PromptKey返回DuplicateKey并fail closed。
- **B2（append/replay/projector一致性）**：**已关闭**。writer append与cold replay共用pure `validate_and_project`；append semantic validation等价于或强于replay validation；writer成功commit的entry必须可project。`apply_committed`只安装预计算trusted delta，增加live-apply/cold-replay等价性测试要求。
- **B3（logical retry operation identity）**：**已关闭**。每Session最多一个current RunningOperation；旧operation terminal/remove或安全drop并关闭结果路径前，不启动retry或下一operation。execution_version继续表示conversation/control basis，不增加operation_instance_id。Steer/FollowUp保持普通FIFO消费语义（物理ingress lane后由ADR 0111修订）；Steer在完整assistant/tool step后每轮pop一条，无ToolCall candidate final在queue非空时保存为Assistant Continue。

针对C组已作决定并落盘，长期决策见[ADR 0110](../adr/0110-prompt-and-skill-use-shared-reloadable-views.md)：

- **C1（Prompt override单调性）**：**已关闭**。删除`DefinitionOverrides`；PromptService共享`PromptResourceView`，Agent/Session只保存PromptId selection，各Turn独立构建PromptSet。Runtime required Prompt不进入selection。
- **C2（role×scope特权）**：**已关闭**。Prompt role只保留System和User；Runtime/Agent可信行为进入System，Session/Workspace/Skill进入User；ModelGateway不再执行Developer lowering。
- **C3（Sandbox capability预执行拒绝）**：**保持开放，本轮不变**。不修改现有ToolSandbox设计。
- **C4（Skill content drift）**：**已关闭**。不采用Catalog revision/exact hash pin；SkillService发布current SkillView，显式reload成功后原子替换，active Turn继续使用captured view和已加载内容。

针对D组已作决定并落盘，长期决策见[ADR 0111](../adr/0111-session-ingress-separates-control-and-work-lanes.md)：

- **D1（控制面与工作面共享bounded FIFO）**：**已关闭**。每个Session使用独立semantic ingress lanes；EmergencyControl不等待普通lane容量，Tool副作用以`ToolExecutionStarted` append为race线性化点；Cancel清理目标Turn的queued Steer但默认保留FollowUp；PrepareForUnload使用有限grace deadline并最终fail closed；Snapshot从immutable published view读取。lane只拆ingress，不增加第二个Session状态或durable owner。

针对E组已作决定并落盘：

- **E1/E2**：**已关闭**，分别由[ADR 0112](../adr/0112-compaction-supports-active-turn-checkpoints.md)记录active-Turn checkpoint与模型感知summary budget。
- **E3（UserQuestion producer与UI/Runtime职责）**：**已关闭**，由[ADR 0113](../adr/0113-user-question-uses-runtime-protocol-and-ui-presentation.md)记录。`request_user_question`是Turn-scoped crate-internal producer seam；首版ask-user route独占、pre-execution且不持锁，`WaitingForUserInput`保持Turn/Session execution Running。Presentation Adapter只拥有presentation，MiniCore拥有Interaction protocol、durable state、resolution校验、无限等待、Cancel/Unload、幂等和recovery；UserQuestion等待不影响其他Session。后续review只需验证实现是否遵守该协议，不再把“UI自行提问”作为可选首版方案。

针对F组已作决定并落盘到[迁移记录的阶段6–8协同交付束](../migration/v1-to-v2.md#阶段-6-8-模型调用协同交付束)：

- **F1（SessionExecutor / ModelGateway / Compaction实现顺序）**：**已关闭**。三个模块保持既有职责边界，但不再按6→7→8独立串行验收。首个实现里程碑使用`ScriptedProviderAdapter`通过真实`PromptSet → ModelCallRequest::new → ModelGateway → ProviderAdapter`路径闭环普通AgentRun与overflow→CompactionSummary→append/apply→reassemble→AgentRun；Rig 0.40.0 spike并行提前执行，并在production provider adapter冻结前作为门禁。

观察协议后续决策见[ADR 0114](../adr/0114-runtime-observation-uses-snapshot-first-streams.md)：首版删除公开RuntimeCursor/SessionCursor、ReadStamp、Gap和event replay；subscribe首帧原子返回Snapshot，之后只发送实时事件，断线/背压/restart后重新subscribe。该变化不影响多Session并行执行。

协议identity后续决议：独立`SubmissionId`没有独立生命周期，已删除。Submit的随机、不可复用`CommandId`在initiating UserMessage append前定位唯一admission candidate和Cancel target；同一Runtime内duplicate in-flight Submit加入原completion，restart后旧CommandId不重放。append后返回`TurnStarted { turn_id }`，后续取消使用`TurnId`。

StateEvent可靠性后续决议：不增加durability enum或第二event通道。所有StateEvent都是当前subscription内按序交付的非durable observer record；payload来源决定restart后的重建方式，Host始终以新Snapshot重置read model。

Item streaming后续决议：不建立StartedItem/DeltaItem/CompletedItem三套存储。SessionExecutor只为AgentRun维护`StreamingItem`累积buffer和未提交`FinalItemCandidate`；正式Item只由append/apply后的projection产生。AgentMessage/Reasoning的started/delta属于ProgressEvent。ToolInvocation Started仍由assistant/intermediate tool_call entry append/apply后的projection派生committed-derived StateEvent；后续`ToolExecutionStarted`只表示真实副作用边界。

Interaction等待后续决议：MVP不把用户沉默解释为领域事件，删除通用`expires_at`、`Expired`和Interaction timeout调度。Pending只由typed host resolution或显式生命周期动作关闭；disconnect和无subscriber保持Pending，Unload grace deadline仍独立生效。

Turn/Item排序后续决议：不增加scope-local DisplaySequence、ordinal或segment entity。Assistant finalized entry在Tool执行前durable创建call/content有序Items；Tool异步完成只更新对应ItemId的原位置。Snapshot/Query ordered Vec和new-Item StateEvent创建顺序是公开排序契约，progress顺序只属于provisional presentation。
