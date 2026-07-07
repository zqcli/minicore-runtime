# System Blueprint Review Issues

日期：2026-07-06

来源：本轮由多个 subagent 对 MiniCore 系统蓝图进行只读审阅后合并整理。

范围：`README.md`、`CONTEXT.md`、`docs/architecture.md`、`docs/implementation-roadmap.md`、`docs/modules/*.md`、`docs/adr/*.md`。

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

处理记录：已决策将 active session 当前 run 的待审批工具调用投影到 `RuntimeSnapshot.active_session.current_run.pending_tool_approvals`。协议新增 `PendingToolApprovalView`，只包含 `session_id`、`run_id`、`call_id`、tool 名称、risk、reason、preview 和创建时间等 UI-safe 字段；冻结的 `prepared_args` 仍由 `ToolApprovalBroker` 内部保存，不进入 snapshot，也不能由 UI 修改。`DecideToolApproval` 继续通过 `session_id`、`run_id`、`call_id` 匹配 broker 中的 pending approval。

原问题：事件文档要求审批状态不能由 UI 私有保存，同一 host 生命周期内的订阅/状态重建后应从 snapshot 的 pending tool calls 恢复；此前 `RuntimeSnapshot` 字段列表只有 `current_run`、tools、queues 等，没有明确 pending approval 字段。

决策结果：pending approval 放在 `RunView`，字段名为 `pending_tool_approvals`。它是 tool-call waiting state 的 UI-safe projection，作用域限定为 active session 的当前 run。

风险：已通过 `RunView.pending_tool_approvals` 处理。同一 host 生命周期内的订阅/状态重建后，UI 可以从 snapshot 恢复审批弹窗，并使用对应 `call_id` 安全发出 approval decision。

待处理方向：已处理。后续实现需验证：`tool_call_approval_requested` 后 snapshot 包含 pending approval；approve/reject、abort、run finished 或 session close 后 snapshot 不再包含该 approval；`prepared_args` 不出现在 snapshot / event / log 中。

### BR-005：`RunTerminalStatus::Paused` 与 run 单终态/暂停语义冲突

状态：Open

问题：协议把 `Paused` 放入 `RunTerminalStatus`，但 Driver 把 `DriveResult::Paused` 作为未来可序列化暂停路径。Paused 到底是终态、可恢复中间态，还是 run lifecycle 的另一类状态，目前不清楚。

证据：

- `docs/modules/agent-runtime-protocol.md`：`RunTerminalStatus { Completed, Failed, Aborted, Paused }`。
- `docs/modules/driver.md`：`DriveResult::Paused { reason, serialized_run }`；MVP 可以暂不产生 `Paused`，但不要把接口设计死成只能 completed/failed。
- `docs/adr/0003-agent-runtime-events-use-event-msg-and-lifecycle-pairs.md`：一次 run 只有一个终态事件 `run_finished { status }`。

风险：UI reducer、session phase、run resume、持久化屏障会混淆 paused 和 terminal finished。

待处理方向：明确 paused 是 terminal status 还是独立 lifecycle event/state；如果是可恢复暂停，可能不应叫 terminal status。

### BR-006：CommandPresentation 可携带完整 Command，可能突破 presentation 边界

状态：Open

问题：`CommandAction::DispatchCommand(Command)` 和 `UiInteractionSubmit::DispatchCommandTemplate` 允许 UI-visible presentation 携带完整 runtime command。但 CommandSurface 安全边界又说 `CommandPresentation` 是语义展示请求，UI 不能通过它修改 runtime state。若 hook/extension 可 patch presentation，可能间接制造高权限结构化命令。

证据：

- `docs/modules/agent-runtime-protocol.md`：`CommandAction::DispatchCommand(Command)`。
- `docs/modules/agent-runtime-protocol.md`：`UiInteractionSubmit::DispatchCommandTemplate`。
- `docs/modules/command-surface.md`：`CommandPresentation` 是语义展示请求，不是 UI 私有回调；UI 不能通过它修改 runtime state。
- `docs/modules/runtime-hooks.md`：`CommandOutputBuild`、`InteractionRequestBuild` 可 patch presentation 语义。

