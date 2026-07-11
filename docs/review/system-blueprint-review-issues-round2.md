# System Blueprint Review Issues — Round 2

日期：2026-07-06

来源：对 `README.md`、`CONTEXT.md`、`docs/architecture.md`、当时存在但后续已删除的 `docs/implementation-roadmap.md`、`docs/modules/*.md`、`docs/adr/*.md`、`docs/review/*.md` 的第二轮全量交叉审阅。本轮只记录第一轮（BR-001 ~ BR-022）未覆盖的新问题，编号从 BR-023 起延续；路线图相关证据仅保留为历史记录。

约定：与第一轮相同，本文只记录待处理问题，不代表已经决定修改方案。后续逐条处理时，应回到对应 source of truth 文档中做设计取舍。

## 总体判断

文档体系的整体质量高：权威归属表、事件所有权矩阵、必测项、"不应承担"段落、ADR 和第一轮自审都是好的实践；Rig sans-IO 主路径、hook typed result、单终态 `run_finished`、`CommandAck` 只表接收、Usage 与 ContextUsage 分离、append-only session tree 等核心决策合理且有外部经验支撑。

本轮发现的问题集中在三类：

1. **协议表面的真实缺口已逐步关闭**：查询响应通道（BR-023）已通过独立 RuntimeQuery 关闭；abort/crash 持久化语义（BR-024）已通过统一 stable batch writer 关闭；abort queue 与 editor ownership 冲突（BR-031）已通过明确 UI-local restore 边界关闭。
2. **安全边界原有两处没有 source of truth**：custom provider 与 project trust 的关系（BR-025）已通过 user-global provider/auth 决策关闭；tool sandbox（BR-037）已通过 `Tools` 权威文档补齐 source of truth。
3. **文档间漂移已经发生**：三份架构图互相矛盾（BR-026）、hook registry 归属漂移（BR-035）、术语表与协议字段直接冲突（BR-034）。这印证了第一轮 BR-022 的担忧——重复声明是漂移温床。

## 高风险

### BR-023：查询型协议命令没有已定义的响应通道

状态：Resolved

决策结果：按 ADR 0018，`AgentRuntime` 稳定 interface 分为四种能力：`dispatch(AgentCommand) -> CommandAck`、`query(RuntimeQuery) -> QueryResponse`、`subscribe() -> EventStream` 和 `snapshot() -> RuntimeSnapshot`。Command 用于 mutation/异步工作，Query 用于只读 typed request/response，Event 用于运行变化，Snapshot 用于带事件水位的 UI 初始化与恢复。

`RuntimeQuery` 按 runtime、session、settings、resources、command surface、models、usage 和 diagnostics 领域分组；当前只固定框架和关键 variants，后续子查询在对应领域 enum 中扩展，不再次改变 AgentRuntime interface。`ListSessions`、资源详情、command catalog/suggestion、usage/context usage 和 settings read 已从 `AgentCommand` 语义中移出。

query 不分配 `CommandId`、不产生 `CommandAck`、不发布 query-response event、不增加 event sequence，也不为了目录/detail 读取加载完整 `SessionRuntime`。`QueryResponse.as_of_sequence` 只说明结果生成时的 runtime 水位；可选领域 revision 用于 cache/stale 判断。JSON-RPC/Tauri/stdio adapter 可以暴露 method-per-query 的 typed transport 方法，但内部统一映射到 `RuntimeQuery`。

### BR-024：run 中断（abort/失败/宿主关闭）后的持久化语义未定义，可能在会话历史中留下 unresolved tool call

状态：Resolved

处理记录：按 ADR 0019，所有 session mutation 统一通过 `SessionWriter.commit(SessionWriteBatch)`。writer 只接受协议完整、可以独立恢复的稳定单元；成功返回是可信写入结果，失败 batch 不得进入恢复投影。MVP 不引入 `SessionRevision`，`CommittedSessionBatch` 只返回 entry ids 与 current leaf。

`UserInput`、完整 `ToolRound`、`AssistantFinal`、`Compaction`、独立 `SessionMutation` 和 `TreeMutation` 分批提交。assistant tool-call message 不能单独持久化；它必须和该 round 的全部 actual/error tool results 作为一个 `ToolRound` commit。DriverHost 只有在 commit 成功后才把 tool results 返回 Driver 并允许下一模型调用，因此 committed history 不会产生 unresolved tool call。

