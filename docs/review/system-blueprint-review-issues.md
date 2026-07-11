# System Blueprint Review Issues

日期：2026-07-06

来源：本轮由多个 subagent 对 MiniCore 系统蓝图进行只读审阅后合并整理。

审阅时范围：`README.md`、`CONTEXT.md`、`docs/architecture.md`、当时存在但后续已删除的 `docs/implementation-roadmap.md`、`docs/modules/*.md`、`docs/adr/*.md`。路线图相关证据保留为历史审阅记录，不再是现行行为权威。

约定：本文只记录待处理问题，不代表已经决定修改方案。后续逐条处理时，应回到对应 source of truth 文档中做设计取舍。

## 状态说明

- `Open`：待分析或待处理。
- `Resolved`：已经处理并验证。
- `Partially Resolved`：核心方向已收敛，但仍有后续场景或配套问题需要处理。
- `Won't Fix`：确认接受现状或暂不处理。

## 高风险

### BR-001：Snapshot 作用域尚未收敛，但已作为公开接口固化

状态：Resolved

处理记录：已决策为 MVP 使用 workspace/runtime-scoped `snapshot() -> RuntimeSnapshot`，不再使用 `snapshot(session_id)`；`RuntimeSnapshot` 不单独持久化，打开 workspace 后默认 `active_session = None`。会话清单改由 `SessionIndex` / `ListSessions` 提供。

问题：`snapshot(session_id)` 已经写入公开 runtime interface，但路线图同时指出 Snapshot scope 尚未决定。当前 Snapshot 又包含 workspace/session catalog、focused session、resources、slash commands、session state 等混合范围字段。

证据：

- `README.md`：`AgentRuntime.snapshot(session_id) -> agent_runtime_protocol::Snapshot`
- `docs/modules/agent-runtime.md`：公开 trait 使用 `snapshot(&self, session_id: SessionId)`
- `docs/modules/agent-runtime-protocol.md`：公开 trait 使用 `snapshot(&self, session_id: SessionId)`
- `docs/implementation-roadmap.md`：先决设计点要求先决定 `SnapshotScope`、`snapshot(workspace_id, session_id?)`，或拆分 workspace/session snapshot。
- `docs/modules/agent-runtime-protocol.md`：`Snapshot` 字段包含 workspace、focused session、sessions catalog、session view、resources、command catalog 等。

风险：早期协议、事件重连、adapter reducer 和下游接入会围绕错误粒度实现；后续拆分 Snapshot 会造成协议级返工。

待处理方向：已收敛。后续如果 GUI 多 tab、多 session detail 或大规模分页需求明确，再评估拆分 `WorkspaceSnapshot` / `SessionSnapshot`。

### BR-002：全局事件 sequence 与单 session snapshot 组合可能导致后台 session 状态丢失

状态：Resolved

处理记录：`snapshot(session_id)` 已改为 `snapshot() -> RuntimeSnapshot`，避免单 session snapshot 携带全局水位。进一步明确 MVP 运行时生命周期约束：UI host 和 `AgentRuntime` 在同一个程序上下文、同一个生命周期中运行，不支持 UI adapter 失败/断线但 runtime daemon 继续后台运行再被重连的模式。因此 `RuntimeSnapshot.last_event_sequence` 的 reconnect 语义只覆盖同一 host 生命周期内的初始化、late subscribe、reducer/subscriber 重建和 sequence gap recovery；不承诺恢复非 active/background session 在 UI 断线期间的完整可视状态。若未来引入独立 runtime server、多窗口共享 runtime 或 daemon 模式，再重新打开该问题，设计 all-loaded-session snapshot 或 scoped event cursor。

问题：原风险成立于 runtime 独立存活、UI 可断线重连的架构假设：事件 sequence 是 runtime 全局单调递增，Snapshot 带 `last_event_sequence`，而 snapshot 只覆盖某个 session 或 active session。多 session 同时 loaded、失焦 session 后台运行时，如果 UI 只拿某个 session 的 snapshot 并跳过 `<= last_event_sequence` 的事件，其他 session 在该水位前的状态可能不可恢复。MVP 不采用这个故障模型。

原证据：

