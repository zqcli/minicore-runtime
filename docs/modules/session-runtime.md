# SessionRuntime

`SessionRuntime` 是单个会话的产品级 Agent 编排对象，对齐 pi coding-agent 的 `AgentSession`。它拥有会话状态、队列、工具、模型状态、运行生命周期和事件归约。每个 `SessionRuntime` 固定一个 workspace cwd；每次 run 启动时通过 `ResourceManager.capture_turn(...)` 捕获当前 `TurnResourceSnapshot`，并把它放入本次 `TurnState`，使后台 run 不受 focused session 切换或资源 reload 影响。

## 核心职责

- 状态访问：messages、current run、model、thinking level、system prompt、active tools、all tools、resources、queues、session id、session name、session file、retry attempt、context usage 和 session stats。
- 事件与持久化：订阅 `Driver` events，归约 streaming state、pending tool calls 和 error state；在 message/save point 上追加 session entries；通过 `AgentRuntime` event bus 向 UI 发布 `agent_runtime_protocol::Event`。
- command 入口：持有 session-scoped `command: Command`，为当前 session 构造 `CommandContext` / `SessionCommandHost`，并调用共享无状态 `CommandManager` 处理 `ExecuteCommandText` / `ExecuteCatalogCommand`。
- prompt 入口：处理 `SubmitPrompt`，执行输入预检、模型/凭据校验、图片策略、skill 展开和 prompt template 展开；后期可在该安全点接入 `BeforeAgentStart` Hook。
- 队列入口：支持 `Steer`、`FollowUp`、`NextTurn`、`ClearQueue`，并持有结构化 `PendingSessionAction`；运行中输入或 deferred session action 不能绕过当前 work 的顺序。
- 自定义消息入口：为后续 extension/runtime command 支持 custom message，允许“只写入会话”或“写入后触发 turn”。
- 运行生命周期：启动 run、continue、post-run 处理、失败消息构造、abort、wait-for-idle、phase 切换和 settled 事件。
- 运行输入 scope：持有固定 `workspace_id` / `cwd`；run 启动时捕获当前 cwd 的 `TurnResourceSnapshot` 并构建 `TurnState`，resource reload 只影响 future run，不会改写正在运行的后台 run。
- 模型与思考等级：支持 set/cycle model、模型认证检查、恢复失败 fallback、set/cycle thinking level、能力裁剪和持久化。
- 工具与系统提示词：持有 session-scoped `Tools` 子系统，由它维护工具定义注册表、活跃工具集合、工具提示片段、工具执行策略、审批、grant、执行协调和 executor；在工具/资源变化后调用 `prompt.rs` 重建 system prompt。
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
  ├─ CurrentRun
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

`SessionRuntime` 不持有跨 turn 的 current resource cache。它可以记录 `ResourceState { last_seen_revision }` 供 UI、diagnostics 或 prompt rebuild 判断使用，但不能用它代替 `ResourceManager.capture_turn(...)`。每次 user turn 真正启动时都必须重新 capture，当 reload 已经完成 `ResourceSnapshotStore::replace_cwd(...)` 后，下一次 capture 自然会读取到新的 current `CwdResourceSnapshot`。

`SessionRuntime` 持有 session-scoped `Command` facade，但不持有 catalog cache。`Command` 每次请求都基于当前 session 的 cwd、resource summary、model、tools、run state 和 settings 构造 `CommandContext`，再调用共享 `CommandManager` materialize/parse/suggest/resolve。只有 resolved 后的 session-scoped 结构化命令或 prompt-like input 才进入 `SessionRuntime` 具体能力。`/status`、`/usage` 这类 query command 通常只读取 snapshot/view；`/model`、`/thinking` 更新会话状态；`/skill code-review`、兼容 `/skill:code-review` 和 `/{template}` 才会构造 user message 并可能启动 Agent run。

## 对齐 pi 的能力