风险：presentation layer 可能成为绕过 CommandSurface parse/plan/phase policy 的后门。

待处理方向：限制 presentation action 的 command 白名单；或让 action 只携带 opaque action id / typed safe action，由 runtime 再次解析和授权。

### BR-007：公开协议暴露过宽的内部 mutation command

状态：Open

问题：MVP `Command` 包含 `AppendMessage`、`SetTools` 等高权限 mutation 能力，但架构边界要求 UI 不拥有 session messages authority、tools authority。这些命令更像 internal/privileged API。

证据：

- `docs/modules/agent-runtime-protocol.md`：`AppendMessage { session_id, message, trigger_turn }`。
- `docs/modules/agent-runtime-protocol.md`：`SetTools { session_id, tools, active_tool_names }`。
- `docs/architecture.md`：工具注册、活跃工具、审批、沙箱和真实副作用执行由 runtime 统一治理。
- `docs/modules/agent-runtime-events.md`：UI 不应持有权威会话状态或工具状态。

风险：下游 adapter 可以直接注入消息、工具配置或绕过资源/tool provider 归一化。

待处理方向：区分 public protocol command 与 privileged/internal command；给高权限命令加 capability、feature gate 或移动到内部 API。

### BR-008：Driver seam 可能过宽，`TurnState` 把资源状态泄漏给 Driver

状态：Open

问题：`DriveRequest` 携带完整 `TurnState`，而 `TurnState` 包含 captured `TurnResourceSnapshot`、tool state、context usage 等会话/资源编排信息。Driver 文档又要求 Driver 不拥有 resource loading、skill expansion、prompt template expansion、system prompt building、active tools 等职责。

证据：

- `docs/modules/driver.md`：`DriveRequest { turn_state: TurnState }`。
- `docs/modules/session-runtime.md`：`TurnState` 包含 `resources: Arc<TurnResourceSnapshot>`。
- `docs/modules/driver.md`：Driver 不负责资源加载、skill/template 展开、system prompt 构建、active tools 管理。

风险：Rig adapter seam 变浅，后续实现容易误用 `TurnResourceSnapshot`，把会话编排逻辑带进 Driver。

待处理方向：考虑传入 `DriverTurnState` / `ModelRunInput` 这类更窄的投影，只包含 Driver 构造 model/tool step 所需字段。

## 中风险

### BR-009：ModelGateway 实现顺序可能导致阶段 5 与阶段 9 返工

状态：Open

问题：阶段 5 已接入真实 Driver、`model_gateway.rs` 和 `model_gateway/rig.rs`，但阶段 9 才收拢 provider/model/auth 生命周期、custom provider、usage 归一化和 context usage。ModelGateway 文档又把 provider/model/auth/fallback/usage/error/cancellation 都定义为调用边界职责。

证据：

- `docs/implementation-roadmap.md`：阶段 5 Text-only Driver integration 包含 `model_gateway.rs`、`model_gateway/rig.rs`。
- `docs/implementation-roadmap.md`：阶段 9 才做 ModelGateway 和 UsageStats 的完整收拢。
- `docs/modules/model-gateway.md`：ModelGateway 负责 provider/model 解析、凭据解析、custom base URL、fallback、usage、错误分类。

风险：阶段 5 为了跑通真实 driver 可能先做临时 gateway，阶段 9 再重构 auth、fallback、usage 和错误语义。

待处理方向：阶段 5 明确只允许 fake/minimal ModelGateway adapter，或提前定义 stable ModelGateway seam 与测试替身。

### BR-010：模型调用 hook owner 不一致

状态：Open

问题：`SessionRuntime` 文档描述 `BeforeModelCall`、`BeforeProviderPayload`、`AfterProviderResponse` 等模型调用前后 hook；`ModelGateway` 文档也把这些 hook 放入 `call_model` 流程。双 owner 会让 hook context、capability、错误策略和 diagnostics 归属不清。

