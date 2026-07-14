# System Blueprint Review Issues — Round 3

日期：2026-07-12

来源：进入实现阶段前的第三轮可实现性审阅。本轮由多个 subagent 分层全文精读执行层（`driver.md` / `model-gateway.md` / `tools.md`）、资源与提示词层（`prompt.md` / `resource-manager.md` / `skills.md` / `prompt-templates.md` / `compaction.md`）、会话/事件/命令/持久化层（`session-manager.md` / `agent-runtime-events.md` / `agent-runtime-protocol.md` / `command-surface.md` / `usage-stats.md` / `runtime-hooks.md`），交叉核对当时全部 20 个 ADR、`CONTEXT.md`、`architecture.md` 与前两轮 review，并首次核对了 Rig 上游源码（rig-core 0.39.0 / 0.40.0）以验证蓝图押注的 sans-IO `AgentRun` 假设。本轮只记录前两轮（BR-001 ~ BR-046）未覆盖的新问题，编号从 BR-047 起延续；关闭 BR-047 时新增 ADR 0021。

约定：与前两轮相同，本文只记录待处理问题，不代表已经决定修改方案。后续逐条处理时，应回到对应 source of truth 文档中做设计取舍。

## 状态说明

- `Open`：待分析或待处理。
- `Resolved`：已经处理并验证。
- `Partially Resolved`：核心方向已收敛，但仍有后续场景或配套问题需要处理。
- `Won't Fix`：确认接受现状或暂不处理。
- `Deferred`：确认 MVP 范围外，条件满足后再打开。

## 总体判断

蓝图已达到可以进入实现的成熟度：四表面分离（ADR 0018）、单可信 batch writer（ADR 0019）、单终态 `run_finished`（ADR 0003）、无 current session（ADR 0020）、级联不可变资源快照（ADR 0010）、`CommandRunPolicy` 与 `PromptDelivery` 正交（ADR 0016）、ModelGateway spine 先行（ADR 0014）构成了自洽且互相咬合的不变量体系。持久化与事件生命周期两个最难闭合的层面已具备 conformance-test-first 的开发条件。

一个重要的外部验证：蓝图整体押注的 Rig sans-IO `AgentRun` / `AgentRunStep` 路径**在上游真实存在**（rig-core 0.39.0，2026-06-19，PR #1899），`CallModel` / `CallTools` / `Done`、`tool_results()`、可序列化 `AgentRun` 与 `driver.md` 伪代码逐字段吻合，存在性风险解除。后续对 rig-core 0.40.0、Pi、Codex、OpenClaw 和 Attractor 的核对进一步确认 Steer/FollowUp 的交付边界可以落在 Rig step 之间；当前没有发现阻止 MiniCore 采用 Rig 的架构级障碍。版本 pin 和字段映射转为真实 Driver integration spike 的延期验收项（BR-049）。

本轮结论是"可以开工，但有前置"。前置项集中在四类：

1. **原唯一架构缺口已关闭**：BR-047 已通过 ADR 0021 定义 per-session actor、显式 `SessionRuntimeHandle`、run-scoped `RunTask`、私有 `RunLink` 和 owned `SessionDriverHost`，approval/abort/queue command 不再被完整 run 阻塞。
2. **命令契约复核已排期**：`project_model_call` receiver 矛盾已按 BR-048 关闭；BR-052 的 cwd 空洞已由 ADR 0022 关闭，未定义载荷和不可达 `ResumeRun` 已延后到开发前与 BR-051 联合执行的 protocol contract / command reachability review。
3. **前两轮遗留项**：BR-036 已由 [ADR 0022](../adr/0022-workspace-is-single-instance-thin-boundary.md) 关闭为单实例薄边界容器；round2 过程性观察 #1/#3 的协议裁边仍待处理（BR-058）。
4. **实现前统一复核的契约与算法空洞**：跨模块 seam 类型及公开 command payload / reachability 已按 BR-051、BR-052 延后到其余 review issue 完成后的开发前 contract closure review；确定性承诺仍缺 canonical 算法（BR-063）。通用 shell 的 OS enforcement 已按 BR-050 移出 MVP，并成为后续启用 `bash` 的硬 gate。

这些都不动摇模块边界，返工风险局限在个别文件的接口形状。BR-036、BR-047 和 BR-048 的前置设计均已完成，其余多为文档级定稿。

## 高风险

### BR-047：Session 内并发/借用模型未定义，按现有草图 run 飞行中的命令投递会死锁

状态：Resolved