| pi `AgentSession` 方法/属性 | 本项目能力 |
| --- | --- |
| `subscribe(listener)` / `dispose()` | `agent_runtime_protocol::Event` 订阅和会话运行时释放 |
| `state` / `messages` / `isStreaming` | `agent_runtime_protocol::RuntimeSnapshot.active_session.messages`、`current_run`、`SessionPhase` |
| `prompt(text, options)` | `SubmitPrompt` |
| `_expandSkillCommand` | `InvokeSkill` |
| `promptTemplates` / `expandPromptTemplate` | `InvokePromptTemplate` |
| command text / slash command expansion | `ExecuteCommandText` 经 `SessionRuntime.command` + `CommandManager` materialize/parse/resolve 后进入 `InvokeSkill` / `InvokePromptTemplate` / `Compact` / `SetModel` 等结构化命令 |
| `steer(text)` / `followUp(text)` | `Steer` / `FollowUp` |
| `sendCustomMessage(...)` | crate-private `InternalAgentCommand::AppendMessage` / `NextTurn` 的内部基础能力 |
| `sendUserMessage(...)` | extension/runtime 触发用户消息 |
| `clearQueue()` / `pendingMessageCount` | `ClearQueue` / `queue_updated` / snapshot queues |
| `abort()` | `AbortRun` + wait-for-idle |
| `setModel()` / `cycleModel()` | `SetModel` / `CycleModel` |
| `setThinkingLevel()` / `cycleThinkingLevel()` | `SetThinkingLevel` / `CycleThinkingLevel` |
| `getActiveToolNames()` / `setActiveToolsByName()` / `getAllTools()` | `SetActiveTools`、tool registry、snapshot tools |
| `setSteeringMode()` / `setFollowUpMode()` | `SetQueueMode` |
| `compact()` / `abortCompaction()` | `Compact` / `AbortCompaction` |
| `setAutoCompactionEnabled()` | `SetAutoCompaction` |
| `abortRetry()` / `setAutoRetryEnabled()` | `AbortRetry` / `SetAutoRetry` |
| `executeBash()` / `recordBashResult()` / `abortBash()` | 后置 shell 工具能力 |
| `setSessionName()` | `SetSessionName` |
| `navigateTree()` | `NavigateSessionTree` |
| `getSessionStats()` / `getContextUsage()` | snapshot stats/context usage |
| `exportToHtml()` / `exportToJsonl()` | `ExportSession` |
| `reload()` | `ReloadResources` / per-cwd `CwdResourceSnapshot` reload |
| `bindExtensions()` / `extensionRunner` | 后续扩展运行时与 `RuntimeHookRegistry` |

## Skill Invocation

`SessionRuntime` 负责把显式技能调用变成一次普通 Agent 输入，但不负责扫描技能文件。边界如下：

```text
ResourceManager owns resolved SkillCatalog in CwdResourceSnapshot
skills.rs provides metadata/frontmatter/format helpers
SessionRuntime reads captured TurnResourceSnapshot, reads skill body/content from resolved resource, and creates user message
Driver executes drive_run
```

`InvokeSkill` 流程：

1. 在真正进入 user turn 时，调用 `ResourceManager.capture_turn(session_id, turn_id, workspace_id, cwd)`，取得 `TurnResourceSnapshot`，并从 `turn.resources.cwd.resolved.skills` 取得当前 cwd 的 effective `SkillCatalog`。
2. 处理结构化 `InvokeSkill`，按 `skill_name` / `ResourceKey` 查找 `SkillMetadata`；不存在时返回结构化错误。raw `/skill name` 或兼容 `/skill:name` 的解析已经在 `CommandManager.resolve_for_execution` 完成。
3. `SessionRuntime` 从 captured `TurnResourceSnapshot.cwd.resolved.skills` 读取 selected `SkillResource.body`；不能绕过 snapshot 重新读取 `metadata.file_path`。
4. `SessionRuntime` 调用 `skills::format_skill_block(metadata, body)` 构造 `<skill>` 块，并追加 `additional_instructions`。
5. `SessionRuntime` 构造新的 user message，使其进入本次 turn 或按 `delivery` 入队。
6. 后续执行路径与普通 `SubmitPrompt` 一致：`Driver`、事件归约和 session writes；后期可在 run 启动安全点接入 `BeforeAgentStart` Hook。

这意味着资源 reload 只影响未来调用和未来 system prompt。已经展开并写入 session 的技能调用是一条历史 user message，不应被新版本技能改写。

如果 `InvokeSkill` / `InvokePromptTemplate` 在 session 正在 running 时进入队列，队列里应保存结构化 intent，而不是立即展开正文。等到下一轮 user turn 真正启动时，再 capture 当时 current cwd 的 `TurnResourceSnapshot` 展开技能或提示模板；这保证 future turn 使用 reload 后的新资源。