streaming delta、partial assistant、pending approval、执行中的 tool round 和其他 `CurrentRun` 状态只存在内存。abort、failure、session close 或 host shutdown 保留此前 committed batches，丢弃当前 incomplete unit；started assistant 仍发布 finished 关闭 UI lifecycle，但 partial 不落盘。stable commit 一旦开始不接收 run cancellation，graceful abort/close/shutdown 等待其确定结果后再完成 terminal handling。hard crash 后只恢复最后一个完整 committed batch，不恢复旧 run、不补 synthetic result，也不做语义 repair。JSONL adapter 一行编码一个 batch，只允许忽略或截断末尾不完整行；中间损坏或违反 tool protocol 的 committed batch fail closed。

该方案接受工具已经产生外部副作用、但对应 running batch 尚未 commit 时 workspace 与 session history 可能不一致；MiniCore 不承诺 tool exactly-once 或文件系统与 session storage 的跨系统原子事务。公共 `persistence_save_point` 同时删除，需要恢复的领域事件在 commit 成功后发布。

### BR-025：custom provider / AuthRef 与 project trust 的边界未定义，存在凭据外泄链

状态：Resolved

处理记录：当前分支已决策 provider settings、auth、custom provider 和 `ModelGateway` 都是 user-global/runtime-global；项目级 settings 不允许声明 custom provider、覆盖 base URL 或引用 credentials。`ProviderRegistry` 只合并 built-in provider、user-global settings 中的 custom provider 和后续 user-trusted extension 声明。

问题：`ProviderRegistry` 会"合并 settings 中的 custom provider"；`CustomProviderConfig` 携带任意 `base_url` 和 `AuthRef`；`AuthRef::Env { var }` 允许指向任意环境变量。`SettingsView` 是 `CwdScopedServices` 成员（按 cwd 解析），暗示存在项目级 settings，但没有任何文档定义 settings 的分层（user/project）及 custom provider 声明允许的来源。`ResourceManager` 的 trust gate 明确只覆盖 prompt、skill、context、extension 资源，不覆盖 provider/settings；ModelGateway 的"Grilling 结论"和"必测项"也没有任何 trust 相关条目。

若项目级 settings 可以声明 custom provider，攻击链为：不受信任的项目仓库携带 settings，声明 `provider_id = "x"`、`base_url = attacker`、`auth_ref = Env("ANTHROPIC_API_KEY")`，并把默认模型指向它（默认模型同样来自 settings）；用户打开该项目后一次普通对话即把真实 API key 发送到攻击者服务器。auth redaction 规则（secret 不进 event/snapshot/log）防不住这条链，因为泄漏发生在合法的 provider HTTP 请求本身。

证据：

- `docs/modules/model-gateway.md`：ProviderRegistry 职责含"合并……settings 中的 custom provider"；`AuthRef::Env { var: String }`；`CustomProviderConfig { base_url, auth_ref }`。
- `docs/modules/agent-runtime.md`：`CwdScopedServices` 包含 `SettingsView` / `ProjectTrustView` / `AuthView`，per-cwd 解析。
- `docs/modules/resource-manager.md`：项目信任一节，trust gate 范围不含 settings/provider。
- `docs/modules/model-gateway.md`：必测项无 trust/来源约束项。

风险：真实安全漏洞类别（凭据外泄），而且是"设计上没说"而不是"实现可能写错"——按当前文档实现出来的系统天然带这条链。

待处理方向：已处理。后续实现必须验证：项目资源或项目 settings 不能注册 custom provider，不能覆盖 provider base URL，也不能让项目选择新的 `AuthRef::Env`；custom provider 只能来自 user-global 配置或后续 user-trusted extension。

### BR-026：三份架构图互相矛盾，模块总览仍是 BR-003 收敛前的旧结构

状态：Resolved

处理记录：当前分支已按新 resource snapshot 模式同步 `docs/architecture.md`、`docs/modules/README.md` 和 `docs/modules/agent-runtime.md` 的架构图：`WorkspaceServices` 持有 `ResourceSnapshotStore`、共享 `ResourceManager`、user-global provider/auth 和 shared `ModelGateway`；不再展示 `CwdServiceRegistry` / `CwdScopedServices`。