处理记录：新增 [ADR 0021](../adr/0021-session-runtime-separates-actor-control-from-run-execution.md)，明确每个 loaded session 由一个持续运行的 `SessionRuntime` actor 持有权威 mutable state，`SessionRuntimeFactory::spawn(...)` 返回显式可克隆 `SessionRuntimeHandle`；每次公开启动的 run 由短期 `RunTask` 持有 `Driver` 和 owned `SessionDriverHost`，Driver 私有持有当前 Rig `AgentRun` segment。生产 host 不再借用 `&mut Tools` / queues / `CurrentRun`，而是持有 run-only `ToolBatchInvoker`、`ModelGateway` handle、cancellation、progress sink 和私有 `RunLink`；actor 保留 tool admin/approval capability。外部 command 与 run safe-point/tool-round/terminal effect 在 actor 处线性化；approval wait 不得持有阻止 decision path 的锁；abort 与 commit 继续按 ADR 0019 的 commit-admission 规则裁决。

验证结果：`DecideToolApproval`、`AbortRun`、Steer/FollowUp/NextTurn admission、`ClearQueue`、pending `Compact`、snapshot 和 shutdown 在 active `RunTask` 等待 model/tool/approval 时仍可由 actor 处理；旧 `SessionDriverHost<'a>` mutable-borrow 草图已从 `session-runtime.md`、`driver.md` 和 ADR 0011 移除，并在事件 Test Matrix 中增加 actor responsiveness、approval wakeup、safe-point transaction、run identity fencing 和 control/progress ordering 场景。

原问题：`session-runtime.md` 的旧 `SessionDriverHost` 草图持有 `tools: &mut Tools`、`queues: &mut QueueState`、`current_run: &mut Option<CurrentRun>`，其独占借用生命周期覆盖整个 `drive_run().await`。同时文档要求多个命令在 run 飞行中触碰这些被独占借用的状态：`DecideToolApproval` 在 `Turn + WaitingApproval` 时合法、`Steer` 在 run 中入 steering queue、`Compact` / `SetModel` 在 run 中存 pending action。`session-manager.md` 当时只提到"同一 session 的 actor ordering"和 `SessionRuntimeFactory::spawn -> SessionRuntimeHandle`，没有定义 actor 循环与 `drive_run` 的关系。若 actor 循环内联 `await drive_run`，mailbox 在整个 run 期间不被消费，形成三方死锁：`ToolApprovalBroker` 等 decision → decision 命令等 actor 循环消费 mailbox → actor 循环等 `invoke_tool_batch` 返回。

原证据：

- `docs/modules/session-runtime.md`：`SessionDriverHost` 构造代码持有 `&mut self.tools` / `&mut self.queues` / `&mut self.current_run`，随后 `driver.drive_run(request, &mut host, cancel).await`。
- `docs/modules/session-runtime.md`：Session Phase 表允许 `DecideToolApproval` 只在 `Turn + WaitingApproval` 合法；队列语义要求 active run 期间入 steering/follow-up queue。
- `docs/adr/0011-tools-are-session-scoped-subsystem.md`：以 borrow-checker 动机论证 `Tools` 内嵌，但未回答 run 在飞时 decision/steer 命令如何到达 broker/queue。

原风险：这是"不先补设计就无法写出第一版 `SessionRuntime`"的缺口。approval、steer、running-compact 三条产品必需路径全部依赖它；每种解法（broker 内置 channel、host 持 sender 而非 `&mut`、actor 在 step 边界交错处理 mailbox）都会改动 ADR 0011 画下的借用草图。

决策结果：采用显式 `SessionRuntimeHandle` + per-session actor + run-scoped `RunTask`。`SessionManager` 只管理 handle/lifecycle，不处理单 session phase/queue/approval 业务；`RunTask` 只推进一次 run，并通过比 Handle 更窄的 `RunLink` 请求 owner actor 执行 safe-point、stable commit 和 terminal arbitration。未采用全局/整对象 mutex、只修 approval channel 或 actor 内联等待 `drive_run()`。

### BR-048：`project_model_call` 的 receiver 在三处文档互相矛盾，Driver 拿不到方法接收者

状态：Resolved

处理记录：采用以 `PromptCallProfile` 和 call-time lanes 为输入的纯 `prompt::project_model_call(ModelCallProjectionInput { profile, ... })`。`PromptTurn` 只负责 pin captured resources、展开 resource-backed `PromptIntent` 并提供原子 profile；Driver 从 `DriverTurnInput` 持有 owned/Arc-backed profile，在 safe point 后整体替换 active profile，再调用 Prompt projection。ADR 0017 已补充 receiver 勘误，ADR 0013 明确该调用不会把 `PromptTurn` 或 resources 扩大进 Driver seam。

验证结果：`prompt.md` facade、`driver.md` CallModel flow、`session-runtime.md` run flow、`model-gateway.md`、`runtime-hooks.md`、`resource-manager.md`、模块总览和 glossary 已统一；当前权威文档中不再存在 `PromptTurn.project_model_call(...)` 或 `Prompt::project_model_call(...)`。projection interface 继续使用 MiniCore-owned provider-neutral types，不提前冻结 BR-049 延后到 Driver integration spike 才验证的 Rig 字段形状。