## System Prompt Rebuild

`SessionRuntime` 负责决定何时重建 system prompt，但不直接加载资源文件。流程对齐 pi `AgentSession._rebuildSystemPrompt()`：

```text
TurnResourceSnapshot.cwd.resolved.prompt_materials()
SessionRuntime Tools: active tools + snippets + guidelines + provider schemas
SessionRuntime cwd/date/model state
  → prompt::build_system_prompt(...)
  → update next TurnState
```

重建触发点包括 user turn 启动、active tools 改变、工具 prompt snippet/guideline 改变和会话启动；后期 hook 可明确替换 system prompt。资源 reload 不会主动改写 running turn；下一次 user turn capture 新的 `TurnResourceSnapshot` 后重建 system prompt。运行中的 turn 使用启动时的 `TurnState`，资源变化只影响未来 turn。

MVP 中 Rig step 不再调用 `ResourceManager`。资源在 user turn 启动时捕获进 `TurnState`，但完整 `TurnState` 不跨过 `Driver` seam。`SessionRuntime` 只把 `DriverTurnInput` 这种窄投影放进 `DriveRequest`；`TurnState.resources` 留在 `SessionRuntime` / `SessionDriverHost` 中，用于工具运行上下文、safe point 决策和未来 `StepResourceSnapshot` parent。`Driver` 不能在 step 中读取 `ResourceManager.current_runtime()`、`current_cwd()` 或 `TurnResourceSnapshot`。未来如果启用 `StepResourceSnapshot`，也应由 `SessionRuntime` / `DriverHost` 基于当前 `TurnState.resources` 创建轻量 parent wrapper，而不是重新捕获上层资源。

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
6. 运行中切换模型可以先被 phase guard 拒绝；完整版本可在 `before_next_model_call` 安全点由 `SessionRuntime` 更新会话事实并 patch future `DriverTurnInput`，但不能替换已经发出的 provider request。

`ModelSummary` 是 UI/snapshot view，不是执行路径身份。执行路径必须使用 `ModelSelection { provider_id, model_id }`，避免把显示名、provider API model name 或 Rig 类型写进 session。

## TurnState

每次启动模型 turn 前，`SessionRuntime` 创建一次稳定快照：

```rust
pub struct TurnState {
    pub resource_revision: ResourceRevision,
    pub resources: Arc<TurnResourceSnapshot>,
    pub messages: Vec<MessageRecord>,
    pub stream_options: StreamOptions,
    pub session_id: SessionId,
    pub system_prompt: String,
    pub model: ActiveModel,
    pub thinking_level: ThinkingLevel,
    pub tools: Vec<ToolDefinitionView>,
    pub active_tools: Vec<ToolDefinitionView>,
    pub context_usage: Option<ContextUsageView>,
}
```

`TurnState` 是 `SessionRuntime` 的内部 run snapshot。它 pin 住资源、system prompt、模型状态、工具视图和 context usage，以保证 running run 不受 reload 或 focus 切换影响；它不是 `Driver` 的输入类型。

`SessionRuntime` 在启动 `Driver` 前投影出更窄的输入：

```rust
pub struct DriverTurnInput {
    pub model: ModelSelection,
    pub system_prompt: String,
    pub active_tool_schemas: Vec<ToolSchema>,
    pub thinking_level: ThinkingLevel,
    pub stream_options: StreamOptions,
}

impl TurnState {
    pub fn driver_turn_input(&self) -> DriverTurnInput {
        DriverTurnInput {
            model: self.model.selection.clone(),
            system_prompt: self.system_prompt.clone(),
            active_tool_schemas: self.active_tool_schemas(),
            thinking_level: self.thinking_level,
            stream_options: self.stream_options.clone(),
        }
    }
}
```

`DriverTurnInput` 不能包含 `Arc<TurnResourceSnapshot>`、`ResourceRevision`、`ContextUsageView`、全量 tools registry、executor handle、approval/policy state、queue state 或 storage handle。它只是给 `Driver` 构造 `ModelCallRequest` 和推进 Rig `AgentRun` 的模型可见输入投影。