问题：`docs/modules/README.md` 的分层图把 `ResourceManager` 画成 `AgentRuntime` 直属顶层服务、把 `ModelGateway` 画在 `SessionRuntime` 之下，图中完全没有 `WorkspaceServices` / `CwdServiceRegistry` / `CwdScopedServices`——这是 BR-003 收敛（commit `eac94c1`）之前的结构。`docs/architecture.md` 的图把 `CommandSurface`、`RuntimeHooks` 画成与 `WorkspaceServices` 平行的顶层分支；而 `docs/modules/agent-runtime.md` 的服务图把 `CommandSurfaceService`、`RuntimeHookRegistry` 放在 `WorkspaceServices` 内部。三份图两两不一致。

证据：

- `docs/modules/README.md` 分层图（ResourceManager/CommandSurface/RuntimeHooks/SessionManager 四分支，ModelGateway 挂 SessionRuntime）。
- `docs/architecture.md` 分层图（WorkspaceServices 与 CommandSurface/RuntimeHooks 并列）。
- `docs/modules/agent-runtime.md` 运行时服务图（CommandSurfaceService/RuntimeHookRegistry 在 WorkspaceServices 内）。

风险：模块总览是新读者入口，与已收敛设计冲突会直接传播错误心智模型；也说明"权威归属"机制对图示没有生效。

待处理方向：已处理。后续仍建议让 `agent-runtime.md` 成为 runtime service shape 的 source of truth，其他图只保留摘要。

## 中风险

### BR-027：`AbortRun { run_id }` 是唯一以 run_id 路由的命令，存在无法取消的竞态窗口

状态：Resolved

处理记录：保留 `AbortRun { run_id: RunId }`，不引入 `AbortRun { session_id, run_id: Option<RunId> }` 或 session 级 current-run abort。`RunId` 被正式定义为一次已经公开启动的 `Driver::drive_run()` / Agent loop 的 runtime-host-global opaque id；它在建立 `CurrentRun`、即将调用 Driver 并发布 `run_started` 时分配，不是 prompt admission、`CommandId`、user turn 或 pending work id。`AgentRuntime` 通过 `LoadedSessionRuntimes.find_current_run(run_id)` 路由已经 started 且尚未 terminal 的 run，这个 lookup 不是 durable registry。

`CommandAck` 和 `session_phase_changed(turn)` 不表示 run 已开始。`Turn + current_run = None` 是不可通过 `AbortRun` 取消的有界 admission/preflight 或 post-run finalization 窗口；其中只允许有限步骤的 validation、captured prompt assembly、required session commit 和有界 hook；bounded 不表示硬实时 deadline，writer 仍遵守不可中断与故障契约。模型/provider/tool/approval 等可能长时间等待的 run work 必须在 `run_started` 之后发生；session-level retry delay 仍属于独立 `RetryBackoff` phase。因此“慢模型首包没有 run id”不成立。

UI 只有在收到 `run_started` 或 snapshot 中存在 `current_run.run_id` 后才展示 run-level Stop 并发送 `AbortRun`。adapter 如需支持用户提前按 Ctrl-C，可以仅在 UI 本地按 originating `command_id` 暂存 abort intent；收到 matching `run_started` 后立即发送普通 `AbortRun { run_id }`，若先收到 rejection、`session_phase_changed(idle)`、`session_settled` 或 session close 则清除。该 intent 不进入 runtime command、queue、event 或 snapshot。

### BR-028：运行输入存在双入口（`SubmitPrompt.delivery` vs `Steer`/`FollowUp`/`NextTurn`），且 `DeliveryMode`、`QueueKind`、`QueueMode` 未定义

状态：Resolved

原问题：`SubmitPrompt`、`InvokeSkill`、`InvokePromptTemplate`、`ExecuteCommandText` 都携带 `delivery: DeliveryMode`，同时又存在独立的 `Steer` / `FollowUp` / `NextTurn` 命令，导致 phase guard、queue event 和 hook 行为存在双入口漂移风险。

证据：

- `docs/modules/agent-runtime-protocol.md`：MVP `AgentCommand` 列表同时含 `SubmitPrompt { delivery }` 与 `Steer`/`FollowUp`/`NextTurn`；`SetQueueMode { queue: QueueKind, mode: QueueMode }`。
- 全仓库无 `DeliveryMode` / `QueueKind` / `QueueMode` 定义。

风险：入口语义二义，实现时两条路径的队列/phase/hook 行为分叉。