原问题：Prompt 的最终投影入口 `project_model_call` 在三处有三种不相容的形态。`prompt.md` facade 把它定义为 `PromptTurn::project_model_call(&self, ModelCallProjectionInput)`，profile 来自 `self.profile`；`driver.md` 的 CallModel step 写成自由函数 `prompt::project_model_call(history, prompt, DriverTurnInput.prompt, context materials)`；`session-runtime.md` 散文又说"由 `Driver` 调用 Prompt 的 final projection seam / `Prompt::project_model_call(...)`"。而 `driver.md` 与 ADR 0013 明令 `DriverTurnInput` 不得携带 `PromptTurn`。三者合起来无解：Driver 是调用者，却拿不到 `PromptTurn` 这个 receiver。

原证据：

- `docs/modules/prompt.md`：facade 定义 `PromptTurn::project_model_call(&self, ...)`。
- `docs/modules/driver.md`：CallModel step 描述为自由函数调用 + `DriverTurnInput` 不含 `PromptTurn` 的禁令。
- `docs/modules/session-runtime.md`：运行流程第 7 步 "Driver ... 调用 `Prompt::project_model_call(...)`"。

原风险：唯一一处"不改文档就无法写代码"的硬矛盾，同时阻塞 `prompt/projection.rs` 和 `driver.rs`。

决策结果：选择自由函数而不是 `PromptCallProfile` 方法，因为 profile 是 projection 的原子静态 baseline，但 durable/current/context/output-contract lanes 是同级 call-time 输入；算法 owner 仍是无状态 Prompt 模块。未选择把 `PromptTurn` 放进 `DriverTurnInput`，也未增加 `DriverHost` / `RunLink` projection callback，避免资源 seam 扩大、host-local mirror/stale lease 和无必要 mailbox round trip。

### BR-049：Rig 版本与字段级映射需要在真实 Driver spike 中定型

状态：Deferred

处理记录：进一步核对 rig-core 0.40.0 源码及 Pi、Codex、OpenClaw、Attractor 的公开实现/契约后，确认 Steer 的通用语义是“当前 assistant turn 的模型输出及完整工具批次结束后、下一次 LLM 调用前交付”，FollowUp 则在当前 work 原本将结束时交付。MiniCore 不要求在 provider streaming、模型请求或工具执行中途修改 Rig 状态。

Rig `AgentRun::tool_results(...)` 后已经到达下一次模型调用前的协议安全点；若当前 assistant turn 没有工具并使 Rig 即将 `Done`，`before_run_finish` 仍可先检查 steering queue。Rig 0.40.0 没有公开的 mid-run history append 方法，但这不构成架构障碍：Driver 可以把一次公开的 MiniCore run 实现为同一 `RunId` 下一个或多个顺序 Rig `AgentRun` segment，在已 committed 的 Steer 安全点使用旧 segment 的 `full_history()` 和 steering message 创建新 segment。segment rollover 是 Driver 私有实现细节；run-level usage、turn/重试预算、cancellation 和事件关联必须在 Driver 外层连续累计。FollowUp 继续遵循 MiniCore 现有语义，在当前 work chain 完成后启动新的公开 run。

验证结论：当前没有发现阻止 MiniCore 采用 Rig sans-IO Agent loop 的架构级障碍；`NextModelCallPlan.persistent_messages` 可以通过 segment rollover 落地，不需要 fork Rig 或要求上游提供 token/tool 执行中的即时注入。原先“steering 可能在 spike 中证伪并连锁推翻接口”的风险解除。

延期事项：真实 Driver integration spike 开始时，按 ADR 0014 的“spine 先行”顺序完成并固化 Rig 映射矩阵：`MessageRecord ⇄ rig::Message/AssistantContent/UserContent`、`ModelCallResult → ModelTurn`、`TokenUsage ⇄ rig::Usage`、`ModelTurnOutcome::NeedsResolution`、`PendingToolCall.preresolved_result`、effective executable/allowed tool names，以及 Steer segment rollover。创建 `Cargo.toml` 时精确 pin 经 spike 验证的 rig-core 版本并提交 lockfile；在此之前继续冻结无测试支撑的 Rig 字段级扩张。

重新打开条件：segment rollover 无法保持协议 history 或 run-level budget/usage 语义；Rig 类型无法通过 MiniCore-owned provider-neutral 类型无损映射；同版本、同 host 生命周期内的 suspend/resume 无法满足要求；或 Rig 升级产生无法局限在 `driver/rig.rs` / `model_gateway/rig.rs` 的接口变化。

### BR-050：通用 shell 的 OS 级 sandbox enforcement 延后实现

状态：Deferred

