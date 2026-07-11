# SessionRuntime

`SessionRuntime` 是单个会话的产品级 Agent 编排对象。它拥有会话状态、队列、工具、模型状态、运行生命周期和事件归约。每个 `SessionRuntime` 固定一个 workspace cwd；每次 run 启动时通过 `ResourceManager.capture_turn(...)` 捕获当前 `TurnResourceSnapshot`，并把它放入本次 `TurnState`，使后台 run 不受 focused session 切换或资源 reload 影响。

## 核心职责

- 状态访问：messages、current run、model、thinking level、system prompt、active tools、all tools、resources、queues、session id、session name、session file、retry attempt、context usage 和 session stats。
- 事件与持久化：订阅 `Driver` events，归约 streaming state、pending tool calls 和 error state；在 message/save point 上追加 session entries；通过 `AgentRuntime` event bus 向 UI 发布 `agent_runtime_protocol::Event`。
- command 入口：持有 session-scoped `command: Command`，为当前 session 构造 `CommandContext` / `SessionCommandHost`，并调用共享无状态 `CommandManager` 处理 `ExecuteCommandText` / `ExecuteCatalogCommand`。
- prompt 入口：处理 `SubmitPrompt` 和其他结构化 `PromptIntent`，执行输入预检、模型/凭据校验、附件策略和 `PromptDelivery` admission；目标 turn 由 `PromptTurn.resolve_intent()` 统一展开 skill/template。
- 队列入口：支持 `Steer`、`FollowUp`、`NextTurn`、`ClearQueue`，并持有结构化 `PendingSessionAction`；运行中输入或 deferred session action 不能绕过当前 work 的顺序。
- 自定义消息入口：为后续 extension/runtime command 支持 custom message，允许“只写入会话”或“写入后触发 turn”。
- 运行生命周期：启动 run、continue、post-run 处理、失败消息构造、abort、wait-for-idle、phase 切换和 settled 事件。
- 运行输入 scope：持有固定 `workspace_id` / `cwd`；run 启动时捕获当前 cwd 的 `TurnResourceSnapshot` 并构建 `TurnState`，resource reload 只影响 future run，不会改写正在运行的后台 run。
- 模型与思考等级：支持 set/cycle model、模型认证检查、恢复失败 fallback、set/cycle thinking level、能力裁剪和持久化。
- 工具与提示词配置：持有 session-scoped `Tools` 子系统；在 turn start 汇合 captured `PromptResourceView`、tool/model/agent/environment/policy views 创建 immutable `PromptTurn`。active tools 或模型可见 profile 在安全点变化时，使用同一 captured resources 重建并整体替换 `PromptCallProfile`。
- 资源与扩展：在 user turn 启动时引用当前 `TurnResourceSnapshot`；该 snapshot pin 住 `CwdResourceSnapshot`，而 cwd snapshot pin 住对应 `RuntimeResourceSnapshot`。reload 后 future turn 使用新资源，运行中的 turn 不被 patch。
- 压缩与重试：支持手动压缩、自动压缩、context overflow 恢复、自动重试、取消压缩和取消重试。
- 会话树：支持 set session name、navigate tree、branch summary、fork selector 所需 user message 列表。
- shell 能力：后置支持 bash/shell 执行、输出流、结果记录、取消和 pending shell message flush；MVP 默认不开启。
- 导出与辅助：支持 session stats、context usage、HTML/JSONL 导出、last assistant text。

## 内部结构

```text
SessionRuntime
  ├─ WorkspaceId / cwd
  ├─ ResourceManager handle
  ├─ SessionHandle
  ├─ SessionPhase
  ├─ CurrentRun { PromptTurn / current-run context }
  ├─ PendingSessionWrites
  ├─ CurrentRunUsage / SessionUsageStats / ContextUsage
  ├─ ModelState
  ├─ ResourceState { last_seen_revision }
  ├─ CompactionState
  ├─ Command
  ├─ Tools
  ├─ QueueState
  ├─ PendingSessionActions
  ├─ RuntimeHookRegistry        // future hook service view
  └─ Driver
```