决策结果：按 ADR 0016 删除独立 `Steer` / `FollowUp` / `NextTurn` protocol command，将 `DeliveryMode` 收窄并改名为 `PromptDelivery { Steer, FollowUp, NextTurn }`。`SubmitPrompt`、skill、prompt template 和 prompt-producing slash command 都归一到 `SessionRuntime.admit_prompt_intent(...)`；`ExecuteCommandText` / `ExecuteCatalogCommand` 携带的 `prompt_delivery` 只在 command 产生模型可见 prompt intent 时生效。

slash command 自身另使用 `CommandRunPolicy { Immediate, IdleOnly, QueueAfterRun }`。`/status` 可在 active work 中立即输出到 message panel；`/compact` 在 active work 中映射为 typed `PendingSessionAction::Compact`；两者都不进入消息队列。`QueueKind { Steering, FollowUp }` 和 `QueueMode { All, OneAtATime }` 已在协议中定义，`NextTurn` 与 pending session action 不受 queue mode 控制。

### BR-029：`SessionPhase` 是 phase guard 的核心，但没有权威完整定义；`branch_summary` 是幽灵 phase

状态：Resolved

决策结果：`SessionPhase` 封闭为 `Idle | Turn | Compaction | RetryBackoff`。`Turn` 是产品级工作窗口，可以包含 prompt preflight、连续多个 Agent runs、approval wait、可恢复 suspend、持久化和 post-run arbitration；它不等于单个 `RunId`。`RetryBackoff` 只表示 Agent run 自动重试前的调度等待，provider fallback/retry 和 compaction summary 内部 retry 不改变 phase。

`WaitingApproval` / `Suspended` 继续属于 `CurrentRunState`，发生时 phase 仍为 `Turn`。`BranchSummary` 保留为 session entry，并在未来真正实现独立摘要模型任务时增加明确 `ModelCallPurpose`；MVP 不定义 `SessionPhase::BranchSummary`。

已在协议文档增加可序列化 enum，在 SessionRuntime 增加 phase/command guard 规则，在 AgentRuntimeEvents 增加完整转换矩阵。只有进入 `Idle` 且不会立即 continuation 时才发布 `session_settled`；同 phase 的 `Turn -> Turn` continuation 不产生虚假 phase-changed 事件。

### BR-030：`WaitForIdle` 作为协议命令在 ack-only 模型下语义不成立

状态：Resolved

处理记录：从公开 `AgentCommand` 删除 `WaitForIdle { session_id }`，不新增 `wait_finished` event，也不在 `AgentRuntime` / `SessionRuntimeHandle` 增加稳定 `wait_until_settled()`。等待未来状态不是 mutation command，也不是立即返回的 query；`CommandAck` 继续只表达接收，`session_settled` 继续表达 session 已进入 Idle 且没有马上继续的 run/compaction/retry/pending action 或 steering/follow-up continuation；`NextTurn` queue 可以保留。

UI 通过 snapshot + EventStream reducer 观察 `run_finished` / `session_settled`。一次性 CLI、RPC client 和测试如需 imperative await，在 adapter/test-support 层先订阅再 dispatch，或使用调用方已有的 session reducer，并提供 `collect_until(...)` / `wait_for_event(...)` 薄 helper；这些 helper 不是 core 稳定 API，也不承诺任意 background session 的 late join。订阅起点、lag 和 gap recovery 留给 BR-044。runtime 内部顺序执行继续使用 phase guard、`CommandRunPolicy::QueueAfterRun` 和 typed `PendingSessionAction`，不采用容易产生 TOCTOU race 的 wait-then-act。

同类项目验证了这个边界：pi core 有进程内 Promise，RPC client 的 `waitForIdle()` 只是等待 `agent_settled` 的客户端 helper，并非 RPC command；Codex 使用 `turn/completed` 与 thread idle status；OpenCode 使用同步 prompt 或 async prompt + session status/idle event；ACP v2/Goose 使用 accepted response + running/idle update；LangGraph 的 `/runs/wait` / run join 绑定具体 run 且属于 server SDK request，不是 session mutation。MiniCore 当前不是独立 server，只有未来出现明确 remote run-join 需求时才在 transport/SDK 层另行设计。

### BR-031：abort 后"归还队列文本给编辑器"没有协议载体

状态：Resolved