处理记录：BR-037 已为 `ToolSandboxView` 补齐 path canonicalization、symlink/父目录校验、denied roots 优先级和 verdict，这些规则可以为 MiniCore 进程内实现的 `read` / `grep` / `find` / `ls` 提供可测试的路径授权，并允许 `write` / `edit` / `apply-patch` 在安全文件打开、路径重校验、审批、abort 和 mutation queue 完成后独立启用。它们不需要先启动任意子进程。

`bash` 的风险不同：入口命令、cwd、环境变量和审批检查无法约束子进程及其后代实际访问的文件、网络、凭据和其他进程。命令名 allowlist 可以被 shell、脚本解释器或子进程绕过；hostname allowlist 若没有强制代理和 direct-network deny 也不能形成可靠保证。UI approval 只表达用户意愿，不能替代 OS enforcement。

决策结果：MiniCore MVP 不启用 `bash`，也不承诺通用子进程的 OS 级隔离。`ToolSandboxView` 明确区分进程内 path authorization、请求的 shell/network policy 和 effective enforcement capability。请求 `Sandboxed`、`DenyAll` 或 proxy host allowlist 时，当前 OS-native/external backend 的 capability 必须满足策略，否则 fail closed 并返回 typed `SandboxUnavailable` / policy-denied tool result，不能静默以普通用户权限执行。显式 `FullAccessWithApproval` 是无隔离的高权限执行模式，必须逐次清楚展示风险，不能称为 sandbox，也不能由普通 remembered grant 隐式获得。

平台实现延后到真正启用 shell 时选择：Linux 可评估 bubblewrap/Landlock/seccomp，macOS 可评估 Seatbelt，Windows 可评估 restricted token/ACL/专用 sandbox user/firewall 或 WSL2/external sandbox。具体 backend 不提前冻结进 MiniCore protocol；实现状态和 capabilities 由 Tools 内部 adapter 探测并投影安全摘要。

重新打开条件：开始实现 `bash` 或任何可启动任意子进程的 external executor；产品要求无审批自动执行 shell；需要把 workspace-only filesystem、network deny 或 host allowlist 声明为对子进程的强保证；或已有 OS-native/external backend 可进入 conformance test。关闭前必须验证限制传播到完整进程树、策略不足 fail closed、sandbox denial 可观察，以及无隔离模式不会被 UI/文档标记为 sandbox。

### BR-051：跨模块 seam 类型在开发前统一闭合

状态：Deferred

问题：多个模块文档以类型为契约展开，但这些类型全仓无定义，实现时各模块会自行发明形状。最关键的是 `ToolBatchResult`——它被引用约 20 次，同时是 commit 事务、Rig 回填和事件归约三方共享的值，形状不定则三处对接无从写起。

证据（被 struct 字段/签名引用但无定义）：

- 执行层：`ToolBatchResult`、`DriveResultSummary`、`TurnCheckpoint`、`FinishCheckpoint`、`DriveLimits`、`DriverError`、`ToolSubsystemError`、`ToolApprovalRequest`、`PreparedToolInvocation`、`ApprovalGrantMatch`、`ModelStreamSink` / `ToolUpdateSink` 方法集。
- 资源层：`CwdResourceLayer`、`ResolvedCwdResourceView`、`CompactionFileOps`、`CompactionDetails`、`SkillLoadInputs` / `SkillLoadReport`、`PromptTemplateLoadInputs`、`ContextMaterialKey` / `ContextSource`。

风险：这些空洞不改变当前已确定的模块 ownership、调用方向和生命周期边界，但若带入实现阶段，各模块会分别发明字段、错误和行为约束，导致 conformance test 无法围绕同一契约编写；其中 `ToolBatchResult` 会直接阻塞 tool 纵切。

决策结果：先完成当前 review 清单中的其余 issue，再在任何生产代码纵切开始前执行一次全仓 interface contract closure review。该复核按 protocol/command、driver/model、tools、resources、compaction/dynamic-context 分组，并与 BR-052 的 command payload / reachability 复核联合执行；不要求现在一次性冻结所有未来字段。每个被引用类型必须明确归属 owner，并得到“定义、删除/合并、或带明确阶段转入后续 issue”三种结果之一。

开发前验收至少覆盖：公开字段和方法集、生产者/消费者、排序与唯一性等数据不变量、cancel/partial-result 语义、typed error taxonomy、持久化/事件投影边界，以及跨模块类型不得泄漏 Rig/provider 私有类型。`ToolBatchResult` 需同时闭合 `invoke_tool_batch` 返回、`ToolRound` 原子 commit、公开事件归约和 Rig tool-result 回填；sink 类型需明确 progress/control lane、backpressure、ack 和关闭行为。

重新打开条件：其余 review issue 全部处理完并准备进入开发；开始规划任何 protocol、text-only、resource 或 tool 纵切；或实现中首次需要上述任一未定义类型。关闭前应通过全仓引用清单确认不存在无 owner 的跨模块类型，并为进入首批实现的契约给出可直接编写 conformance test 的定义。