证据：

- `docs/modules/session-runtime.md`：列出模型调用前后 hook，并说 `SessionRuntime` 按 error policy 处理失败。
- `docs/modules/model-gateway.md`：SubmitPrompt flow 中 `ModelGateway.call_model` 执行 `BeforeModelCall`、provider payload hook、`AfterProviderResponse` / `ProviderUsageNormalized`。
- `docs/modules/runtime-hooks.md`：同时存在 run safe point hooks 和 provider hooks。

风险：hook 执行顺序、失败回滚、权限边界、diagnostics 可能出现双重处理或遗漏。

待处理方向：明确 provider-neutral hook 与 provider-payload hook 的 owner；例如 `SessionRuntime` 负责 run safe point，`ModelGateway` 负责 provider boundary。

### BR-011：`UsagePurpose` 与 `ModelCallPurpose` 同值异名

状态：Open

问题：`UsagePurpose` 和 `ModelCallPurpose` 的枚举值基本相同，但分别出现在 usage stats 和 model gateway 文档中，没有说明是否故意区分。

证据：

- `docs/modules/usage-stats.md`：`UsagePurpose { AgentRun, CompactionSummary, Retry, Background }`。
- `docs/modules/model-gateway.md`：`ModelCallPurpose { AgentRun, CompactionSummary, Retry, Background }`。
- `docs/modules/session-manager.md`：`SessionEntry::Usage` 使用 `UsagePurpose`。

风险：同一模型调用目的在执行、usage、持久化中有两套名称，容易映射漂移。

待处理方向：合并为一个权威名称，或明确两者边界和转换规则。

### BR-012：CommandSurface 与 CommandSurfaceService 命名层级仍需决定

状态：Open

问题：模块名是 `CommandSurface`，文件规划是 `src/command_surface.rs`，但 AgentRuntime 服务列表使用 `CommandSurfaceService`。这可能只是实现类型名，但目前没有明确说明。

证据：

- `docs/modules/command-surface.md`：模块为 `CommandSurface`，内部示例为 `CommandSurfaceService`。
- `docs/modules/agent-runtime.md`：`AgentRuntime` 持有 `CommandSurfaceService`。
- `docs/modules/README.md`：Rust 文件规划只有 `src/command_surface.rs`。

风险：实现时 struct、module、trait 命名可能继续漂移。

待处理方向：决定 public module 名、主 struct 名和 service 聚合里的字段名。例如模块 `command_surface`，主类型 `CommandSurface` 或 `CommandSurfaceService` 二选一。

### BR-013：Compaction 与 ModelGateway 请求类型重复

状态：Open

问题：`compaction::build_summary_request()` 返回 `SummaryModelRequest`，并说这是给 ModelGateway 的请求；ModelGateway 的实际执行入口又是 `ModelCallRequest`。两者关于 model、tools、thinking、stream options、max tokens 的责任分配不完全清晰。

证据：

- `docs/modules/compaction.md`：`SummaryModelRequest` 是给 ModelGateway 的请求。
- `docs/modules/model-gateway.md`：`ModelCallRequest` 是 Driver / Compaction 给 ModelGateway 的 provider-neutral 请求。
- `docs/modules/model-gateway.md`：Compaction flow 把 summary request 转为 `ModelCallRequest { purpose: CompactionSummary, tools: [] }`。

风险：SessionRuntime、Compaction、ModelGateway 谁负责选择摘要模型和填充调用选项会变模糊。

待处理方向：让 Compaction 只生成 summary prompt/materials，由 SessionRuntime/ModelGateway 构造 `ModelCallRequest`；或让 `SummaryModelRequest` 明确只是中间 DSL。

### BR-014：手动 `/compact` 运行中行为和 phase policy 冲突

状态：Open