`SessionRuntime` 是产品编排层，不应把 Rig 类型泄漏给 UI，也不应把工具执行交给 UI。

`SessionRuntime` 不持有跨 turn 的 current resource cache。它可以记录 `ResourceState { last_seen_revision }` 供 UI 或 diagnostics 使用，但不能用它代替 `ResourceManager.capture_turn(...)`。每次 user turn 真正启动时都必须重新 capture，当 reload 已经完成 `ResourceSnapshotStore::replace_cwd(...)` 后，下一次 capture 自然会读取到新的 current `CwdResourceSnapshot`；active run 继续使用 `CurrentRun.prompt_turn` pin 住的旧 snapshot。

`SessionRuntime` 持有 session-scoped `Command` facade，但不持有 catalog cache。`Command` 每次请求都基于当前 session 的 cwd、resource summary、model、tools、run state 和 settings 构造 `CommandContext`，再调用共享 `CommandManager` materialize/parse/suggest/resolve。只有 resolved 后的 session-scoped 结构化命令或 prompt intent 才进入 `SessionRuntime`。`CommandRunPolicy` 决定 handler 立即执行、active work 中拒绝或保存为 typed pending action；`PromptDelivery` 决定模型可见输入进入 steer/follow-up/next-turn。两者不能互相转换。

## Session Phase

`SessionPhase` 的权威封闭集合是 `Idle | Turn | Compaction | RetryBackoff`。它只描述哪个 session-level workflow 当前拥有互斥写入/调度权，不复制 run、tool、approval 或 model-call 子状态。

| Phase | `current_run` | 允许的核心动作 |
| --- | --- | --- |
| `Idle` | `None` | 立即启动 prompt turn 或 compaction；执行读取型/配置型命令 |
| `Turn` | 通常 `Some`；preflight/post-run 短窗口可暂为 `None` | 接收 PromptDelivery queue；处理 active run、approval、suspend/resume、持久化与 post-run arbitration |
| `Compaction` | `None` | 只推进当前 compaction；prompt intent 可以排队但不能启动 Agent run |
| `RetryBackoff` | `None` | 等待/取消 Agent run retry；prompt intent 可以排队但不能绕过既定 work chain |

phase guard 还必须结合 `CurrentRunState` 和 command policy：`DecideToolApproval` 只在 `Turn + WaitingApproval` 合法，`ResumeRun` 只在 `Turn + Suspended` 合法；`AbortCompaction` 只在 `Compaction` 合法，`AbortRetry` 只在 `RetryBackoff` 合法。`Compact` 在 `Idle` 立即进入 `Compaction`，在 `Turn` / `RetryBackoff` 保存唯一 pending action，在 `Compaction` 返回 `CompactionAlreadyRunning`。

`Turn` 可以包含连续多个 run；只要 immediate retry/continuation、pending action 或 overflow recovery 将立即继续，就不经过 `Idle`，也不发送 `session_settled`。完整转换矩阵见 [AgentRuntimeEvents](agent-runtime-events.md)。`BranchSummary` 不属于 MVP phase；未来若实现独立摘要模型任务，应定义明确 `ModelCallPurpose` 和 lifecycle，而不是预留幽灵 phase。

## Prompt Admission And Turn Assembly

`SessionRuntime` 负责何时接收、排队和启动 prompt-like work，但不再手工拼 skill/template/system prompt。边界如下：

```text
CommandManager / protocol
  → structured PromptIntent
  → SessionRuntime.admit_prompt_intent(intent, delivery)

SessionRuntime at target delivery boundary
  → ResourceManager.capture_turn(...)
  → PromptResourceView + tool/model/agent/environment/policy views
  → prompt::begin_turn(...)
  → PromptTurn.resolve_intent(intent)
  → ResolvedPromptInput
  → persist current user input / start Driver
```

规则：

