# System Blueprint Review Issues — Round 3

日期：2026-07-12

来源：进入实现阶段前的第三轮可实现性审阅。本轮由多个 subagent 分层全文精读执行层（`driver.md` / `model-gateway.md` / `tools.md`）、资源与提示词层（`prompt.md` / `resource-manager.md` / `skills.md` / `prompt-templates.md` / `compaction.md`）、会话/事件/命令/持久化层（`session-manager.md` / `agent-runtime-events.md` / `agent-runtime-protocol.md` / `command-surface.md` / `usage-stats.md` / `runtime-hooks.md`），交叉核对全部 20 个 ADR、`CONTEXT.md`、`architecture.md` 与前两轮 review，并首次核对了 Rig 上游源码（rig-core 0.39.0 / 0.40.0）以验证蓝图押注的 sans-IO `AgentRun` 假设。本轮只记录前两轮（BR-001 ~ BR-046）未覆盖的新问题，编号从 BR-047 起延续。

约定：与前两轮相同，本文只记录待处理问题，不代表已经决定修改方案。后续逐条处理时，应回到对应 source of truth 文档中做设计取舍。

## 状态说明

- `Open`：待分析或待处理。
- `Resolved`：已经处理并验证。
- `Partially Resolved`：核心方向已收敛，但仍有后续场景或配套问题需要处理。
- `Won't Fix`：确认接受现状或暂不处理。
- `Deferred`：确认 MVP 范围外，条件满足后再打开。

## 总体判断

蓝图已达到可以进入实现的成熟度：四表面分离（ADR 0018）、单可信 batch writer（ADR 0019）、单终态 `run_finished`（ADR 0003）、无 current session（ADR 0020）、级联不可变资源快照（ADR 0010）、`CommandRunPolicy` 与 `PromptDelivery` 正交（ADR 0016）、ModelGateway spine 先行（ADR 0014）构成了自洽且互相咬合的不变量体系。持久化与事件生命周期两个最难闭合的层面已具备 conformance-test-first 的开发条件。

一个重要的外部验证：蓝图整体押注的 Rig sans-IO `AgentRun` / `AgentRunStep` 路径**在上游真实存在**（rig-core 0.39.0，2026-06-19，PR #1899），`CallModel` / `CallTools` / `Done`、`tool_results()`、可序列化 `AgentRun` 与 `driver.md` 伪代码逐字段吻合，存在性风险解除。但 0.40.0（2026-07-10）已围绕该 API 做了一轮 breaking change，且蓝图从未 pin 版本——细节形状风险仍在（BR-049）。

本轮结论是"可以开工，但有前置"。前置项集中在四类：

1. **一个真正的架构缺口**：session 内并发/借用模型未定义，按现有草图 approval 会死锁（BR-047）。这是唯一需要新 ADR 的结构性问题，也是第一条真实纵切就会撞上的问题。
2. **少量硬矛盾与死表面**：`project_model_call` 的 receiver 三方不一致（BR-048）、`ResumeRun` 是 MVP 里没有产生源的死命令、若干 MVP 载荷类型被引用而未定义（BR-052）。
3. **前两轮遗留的 Open/观察未执行**：BR-036（多 workspace）仍 Open，本轮确认它是 `AgentRuntime` / `SessionManager` bootstrap 的开发阻塞，建议开工前用一页 ADR 关闭；round2 过程性观察 #1/#3 的协议裁边至今未做（BR-058）。
4. **实现前需定型的类型与算法空洞**：约 15 个 seam 类型零定义（BR-051）、确定性承诺缺 canonical 算法（BR-063）、sandbox 只有校验规范无 enforcement 方案（BR-050）。

这些都不动摇模块边界，返工风险局限在个别文件的接口形状。除 BR-047 需要新 ADR、BR-036 需要一页 ADR 外，其余多为文档级定稿，合计约 3~5 天。

## 高风险

### BR-047：Session 内并发/借用模型未定义，按现有草图 run 飞行中的命令投递会死锁

状态：Open