- `docs/modules/agent-runtime-events.md`：`Snapshot.last_event_sequence` 是 UI 看到的权威状态水位。
- `docs/modules/session-manager.md`：focused session 不表示唯一 loaded session，也不表示唯一 running session；失去 focus 的 session 可以继续后台 run。
- `docs/modules/agent-runtime-protocol.md`：原 Snapshot 草案偏向单 session view，同时携带全局/工作区字段。

风险：如果未来改为独立 runtime server、多窗口共享 runtime 或 daemon 模式，重连后 pending run、tool activity、approval、queue 等跨 session 状态可能丢失或无法正确 reduce。

待处理方向：已按 MVP 生命周期约束关闭。未来只有在引入独立 runtime 生命周期时才需要重新设计：其一，让 `RuntimeSnapshot` 覆盖所有 loaded/running session 的最小恢复态；其二，让 event sequence cursor 具备 scope 语义，避免用全局水位跳过未被 snapshot 覆盖的 session 事件。

### BR-003：RuntimeServices 作用域与多 session/background run 存在冲突

状态：Resolved

处理记录：已收敛到单 UI/runtime 进程内多 session 运行模式。最新修订不再引入 `CwdServiceRegistry` / `CwdScopedServices` / service generation pinning；`AgentRuntime` 拥有 `WorkspaceServices`，其中共享 `ResourceManager` 管理 `RuntimeResourceSnapshot -> CwdResourceSnapshot -> TurnResourceSnapshot -> StepResourceSnapshot` 级联不可变资源快照。每个 `SessionRuntime` 固定一个 workspace cwd；每次 run 启动时捕获 `TurnResourceSnapshot` 并构建 `TurnState`。provider settings、auth、custom provider 和 `ModelGateway` 均为 user-global/runtime-global；focused session 只影响 UI 和默认命令路由，不作为资源或服务 scope 锚点。

问题：原文档一方面说 `RuntimeServices` 绑定有效工作区，另一方面说打开/导入/恢复到不同 cwd 的会话时必须重建这些服务；同时 `SessionManager` 允许多个 loaded session 和后台 run。旧 session 应持有旧 services、被 shutdown，还是迁移到新 services，此前不清楚。

原证据：

- `CONTEXT.md`：Runtime services 是绑定到有效工作区的后端依赖集合。
- `docs/modules/agent-runtime.md`：运行时服务绑定有效工作区；不同 cwd 会话切换时必须重新创建服务。
- `docs/modules/session-manager.md`：多 session 同时运行时，失去 focus 的 session 可以继续执行后台 run。
- `docs/implementation-roadmap.md`：先决设计点要求明确服务绑定到 workspace、cwd、focused session 还是 loaded session。

风险：如果实现偏离该约束，资源快照仍可能在后台 session 和 focused session 之间互相污染；也可能导致后台 run 被隐式中断。provider/settings/auth 不再是 cwd-scoped 风险点，因为当前产品约束为 user-global，并禁止项目级 custom provider 覆盖。

待处理方向：已处理。后续实现必须验证：打开两个不同 cwd 的 session，`ResourceManager` 为每个 cwd 保存 current `CwdResourceSnapshot`；后台 running session 继续使用 run 启动时捕获进 `TurnState` 的旧 `TurnResourceSnapshot`；focus 切换、resource reload、close/unload 不污染其他 loaded session。

### BR-004：pending tool approval 从 RuntimeSnapshot 恢复

状态：Resolved

处理记录：已决策将 active session 当前 run 的待审批工具调用投影到 `RuntimeSnapshot.active_session.current_run.pending_tool_approvals`。协议新增 `PendingToolApprovalView`，只包含 `approval_id`、`session_id`、`run_id`、`call_index`、`call_id`、tool 名称、risk、reason、preview 和创建时间等 UI-safe 字段；冻结的 `prepared_args` 仍由 `ToolApprovalBroker` 内部保存，不进入 snapshot，也不能由 UI 修改。`DecideToolApproval` 通过 `approval_id` 与 `session_id`、`run_id`、`call_id` 匹配 broker 中的 pending approval；重复或过期 decision 归约为 `ApprovalDecisionOutcome`，不能重复执行。

原问题：事件文档要求审批状态不能由 UI 私有保存，同一 host 生命周期内的订阅/状态重建后应从 snapshot 的 pending tool calls 恢复；此前 `RuntimeSnapshot` 字段列表只有 `current_run`、tools、queues 等，没有明确 pending approval 字段。