1. idle submission、`FollowUp` 和 `NextTurn` 在目标 future turn 启动时 capture current resources，再创建新 `PromptTurn`。
2. active `Steer` 在 `before_next_model_call` 使用 `CurrentRun.prompt_turn` 展开，因此继续使用 active turn snapshot。
3. queue 只保存结构化 `PromptIntent` 的 resource key、args、additional instructions 和附件引用；不保存 raw slash text 或提前展开正文。
4. `PromptTurn.resolve_intent()` 从 captured `PromptResourceView` 查询 selected `SkillResource` / `PromptTemplateResource`，调用 `skills.rs` / `prompt_templates.rs` helper，并返回 `ResolvedPromptInput`。
5. active snapshot 缺少目标资源时返回 `SkillUnavailableInTurnSnapshot` / `PromptTemplateUnavailableInTurnSnapshot`；不能重新读取文件、使用 current 新 revision 或静默改变 delivery。
6. 已经展开并持久化的 skill/template invocation 是历史 user message，后续 reload 不改写它。

## PromptTurn Lifecycle

`SessionRuntime` 是 Prompt 的 Pull Master：

```text
TurnResourceSnapshot.prompt_view()
Tools.prompt_view()
ModelState.prompt_view()
Product / agent / environment / policy views
  → prompt::begin_turn(TurnPromptInputs { ... })
  → PromptTurn { PromptCallProfile, contribution stamps, fingerprint }
```

创建时机包括新 user turn、follow-up/next-turn continuation 和 overflow recovery 后的新 continuation。普通 resource reload 不 patch active `PromptTurn`。

如果运行中合法切换模型可见工具、agent profile 或其他会影响 system prompt 的 profile，`SessionRuntime` 在 `before_next_model_call` 安全点使用 active turn 的同一个 `PromptResourceView` 重新执行 `begin_turn()`，并整体替换 `PromptCallProfile`。不能分别 patch system prompt 和 active tool schemas。

MVP 中 Rig step 不调用 `ResourceManager`。完整 resources 和 `PromptTurn` 留在 `TurnState` / `SessionDriverHost`；跨到 `Driver` 的只有窄 `DriverTurnInput { model, prompt: PromptCallProfile, thinking_level, stream_options }`。未来 `StepResourceSnapshot` 也必须以 active `TurnResourceSnapshot` 为 parent，不能读取 ResourceManager current pointer。

## Model State And Selection

`SessionRuntime` 拥有会话级 `ModelState`，但不拥有 provider client、API key、OAuth token、base URL 解析或 raw provider payload。模型调用的执行边界见 [ModelGateway](model-gateway.md)。

```rust
pub struct ModelState {
    pub selected: ModelSelection,
    pub thinking_level: ThinkingLevel,
    pub stream_options: StreamOptions,
    pub fallback_policy: Option<ModelFallbackPolicy>,
}

pub struct ModelSelection {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
}

pub struct ActiveModel {
    pub selection: ModelSelection,
    pub summary: ModelSummary,
    pub capabilities: ModelCapabilities,
}
```

生命周期：

1. 会话创建时，从 settings/default model 初始化 `ModelState.selected`。
2. 会话恢复时，从 root-to-leaf path 上最新 `SessionEntry::ModelChange { provider_id, model_id }` 恢复选择；如果 provider/model 已失效，通过 `ProviderRegistry` fallback，并记录 diagnostics / `model_fallback_message`。
3. `SetModel` / `CycleModel` 只更新 `ModelSelection`，写入 `SessionEntry::ModelChange`，并发 `session_model_changed`；它们不构造 provider client，也不读取 credentials。
4. `SetThinkingLevel` / `CycleThinkingLevel` 会按当前 `ModelCapabilities` 裁剪；不支持 thinking 的模型应降级为 `ThinkingLevel::Off` 或返回结构化错误。
5. 每次启动 run 前，`SessionRuntime` 从 `ModelState` 和 `ProviderRegistry` 构造 `ActiveModel`，放进 `TurnState`。
6. 运行中切换模型可以先被 phase guard 拒绝；完整版本可在 `before_next_model_call` 安全点由 `SessionRuntime` 更新会话事实，并原子替换 future model/options 与必要的 `PromptCallProfile`，但不能替换已经发出的 provider request。

`ModelSummary` 是 UI/snapshot view，不是执行路径身份。执行路径必须使用 `ModelSelection { provider_id, model_id }`，避免把显示名、provider API model name 或 Rig 类型写进 session。

## TurnState