问题：`session-runtime.md` 的 `SessionDriverHost` 草图持有 `tools: &mut Tools`、`queues: &mut QueueState`、`current_run: &mut Option<CurrentRun>`，其独占借用生命周期覆盖整个 `drive_run().await`。同时文档要求多个命令在 run 飞行中触碰这些被独占借用的状态：`DecideToolApproval` 在 `Turn + WaitingApproval` 时合法、`Steer` 在 run 中入 steering queue、`Compact` / `SetModel` 在 run 中存 pending action。`session-manager.md` 提到"同一 session 的 actor ordering"和 `SessionRuntimeFactory::spawn -> SessionRuntimeHandle`，暗示 per-session actor，但没有定义 actor 循环与 `drive_run` 的关系。若 actor 循环内联 `await drive_run`，mailbox 在整个 run 期间不被消费，形成三方死锁：`ToolApprovalBroker` 等 decision → decision 命令等 actor 循环消费 mailbox → actor 循环等 `invoke_tool_batch` 返回。

证据：

- `docs/modules/session-runtime.md`：`SessionDriverHost` 构造代码持有 `&mut self.tools` / `&mut self.queues` / `&mut self.current_run`，随后 `driver.drive_run(request, &mut host, cancel).await`。
- `docs/modules/session-runtime.md`：Session Phase 表允许 `DecideToolApproval` 只在 `Turn + WaitingApproval` 合法；队列语义要求 active run 期间入 steering/follow-up queue。
- `docs/adr/0011-tools-are-session-scoped-subsystem.md`：以 borrow-checker 动机论证 `Tools` 内嵌，但未回答 run 在飞时 decision/steer 命令如何到达 broker/queue。

风险：这是"不先补设计就无法写出第一版 `SessionRuntime`"的缺口。approval、steer、running-compact 三条产品必需路径全部依赖它；每种解法（broker 内置 channel、host 持 sender 而非 `&mut`、actor 在 step 边界交错处理 mailbox）都会改动 ADR 0011 画下的借用草图。

待处理方向：补一篇并发模型 ADR，定义 per-session actor 循环、`drive_run` 在飞时命令如何到达 broker/queue、`&mut` 草图改为何种通道拓扑，以及 `DecideToolApproval` / `Steer` / `Compact` 在 run 期间的投递路径。这是阶段 3 text-only 纵切的前置。

### BR-048：`project_model_call` 的 receiver 在三处文档互相矛盾，Driver 拿不到方法接收者

状态：Open

问题：Prompt 的最终投影入口 `project_model_call` 在三处有三种不相容的形态。`prompt.md` facade 把它定义为 `PromptTurn::project_model_call(&self, ModelCallProjectionInput)`，profile 来自 `self.profile`；`driver.md` 的 CallModel step 写成自由函数 `prompt::project_model_call(history, prompt, DriverTurnInput.prompt, context materials)`；`session-runtime.md` 散文又说"由 `Driver` 调用 Prompt 的 final projection seam / `Prompt::project_model_call(...)`"。而 `driver.md` 与 ADR 0013 明令 `DriverTurnInput` 不得携带 `PromptTurn`。三者合起来无解：Driver 是调用者，却拿不到 `PromptTurn` 这个 receiver。

证据：

- `docs/modules/prompt.md`：facade 定义 `PromptTurn::project_model_call(&self, ...)`。
- `docs/modules/driver.md`：CallModel step 描述为自由函数调用 + `DriverTurnInput` 不含 `PromptTurn` 的禁令。
- `docs/modules/session-runtime.md`：运行流程第 7 步 "Driver ... 调用 `Prompt::project_model_call(...)`"。

风险：唯一一处"不改文档就无法写代码"的硬矛盾，同时阻塞 `prompt/projection.rs` 和 `driver.rs`。

待处理方向：二选一（建议前者）：(a) 把 projection 改为以 `&PromptCallProfile` 为输入的自由函数或 `PromptCallProfile` 方法，同步更新 `prompt.md` facade，"system prompt 与 tool schemas 同 profile"的校验退化为单参数自洽；(b) 增加 `DriverHost` 方法把 projection 路由回持有 `PromptTurn` 的 `SessionRuntime`。可作为 ADR 0017 的勘误补充。

### BR-049：Rig 版本未 pin，且大量字段级类型先于 spike 冻结在未验证的 Rig 形状上

状态：Open