决策结果：pending approval 放在 `RunView`，字段名为 `pending_tool_approvals`。它是 tool-call waiting state 的 UI-safe projection，作用域限定为 active session 的当前 run。

风险：已通过 `RunView.pending_tool_approvals` 处理。同一 host 生命周期内的订阅/状态重建后，UI 可以从 snapshot 恢复审批弹窗，并使用对应 `approval_id` 与 `call_id` 安全发出 approval decision。

待处理方向：已处理。后续实现需验证：`tool_call_approval_requested` 后 snapshot 包含 pending approval；approve/reject、abort、run finished 或 session close 后 snapshot 不再包含该 approval；`prepared_args` 不出现在 snapshot / event / log 中。

### BR-005：`RunTerminalStatus::Paused` 与 run 单终态/暂停语义冲突

状态：Resolved

处理记录：已决策 `paused` 不再是 run terminal status。`RunTerminalStatus` 只包含 `Completed`、`Failed`、`Aborted`；可恢复暂停表达为 current run 的 `CurrentRunState::Suspended { resume_id, reason }`，并通过 `RunEvent::Suspended` / `RunEvent::Resumed` 暴露。`DriveResult` 中对应结果改为 `Suspended { reason: SuspendReason, serialized_run }`，由 `SessionRuntime` 分配 `ResumeId` 并持有 resume state。`run_finished` 仍是唯一终态事件，不存在 `run_finished { status: paused }`。

原问题：协议把 `Paused` 放入 `RunTerminalStatus`，但 Driver 把 `DriveResult::Paused` 作为未来可序列化暂停路径。Paused 到底是终态、可恢复中间态，还是 run lifecycle 的另一类状态，此前不清楚。

原证据：

- `docs/modules/agent-runtime-protocol.md`：`RunTerminalStatus { Completed, Failed, Aborted, Paused }`。
- `docs/modules/driver.md`：`DriveResult::Paused { reason, serialized_run }`；MVP 可以暂不产生 `Paused`，但不要把接口设计死成只能 completed/failed。
- `docs/adr/0003-agent-runtime-events-use-event-msg-and-lifecycle-pairs.md`：一次 run 只有一个终态事件 `run_finished { status }`。

原风险：UI reducer、session phase、run resume、持久化屏障会混淆 paused 和 terminal finished。

决策结果：paused/suspended 是可恢复 checkpoint 状态，不是终态。典型 checkpoint 包括 tool result 已产生但尚未回填给 Rig / provider、等待用户交互、external job pending、用户在 safe point 主动暂停、host shutdown checkpoint。普通 focus 切换不是暂停；MVP pending approval 可以作为 current run 的 waiting substate 表达，不必进入 suspended，除非需要挂起并等待显式 resume。

待处理方向：已处理。后续实现需验证：`run_suspended` 后 `RuntimeSnapshot.active_session.current_run.state` 为 `Suspended`；`run_resumed` 后回到 `Running` 或 `WaitingApproval`；最终仍必须且只能产生一个 `run_finished { status: Completed | Failed | Aborted }`。

### BR-006：CommandPresentation 可携带完整 Command，可能突破 presentation 边界

状态：Resolved

原问题：`CommandAction::DispatchCommand(Command)` 和 `UiInteractionSubmit::DispatchCommandTemplate` 允许 UI-visible presentation 携带完整 runtime command。但 CommandSurface 安全边界又说 `CommandPresentation` 是语义展示请求，UI 不能通过它修改 runtime state。若 hook/extension 可 patch presentation，可能间接制造高权限结构化命令。

决策结果：协议层把公开命令枚举改名为 `agent_runtime_protocol::AgentCommand`，并移除 UI-visible presentation 携带完整 command/template 的设计。UI 选择 command catalog item 时只能提交 `ExecuteCommandText` 或 `ExecuteCatalogCommand`；runtime 必须基于当前 session context 重新 materialize catalog，并执行 `resolve_for_execution` 校验 selection、bindings、args、phase、trust、capability 和 handler binding。Command output action 如未来保留，也只能使用 runtime-owned opaque action id。

后续实现需验证：catalog/output/interaction metadata 中不能序列化完整 `AgentCommand`、internal handler key、resource body 或 credentials；hook patch 后必须经过 UI-safe redaction。

### BR-007：公开协议暴露过宽的内部 mutation command

状态：Resolved