每次启动模型 turn 前，`SessionRuntime` 创建一次稳定快照：

```rust
pub struct TurnState {
    pub resource_revision: ResourceRevision,
    pub resources: Arc<TurnResourceSnapshot>,
    pub prompt_turn: Arc<PromptTurn>,
    pub messages: Vec<MessageRecord>,
    pub stream_options: StreamOptions,
    pub session_id: SessionId,
    pub model: ActiveModel,
    pub thinking_level: ThinkingLevel,
    pub tools: Vec<ToolDefinitionView>,
    pub active_tools: Vec<ToolDefinitionView>,
    pub context_usage: Option<ContextUsageView>,
}
```

`TurnState` 是 `SessionRuntime` 的内部 run snapshot。它 pin 住 resources、immutable `PromptTurn`、模型状态、工具视图、消息基线和 context usage，以保证 running run 不受 reload 或 focus 切换影响；它不是 `Driver` 的输入类型。

`SessionRuntime` 在启动 `Driver` 前投影出更窄的输入：

```rust
pub struct DriverTurnInput {
    pub model: ModelSelection,
    pub prompt: PromptCallProfile,
    pub thinking_level: ThinkingLevel,
    pub stream_options: StreamOptions,
}

impl TurnState {
    pub fn driver_turn_input(&self) -> DriverTurnInput {
        DriverTurnInput {
            model: self.model.selection.clone(),
            prompt: self.prompt_turn.profile().clone(),
            thinking_level: self.thinking_level,
            stream_options: self.stream_options.clone(),
        }
    }
}
```

`DriverTurnInput` 不能包含 `Arc<TurnResourceSnapshot>`、`PromptTurn`、`ResourceRevision`、`ContextUsageView`、全量 tools registry、executor handle、approval/policy state、queue state 或 storage handle。`PromptCallProfile` 是窄的模型可见基线，不允许 Driver 访问 resource catalogs。

运行中如果用户切换模型或活跃工具，`SessionRuntime` 先记录待应用事实，在 `DriverHost::before_next_model_call` 安全点使用 active captured resources 创建新的 immutable `PromptTurn`，并在同一临界区替换 `CurrentRun.prompt_turn`，通过组合式 `NextModelCallPlan.prompt_profile` 整体替换 future profile。旧 `PromptTurn` 不原地修改。资源 reload 不 patch 当前 `TurnState`；当前 run 的新旧 PromptTurn 都继续引用启动时捕获的 resources。provider 解析和 auth 注入仍只发生在 `ModelGateway`。

## Driver / Tools Coordination

`SessionRuntime` 长期存活并持有 session 状态；`Driver` 基本无状态，只负责推进 Rig `AgentRun`；`DriverHost` 是一次 run 期间让 `Driver` 回调外部能力的 trait seam。

代码形态可以简化为：

```rust
pub struct SessionRuntime {
    session_id: SessionId,
    cwd: PathBuf,
    tools: Tools,
    services: Arc<WorkspaceServices>,
    event_sink: SessionEventSink,
    queues: QueueState,
    current_run: Option<CurrentRun>,
}
```

`Tools` 是 session-owned 子系统，因为它持有 active tools、pending approvals、approval mode、grants、sandbox/mutation state 等会话中间状态。`Driver` 只是执行协作者；它可以在 run start 时临时创建或 clone，不应该携带 session identity。

工具调用不由 `Driver` 直接执行。`SessionRuntime` 可以直接实现 `DriverHost`，但真实实现更推荐创建 per-run `SessionDriverHost` wrapper：

```rust
let run_id = self.allocate_run_id();
let turn_resources = turn_state.resources.clone();

let request = DriveRequest {
    run_id,
    session_id: self.session_id,
    entry,
    turn: turn_state.driver_turn_input(),
    limits: self.drive_limits(),
};

let mut host = SessionDriverHost {
    session_id: self.session_id,
    run_id,
    cwd: self.cwd.clone(),
    turn_resources,
    tools: &mut self.tools,
    model_gateway: &self.services.model_gateway,
    event_sink: &mut self.event_sink,
    queues: &mut self.queues,
    current_run: &mut self.current_run,
};

let driver = Driver::new();
driver.drive_run(request, &mut host, cancel).await
```