问题：Driver 设计的字段级细节建立在 Rig sans-IO API 的具体形状上，但蓝图从未 pin rig-core 版本。上游 0.39.0 落地该 API 后，0.40.0（两天前）已围绕它做了一轮 breaking change（hook system v2、`AgentRunner`、structured tool-execution results、builder 改名），Unreleased 还有 `PromptError` non-exhaustive 等破坏性变更。同时 `driver.md` 对 Rig 真实协议分支建模偏浅：上游存在而文档未覆盖的有 `ModelTurnOutcome::NeedsResolution(InvalidToolCallContext)`（需选恢复策略并调 `resolve_invalid_tool_call`）、`PendingToolCall.preresolved_result`（有值时须绕过工具执行直接回填，与全治理管线的关系未定义）、构造 `ModelTurn` 需要 rig `Usage` 与 `executable/allowed_tool_names`（要求 `ModelCallResult` 的 usage 归一化**可逆**回 rig 类型，而当前 `ModelCallResult` 无承载字段）。此外 `NextModelCallPlan.persistent_messages`（steering）和 `FinishDecision::ContinueWithMessages` 都要往进行中的 run 注入消息，但 `AgentRun` 公开 API 只有构造期 `with_history`，没有 mid-run append 入口——这是"承诺契约"里唯一在上游找不到直接支撑的点。

证据：

- `docs/modules/driver.md`：`feed_model_response_to_rig` / `feed_tool_results_to_rig` 等伪代码假设的 Rig 形状；未出现 `ModelTurnOutcome::NeedsResolution` / `preresolved_result` 处理路径。
- `docs/modules/model-gateway.md`：`ModelCallResult` 无回传 rig `Usage` / tool-name 集合的字段。
- `docs/modules/session-runtime.md`：`NextModelCallPlan.persistent_messages`；Rig 无 mid-run history append 入口（上游核对）。
- round2 过程性观察 #1 已预警"字段级设计先行于 Rig 验证"。

风险：Rig 每次升级都是隐形返工；steering 注入若在 spike 中证伪，`persistent_messages` / `ContinueWithMessages` 一线类型连锁返工。

待处理方向：按 ADR 0014 的"spine 先行"顺序，把阶段 2 spike 的验收产物固化为一份"Rig 映射矩阵"文档（`MessageRecord ⇄ rig::Message/AssistantContent/UserContent`、`ModelCallResult → ModelTurn`、`TokenUsage ⇄ rig Usage`、`ModelTurnOutcome` / `preresolved_result` 处理、steering 注入策略）；pin rig-core 精确版本；矩阵产出前冻结 driver/model-gateway 的字段级扩张。`Steer` 不可用时必须返回 `SteerUnavailable`（已在 `session-runtime.md` 规定），该保险在 spike 结论前保留。

### BR-050：tool sandbox 只有进程内校验规范，缺 OS 级 enforcement 方案

状态：Open

问题：BR-037 已为 `ToolSandboxView` 补齐 path canonicalization、symlink/父目录校验、denied roots 优先级、verdict 等"校验器"层的 source of truth，这部分闭合到可测。但 `NetworkSandboxPolicy` / `ProcessSandboxPolicy` 对 `bash` 的**落地机制**（纯进程内检查？Job Object / AppContainer / seccomp？）未定义。MVP 只读工具不受影响，但进入 `bash` / `write` 阶段时，"限制网络/进程"到底靠什么强制执行是空白。

证据：

- `docs/modules/tools.md`：Sandbox Source Of Truth 小节定义了校验规则，但 process/network policy 只描述判定，未描述 enforcement。
- 本仓库在 Windows/OneDrive 路径下开发，bash 网络限制的平台落地不是假想问题。

风险：mutation/bash 阶段的核心安全语义靠实现时即兴决定；"限制网络后 bash 仍能联网"这类失败无从测试。

待处理方向：不阻塞 MVP 只读工具与文本纵切；在 roadmap 上占位，进入 `bash` / `write` / `edit` / `apply-patch` 阶段前补一篇 enforcement 设计（明确进程内 best-effort 还是 OS 级隔离、平台差异、可观测点）。

### BR-051：约 15 个跨模块 seam 类型被反复引用却从未定义

状态：Open

问题：多个模块文档以类型为契约展开，但这些类型全仓无定义，实现时各模块会自行发明形状。最关键的是 `ToolBatchResult`——它被引用约 20 次，同时是 commit 事务、Rig 回填和事件归约三方共享的值，形状不定则三处对接无从写起。