原问题：MVP `Command` 包含 `AppendMessage`、`SetTools` 等高权限 mutation 能力，但架构边界要求 UI 不拥有 session messages authority、tools authority。这些命令更像 internal/privileged API。

决策结果：公开协议枚举改名为 `AgentCommand`，只表达下游 UI/CLI 可提交的用户意图；`AppendMessage`、工具定义替换、会话历史改写等能力移入 `InternalAgentCommand` / 内部 API，不进入公开协议、快照、事件或 command catalog。公开工具相关命令只允许 `SetActiveTools { tool_names }` 这类基于 registry allowlist 的安全意图。

后续实现需验证：公开 transport / SDK 不能构造内部 mutation；测试 harness 如需内部能力必须走 crate-private API 或显式 privileged feature。

### BR-008：Driver seam 可能过宽，`TurnState` 把资源状态泄漏给 Driver

状态：Resolved

原问题：`DriveRequest` 携带完整 `TurnState`，而 `TurnState` 包含 captured `TurnResourceSnapshot`、tool state、context usage 等会话/资源编排信息。Driver 文档又要求 Driver 不拥有 resource loading、skill expansion、prompt template expansion、system prompt building、active tools 等职责。

原证据：

- `docs/modules/driver.md`：`DriveRequest { turn_state: TurnState }`。
- `docs/modules/session-runtime.md`：`TurnState` 包含 `resources: Arc<TurnResourceSnapshot>`。
- `docs/modules/driver.md`：Driver 不负责资源加载、skill/template 展开、system prompt 构建、active tools 管理。

决策结果：`TurnState` 保留为 `SessionRuntime` 内部 run snapshot，不再作为 `DriveRequest` 输入。Prompt 设计进一步收敛后，`DriverTurnInput` 只携带 `model`、原子 `PromptCallProfile`、`thinking_level` 和 `stream_options`；system prompt 与 active tool schemas 不再作为可独立 patch 的字段。`TurnResourceSnapshot`、`PromptTurn`、resource revision、context usage、queue/storage 和工具治理状态都不进入 `DriverTurnInput`。turn resources 留在 per-run `SessionDriverHost` 中，用于工具上下文、active PromptTurn rebuild 和 future `StepResourceSnapshot` parent。

后续实现需验证：`DriveRequest` 不能引用 `TurnState` 或 `TurnResourceSnapshot`；`Driver` 单测只需构造 `DriverTurnInput` 和 fake `DriverHost`；工具执行需要资源/cwd 时必须经 `SessionDriverHost -> Tools::invoke_batch(...)`。

## 中风险

### BR-009：ModelGateway 实现顺序可能导致阶段 5 与阶段 9 返工

状态：Resolved

原问题：阶段 5 已接入真实 Driver、`model_gateway.rs` 和 `model_gateway/rig.rs`，但阶段 9 才收拢 provider/model/auth 生命周期、custom provider、usage 归一化和 context usage。ModelGateway 文档又把 provider/model/auth/fallback/usage/error/cancellation 都定义为调用边界职责。

原证据：

- `docs/implementation-roadmap.md`：阶段 5 Text-only Driver integration 包含 `model_gateway.rs`、`model_gateway/rig.rs`。
- `docs/implementation-roadmap.md`：阶段 9 才做 ModelGateway 和 UsageStats 的完整收拢。
- `docs/modules/model-gateway.md`：ModelGateway 负责 provider/model 解析、凭据解析、custom base URL、fallback、usage、错误分类。

决策结果：实现顺序改为先落 `ModelGateway` 最小稳定 spine，再进行 text-only `Driver` integration。阶段 4 改为 `Rig Driver + ModelGateway seam spike`，必须固定 `ModelSelection`、`ModelCallPurpose`、`ModelCallRequest`、`ModelCallResult`、`ModelCallErrorKind`、`ModelCallUsage` shape、`ModelGateway.call_model(...)`、最小 `ProviderRegistry.resolve(...)` 和 `AuthStore.resolve(...)`。阶段 5 只能通过 `DriverHost::call_model -> ModelGateway.call_model(...)` 调模型，不能在 `Driver` / `SessionDriverHost` 中 match provider、直读 env 或构造 Rig provider client。阶段 9 改为在既有 spine 上扩展 custom provider、完整 auth、fallback、usage normalization 和 context usage。