运行中如果用户切换模型或活跃工具，`SessionRuntime` 可以先记入 `pendingSessionWrites`，然后在 `DriverHost::before_next_model_call` 安全点重新构建或 patch future `DriverTurnInput`。资源 reload 不 patch 当前 `TurnState`；当前 run 继续使用启动时捕获的 `resources` 和 `system_prompt`。`Driver` 只能把 `DriverTurnInput.model` 复制进 `ModelCallRequest`；provider 解析和 auth 注入必须发生在 `ModelGateway`。

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
  → computes ContextUsage from projected TurnState
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

- `BeforeAgentStart`：启动 run 前追加 custom message、patch stream options 或 patch system prompt。
- `PromptBuilt`：`prompt.rs` 生成最终 system prompt 后追加或 privileged replace。
- `ContextProjection`：每次模型调用前调整模型可见 messages；结果必须满足 provider protocol，不得留下 orphan tool call/result。
- `BeforeNextModelCall` / `BeforeRunFinish`：run 级安全点，用于处理队列、暂停、follow-up、future `DriverTurnInput` patch 或 run 结束决策。
- `ToolResultBeforeAppend`：tool-result message 写入前的受控干预；工具治理链路内部 hook 由 `Tools` 拥有。
- `SessionBeforeCompact`：压缩前取消、追加说明或提供完整 `CompactionResult`。
- `AfterSavePoint`：保存点后做 observer 型同步、索引、备份或 telemetry。

模型/provider 边界 hook 不属于 `SessionRuntime`：`BeforeModelCall`、`BeforeProviderPayload`、`AfterProviderResponse` 和 `ProviderUsageNormalized` 由 `ModelGateway` 在 `call_model(...)` 内拥有。`Driver` 不调用 hook。`SessionRuntime` 应按自己 hook point 的 error policy 处理失败：observer hook 失败进入 diagnostics；context 变换失败不得产生半写入 session entry。

## Compaction Orchestration

压缩由 `SessionRuntime` 编排，算法和 prompt helper 放在平级 [Compaction](compaction.md) 模块。`Driver` 不执行压缩，只把 usage、完成消息和 provider error 归约给 `SessionRuntime`。

手动压缩流程：

1. 收到 `agent_runtime_protocol::AgentCommand::Compact { instructions }`。
2. 若 session 已 `idle`，立即进入第 4 步。若存在 active run、waiting approval、suspended run 或立即 retry chain，则保存唯一 `PendingSessionAction::Compact { command_id, instructions }`，发 `queue_updated`，不 abort、不改 phase、不清理任何 message queue。
3. 当前 work terminal event 和相关 save point 完成后，如果没有立即 retry / overflow recovery，先移除 pending compact 并发 `queue_updated`，再切换到 `SessionPhase::Compaction`；该动作优先于 follow-up / next-turn continuation。若 `AbortRun`、`ClearQueue`、session close 或 shutdown 发生，则移除 pending compact，不启动压缩。重复 `Compact` 返回 `CompactAlreadyQueued`；已处于 compaction phase 返回 `CompactionAlreadyRunning`。
4. 读取当前 leaf 的 root-to-leaf `SessionEntry` path。
5. 调用 `compaction::prepare_compaction(path, settings)`，得到 `CompactionPreparation`。
6. 后期启用 hook system 时，触发 `RuntimeHookRegistry.invoke(SessionBeforeCompact)`；Hook 可以取消、patch instructions 或提供完整 `CompactionResult`。当前 MVP 直接进入下一步。
7. 如果未提供 hook result，则让 `Compaction` 构造 `CompactionSummaryMaterial`；`SessionRuntime` 选择摘要模型和调用选项，构造 `ModelCallPurpose::CompactionSummary` 的唯一 `ModelCallRequest`，再通过 ModelGateway 调用摘要模型；这不是 `Driver.drive_run()`。
8. 追加 `SessionEntry::Compaction`，再调用 `SessionHandle` 的上下文构建能力重建 messages。
9. 发出 `compaction_finished` 和 `persistence_save_point`，随后 phase 回到 `idle`；如果还有 follow-up / next-turn，则继续对应 work，否则发出 `session_settled`。如果是 overflow recovery 且需要立即重试，则不先发 `session_settled`，直接启动后续 run。

自动压缩流程：