该 wrapper 实现下面的 seam：

```text
DriverHost::invoke_tool_batch(request, updates, cancel)
  → SessionRuntime attaches ToolRunContext { session_id, run_id, cwd, turn resources, abort signal }
  → Tools::invoke_batch(request, context, updates, cancel)
  → ToolBatchResult
```

`Tools` 返回内部工具结果和 update；`SessionRuntime` 负责归约 UI event、session writes、pending approval snapshot projection 和 save point。`Driver` 只把 `ToolBatchResult` 映射回 Rig tool results，并继续推进 `AgentRun`。

选择 `SessionDriverHost` 而不是只写 `impl DriverHost for SessionRuntime` 的缘由：

- 直接实现是合法的最小版本，但 wrapper 可以限制 `DriverHost` 方法只能访问本次 run 需要的字段，避免把整个 `SessionRuntime` 暴露给 driver seam。
- wrapper 将 `run_id`、turn resources、event correlation、checkpoint context 等 run-scoped 数据与 session-scoped state 分离。
- `DriveRequest` 只带 `DriverTurnInput`，而 `SessionDriverHost` 持有 turn resources；这同时收窄了 driver 主输入和 host 回调上下文。
- wrapper 避免 `self.driver.drive_run(..., self, ...)` 这类 Rust 自借用形态；`Driver` 越无状态，这个边界越简单。
- wrapper 让 `Driver` 单测只需要 fake host，不需要构造真实 session runtime、tools、storage 和 event bus。

## Usage And Context Usage

token 消耗统计和上下文占用口径见 [UsageStats](usage-stats.md)。`SessionRuntime` 是这两类 UI view 的 owner：

```text
ModelGateway
  → normalizes provider raw usage into ModelCallUsage

Driver
  → accumulates usage for current drive_run only
  → returns DriveResult / DriverEvent usage facts

SessionRuntime
  → updates CurrentRunUsage
  → updates SessionUsageStats
  → computes ContextUsage from latest ModelInputProjection
  → emits usage_updated
  → includes usage in run_finished and agent_runtime_protocol::RuntimeSnapshot.active_session
```

`UsageSummary` 是一次 run 内所有模型调用的消耗汇总，不是最后一次模型调用。一次 run 如果经历 `model -> tools -> model -> tools -> model`，`run_finished { usage }` 必须覆盖这几次模型调用。

`ContextUsageView` 表示下一次模型请求会占用多少上下文窗口。它应优先使用最近一次有效 assistant provider usage，再加上后续消息的本地估算；如果没有 provider usage，才估算整个模型可见上下文。

压缩只改变 `ContextUsageView.current_tokens`，不减少 `SessionStatsView.total_usage`。会话累计消耗是历史账本；当前上下文占用是窗口状态。

`usage_updated` 是实时 UI 事件，不代表 stats 已经持久化。可恢复边界仍然由 `persistence_save_point` 表达。UI 重连时以 `agent_runtime_protocol::RuntimeSnapshot.active_session.session_stats` 和 `agent_runtime_protocol::RuntimeSnapshot.active_session.context_usage` 为权威恢复显示。

## RuntimeHooks

Hook 的边界、capability、typed result 和安全点规划见 [RuntimeHooks](runtime-hooks.md)。当前 MVP 不实现 hook system；后期启用时，`SessionRuntime` 只负责在自己拥有的会话状态机安全点调用 hook，并把 hook 结果应用到会话事实上。它不把 hook 暴露给 UI，也不让 hook 直接发布 `agent_runtime_protocol::Event`。

后期由 `SessionRuntime` 拥有的 hook 点：