证据（被 struct 字段/签名引用但无定义）：

- 执行层：`ToolBatchResult`、`DriveResultSummary`、`TurnCheckpoint`、`FinishCheckpoint`、`DriveLimits`、`DriverError`、`ToolSubsystemError`、`ToolApprovalRequest`、`PreparedToolInvocation`、`ApprovalGrantMatch`、`ModelStreamSink` / `ToolUpdateSink` 方法集。
- 资源层：`CwdResourceLayer`、`ResolvedCwdResourceView`、`TurnResourceView`、`CompactionFileOps`、`CompactionDetails`、`SkillLoadInputs` / `SkillLoadReport`、`PromptTemplateLoadInputs`、`ContextMaterialKey` / `ContextSource`。

风险：机械工作，但不闭合就无法按文档写单测；`ToolBatchResult` 未定型会阻塞 tool 纵切。

待处理方向：在第一个纵切前给出这些类型的字段形状；`ToolBatchResult` 优先（它决定 `invoke_tool_batch` 的返回、`ToolRound` batch 组装和 `message_tool_result_appended` 归约三方对接）。其余可随对应模块开工时定型。

### BR-052：MVP 命令表存在缺定义载荷类型、死表面和 cwd 来源空洞

状态：Open

问题：三个问题集中在 MVP `AgentCommand` 表面：(a) `SubmitPrompt.input: UserInput`、`SetStreamOptions.options: StreamOptions`、`SetThinkingLevel.level: ThinkingLevel` 三个载荷类型出现在 MVP 命令签名里，但全仓无任何定义；(b) `ResumeRun { session_id, resume_id }` 列在 MVP 命令中，但 MVP 没有任何能产生 suspension 的源——`SuspendReason::UserSuspendedAtSafePoint` 没有对应 suspend 命令，能触发 pause 的 `BeforeNextModelCall` hook 是后期能力，因此 `ResumeRun` 是 MVP 表里的死表面；(c) `NewSession { workspace_id }` 没有 cwd 参数，新会话的固定 cwd 如何确定在被引用文档中不可知。

证据：

- `docs/modules/agent-runtime-protocol.md`：MVP `AgentCommand` 列表含 `SubmitPrompt { input: UserInput, ... }`、`SetStreamOptions { options: StreamOptions }`、`SetThinkingLevel { level: ThinkingLevel }`、`ResumeRun { session_id, resume_id }`、`NewSession { workspace_id }`。
- 全仓无 `UserInput` / `StreamOptions` / `ThinkingLevel` 的类型定义；无 `NewSession` cwd 来源说明。

风险：实现第一周就会碰到的实际空洞；`ResumeRun` 留在 MVP 会误导验收范围。

待处理方向：定义 `UserInput` / `StreamOptions` / `ThinkingLevel`；把 `ResumeRun` 移入"后续命令"；写明 `NewSession` 的 cwd 来源（workspace root、显式参数还是 settings 默认）。

## 中风险

### BR-053：Prompt 预检超限到 SessionRuntime compaction 的信号路径未定义

状态：Open

问题：`prompt.md` 校验条目说"估算 token 超预检上限时返回结构化 outcome，由 `SessionRuntime` 编排 compaction"。但该校验发生在 Driver 的 CallModel step 内，而 `compaction.md` 的 overflow recovery 只挂在 `DriveResult::Failed { error: ContextOverflow }`（provider 侧错误）上。Prompt 侧预检失败如何变成 `SessionRuntime` 可编排的信号——映射为同一 `ContextOverflow` kind？新增 `DriveResult` 变体？——没有任何文档回答。

证据：

- `docs/modules/prompt.md`：校验清单中的 preflight 超限 outcome。
- `docs/modules/compaction.md`：overflow recovery 仅由 `DriveResult::Failed` 的 provider error 触发。

风险：首个大 prompt（巨型 skill 正文或粘贴）会直接踩中这条未定义路径。

待处理方向：定义 Prompt preflight 超限 → SessionRuntime 的信号（复用 `ContextOverflow` kind 或新增显式 `DriveResult` / preflight outcome 变体），并在 `driver.md` / `compaction.md` / `prompt.md` 三处对齐。