问题：CommandSurface 有 `IdleOnly`、`QueueAsSteer`、`ImmediateRuntimeAction` 等 phase policy，但 Compaction/SessionRuntime 文档描述手动 compact 在非 idle 时会 abort 当前 run 并 wait for idle。这使 `/compact` 隐含了“中止当前任务”的高风险行为。

证据：

- `docs/modules/command-surface.md`：`SlashCommandPhasePolicy`。
- `docs/modules/command-surface.md`：运行中命令必须遵守 phase policy 和 queue semantics。
- `docs/modules/compaction.md`：手动压缩流程包含 `abort current run and wait for idle`。
- `docs/modules/session-runtime.md`：手动 compact 流程在非 idle 时先 abort 当前 run。

风险：用户触发 `/compact` 可能丢失未完成输出、pending tool 或队列语义不明确。

待处理方向：明确 `/compact` 默认 `IdleOnly`，或把 abort 行为做成显式确认/单独命令。

### BR-015：ResourceManager 原子快照与技能正文晚读文件不一致

状态：Resolved

问题：ResourceManager 负责资源快照、trust gate、atomic reload；但 skills catalog 默认只保存 metadata，显式调用时 SessionRuntime 再读取 `metadata.file_path`。这意味着 catalog revision 和最终注入模型的技能正文未必来自同一原子快照。

证据：

- `docs/modules/resource-manager.md`：ResourceManager 拥有资源快照、trust gate、atomic reload。
- `docs/modules/skills.md`：SkillCatalog 默认只保存 metadata，不缓存正文。
- `docs/modules/session-runtime.md`：处理 `InvokeSkill` 时读取技能正文并构造 user message。

风险：reload 后到调用前文件可被修改或替换；历史记录中的 resource revision 不能完全证明注入正文内容。

处理记录：已在 `docs/modules/resource-manager.md` 和 `docs/modules/skills.md` 中规定：进入 `CwdResourceSnapshot.resolved` 的 selected skill 应保存 stable body content，或保存 content hash + immutable loaded content reference；`SessionRuntime` 显式调用技能时从 captured `TurnResourceSnapshot.cwd.resolved.skills` 读取 `SkillResource.body`，不能绕过 snapshot 重新读文件。

补充处理：已明确 `skills.rs` 和 `ResourceManager` 的边界。`skills.rs` 负责 `SkillMetadata` / `SkillResource` / `SkillCatalog` 类型、给定目录后的发现、frontmatter 解析、校验和格式化 helper；`ResourceManager` 负责 skill roots、trust gate、runtime/cwd 分层、cwd-over-runtime overlay、reload/ensure/recompose 生命周期和 current snapshot 发布。

## 低风险 / 术语与一致性

### BR-016：Prompt template 没有独立 source of truth

状态：Open

问题：实现规划已有 `src/prompt_templates.rs`，但没有对应模块文档。Prompt template 的职责散落在 ResourceManager、CommandSurface、SessionRuntime 中。

证据：

- `docs/modules/README.md`：文件规划包含 `src/prompt_templates.rs`。
- `docs/modules/resource-manager.md`：描述 prompt template metadata 和资源投影。
- `docs/modules/command-surface.md`：描述 `/{template}` 命令投影。
- `docs/modules/session-runtime.md`：描述 `InvokePromptTemplate` 展开为 user message。

风险：catalog 生命周期、参数替换、详情查询、正文读取归属可能漂移。

待处理方向：考虑新增 `docs/modules/prompt-templates.md`，或明确它只是 ResourceManager 子能力而非独立模块。

### BR-017：`TurnState` 是核心类型但 CONTEXT.md 没有术语定义

状态：Open

问题：`TurnState` 在 SessionRuntime、Driver、ModelGateway、Compaction 多处引用，但 CONTEXT.md 没有独立术语。

证据：

- `docs/modules/session-runtime.md`：定义 `TurnState`。
- `docs/modules/driver.md`、`docs/modules/model-gateway.md`、`docs/modules/compaction.md`：多处引用 `TurnState`。
- `CONTEXT.md`：只有 `ActiveModel` 提到可以进入 `TurnState`，没有完整词条。