问题：session-runtime.md 要求"在 `AbortRun` 时返回被清空的 steering 与 follow-up 消息，供 UI 恢复到编辑器"，但 `CommandAck` 不携带数据，`queue_updated` 只发送清空后的完整队列快照（不含"被移除的消息"）。按现有协议，UI 只能靠本地缓存上一次 `queue_updated` 来恢复文本——这与"UI 权威输入只有 snapshot/event"的原则冲突。

证据：

- `docs/modules/session-runtime.md`：队列语义段。
- `docs/modules/agent-runtime-events.md`：Abort Lifecycle 中 `queue_updated?` 仅为快照；`QueueEvent::Updated` 无 removed 字段。

风险：pi 的这条产品体验在协议化后静默丢失，或以 UI 私有状态实现。

处理记录：明确放弃 core 层“abort 后归还队列文本给编辑器”的行为。editor draft、光标、submission history 和 undo state 属于 UI adapter；runtime queue 保存结构化 prompt intent，不保证保留 raw slash text，也不能可靠重建原始编辑器内容。因此 `CommandAck`、`QueueEvent` 和 snapshot 不增加 `returned_to_editor`、removed delta 或 abort 专属 UI 字段。

`AbortRun` 清除尚未消费的 steering/follow-up 和 pending session actions，保留不会自动启动 run 的 `NextTurn`；若 queue state 实际变化，则发布完整 `queue_updated` snapshot。已经在 safe point 完成 `UserInput` commit 的 steer 已属于 durable history，不可因 abort 删除。显式 `ClearQueue` 才清除 steering、follow-up、next-turn 和 pending actions 全部 queue state。具体 UI 可以基于本地 submission history 与 reducer state 提供同一 host 内的 best-effort restore，但它不是 core protocol、session storage 或 reconnect guarantee。

### BR-032：`skill_invoked` / `prompt_template_invoked` 的 `run_id` 必填与队列 delivery 冲突

状态：Resolved

处理记录：BR-027 明确 `RunId` 只在 `run_started` 边界分配后，本项决策同步修正为 optional run association。invoked 事件表示目标 `PromptTurn.resolve_intent()` 已实际展开且对应 durable input commit 成功，不再声称 idle admission 已绑定公开 run：idle/future-turn 使用 `run_id = None`，active Steer 使用 `Some(current_run_id)`。FollowUp/NextTurn 入队时仍只通过 `CommandAck` 和 `queue_updated` 表达受理，不提前发 invoked；展开或 commit 失败不发 invoked。

### BR-033：`EventMsg` 内层路由字段的有无不一致

状态：Open

问题：外层 `Event` 是路由权威，但部分事件族在 msg 内重复携带路由 id（SessionEvent、RunEvent、UsageEvent、CompactionEvent、RetryEvent、SkillEvent 带 session_id/run_id），另一部分完全不带（MessageEvent 只有 message_id、ToolCallEvent 只有 call_id、QueueEvent 什么都没有）。协议文档只对 `CommandResultEvent` / `UiInteractionRequest` 的 command_id 解释了“内层重复是为了脱离事件上下文渲染”，没有给出统一规则。

证据：

- `docs/modules/agent-runtime-protocol.md`：`EventMsg` 各族定义对比。

风险：UI reducer 对不同事件族要走两套取 id 逻辑；序列化后内外字段不一致时无仲裁规则。

待处理方向：定一条规则（全部依赖外层，或全部内层冗余）并统一各族定义。

### BR-034：`focused` 与 `active` 术语直接冲突，协议字段使用了术语表明令避免的词

状态：Open

问题：CONTEXT.md "聚焦会话（focused session）"词条明确 _避免_："active session"；事件也叫 `session_focus_changed`、字段 `focused_session_id`。但 `RuntimeSnapshot` 的字段是 `active_session_id` / `active_session`，其注释"runtime-visible 的当前默认会话目标"与 focused session 定义完全同义；CONTEXT.md 自己的 RuntimeSnapshot 词条也写"默认可以没有 active session"。同一概念在协议和术语表中使用互相禁止的两个名字。

证据：

- `CONTEXT.md`：聚焦会话词条 avoid 列表 vs RuntimeSnapshot 词条。
- `docs/modules/agent-runtime-protocol.md`：`RuntimeSnapshot.active_session_id` 注释。

风险：低成本但高频的认知摩擦；如果 active_session 实际是"focused 且 loaded 的投影"这类微妙差异，现在的文档也没有说清。

待处理方向：统一为一个词（建议协议字段改 `focused_session`），或在 CONTEXT.md 显式定义两者差异。