后续实现需验证：阶段 5 的 fake/minimal provider adapter 也必须走 `ProviderRegistry` / `AuthStore` / `ModelGateway` seam；阶段 9 只能扩展 seam，不能替换模型调用路径；provider/auth/raw usage/error 字符串不能泄漏到 `Driver`、event、snapshot 或 session JSONL。

### BR-010：模型调用 hook owner 不一致

状态：Resolved

原问题：`SessionRuntime` 文档描述 `BeforeModelCall`、`BeforeProviderPayload`、`AfterProviderResponse` 等模型调用前后 hook；`ModelGateway` 文档也把这些 hook 放入 `call_model` 流程。双 owner 会让 hook context、capability、错误策略和 diagnostics 归属不清。

原证据：

- `docs/modules/session-runtime.md`：列出模型调用前后 hook，并说 `SessionRuntime` 按 error policy 处理失败。
- `docs/modules/model-gateway.md`：SubmitPrompt flow 中 `ModelGateway.call_model` 执行 `BeforeModelCall`、provider payload hook、`AfterProviderResponse` / `ProviderUsageNormalized`。
- `docs/modules/runtime-hooks.md`：同时存在 run safe point hooks 和 provider hooks。

决策结果：hook owner 固定为“谁拥有安全点业务不变量，谁调用 hook、应用 typed result、重新校验并记录 diagnostics”。`SessionRuntime` 拥有 run/prompt/context/queue/compaction/persistence 安全点；`Tools` 拥有工具治理安全点；`ModelGateway` 拥有 model/provider 边界安全点；`CommandManager` / session `Command` 拥有 command catalog/resolve/output 安全点。`Driver` 不调用 hook，`RuntimeHookRegistry` 只保存 handler 和策略，不拥有业务流程。

实现顺序也已收敛：当前 MVP 不实现 hook system，不新增 `RuntimeHookRegistry` / hook invocation 阶段；只在文档中固定 owner 分层和禁止边界。后期第一批 hook 仅接入已稳定 owner 流程，例如 `BeforeAgentStart`、`PromptBuilt`、`ContextProjection`、`ToolBeforePolicy`、`ToolAfterExecute`、`SessionBeforeCompact`、`AfterSavePoint`、`CommandOutputBuild`。`BeforeModelCall`、`BeforeProviderPayload`、`AfterProviderResponse` 和 `ProviderUsageNormalized` 即使后期开放，也由 `ModelGateway.call_model(...)` 拥有；raw provider payload patch 默认不开放。

### BR-011：`UsagePurpose` 与 `ModelCallPurpose` 同值异名

状态：Resolved

原问题：`UsagePurpose` 和 `ModelCallPurpose` 的枚举值基本相同，但分别出现在 usage stats 和 model gateway 文档中，没有说明是否故意区分。

证据：

- `docs/modules/usage-stats.md`：`UsagePurpose { AgentRun, CompactionSummary, Retry, Background }`。
- `docs/modules/model-gateway.md`：`ModelCallPurpose { AgentRun, CompactionSummary, Retry, Background }`。
- `docs/modules/session-manager.md`：`SessionEntry::Usage` 使用 `UsagePurpose`。

决策结果：删除 `UsagePurpose`，只保留 `ModelCallPurpose` 作为模型调用业务目的的权威类型。固定传播链为 `ModelCallRequest.purpose -> ModelCallUsage.purpose -> future SessionEntry::Usage.purpose`，usage/persistence 层不得重新分类。当前变体收敛为 `AgentRun` 和 `CompactionSummary`；未来模型任务使用明确业务变体。

同时明确 `Retry` / `Background` 不是 purpose：provider fallback/retry 由 `ModelCallAttempt` 表达，session/run retry 由 `RetryReason` / `DriveEntry::Retry` / future call lineage 表达；session 因 focus 切换在后台运行时仍是 `AgentRun`。run-level `UsageSummary` 只聚合该 run 的 `AgentRun` 调用，独立 `CompactionSummary` usage 只进入 session 累计统计。

### BR-012：CommandSurface 与 CommandSurfaceService 命名层级仍需决定

状态：Resolved

原问题：模块名是 `CommandSurface`，文件规划是 `src/command_surface.rs`，但 AgentRuntime 服务列表使用 `CommandSurfaceService`。这可能只是实现类型名，但目前没有明确说明。