1. `Driver` 返回 `DriveResult::Completed` 后，`SessionRuntime` 写入消息并 flush save point。
2. 从最新 assistant usage 或估算值计算 context usage。
3. 如果 `compaction::should_compact(...)` 为 true，执行 `run_auto_compaction(reason = Threshold, will_retry = false)`。
4. threshold 压缩完成后不重跑刚完成的回答；如果队列里还有 follow-up/steering/next-turn，再从重建后的上下文继续。

overflow recovery 流程：

1. `DriveResult::Failed` 中的错误被识别为当前模型的 context overflow。
2. 如果本轮尚未尝试过 overflow recovery，执行 `run_auto_compaction(reason = Overflow, will_retry = true)`。
3. transient overflow failure 如果要持久化，必须标记为 `ContextVisibility::UiOnly` 或写成 diagnostic/custom entry，不能进入重试上下文。
4. 压缩成功后，用重建后的 session context 启动新的 `DriveEntry::Continue { reason: ContextOverflowRecovery }`。
5. 不使用 `DriveEntry::Resume` 恢复旧 `AgentRun`，因为旧 serialized run 内含压缩前 history。

压缩摘要消息进入 `TurnState.messages`，不是 `TurnState.system_prompt`。`prompt.rs` 仍按资源、工具和会话状态纯构建系统提示词。

## 队列语义

目标队列能力对齐 pi core `Agent` 和 pi coding-agent `AgentSession`：

- `steerQueue`：运行中注入，尽量在当前工具批次完成后、下一次模型调用前进入上下文。
- `followUpQueue`：当前 Agent 运行本来要结束时再注入，作为后续用户输入继续执行。
- `nextTurnQueue`：无论当前是否运行，都排到下一次用户 turn 前，与下一次显式 prompt 一起进入上下文。
- `PendingSessionActions`：保存不进入模型上下文的结构化 post-run action。当前只支持一个 pending manual compact；它在当前 work chain 稳定结束后、follow-up / next-turn 之前执行。

队列模式支持 `all` 和 `one-at-a-time`。运行时应在消息队列或 pending action 变化时发出完整 `queue_updated`。`AbortRun` / `ClearQueue` 清除 pending manual compact；pending action 不作为文本返回编辑器，因为它不是 `QueuedMessage`。

post-run arbitration 顺序固定为：

```text
terminal run facts + persistence save point
  → required overflow recovery / immediate retry chain
  → pending manual compact
  → threshold auto compaction（若 manual compact 已执行则跳过重复压缩）
  → follow-up / next-turn continuation
  → session_settled
```

Rig 的 `AgentRun` 不一定暴露与 pi `runAgentLoop` 完全相同的运行中注入点，因此实现可以分阶段：MVP 把运行中输入降级为 follow-up；完整实现中由 `Driver` 在 `before_next_model_call` 安全点让 `SessionRuntime` 检查 steering 队列。

## 运行流程

1. 校验当前 `SessionPhase` 是否允许启动运行，并切换为 `turn`。
2. 构建 `TurnState`。
3. 后期启用 hook system 时，可触发 `BeforeAgentStart` / `PromptBuilt` Hook。当前 MVP 直接进入下一步。
4. 将 prompt、skill 调用或提示模板展开结果作为 `DriveEntry::Prompt`，并把 `TurnState` 投影成 `DriverTurnInput` 后交给 `Driver.drive_run()`。
5. 下一次模型调用前进入 `SessionRuntime` run safe point；后期启用 hook 时这里只能触发 `ContextProjection` / `BeforeNextModelCall` 这类 run 级 hook。`BeforeModelCall` 和 provider payload hook 属于 `ModelGateway.call_model(...)`。
6. 工具调用通过 `SessionRuntime` 内部的 `Tools` 子系统治理和执行，再由 `Driver` 回填 Rig。
7. 每个 run 的可恢复边界 flush `pendingSessionWrites`，发出 `persistence_save_point`。
8. 下一次模型调用前进入 `before_next_model_call` 安全点。
9. 运行结束时发出唯一终态 `run_finished { status }`；按 post-run arbitration 处理 retry、pending manual compact、auto compaction 和 queued continuation。只有这些动作都不会立即开始时，才切回 `idle` 并发出 `session_settled`。

如果运行失败，`SessionRuntime` 应构造失败 assistant message或 diagnostic entry，写入会话并发出 `diagnostics_error` + `run_finished { status: failed }`，避免 UI 卡在运行态。