### BR-052：MVP command payload 与可达性在开发前统一闭合

状态：Deferred

原始问题：三个问题集中在 MVP `AgentCommand` 表面：(a) `SubmitPrompt.input: UserInput`、`SetStreamOptions.options: StreamOptions`、`SetThinkingLevel.level: ThinkingLevel` 三个载荷类型出现在 MVP 命令签名里，但全仓无任何定义；(b) `ResumeRun { session_id, resume_id }` 列在 MVP 命令中，但 MVP 没有任何能产生 suspension 的源——`SuspendReason::UserSuspendedAtSafePoint` 没有对应 suspend 命令，能触发 pause 的 `BeforeNextModelCall` hook 是后期能力，因此 `ResumeRun` 是 MVP 表里的死表面；(c) `NewSession { workspace_id }` 没有 cwd 参数，新会话的固定 cwd 如何确定在被引用文档中不可知。

当前状态与证据：

- `NewSession` cwd 来源已由 [ADR 0022](../adr/0022-workspace-is-single-instance-thin-boundary.md) 关闭：公开命令为 `NewSession { workspace_id, cwd: Option<PathBuf> }`，`None` 使用 workspace root，显式 cwd canonicalize 后必须位于 root 内。
- `docs/modules/agent-runtime-protocol.md`：MVP `AgentCommand` 仍含 `SubmitPrompt { input: UserInput, ... }`、`SetStreamOptions { options: StreamOptions }`、`SetThinkingLevel { level: ThinkingLevel }` 和 `ResumeRun { session_id, resume_id }`。
- 全仓仍无 `UserInput` / `StreamOptions` / `ThinkingLevel` 的字段定义；MVP 仍无 suspension 产生源。

风险：这些缺口不改变 `AgentCommand -> AgentRuntime -> SessionRuntime` 的调用方向，但若带入开发，adapter、protocol、session state、持久化和 ModelGateway 会分别发明 payload 与生命周期语义；不可达的 `ResumeRun` 还会把 post-MVP suspend/resume 状态机误列为 MVP 验收范围。

决策结果：先完成其余 review issue，再与 BR-051 的全仓 interface contract closure review 联合处理。`UserInput`、`StreamOptions`、`ThinkingLevel` 作为 `agent_runtime_protocol` owned、provider-neutral 的公开 payload 定型；复核必须同时明确 wire 形状、默认值、持久化/恢复边界、future-run 或 safe-point 生效规则、capability clamp 和 provider mapping owner。`UserInput` 的完成条件与 BR-055 / ADR 0023 的 `compose_user_message(...) -> CanonicalUserMessage` 规则联动，确保 durable input 与模型可见输入来自同一 committed seed。

`ResumeRun` 纳入 MVP command reachability review，默认方案是移入“后续命令”，并把对应 phase guard、suspend/resume event 和 Driver resume seam 明确标为 post-MVP reserved；不为了保留该命令而凭空增加 `SuspendRun`。每个保留在 MVP 的公开 command 必须存在至少一条文档内可构造的合法状态路径。

重新打开条件：其余 review issue 全部处理完并准备进入开发；开始实现 protocol/adapter、session settings、prompt input 或 run lifecycle；或首次需要上述任一 payload。关闭前必须完成三类 payload 契约、`ResumeRun` 的保留/移出决定及全协议 MVP command reachability 检查，并确认 ADR 0022 的 cwd 语义在 protocol、SessionManager 和持久化 metadata 中一致。

## 中风险

### BR-053：Prompt 预检超限到 SessionRuntime compaction 的信号路径未定义

状态：Resolved

问题：`prompt.md` 校验条目说"估算 token 超预检上限时返回结构化 outcome，由 `SessionRuntime` 编排 compaction"。但该校验发生在 Driver 的 CallModel step 内，而 `compaction.md` 的 overflow recovery 只挂在 `DriveResult::Failed { error: ContextOverflow }`（provider 侧错误）上。Prompt 侧预检失败如何变成 `SessionRuntime` 可编排的信号——映射为同一 `ContextOverflow` kind？新增 `DriveResult` 变体？——没有任何文档回答。

证据：

- `docs/modules/prompt.md`：校验清单中的 preflight 超限 outcome。
- `docs/modules/compaction.md`：overflow recovery 仅由 `DriveResult::Failed` 的 provider error 触发。

风险：首个大 prompt（巨型 skill 正文或粘贴）会直接踩中这条未定义路径。

决策结果：`prompt::project_model_call(...)` 的限制检查定性为每次 CallModel 前的最终 projection validation，而非 `SubmitPrompt` admission。它返回结构化 `PromptError::ContextLimitExceeded`；`Driver` 将其与 provider `ModelCallErrorKind::ContextOverflow` 归一为 `DriveResult::Failed { DriverError::ContextLimitExceeded { source, ... } }`，但保留 `PromptProjection` / `Provider` 来源、usage 和 diagnostics 差异，不新增平行 `DriveResult` terminal variant，也不在本地超限后调用 provider。