决策结果：`CommandSurface` 保留为领域总称，不作为主 struct 名。实现类型拆为共享无状态 `CommandManager` 和 `SessionRuntime` 持有的 session-scoped `Command` facade；文件规划改为 `src/command.rs` 与 `src/command/` 子模块。协议命令枚举改名为 `AgentCommand`，避免与 `command::Command` 混淆。

后续实现需验证：不再引入有状态 `CommandSurfaceService`；`CommandManager` 不缓存 catalog 或 UI state。

### BR-013：Compaction 与 ModelGateway 请求类型重复

状态：Resolved

原问题：`compaction::build_summary_request()` 返回 `SummaryModelRequest`，并说这是给 ModelGateway 的请求；ModelGateway 的实际执行入口又是 `ModelCallRequest`。两者关于 model、tools、thinking、stream options、max tokens 的责任分配不完全清晰。

证据：

- `docs/modules/compaction.md`：`SummaryModelRequest` 是给 ModelGateway 的请求。
- `docs/modules/model-gateway.md`：`ModelCallRequest` 是 Driver / Compaction 给 ModelGateway 的 provider-neutral 请求。
- `docs/modules/model-gateway.md`：Compaction flow 把 summary request 转为 `ModelCallRequest { purpose: CompactionSummary, tools: [] }`。

决策结果：删除 `SummaryModelRequest`，改为纯中间值 `CompactionSummaryMaterial { system_prompt, messages, max_output_tokens }`。`Compaction` 只负责 cut point、summary instructions/prompt、模型可见摘要输入和输出预算；不选择 provider/model，不决定 thinking/stream policy，不分配 call/run id，也不构造 `ModelCallRequest`。

唯一模型请求由 `SessionRuntime` 构造：它选择摘要模型和调用策略，并生成 `ModelCallRequest { purpose: ModelCallPurpose::CompactionSummary, tools: [], max_output_tokens: Some(material.max_output_tokens), ... }`；`ModelGateway` 只负责 provider/auth/fallback/usage/error/cancellation。后续实现需验证 `Compaction` public API 中不再出现 `ModelCallRequest`、`ModelSelection`、thinking/stream policy 或 `SummaryModelRequest`。

### BR-014：手动 `/compact` 运行中行为和 phase policy 冲突

状态：Resolved

原问题：CommandSurface 有 `IdleOnly`、`QueueAsSteer`、`ImmediateRuntimeAction` 等 phase policy，但 Compaction/SessionRuntime 文档描述手动 compact 在非 idle 时会 abort 当前 run 并 wait for idle。这使 `/compact` 隐含了“中止当前任务”的高风险行为。

证据：

- `docs/modules/command-surface.md`：`SlashCommandPhasePolicy`。
- `docs/modules/command-surface.md`：运行中命令必须遵守 phase policy 和 queue semantics。
- `docs/modules/compaction.md`：手动压缩流程包含 `abort current run and wait for idle`。
- `docs/modules/session-runtime.md`：手动 compact 流程在非 idle 时先 abort 当前 run。

决策结果：手动 `/compact` / `AgentCommand::Compact` 使用 `CommandRunPolicy::QueueAfterRun`，绝不隐式 abort。session idle 时立即执行；存在 active run、waiting approval、suspended run 或立即 retry chain 时，`SessionRuntime` 保存唯一 `PendingSessionAction::Compact { command_id, instructions }`，当前 work 继续运行。pending action 通过 `queue_updated.pending_actions` 和 `QueueSnapshot.pending_actions` 暴露，不进入模型上下文或 follow-up/next-turn message queue。早期名称 `CommandPhasePolicy::DeferUntilPostRun` 已由 ADR 0016 收窄并替换。

执行顺序固定为：当前 work terminal facts + save point → required overflow recovery / immediate retry chain → pending manual compact → threshold auto compaction（manual 已执行则跳过）→ follow-up / next-turn → `session_settled`。重复 compact 返回 `CompactAlreadyQueued`；已在 compaction phase 返回 `CompactionAlreadyRunning`。`AbortRun`、`ClearQueue`、session close 或 shutdown 清除 pending compact。后续实现需验证 running compact 不改变 current run、pending approval、resume state、message queues 或 session leaf。

### BR-015：ResourceManager 原子快照与技能正文晚读文件不一致

状态：Resolved