- `BeforeAgentStart`：启动 run 前 transform/gate 结构化 current input 或 stream options。
- `PromptBuilt`：`PromptTurn` 形成 profile 后追加或 privileged replace system section；应用后必须重建并重新 fingerprint `PromptCallProfile`。
- `ContextProjection`：为 `CurrentRun` / `CurrentCall` 返回显式 `ContextMaterialContribution::Available/Unavailable`，或在 privileged capability 下替换模型可见 messages；最终必须回到 Prompt 校验。
- `BeforeNextModelCall` / `BeforeRunFinish`：run 级安全点，用于处理队列、暂停、follow-up、组合式 `NextModelCallPlan` 或 run 结束决策。
- `ToolResultBeforeAppend`：tool-result message 写入前的受控干预；工具治理链路内部 hook 由 `Tools` 拥有。
- `SessionBeforeCompact`：压缩前取消、追加说明或提供完整 `CompactionResult`。
- `AfterSavePoint`：保存点后做 observer 型同步、索引、备份或 telemetry。

模型/provider 边界 hook 不属于 `SessionRuntime`：`BeforeModelCall`、`BeforeProviderPayload`、`AfterProviderResponse` 和 `ProviderUsageNormalized` 由 `ModelGateway` 在 `call_model(...)` 内拥有。`Driver` 不调用 hook。`SessionRuntime` 应按自己 hook point 的 error policy 处理失败：observer hook 失败进入 diagnostics；context 变换失败不得产生半写入 session entry。

## Compaction Orchestration

压缩由 `SessionRuntime` 编排，算法和 prompt helper 放在平级 [Compaction](compaction.md) 模块。`Driver` 不执行压缩，只把 usage、完成消息和 provider error 归约给 `SessionRuntime`。

手动压缩流程：

1. 收到 `agent_runtime_protocol::AgentCommand::Compact { instructions }`。
2. 若 session 已 `idle`，立即进入第 4 步。若存在 active run、waiting approval、suspended run 或立即 retry chain，则保存唯一 `PendingSessionAction::Compact { command_id, instructions }`，发 `queue_updated`，不 abort、不改 phase、不清理任何 message queue。
3. 当前 work terminal event 和相关 save point 完成后，如果没有立即 retry / overflow recovery，先移除 pending compact 并发 `queue_updated`，再切换到 `SessionPhase::Compaction`；该动作优先于 queued steering continuation / follow-up。若 `AbortRun`、`ClearQueue`、session close 或 shutdown 发生，则移除 pending compact，不启动压缩。重复 `Compact` 返回 `CompactAlreadyQueued`；已处于 compaction phase 返回 `CompactionAlreadyRunning`。
4. 读取当前 leaf 的 root-to-leaf `SessionEntry` path。
5. 调用 `compaction::prepare_compaction(path, settings)`，得到 `CompactionPreparation`。
6. 后期启用 hook system 时，触发 `RuntimeHookRegistry.invoke(SessionBeforeCompact)`；Hook 可以取消、patch instructions 或提供完整 `CompactionResult`。当前 MVP 直接进入下一步。
7. 如果未提供 hook result，则让 `Compaction` 构造 `CompactionSummaryMaterial`；`SessionRuntime` 选择摘要模型和调用选项，构造 `ModelCallPurpose::CompactionSummary` 的唯一 `ModelCallRequest`，再通过 ModelGateway 调用摘要模型；这不是 `Driver.drive_run()`。
8. 追加 `SessionEntry::Compaction`，再调用 `SessionHandle` 的上下文构建能力重建 messages。
9. 发出 `compaction_finished` 和 `persistence_save_point`，随后 phase 回到 `idle`；如果还有 queued steering continuation / follow-up，则继续对应 work，否则发出 `session_settled`。`NextTurn` 可以在 settled 状态保留。如果是 overflow recovery 且需要立即重试，则不先发 `session_settled`，直接启动后续 run。

自动压缩流程：

1. `Driver` 返回 `DriveResult::Completed` 后，`SessionRuntime` 写入消息并 flush save point。
2. 从最新 assistant usage 或估算值计算 context usage。
3. 如果 `compaction::should_compact(...)` 为 true，执行 `run_auto_compaction(reason = Threshold, will_retry = false)`。
4. threshold 压缩完成后不重跑刚完成的回答；如果队列里还有 queued steering continuation / follow-up，再从重建后的上下文继续。`NextTurn` 等下一次显式 prompt。

overflow recovery 流程：