### BR-054：五个 prompt 输入 view 无 owner，`EnvironmentPromptView` 隐含 I/O 职责

状态：Open

问题：`TurnPromptInputs` 的 7 个 view 里，`ToolPromptView`（`tools.md`）和 `PromptResourceView`（`resource-manager.md`）有 owner，而 `ProductPromptView` / `AgentPromptView` / `EnvironmentPromptView` / `PolicyPromptView` / `ModelPromptView` 五个类型全仓库只在 `prompt.md` 出现一次，没有任何模块定义谁构造、字段是什么、何时刷新。尤其 `EnvironmentPromptView` 含"VCS 摘要"，这需要 I/O，而 Prompt 禁止 I/O，`session-runtime.md` 也没认领这项采集。

证据：

- `docs/modules/prompt.md`：`begin_turn(TurnPromptInputs { ... })` 列出七个 view，五个无外部 owner。

风险：五个类型会被各模块自行发明；`EnvironmentPromptView` 的 VCS 采集是隐藏的 I/O owner 缺口。

待处理方向：为五个 view 指派 owner 或合并（建议把 Product/Agent/Policy 合成一个由 `SessionRuntime` 从 settings 构造的 defaults view）；`EnvironmentPromptView` 的 VCS 摘要指定由 `SessionRuntime` 在 turn start 采集，或 MVP 直接砍掉。

### BR-055：`ResolvedPromptInput.parts` 与 `messages` 的折叠关系未定义

状态：Open

问题：`PromptTurn.resolve_intent()` 返回 `ResolvedPromptInput`，其 `parts` 如何折叠成 `MessageRecord`、折叠规则是什么，没有定义。这直接决定"持久化的 `UserInput` batch 内容 == 模型可见输入"这一不变量能否成立，而 `skills.md` 的 commit 流程依赖它。

证据：

- `docs/modules/prompt.md`：`ResolvedPromptInput { parts, ... }`。
- `docs/modules/skills.md`：运行流程第 7/8 步的 `UserInput` commit 依赖 parts→message 的确定折叠。

风险：折叠规则不定，持久化内容与模型可见输入可能分叉。

待处理方向：定义 `parts -> Vec<MessageRecord>` 的折叠 owner 和规则，并声明它是 `UserInput` batch 与模型输入的共同来源。

### BR-056：retry attempt 跨 compaction 的配对未定义

状态：Open

问题：`agent-runtime-events.md` 的转换矩阵允许 `RetryBackoff → Compaction`（retry 前先做 overflow recovery）和 `Compaction → RetryBackoff`，但 retry 配对规则（每个 `retry_auto_started { attempt=N }` 必须由同 attempt 的 `retry_auto_finished` 关闭）只定义了线性链。当 attempt=N 的 `retry_auto_started` 已发出、随后插入的 overflow compaction 失败或被 abort 时，`retry_auto_finished` 发不发、status 是什么，无处可查。这是唯一一处配对不变量可能开天窗的地方。

证据：

- `docs/modules/agent-runtime-events.md`：转换矩阵允许 retry 与 compaction 互切，但 §15 retry 配对只覆盖线性链。

风险：conformance test 无法为该组合写断言；实现可能产生悬空的 `retry_auto_started`。

待处理方向：在事件文档补 retry-attempt 跨 compaction 的配对规则，并加入 Test Matrix。

### BR-057：`reload_runtime` / `recompose_cwd` 在 MVP 实际不可达，且 recompose 后 revision 不通知 UI

状态：Open

问题：`resource-manager.md` 的 trait 定义 `reload_runtime()`，`recompose_cwd` 整套懒重组机制围绕 runtime revision 变化设计。但公开协议只有 cwd 级 `ReloadResources { workspace_id, cwd }`，MVP 中没有任何路径能改变 runtime revision，整条 lazy recompose 分支实际不可达。此外，懒 recompose 经 `replace_cwd` 发布新 `CwdResourceSnapshot.revision`，但 `resources_changed` 只在显式 reload 流程定义，UI 对 recompose 后的 revision 永远不知情。

证据：

- `docs/modules/resource-manager.md`：`reload_runtime()`、`recompose_cwd`、`ResourceReloadResult { scope, runtime_revision }`。
- `docs/modules/agent-runtime-protocol.md`：只有 `ReloadResources { workspace_id, cwd }`，无 runtime 级 reload 命令。