`SessionRuntime` 在当前 run terminal handling 后拥有唯一恢复路径：整个 work chain 最多执行一次 `reason = Overflow` compaction，随后从 committed、重建后的 context 使用新 `RunId` 和 `DriveEntry::Continue { reason: ContextOverflowRecovery }` 继续；再次超限、无可压缩历史或 protected current input 本身过大时 fail closed。另在最终 UserInput commit 后、分配 RunId 前增加 best-effort threshold gate，以提前处理常见大上下文；它只复验一次，不替代 Driver 对 tool result / Steer / transient context 后续 CallModel 的最终校验。正常 completed run 的 post-run threshold compaction仍不重跑已完成回答。

压缩执行方法由当前模型 `CompactionCapabilities`、trigger 和用户 preference 解析。MVP baseline 保持 `SummaryModel` 和 portable summary；后期 `ProviderNative` 可用于 GPT 类专用 compact endpoint。若端点返回需后续原样回传的加密 model-bound artifact，它只持久化一次并由同 provider adapter 注入后续请求，不作为普通 message/event；模型不兼容时从保留的原始 durable entries 重新压缩。artifact envelope、compatibility key 和 provider payload 细节延后到 ProviderNative integration design。

关闭理由：Prompt → Driver → SessionRuntime 的 typed signal、run terminal、单次 recovery budget、current-input protection、pre/post-run 场景和 provider-native 扩展边界已对齐；剩余字段形状纳入 BR-051 的开发前 interface contract closure review，不再构成独立架构空洞。

### BR-054：五个 prompt 输入 view 无 owner，`EnvironmentPromptView` 隐含 I/O 职责

状态：Resolved

处理记录：已按确认后的 ResourceManager / Prompt 架构重写 `resource-manager.md`、`prompt.md`、`session-runtime.md`、ADR 0010、ADR 0017 和总览文档。当前结论是：`PromptResourceView` 是所有非工具稳定 Prompt 输入的唯一 seam，暴露 materials、behavior、model、environment、policy、skill/template 和 fingerprint；`ToolPromptView` 保持 Tools 独立。Prompt 入口收敛为 `prompt::assemble_turn(PromptTurnSpec { resources: PromptResourceView, tools: ToolPromptView }) -> PromptTurn`。旧的多 owner prompt view 方案、turn prompt wrapper 方案和 VCS-in-environment 方案均被 superseded。

现行 `TurnResourceSnapshot` 直接包含 `session_id`、`user_turn_id`（不是 `RunId`）、`Arc<CwdResourceSnapshot>`、behavior、model、environment、policy 和 fingerprint。MVP `environment` 只包含 workspace root、fixed cwd、platform、date/time/timezone 和 interaction capabilities，不包含 VCS I/O。ResourceManager 只冻结 owner-produced prompt-safe projections，不反向读取 `ModelState`、`Tools`、`AuthStore` 或 settings owner handles。

原问题：`TurnPromptInputs` 的 7 个 view 里，`ToolPromptView`（`tools.md`）和 `PromptResourceView`（`resource-manager.md`）有 owner，而 `ProductPromptView` / `AgentPromptView` / `EnvironmentPromptView` / `PolicyPromptView` / `ModelPromptView` 五个类型全仓库只在 `prompt.md` 出现一次，没有任何模块定义谁构造、字段是什么、何时刷新。尤其 `EnvironmentPromptView` 含"VCS 摘要"，这需要 I/O，而 Prompt 禁止 I/O，`session-runtime.md` 也没认领这项采集。

原证据：

- `docs/modules/prompt.md` 曾列出 `begin_turn(TurnPromptInputs { ... })` 和七个 view，五个无外部 owner。

原风险：五个类型会被各模块自行发明；`EnvironmentPromptView` 的 VCS 采集是隐藏的 I/O owner 缺口。

关闭理由：现行权威文档不再定义五个无 owner view，也不再允许 Prompt 或 ResourceManager 执行 VCS I/O。后续 safe-point profile mutation 必须通过 step snapshot 或明确 step override 原子替换 profile 与 future tool invoker。

### BR-055：`ResolvedPromptInput.parts` 与 `messages` 的折叠关系未定义

状态：Resolved / Closed

处理记录：新增 [ADR 0023](../adr/0023-driver-starts-from-one-committed-conversation-seed.md)，接受 Transcript-First 设计并固定 public seam 命名：`ResourceManager.capture_turn_resources`、`Tools.capture_turn_tools -> TurnToolProfile`、`Prompt.prepare_message_turn -> PreparedMessageTurn -> ModelContextProfile`、`compose_user_message -> CanonicalUserMessage`、`assemble_model_context -> AssembledModelContext`、`ConversationSeed`、`CommittedConversationState/Delta`、`Driver.drive_conversation`、`ModelGateway.generate_model_turn`、`execute_and_commit_tool_round`、`commit_pending_messages` 和 `commit_final_assistant_message`。