风险：核心概念没有 glossary anchor，后续容易被理解成 prompt、context projection 或 driver request 的混合物。

待处理方向：在 CONTEXT.md 增加 `TurnState` 术语。

### BR-018：`ModelCallRequest` 的 CONTEXT.md 描述遗漏 thinking level

状态：Open

问题：CONTEXT.md 对 `ModelCallRequest` 的描述包含模型选择、消息、system prompt、tools、purpose、stream options，但实际 ModelGateway 定义还有 `thinking_level`。

证据：

- `CONTEXT.md`：`ModelCallRequest` 术语描述。
- `docs/modules/model-gateway.md`：`ModelCallRequest` 定义包含 `thinking_level: ThinkingLevel`。

风险：术语表与协议/模块文档不完全一致。

待处理方向：补充 thinking level，或明确 thinking level 属于 model options。

### BR-019：CommandSurface catalog revision 在模块文档中不够显式

状态：Open

问题：协议定义 `SlashCommandCatalogRevision` 和 `SlashCommandEvent::CatalogChanged`，但 CommandSurface 的 catalog 设计段落主要展示 `SlashCommandSummary`，revision 语义不够突出。

证据：

- `docs/modules/agent-runtime-protocol.md`：定义 `SlashCommandCatalogRevision`。
- `docs/modules/command-surface.md`：Catalog 段落没有把 revision 放在 catalog 类型草图中。

风险：实现时可能只做 Vec 比较，忽略 catalog revision、资源 revision 和 availability 变化的关系。

待处理方向：在 CommandSurface 文档中显式补充 catalog revision owner 和更新条件。

### BR-020：首个 fake-driver 纵切事件验收写法不一致

状态：Open

问题：阶段 3 表格只写 `assistant delta`，后文首个开发切片要求 `message_assistant_started`、`message_assistant_text_delta*`、`message_assistant_finished`。

证据：

- `docs/implementation-roadmap.md`：阶段 3 可验证产物写 `assistant delta`。
- `docs/implementation-roadmap.md`：首个开发切片列出 started/delta/finished 完整序列。

风险：执行路线图时，早期测试可能漏掉 assistant lifecycle 配对。

待处理方向：统一阶段 3 验收文本，明确必须覆盖 assistant started/delta/finished。

### BR-021：persistence_save_point 数量与语义在文档中表达不完全一致

状态：Open

问题：AgentRuntimeEvents 的 Submit Prompt lifecycle 中 user message durable 和 assistant/tool results durable 各有一个 save point；SessionRuntime 文档只概括为“每个 run 的可恢复边界 flush pendingSessionWrites，发出 persistence_save_point”。

证据：

- `docs/modules/agent-runtime-events.md`：Submit Prompt lifecycle 明确两个 `persistence_save_point`。
- `docs/modules/session-runtime.md`：运行流程只笼统描述每个 run 的可恢复边界。

风险：实现者可能以为每个 run 只有一个 save point，导致 user message append 的恢复边界不清晰。

待处理方向：在 SessionRuntime 文档中明确 user message save point 与 run result save point 的关系。

### BR-022：旧 pi 概念引用较多，可能影响术语纯度

状态：Open

问题：`ExtensionRunner`、`AgentHarness` 等 pi 历史概念在多个文档中出现。部分是合理映射，但重复引用可能让读者误以为 MiniCore 也有对应模块。

证据：

- `docs/architecture.md`：pi 经验映射包含 `ExtensionRunner`、`AgentHarness`。
- `docs/modules/runtime-hooks.md`：引用 pi coding-agent `ExtensionRunner` 经验。
- `CONTEXT.md`：明确避免把 MiniCore runtime 称为 `AgentHarness`。

风险：低。主要是阅读噪音和命名漂移风险。

待处理方向：保留 ADR/经验映射中的必要引用，减少模块文档中的历史名词。