风险：死路径会被当作 MVP 验收范围实现；recompose 后 UI 资源摘要与实际 revision 漂移。

待处理方向：要么补 runtime 级 reload 命令，要么把 `reload_runtime` / `recompose_cwd` 显式标注为 post-MVP；同时规定 recompose 是否发 `resources_changed`（或声明 recompose 不 bump 公开 revision）。

### BR-058：round2 过程性观察 #1/#3 的协议裁边至今未执行

状态：Open

问题：round2 过程性观察 #3 建议"后续命令只保留名字与一句话意图"，#1 建议"统一标注哪些类型是承诺、哪些是草图"。两项均未执行：`agent-runtime-protocol.md` 的"后续命令"块仍是 17 条字段级签名（`SubmitInteraction`、`PatchSettings`、`ExportSession`、`NavigateSessionTree`、`ForkSession`、`CycleModel`、`SetAutoCompaction` 等），配套类型（`InteractionSubmitPolicy`、`ApprovalGrantScope`、`SettingsTarget` / `SettingsRevision` / `SettingsPatch` 等）已完整成型，其中 `SettingsPatch` 一族甚至被引用而未定义；全仓无任何"草图 vs 承诺"标注。

证据：

- `docs/modules/agent-runtime-protocol.md`：后续命令块 17 条字段级定义；`PatchSettings` 引用未定义的 `SettingsTarget` / `SettingsRevision` / `SettingsPatch`。
- `docs/review/system-blueprint-review-issues-round2.md`：过程性观察 #1、#3。

风险：实现者误把 17 条后续命令当验收范围；未标注草图的字段在 Rig spike 后返工时没有提示机制。需区分对待——`ForkSession` / `NavigateSessionTree` 的字段级语义实质约束了 day-one 存储格式（stable batch boundary、`get_committed_batches_to_leaf`），这部分提前定死是正确的，应保留；`SubmitInteraction` / `PatchSettings` / `Cycle*` / `SetAuto*` / `ExportSession` 应降级为名字+意图。

待处理方向：做一次机械裁边（半天）：保留 fork/tree 语义，其余后续命令降级为名字+一句话；给 `protocol.md` 补"承诺 vs 草图"标注，spike 前冻结字段级扩张。

## 低风险 / 一致性

### BR-059：`SessionListFilter` 双形状，`SessionManager` 方法名漂移

状态：Open

问题：`SessionListFilter` 有两种形状——`agent-runtime-protocol.md` 定义 `SessionListFilter { query: Option<String> }` 且 scope 独立为 `SessionListScope`；`session-manager.md` 却要求它支持按 workspace scope 查询，而 `SessionManager.list(filter)` 签名没有 scope 参数。方法名也漂移：trait 定义 `list` / `open` / `create`，而 `session-manager.md` 正文与事件文档分别写 `list_sessions(...)` / `open_handle(...)` / `create_handle(...)`。

证据：

- `docs/modules/agent-runtime-protocol.md`：`SessionListFilter { query }` + 独立 `SessionListScope`。
- `docs/modules/session-manager.md`：filter 支持 workspace scope 的表述；方法名多写法并存。

风险：同名类型两种形状；trait 签名与散文不一致。

待处理方向：统一 `SessionListFilter` 形状与 `SessionManager.list` 签名（scope 放 filter 还是独立参数二选一）；统一方法命名。

### BR-060：`message_assistant_finished` 双语义不可从事件区分，`usage_updated` 非强制且无 only-on-change

状态：Open

问题：`message_assistant_finished` 承载两种语义——commit 后的持久化 finished 与 abort/failure 时"仅关闭 UI lifecycle、不落盘"的 finished 共用同一 payload（`AssistantFinished { message_id }` 无标志位）。这是故意设计（测试需关联 `run_finished.status` 判定），但 conformance test 必须显式编码这条关联规则，而事件文档未把它列进 Test Matrix。此外 `usage_updated` 是"推荐发送时机"而非强制，且无 only-on-change 规则（queue 有，usage 没有），测试只能断言相对顺序、不能断言次数。

证据：