### BR-035：`RuntimeHookRegistry` 归属表述漂移；workspace 级 registry 与 per-cwd trust 的交互未定义

状态：Resolved

原问题：runtime-hooks.md 说 `AgentRuntime` 拥有 registry、`SessionRuntime` 持有"session-scoped view"；但 session-runtime.md 的内部结构图和 agent-runtime-events.md 的 ownership matrix 都把 `RuntimeHookRegistry` 直接列为 SessionRuntime 持有状态，无 view 字样。更深一层：registry 在 `WorkspaceServices`（workspace 级单例），而 hook 的 capability gate 依赖 `TrustLevel`。当前分支移除了 `CwdScopedServices`，因此 hook trust 应按调用时 session fixed cwd / resource load trust input 判定，但该规则仍未写成 source of truth。

证据：

- `docs/modules/runtime-hooks.md`：Registry And Ownership。
- `docs/modules/session-runtime.md`：内部结构图。
- `docs/modules/agent-runtime-events.md`：Ownership Matrix。
- `docs/modules/agent-runtime.md`：`RuntimeHookRegistry` 在 WorkspaceServices，resource trust 现在由 per-cwd resource loading 输入表达。

处理结果：已决策当前 MVP 不实现 hook system，`RuntimeHookRegistry` 是后期 workspace/runtime service；`SessionRuntime` 只持有 future session-scoped registry view。hook owner 固定为拥有对应安全点业务不变量的模块，`RuntimeHookRegistry` 不拥有业务流程。后期 trust / capability gate 必须由 hook owner 在调用时按当前上下文计算；session-scoped hook 使用该 `SessionRuntime` 的 fixed workspace/cwd 和对应 resource trust summary，不能使用 focused session 或全局默认 cwd。相关图和 ownership matrix 已同步改为 future hook service / future typed result。

剩余风险：等实际实现 hook system 时，还需要为每个 hook point 补具体 typed result、timeout、failure policy 和 conformance tests。

### BR-036：多 workspace 语义不闭合

状态：Open

问题：协议表面广泛携带 workspace 维度（`ReloadResources { workspace_id }`、`SessionListScope::AllWorkspaces / Workspace{id}`、`EventScope::Workspace`、registry key 含 workspace_id、`NewSession { workspace_id }`），但 `RuntimeSnapshot.workspace` 是单个 Option、Workspace Lifecycle 状态机是单 workspace 的（NoWorkspace → Open → …），`OpenWorkspace` 被第二次调用是替换、并存还是拒绝没有任何说明。roadmap 把多 workspace 列为后续增强，但协议已经按多 workspace 形状铺开。

证据：

- `docs/modules/agent-runtime-protocol.md`：Command/scope 定义 vs `RuntimeSnapshot` 单 workspace 字段。
- `docs/modules/agent-runtime-events.md`：Workspace Lifecycle。

风险：与 BR-001 同类的"形态先行"：实现者不知道该按单还是多 workspace 实现路由与 registry。

待处理方向：明确 MVP 单 workspace（`OpenWorkspace` 重复调用 = teardown 后替换或拒绝），workspace_id 仅作前向兼容字段；多 workspace 升级路径另立文档。

### BR-037：tool sandbox 被定位为真正的安全边界，但没有任何 source of truth

状态：Resolved

问题：文档多处依赖 sandbox：`ToolPolicyInput.sandbox: ToolSandboxView`、`CwdScopedServices` 含 `ToolSandboxRoot`、hook 规则"rewrite 后必须重新 sandbox check"、tools.md 明言"UI 审批只是 ToolPolicy 的输入，不是安全边界"（言下之意 sandbox 才是）、roadmap 阶段 15 要求 bash 的 cwd/sandbox 可观察。但 `ToolSandboxView` / `ToolSandboxRoot` 从未被定义，sandbox 的模型（路径边界？只读/读写域？进程/网络限制？平台差异？）在全部 16 个模块文档中没有一节描述，CONTEXT.md 也无词条。

证据：

- `docs/modules/tools.md`、`docs/modules/agent-runtime.md`、`docs/modules/runtime-hooks.md`、`docs/implementation-roadmap.md` 中的引用点。

风险：mutation/bash 阶段（roadmap 14/15）的核心安全语义完全靠实现时即兴决定；"重新 sandbox check"无从测试。