问题：ResourceManager 负责资源快照、trust gate、atomic reload；但 skills catalog 默认只保存 metadata，显式调用时 SessionRuntime 再读取 `metadata.file_path`。这意味着 catalog revision 和最终注入模型的技能正文未必来自同一原子快照。

证据：

- `docs/modules/resource-manager.md`：ResourceManager 拥有资源快照、trust gate、atomic reload。
- `docs/modules/skills.md`：SkillCatalog 默认只保存 metadata，不缓存正文。
- `docs/modules/session-runtime.md`：处理 `InvokeSkill` 时读取技能正文并构造 user message。

风险：reload 后到调用前文件可被修改或替换；历史记录中的 resource revision 不能完全证明注入正文内容。

处理记录：已在 `docs/modules/resource-manager.md` 和 `docs/modules/skills.md` 中规定：进入 `CwdResourceSnapshot.resolved` 的 selected skill 应保存 stable body content，或保存 content hash + immutable loaded content reference；目标 `PromptTurn.resolve_intent()` 从 captured `PromptResourceView` 读取 `SkillResource.body`，不能绕过 snapshot 重新读文件。

补充处理：已明确 `skills.rs`、`ResourceManager` 和 Prompt 的边界。`skills.rs` 负责类型、发现、frontmatter、校验和格式 helper；`ResourceManager` 负责 roots、trust、overlay、snapshot 和 canonical resource identity；目标 `PromptTurn.resolve_intent()` 从 captured `PromptResourceView` 查询正文并组装普通 user message。

`PromptDelivery` 规则也已固定：active `Steer` 必须使用 `CurrentRun.prompt_turn` 的 active snapshot；idle、`FollowUp` 和 `NextTurn` 在目标 future turn capture 后展开。active snapshot 缺少 skill 时返回 `SkillUnavailableInTurnSnapshot`，不能读取 current 新 revision、重新读 `metadata.file_path` 或静默降级成 FollowUp。

## 低风险 / 术语与一致性

### BR-016：Prompt template 没有独立 source of truth

状态：Resolved

问题：实现规划已有 `src/prompt_templates.rs`，但没有对应模块文档。Prompt template 的职责散落在 ResourceManager、CommandSurface、SessionRuntime 中。

处理记录：新增 `docs/modules/prompt-templates.md` 作为类型、frontmatter、参数语法和纯展开 helper 的 source of truth。`prompt_templates.rs` 是与 `skills.rs` 平级的纯模块，不是 service；`ResourceManager` 拥有 roots、trust、overlay、snapshot、immutable body 和 diagnostics；`CommandManager` 只消费 metadata；目标 `PromptTurn.resolve_intent()` 从 captured `PromptResourceView` 展开正文与 required skills。

参数语法采用 pi-compatible 单次替换：`$N`、`$@` / `$ARGUMENTS`、`${N:-default}`、`${@:N}`、`${@:N:L}`，参数支持单双引号分组且不递归替换。禁止 shell/env/file interpolation、template include、展开后重解析 slash command。canonical command 为 `/template <name>`；`/{name}` 仅在无命令冲突时 materialize。

snapshot 规则与 BR-015 对齐：active Steer 使用 active template/skill revision；FollowUp/NextTurn 使用 future revision；队列只保存 `template_key + args + additional_instructions`，不保存 raw slash text 或提前展开正文。

### BR-017：`TurnState` 是核心类型但 CONTEXT.md 没有术语定义

状态：Resolved

原问题：`TurnState` 在 SessionRuntime、Driver、ModelGateway、Compaction 多处引用，但 CONTEXT.md 没有独立术语。

原证据：

- `docs/modules/session-runtime.md`：定义 `TurnState`。
- `docs/modules/driver.md`、`docs/modules/model-gateway.md`、`docs/modules/compaction.md`：多处引用 `TurnState`。
- `CONTEXT.md`：只有 `ActiveModel` 提到可以进入 `TurnState`，没有完整词条。

决策结果：`CONTEXT.md` 已新增 `TurnState` 和 `DriverTurnInput` 词条，明确 `TurnState` 是 `SessionRuntime` 内部稳定 run snapshot，不跨过 `Driver` seam；`DriverTurnInput` 是投影给 `Driver` 的窄输入。

后续实现需验证：文档和代码不再把 `TurnState` 当作 driver request、公开协议快照或 ResourceManager current view。