- `docs/modules/agent-runtime-protocol.md`：`MessageEvent::AssistantFinished { message_id }` 无落盘标志。
- `docs/modules/session-runtime.md`：abort/failure 时仍发 `message_assistant_finished` 关闭 UI lifecycle。
- `docs/modules/usage-stats.md` / `agent-runtime-events.md`：`usage_updated` 为推荐时机，无 only-on-change。

风险：可测试性微缺口，非语义错误；测试写法容易各自为政。

待处理方向：把"`message_assistant_finished` 双语义靠 `run_finished.status` 关联判定"写进 Test Matrix；决定 `usage_updated` 是否需要 only-on-change 约束。

### BR-061：后期 hook 侵入 MVP 事件/管线的强制规则

状态：Open

问题：`agent-runtime-events.md` 的 Driver Ownership 节写"`result-ready` 仍可能被 `ToolResultBeforeCommit` 改写，不能直接发布为 UI `tool_call_finished`"，把一个 post-MVP hook 写进了 MVP 事件管线的强制规则。MVP 实现者需要自行推断"无 hook 时 result-ready 即 final"。

证据：

- `docs/modules/agent-runtime-events.md`：Driver Ownership 节引用 `ToolResultBeforeCommit`。
- `docs/modules/session-runtime.md`：`ToolResultBeforeCommit` 明确标注 MVP 不启用。

风险：MVP 管线规则里混入后期 hook，读者需自行剥离。

待处理方向：在事件文档标注"MVP 无 hook 时 result-ready 即 final"，把 hook 改写路径显式圈为 post-MVP。

### BR-062：`capture_turn` 不变量措辞与兜底读盘冲突，职责重复声明

状态：Open

问题：`resource-manager.md` 的不变量清单写"`capture_turn(...)` 只读取当前指针；不读磁盘、不自动 reload"，但同文档 `capture_turn` 实现在 miss 时调用 `ensure_cwd_snapshot`（首次 turn 会读盘）、在 revision 漂移时调用 `recompose_cwd`（发布新指针）。ADR 0010 的"三道防线"说明意图是"steady-state 不读盘"，但不变量的字面表述会被直接写成错误测试。runtime revision 陈旧检查的 owner 也在两处重复声明（`capture_turn` 伪代码内 vs `ensure_cwd_snapshot` 内部）。

证据：

- `docs/modules/resource-manager.md`：不变量清单 vs `capture_turn` 伪代码 vs `ensure_cwd_snapshot` 职责。
- `docs/adr/0010-use-per-cwd-resource-snapshots-for-multi-session-runtime.md`：三道防线。

风险：不变量字面表述会写坏测试；职责重复是漂移温床。

待处理方向：给不变量加"steady-state"限定语；把 stale-runtime-revision 检查的 owner 收敛到一处表述。

### BR-063：确定性承诺缺少 canonical 算法，`baseline_tokens` 为魔数

状态：Open

问题：多处"相同输入得到相同结果"的硬承诺缺少算法定义：fingerprint 三胞胎（`PromptFingerprint` / `PromptProfileFingerprint` / `ModelInputFingerprint`）的 canonical 序列化算法未定；overlay"稳定排序"的 collation key（按 canonical path？声明顺序？）未定；`ResourceRevision(u64)` 的作用域（全局单调？per-cwd？）未定；token 估算方法未定；`ContextUsageView.baseline_tokens = 12_000` 是魔数。

证据：

- `docs/modules/prompt.md`：fingerprint 类型群，无 canonical 算法。
- `docs/modules/resource-manager.md`：overlay 稳定排序无 collation 定义；`ResourceRevision` 作用域未声明。
- `docs/modules/usage-stats.md`：`baseline_tokens = 12_000`。

风险：fingerprint 是最容易各写各的地方；魔数无来源说明。

待处理方向：钉死 fingerprint canonical 序列化、collation key、`ResourceRevision` 作用域、token 估算方法；给 `baseline_tokens` 加来源说明或设为可配置。

### BR-064：Windows 平台细节与 JSONL 并发未声明

状态：Open