关闭理由：MVP current input 不再暴露 `parts + messages` 双 source of truth；`compose_user_message(...)` 是 canonical lowering seam，一个 prompt-like intent 产出一条 `CanonicalUserMessage`。SessionRuntime 在 session open/recovery 时从 durable storage 建立 `CommittedConversationState`，稳态 commit 后只应用可信 delta，并从该热视图构造 `ConversationSeed`，其中 current input 恰好出现一次；Driver/Rig 只能从 committed seed/delta 推进；Prompt 是 AgentRun 与 CompactionSummary 的唯一 `AssembledModelContext` 组装 seam；ModelGateway 只编码/调用 provider，不判断 session message visibility。BR-049 Rig spike 仍 Deferred，但只验证 private adapter mapping，不再反向决定 MiniCore public seam。

问题：`PromptTurn.resolve_intent()` 返回 `ResolvedPromptInput`，其 `parts` 如何折叠成 `MessageRecord`、折叠规则是什么，没有定义。这直接决定"持久化的 `UserInput` batch 内容 == 模型可见输入"这一不变量能否成立，而 `skills.md` 的 commit 流程依赖它。

证据：

- `docs/modules/prompt.md`：`ResolvedPromptInput { parts, ... }`。
- `docs/modules/skills.md`：运行流程第 7/8 步的 `UserInput` commit 依赖 parts→message 的确定折叠。

风险：折叠规则不定，持久化内容与模型可见输入可能分叉。

处理结果：旧“`parts -> Vec<MessageRecord>`”问题被 Transcript-First 命名取代；历史调研正文保留在 [background-session-runtime-progress](background-session-runtime-progress.md) 中，但本 issue 不再 Open。

### BR-056：retry attempt 跨 compaction 的配对未定义

状态：Open

问题：`agent-runtime-events.md` 的转换矩阵允许 `RetryBackoff → Compaction`（retry 前先做 overflow recovery）和 `Compaction → RetryBackoff`，但 retry 配对规则（每个 `retry_auto_started { attempt=N }` 必须由同 attempt 的 `retry_auto_finished` 关闭）只定义了线性链。当 attempt=N 的 `retry_auto_started` 已发出、随后插入的 overflow compaction 失败或被 abort 时，`retry_auto_finished` 发不发、status 是什么，无处可查。这是唯一一处配对不变量可能开天窗的地方。

证据：

- `docs/modules/agent-runtime-events.md`：转换矩阵允许 retry 与 compaction 互切，但 §15 retry 配对只覆盖线性链。

风险：conformance test 无法为该组合写断言；实现可能产生悬空的 `retry_auto_started`。

待处理方向：在事件文档补 retry-attempt 跨 compaction 的配对规则，并加入 Test Matrix。

### BR-057：`reload_runtime` / `recompose_cwd` 在 MVP 实际不可达，且 recompose 后 revision 不通知 UI

状态：Resolved

处理记录：最终决策是不补 runtime 级 reload。runtime 与 UI/`AgentRuntime` 生命周期绑定，`RuntimeResourceSnapshot` 在 `OpenWorkspace` 初始化一次，MVP 不使用递增 runtime 级 revision；全局资源变化通过重建 `AgentRuntime` 生效。cwd reload 保留，`ResourceSnapshotStore::replace_cwd(...)` 是唯一资源 current-pointer 替换线性化点。旧 runtime reload、runtime current-pointer 替换、runtime-version drift 和 lazy cwd recompose 语义均从权威文档删除。

原问题：`resource-manager.md` 的 trait 定义 `reload_runtime()`，`recompose_cwd` 整套懒重组机制围绕 runtime revision 变化设计。但公开协议只有 cwd 级 `ReloadResources { workspace_id, cwd }`，MVP 中没有任何路径能改变 runtime revision，整条 lazy recompose 分支实际不可达。此外，懒 recompose 经 `replace_cwd` 发布新 `CwdResourceSnapshot.revision`，但 `resources_changed` 只在显式 reload 流程定义，UI 对 recompose 后的 revision 永远不知情。

原证据：

- `docs/modules/resource-manager.md` 曾包含 `reload_runtime()`、`recompose_cwd`、`ResourceReloadResult { scope, runtime_revision }`。
- `docs/modules/agent-runtime-protocol.md` 只有 cwd 级 `ReloadResources { workspace_id, cwd }`，无 runtime 级 reload 命令。

原风险：死路径会被当作 MVP 验收范围实现；recompose 后 UI 资源摘要与实际 revision 漂移。