处理记录：已在 `docs/modules/tools.md` 的 "Sandbox Source Of Truth" 小节定义 `ToolSandboxView`、process/network/env policy、sandbox verdict、path canonicalization、symlink/父目录校验、denied roots 优先级、bash cwd/network 边界，以及 hook rewrite 后必须重新 validate/canonicalize/sandbox/policy 的规则；`CONTEXT.md` 也补充了 `ToolSandboxView` 术语。

## 低风险 / 一致性

### BR-038：failure → retry 的 phase 抖动，两处 lifecycle 描述不一致

状态：Open

问题：Failure Lifecycle 规定失败后 `session_phase_changed { idle }` → `session_settled or retry_auto_started`，即 retry 前 phase 必经 idle（UI 会闪现空闲态）；而 Session Phase Lifecycle 图画的是 `Idle -- session_phase_changed(retry) --> Retry -- run_started --> Turn`。retry 期间 phase 究竟是 idle、retry 还是 turn，两图拼不出一致答案。

证据：`docs/modules/agent-runtime-events.md` Failure Lifecycle vs Session Phase Lifecycle。

待处理方向：给出 failed → retry 的完整 phase/事件时序，消除 idle 抖动或明确接受它。

### BR-039：协议字段使用 `usize`

状态：Resolved

问题：原 `ListSessions { limit: usize }`、`SessionEvent::Settled { next_turn_count: usize }` 使用平台相关宽度类型；协议其余部分统一 u32/u64。wire 协议类型应固定宽度（参考 Codex 用 i64）。

证据：`docs/modules/agent-runtime-protocol.md`。

处理记录：已将公开协议中现归属 `SessionQuery::List` 的 `limit` 改为 `u32`，`SessionEvent::Settled.next_turn_count` 改为 `u64`，`ToolApprovalPreview::FileEdit.replacements` 改为 `u32`。内部实现文档若使用数组索引，可以在模块内部转换，但 wire 协议不再暴露裸 `usize`。

### BR-040：`ToolPolicy` 声明为纯判断器，却要构造需要 I/O 的 `ToolApprovalPreview`

状态：Resolved

问题：tools.md 说 "ToolPolicy constructs it（ToolApprovalPreview）"，而 `FileEdit { diff }` / `Patch { diff_preview }` 类 preview 需要读取目标文件计算 diff——与"ToolPolicy 是纯策略判断器"的定位矛盾。

证据：`docs/modules/tools.md` 数据结构草案注释与 ToolPolicy 定位段。

处理记录：已在 `docs/modules/tools.md` 中明确 `ToolPolicy` 是纯判断器，不等待 UI、不执行工具、不构造需要 I/O 的 preview；diff preview、patch preview、bash command preview、path canonicalization 和 sandbox check 都由 `ToolInvocationPlanner` 在 policy 前准备，policy 只返回 `RequireApproval { reason }` 等 decision。

### BR-041：`GetContextFile { path }` 未定义校验边界

状态：Resolved

处理记录：已在 `docs/modules/agent-runtime-protocol.md` 中规定：`ResourceQuery::GetContextFile` 的 path 必须命中当前 cwd `CwdResourceSnapshot` 已登记的 context file canonical path；读取必须经过 `AgentRuntime` / `ResourceManager`，不能由 UI adapter 直接读文件。

问题：该命令以任意 `PathBuf` 为参数，文档只说"读取必须经过 AgentRuntime / ResourceManager"，未规定 path 必须命中当前资源快照中已登记的 context file。不加约束就是一条 UI 任意读文件通道。

证据：`docs/modules/agent-runtime-protocol.md` 后续命令段。

待处理方向：已处理。实现时需加入 canonical path 匹配测试。

### BR-042：`SessionEntry::Leaf` 条目与 `SessionStorage::set_leaf_id()` 双机制关系未定义

状态：Resolved

处理记录：ADR 0019 的 batch writer 同时移除了 `SessionStorage::set_leaf_id()` 和 `SessionEntry::Leaf`。每个 committed `session_batch` 携带唯一 `BatchLeafUpdate`：所有追加 entries 的 batch（包括 metadata-only `SessionMutation`）使用 `AdvanceToLastEntry`，只有不追加 entry 的导航使用 `MoveTo(target)`。target 必须是 committed append batch 的最后一个 entry；`ToolRound` interior entry 会被拒绝，避免恢复半个 stable batch。writer 在同一 commit 内原子处理 entries 与 logical current leaf；`get_path_to_root()` 只遍历领域 entries，不存在 marker entry 跳过规则。InMemory 和 JSONL adapters 必须复用该 contract。