1. `DriveResult::Failed` 中的错误被识别为当前模型的 context overflow。
2. 如果本轮尚未尝试过 overflow recovery，执行 `run_auto_compaction(reason = Overflow, will_retry = true)`。
3. transient overflow failure 如果要持久化，必须标记为 `ContextVisibility::UiOnly` 或写成 diagnostic/custom entry，不能进入重试上下文。
4. 压缩成功后，用重建后的 session context 启动新的 `DriveEntry::Continue { reason: ContextOverflowRecovery }`。
5. 不使用 `DriveEntry::Resume` 恢复旧 `AgentRun`，因为旧 serialized run 内含压缩前 history。

压缩摘要消息进入 `TurnState.messages` / durable history，不进入 `PromptCallProfile.system_prompt`。启动新 prompt 时应把压缩后的 durable history 与受保护 current input 分开交给 `PromptTurn.project_model_call()`；当前输入不能在同一启动边界上被摘要。

## 队列语义

目标队列能力使用 MiniCore 自己的 `PromptDelivery`、`QueueKind` 和 `QueueMode` 表达：

- steering queue：`PromptDelivery::Steer` 的模型可见输入；在 active run 的最早 `before_next_model_call` 安全点注入。session idle 时直接启动 run；compaction、suspended 等暂时无安全点的状态保留队列，不能静默改写成 follow-up。
- follow-up queue：`PromptDelivery::FollowUp` 的模型可见输入；不修改 active run，在当前 work chain、必要 retry/recovery 和 pending session action 完成后启动后续运行。session idle 时直接启动。
- `nextTurnQueue`：无论当前是否运行，都排到下一次用户 turn 前，与下一次显式 prompt 一起进入上下文。
- `PendingSessionActions`：`CommandRunPolicy::QueueAfterRun` 产生的不进入模型上下文的结构化 post-run action。当前只支持一个 pending manual compact；它在当前 work chain 稳定结束后、queued steering continuation / follow-up 之前执行。

公开 prompt-producing 入口统一调用：

```text
SessionRuntime.admit_prompt_intent(intent, PromptDelivery)
  → Steer     → start now or steering queue
  → FollowUp  → start now or follow-up queue
  → NextTurn  → nextTurnQueue
```

不存在独立的 `Steer` / `FollowUp` / `NextTurn` protocol command。普通 prompt、skill、prompt template 和 prompt-producing slash command 都必须经过同一个 admission path，因此 phase guard、queue event、resource capture 和后期 hook 不会分叉。slash command 只能把已经 resolve 的结构化 prompt intent 交给该入口，不能把 raw slash text 放入消息队列。

`QueueMode::All` / `OneAtATime` 只控制 steering/follow-up 每个安全点消费全部还是一条。运行时应在消息队列或 pending action 变化时发出完整 `queue_updated`。`AbortRun` / `ClearQueue` 清除 pending manual compact；pending action 不作为文本返回编辑器，因为它不是 `QueuedMessage`。

post-run arbitration 顺序固定为：

```text
terminal run facts + persistence save point
  → required overflow recovery / immediate retry chain
  → pending manual compact
  → threshold auto compaction（若 manual compact 已执行则跳过重复压缩）
  → queued steering continuation（仅当前没有可注入的 active run 时）
  → follow-up continuation
  → session_settled（next-turn queue 可以保持非空）
```

Rig 的 `AgentRun` 是否提供运行中注入点必须通过 Driver seam 验证；未实现 `before_next_model_call` 注入能力时，runtime 必须把 `Steer` 标记为不可用或返回 `SteerUnavailable`，不能静默降级为 follow-up。完整实现由 `Driver` 在 `before_next_model_call` 安全点让 `SessionRuntime` 检查 steering 队列。

## Next Model Call Plan

同一个安全点可能同时发生 steer、active tools/model profile 更新、RAG/memory context 和控制决策，因此返回组合式 plan，而不是互斥 enum：

```rust
pub struct NextModelCallPlan {
    pub control: NextModelCallControl,
    pub persistent_messages: Vec<MessageRecord>,
    pub prompt_profile: Option<PromptCallProfile>,
    pub current_run_context: Vec<ContextMaterialContribution>,
    pub current_call_context: Vec<ContextMaterialContribution>,
}
```