关闭理由：现行 MVP 只有 cwd reload 会发布新 `CwdResourceSnapshot` 和 `resources_changed`；不存在 recompose 后是否通知 UI 的状态分支。

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

状态：Partially Resolved

处理记录：ADR 0022 集成时已统一 catalog list seam：协议 `scope/filter/cursor/limit` 一一映射到 `SessionManager.list(SessionListRequest)`，scope 不再塞入 `SessionListFilter`；`Recent` 明确为显式跨 workspace 全局查询。剩余问题只限旧 review/示例中的 `open_handle` / `create_handle` 等方法名漂移。

原风险：同名类型两种形状会导致 workspace catalog 过滤实现分叉。

待处理方向：后续机械统一遗留方法名，不再改 list 请求形状。

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

状态：Resolved

处理记录：最终决策是去掉 turn capture 兜底读盘和 stale-runtime 分支，而不是只给旧不变量补限定语。`OpenWorkspace` 保证 runtime snapshot 初始化一次；`OpenSession` / `NewSession` 保证目标 cwd snapshot 存在；`capture_turn(...)` 在稳态只读取 current cwd pointer，并冻结传入 typed projections。capture 与 `replace_cwd` 按读取 current pointer 的时点线性化。

原问题：`resource-manager.md` 的不变量清单写"`capture_turn(...)` 只读取当前指针；不读磁盘、不自动 reload"，但同文档 `capture_turn` 实现在 miss 时调用 `ensure_cwd_snapshot`（首次 turn 会读盘）、在 revision 漂移时调用 `recompose_cwd`（发布新指针）。ADR 0010 的"三道防线"说明意图是"steady-state 不读盘"，但不变量的字面表述会被直接写成错误测试。runtime revision 陈旧检查的 owner 也在两处重复声明（`capture_turn` 伪代码内 vs `ensure_cwd_snapshot` 内部）。

原证据：

- `docs/modules/resource-manager.md` 曾同时描述不变量、`capture_turn` 伪代码与 `ensure_cwd_snapshot` 职责。
- `docs/adr/0010-use-per-cwd-resource-snapshots-for-multi-session-runtime.md` 曾描述三道防线。

原风险：不变量字面表述会写坏测试；职责重复是漂移温床。

关闭理由：现行权威文档中 capture 不读盘、不自动 reload、不发布新 snapshot，也不处理 runtime-version drift；缺失 cwd snapshot 是 lifecycle ensure 的错误。

### BR-063：确定性承诺缺少 canonical 算法，`baseline_tokens` 为魔数

状态：Open

问题：多处"相同输入得到相同结果"的硬承诺缺少算法定义：fingerprint 类型群（`TurnResourceFingerprint` / `PromptFingerprint` / `PromptProfileFingerprint` / `ModelInputFingerprint`）的 canonical 序列化算法未定；overlay"稳定排序"的 collation key（按 canonical path？声明顺序？）未定；`CwdResourceRevision(u64)` 的生成与持久范围未定；token 估算方法未定；`ContextUsageView.baseline_tokens = 12_000` 是魔数。

证据：

- `docs/modules/prompt.md`：fingerprint 类型群，无 canonical 算法。
- `docs/modules/resource-manager.md`：overlay 稳定排序无 collation 定义；`CwdResourceRevision` 的生成与持久范围、`TurnResourceFingerprint` canonical 输入未声明。
- `docs/modules/usage-stats.md`：`baseline_tokens = 12_000`。

风险：fingerprint 是最容易各写各的地方；魔数无来源说明。

待处理方向：钉死 fingerprint canonical 序列化、collation key、`CwdResourceRevision` 生成/持久范围、token 估算方法；给 `baseline_tokens` 加来源说明或设为可配置。

### BR-064：Windows 平台细节与 JSONL 并发未声明

状态：Open

问题：`skills.md` 要求 symlink canonical path 去重、遵守 `.gitignore`；`canonicalize` 在 Windows 产生 `\\?\` 前缀，与 `ResourceKey` / `location` 的展示和比较规则需要统一。同一 JSONL 被两个进程打开的行为未提及。本仓库本身在 Windows/OneDrive 路径下开发，这些不是假想问题。

证据：

- `docs/modules/skills.md`：symlink 去重、`.gitignore` 要求。
- `docs/modules/session-manager.md`：JSONL 读写并发可见性只对 InMemory 写了必测项，未提跨进程打开。

风险：Windows canonical 前缀会污染 `ResourceKey` 比较；嵌入式场景下 JSONL 双进程行为未定。

待处理方向：补一句 JSONL 单写入者/跨进程打开的声明。workspace root 的 canonical identity 已由 [ADR 0022](../adr/0022-workspace-is-single-instance-thin-boundary.md) D2 定型；`ResourceKey` / skill 展示比较规则和 JSONL 并发仍 Open。

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