### BR-043：`CwdScopedServices` generation 的回收策略未定义

状态：Resolved

处理记录：当前分支移除了 `CwdScopedServices` / service generation pinning。reload 不再创建一整套 cwd-scoped service generation，而是为目标 `(workspace_id, cwd)` 生成新的 immutable `CwdResourceSnapshot` 并原子替换 `ResourceSnapshotStore` 的 current cwd pointer。旧 snapshot 只由正在 running 的 `TurnResourceSnapshot` / `CurrentRun` 持有，引用计数归零后自然释放。

问题：reload 会为同一 cwd 产生新 generation，旧 generation 被后台 session pin 住；文档定义了 SessionRuntime 的卸载条件，但没有定义 generation 本身的回收（引用计数？最后一个 pin 释放后丢弃？），长会话 + 频繁 reload 会累积服务实例（含 ModelGateway、ResourceManager 全套）。

证据：`docs/modules/agent-runtime.md` 运行时服务段。

待处理方向：已处理。实现上需要验证旧 `CwdResourceSnapshot` / `TurnResourceSnapshot` 在 running run 持有期间可用，run 结束后不再被 current store 引用即可释放。

### BR-044：`EventStream` 的订阅语义未定义

状态：Open

问题：`subscribe()` 的多订阅者语义（新订阅者从何处开始）、慢消费者/背压策略（lag 时是丢事件产生 sequence gap 还是阻塞发布者）、ring buffer 容量与 gap recovery 的关系都未定义，而 sequence gap recovery 恰恰依赖这些行为。

证据：`docs/modules/agent-runtime-protocol.md`（EventStream 仅出现类型名）；`docs/modules/agent-runtime-events.md` RuntimeSnapshot 水位段提及 ring buffer。

待处理方向：在 AgentRuntimeEvents 中定义订阅起点、lag 策略与 gap 的产生条件。

### BR-045：CONTEXT.md 会话条目类型列表滞后

状态：Resolved

处理记录：CONTEXT.md 已同步当前 entry families，并明确完整集合以 SessionManager 文档为准；`leaf` 不再是 entry，current leaf 由 committed batch 的 `BatchLeafUpdate` 维护。

### BR-046：`UiInteractionSubmit::ExecuteSlashCommandTemplate` 的占位符语法未定义

状态：Resolved

原问题：MVP 依赖 picker 选择后按模板重新提交 slash command（示例 `/model {item.id}`），但模板占位符语法（可用变量、转义规则）没有定义，TUI/GUI 各自实现必然分叉。

决策结果：移除 template submit 通道。UI 选择 command catalog item 后提交 `ExecuteCatalogCommand { selection, args, prompt_delivery }`；若确实是文本入口，则提交明确的 `ExecuteCommandText { raw, prompt_delivery }`。runtime 收到后重新 materialize catalog 并执行 `resolve_for_execution`，不需要 UI 实现占位符模板语言。`prompt_delivery` 只在节点产生 prompt intent 时生效。

## 过程性观察（不编号）

1. **字段级设计先行于 Rig 验证。** roadmap 正确地把 Rig spike 排在阶段 4，但 `DriveEntry::Resume { serialized_run }`、`DriveResult::Suspended`、usage extraction、cancellation 等大量字段级类型已按未验证的 Rig sans-IO 假设写死。command-surface.md 有"类型片段是设计草图"的免责声明，其他文档（尤其 protocol.md）没有——建议统一标注哪些类型是承诺、哪些是草图，并在 spike 前冻结字段级细节的进一步扩张。
2. **重复边界声明是已被证实的漂移温床。** 例如"hook 不能发 event / 读写 storage / 执行工具 / 读凭据"在 ≥6 个文档中整段复制；本轮 BR-026/BR-034/BR-035 与第一轮 BR-010/BR-012/BR-022 的漂移多发生在复制文本上。建议机械执行权威归属表：非权威文档一句话 + 链接，禁止复制列表和图。
3. **协议表面积与 MVP 范围不匹配。** 20+ "后续命令"已给出字段级定义（fork/import/export/tree/interaction submit/cycle 等），与 BR-001 的教训同构。建议后续命令只保留名字与一句话意图，字段定义等实现临近时再补。