### BR-018：`ModelCallRequest` 的 CONTEXT.md 描述遗漏 thinking level

状态：Resolved

处理记录：`CONTEXT.md` 的 `ModelCallRequest` 词条已补充 thinking level，并明确 Agent run 请求必须先来自已校验的 `ModelInputProjection`。`docs/modules/model-gateway.md` 同时加入可选 `OutputContract`，用于 JSON schema、response format 等 provider-neutral 调用契约；provider-specific mapping 仍由 `ModelGateway` 负责。

### BR-019：CommandSurface catalog revision 在模块文档中不够显式

状态：Resolved

原问题：协议定义 `SlashCommandCatalogRevision` 和 `SlashCommandEvent::CatalogChanged`，但 CommandSurface 的 catalog 设计段落主要展示 `SlashCommandSummary`，revision 语义不够突出。

决策结果：命令目录改为 `CommandSnapshot { revision, commands, diagnostics }` / `CommandCatalogRevision`，由 `CommandManager` 基于 command pack revision、resource revision、model/tools/settings/feature/run-state revision 等输入现算。UI 通过 `ExecuteCatalogCommand { selection.catalog_revision, ... }` 回传 selection，runtime 重新 materialize 并校验 stale/expired/binding invalid。

后续实现需验证：catalog revision 变化会触发 `command_catalog_changed`；旧 UI selection 不能绕过当前 catalog 重新校验。

### BR-020：首个 fake-driver 纵切事件验收写法不一致

状态：Resolved

处理记录：旧集中式开发路线图已删除，assistant lifecycle 不再由阶段表重复描述。权威事件文档固定 `message_assistant_started -> message_assistant_text_delta* -> message_assistant_finished` 生命周期：started/finished 各恰好一次，三者 `message_id` 一致，delta 只能出现在 started/finished 之间，最终 RuntimeSnapshot assistant message 等于 delta 拼接结果。user/run-result 持久化边界由 BR-021 单独处理。

### BR-021：persistence_save_point 数量与语义在文档中表达不完全一致

状态：Resolved

处理记录：save point 已定义为“可独立恢复的 session write batch”完成后的 durable barrier，数量不与 run 一一对应。正常 text-only `Completed` run 至少有两个：user-message save point 必须早于 `run_started`，final run-result save point 必须早于 `run_finished { status: completed }`。包含工具时，每个将被下一次模型调用消费的完整 tool-call/result batch 必须先提交并形成 save point；并行结果可以按 `call_index` 归约后共享一个 batch。

外层 event sequence 是覆盖水位：save point 确认同一 session 在该 sequence 前已由 `SessionRuntime` 提交的相关 writes 可恢复。流式 delta、tool output delta 和 queue state 不属于 session writes。`SessionRuntime` 是唯一 save-point event owner；abort/failure partial output、synthetic tool result 和 orphan protocol repair 继续由 BR-024 单独处理。

### BR-022：旧 pi 概念引用较多，可能影响术语纯度

状态：Resolved

处理记录：完成纯文档语言治理，没有改变任何 owner、interface、状态机、协议或兼容行为。`CONTEXT.md`、架构入口和模块正文现在使用 MiniCore 自身的 `AgentRuntime`、`SessionManager`、`SessionRuntime`、`Driver`、`ResourceManager`、`Prompt`、`Tools` 等术语解释当前系统；删除了 `AgentSessionRuntime`、`AgentSession.*`、`DefaultResourceLoader`、`ExtensionRunner`、`runAgentLoop`、`steerQueue` / `followUpQueue` 等历史实现名在现行流程中的映射和调用链。

pi、Codex、LangChain 等仍可出现在明确标注的设计参考段落和 ADR 中，但只用于说明可借鉴的设计思路，不构成类型、API、文件格式、事件顺序或行为兼容承诺。真实兼容要求必须由 MiniCore 文档显式声明；本轮把 `pi-compatible` / “对齐 pi”一类模糊措辞改为 MiniCore 独立定义的语法、工具名和执行规则。Rig `AgentRun` / `AgentRunStep` 是当前真实依赖类型，不属于历史术语清理范围。

后续文档验收规则：非 ADR/review 文档中的外部项目引用必须位于参考语境，当前运行流程和领域定义不得依赖读者先理解外部项目内部类型。