`persistent_messages` 是已由 active `PromptTurn.resolve_intent()` 展开的 steer，必须进入 Rig/run history；`prompt_profile` 只能是用 active captured resources 重建后的完整 profile；`current_call_context` 只影响下一次调用。context owner 的获取失败也必须作为 `Unavailable` 保留，不能通过缺项吞掉 required failure。`SessionRuntime` 不直接构造最终 messages，它把 plan 返回给 Driver，由 Driver 调用 Prompt 的 final projection seam。

## Command Run Policy

`SessionRuntime` 是 command run policy 的最终执行 owner：

```text
resolved command
  → Immediate     → 立即调用 handler；query output 可并发追加到 message panel
  → IdleOnly      → idle 时执行，active work 时返回 command output error
  → QueueAfterRun → idle 时执行，active work 时保存 typed PendingSessionAction
```

`CommandManager` 只解析、校验和绑定 policy，不持有队列。`QueueAfterRun` 不能保存 raw command text、完整 `AgentCommand` 或稍后重放任意 handler；每种 command 必须显式映射到 `PendingSessionAction` variant。prompt-producing command 通常使用 `Immediate` 完成 resolve，然后调用 `admit_prompt_intent(...)`，其消息时机由 `PromptDelivery` 决定。

## 运行流程

1. 校验当前 `SessionPhase` 是否允许启动运行，并切换为 `turn`。
2. capture `TurnResourceSnapshot`，创建 `PromptTurn`，再让它展开目标 `PromptIntent`。
3. 将 resolved current input 写入 session，形成 user-message `persistence_save_point`；构建不含重复 current input 的 durable history baseline。
4. 构建 `TurnState { resources, prompt_turn, messages, model, tools, ... }`；后期可触发 `BeforeAgentStart` / `PromptBuilt`，应用结果后重新校验 profile。
5. 将 `ResolvedPromptInput` 作为 `DriveEntry::Prompt`，把 `TurnState` 投影成 `DriverTurnInput { model, prompt, options }` 后交给 `Driver.drive_run()`。
6. 每次下一模型调用前进入 run safe point；`SessionRuntime` 消费 steer、应用合法 profile 变化、收集 typed context materials，并返回 `NextModelCallPlan`。
7. `Driver` 应用 persistent messages/profile 后调用 `Prompt::project_model_call(...)`，得到唯一 `ModelInputProjection`，再构造 `ModelCallRequest`。`BeforeModelCall` 和 provider payload hook 仍属于 `ModelGateway.call_model(...)`。
8. 工具调用通过 `Tools` 子系统治理和执行，再由 Driver 回填 Rig；所有已进入历史的 tool call 最终必须有 tool result。
9. run result 写入完成后 flush run-result save point，发出唯一终态 `run_finished { status }`；按 post-run arbitration 处理 retry、pending manual compact、auto compaction 和 queued continuation。只有这些动作都不会立即开始时，才切回 `idle` 并发出 `session_settled`。

save point 的数量由可恢复写入批次决定，不按 run 固定。正常 text-only `Completed` run 至少包含 user-message 和 final run-result 两个 save point；前者必须早于 `run_started`，后者必须早于 `run_finished { status: completed }`。若一次 run 有多个 tool rounds，每个即将进入下一次模型调用上下文的完整 tool-call/result batch 都必须先通过 `SessionHandle` 提交并形成 save point。并行工具结果可以归约成一个 batch；流式 delta 和 queue state 不触发 session save point。

外层 event sequence 定义 barrier 覆盖范围：某个 `persistence_save_point` 确认同一 session 在它之前已提交的相关 writes 可恢复。`SessionRuntime` 是 batch/flush 时机和事件发布的唯一 owner；`Driver`、`Tools`、`SessionHandle` 和 storage adapter 都不能自行发布该事件。abort/failure 的补偿写入与 unresolved tool protocol 修复继续由 BR-024 定义。

如果运行失败，`SessionRuntime` 应构造失败 assistant message或 diagnostic entry，写入会话并发出 `diagnostics_error` + `run_finished { status: failed }`，避免 UI 卡在运行态。