问题：`skills.md` 要求 symlink canonical path 去重、遵守 `.gitignore`；`canonicalize` 在 Windows 产生 `\\?\` 前缀，与 `ResourceKey` / `location` 的展示和比较规则需要统一。同一 JSONL 被两个进程打开的行为未提及。本仓库本身在 Windows/OneDrive 路径下开发，这些不是假想问题。

证据：

- `docs/modules/skills.md`：symlink 去重、`.gitignore` 要求。
- `docs/modules/session-manager.md`：JSONL 读写并发可见性只对 InMemory 写了必测项，未提跨进程打开。

风险：Windows canonical 前缀会污染 `ResourceKey` 比较；嵌入式场景下 JSONL 双进程行为未定。

待处理方向：统一 Windows `\\?\` 前缀的处理规则；补一句 JSONL 单写入者/跨进程打开的声明（可容忍即显式声明可容忍）。

### BR-065：skill 复现边界未写明，若干命名/措辞双轨

状态：Open

问题：skill body 原子化只覆盖 `SKILL.md` 正文；`<skill location="...">` 块指示模型用 `read` 工具按 skill 目录解析相对路径加载引用文件，这些后续 read 走 live 文件系统、不受 snapshot pin。这是渐进披露的固有属性，但"旧 turn 可复现"的保证边界应在 `skills.md` 写明，否则实现者会误以为要 snapshot 整个 skill 目录。此外若干小口径命名双轨：`DriverEvent::{BeforeNextModelCall, BeforeRunFinish}` 与同名 host 方法用途未区分；`ModelTextDelta` 与 `AssistantMessageDelta` 两个 delta 事件分工未说明；`tools.md` 自定义 `ToolApprovalDecision` 与 `agent_runtime_protocol::ToolApprovalDecision` 同名两层未声明镜像关系；commit seam 在 `driver.md` 代码里是 `self.session.commit(...)`（`SessionHandle`），正文写 `SessionWriter.commit(...)`。

证据：

- `docs/modules/skills.md`：`<skill location>` 块的 read 语义。
- `docs/modules/driver.md`：`DriverEvent` 与 host 方法双轨；commit seam 措辞。
- `docs/modules/tools.md`：`ToolApprovalDecision` 同名两层。

风险：均为"补一句话"级别，不构成结构性矛盾，但会造成读者认知摩擦。

待处理方向：在 `skills.md` 写明复现保证只覆盖 `SKILL.md` 正文；逐项声明命名双轨的用途或统一措辞。

## 过程性观察（不编号）

1. **Rig 存在性风险已解除，形状风险转入 spike。** 本轮首次核对上游源码确认 sans-IO `AgentRun`、可序列化 suspend/resume、批量 tool 回调、usage 提取全部真实存在（0.39/0.40），ADR 0014 规定的"spine 先行"顺序恰好是正确的验证载体。最大的剩余不确定性不在已评审文档内部，而在 Driver/Rig 字段级假设——这支持"先做存储与事件层、把 Driver 集成放在 spike 之后"的实现顺序。

2. **战略方向仍未正式回答。** `background-session-runtime-progress.md` 记录了路线 A（单进程嵌入式，当前基线）与路线 B（Claude-like 多进程 supervisor daemon）的分叉。BR-002 的关闭、BR-044 的延期、`RuntimeSnapshot` 的 all-loaded 设计都押在"UI 与 runtime 同生命周期"上；若产品真实目标是后台 session daemon，这三个决策需重开。建议在 MVP 纵切跑通后、投入大量 adapter 开发前，正式回答 progress 文档末尾 9 问中的第一问。不阻塞 MVP，但应有意识。

3. **文档权威应逐步让位给代码。** 权威归属表 + 两轮 grilling 已把漂移压得很低，但本轮发现的一致性问题（BR-059/BR-062/BR-065）仍多发生在重复声明的文本上，印证 round2 观察 #2。进入实现后，应让代码和 conformance test 逐步接管"权威"，文档退到 seam 和不变量层面，避免继续维护容易漂移的字段级副本。

4. **过度配重值得在实现前主动削减。** `contribution_stamps` 出现在 `PromptTurn` / `PromptCallProfile` / `ResolvedPromptInput` / `ModelInputProjection` 四层且无合并规则；`ContextMaterial` 三通道机制在 MVP 是零生产者的全量管道（无 hooks、无 RAG/memory）。前者建议只在 `PromptCallProfile` 和 `ModelInputProjection` 保留、其余定义为派生；后者作为自觉的"为未来付费"可接受，但要意识到 MVP 会带着一条无水的水管。
