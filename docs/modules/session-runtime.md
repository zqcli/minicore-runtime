# SessionRuntime

`SessionRuntime` 是单个会话的产品级 Agent 编排对象和 per-session actor。它拥有会话状态、队列、工具、模型状态、运行生命周期和事件归约，并通过 mailbox 在 active run 飞行期间继续处理 approval、abort、prompt delivery、pending action、snapshot 和 shutdown。每个 `SessionRuntime` 固定一个 workspace cwd；每次 run 启动时通过 `ResourceManager.capture_turn(...)` 捕获当前 `TurnResourceSnapshot`，并把它放入本次 `TurnState`，使该 run 不受客户端 session selection 变化或资源 reload 影响。

## 核心职责

- 状态访问：messages、current run、model、thinking level、system prompt、active tools、all tools、resources、queues、session id、session name、session file、retry attempt、context usage 和 session stats。
- 事件与持久化：订阅 `Driver` events，归约 streaming state、pending tool calls 和 error state；把稳定事实组装为 `SessionWriteBatch` 并统一调用 `SessionHandle.commit(...)`，成功后再通过 `AgentRuntime` event bus 发布对应领域事件。
- command 入口：持有 session-scoped `command: Command`，为当前 session 构造 `CommandContext` / `SessionCommandHost`，并调用共享无状态 `CommandManager` 处理 `ExecuteCommandText` / `ExecuteCatalogCommand`。
- prompt 入口：处理 `SubmitPrompt` 和其他结构化 `PromptIntent`，执行输入预检、模型 catalog/capability 与 cached redacted auth-availability 校验、附件策略和 `PromptDelivery` admission；真实 `AuthStore.resolve(...)` / provider client 构造只能在 `run_started` 后进入 `ModelGateway`；目标 turn 由 `PromptTurn.resolve_intent()` 统一展开 skill/template。
- 队列入口：支持 `Steer`、`FollowUp`、`NextTurn`、`ClearQueue`，并持有结构化 `PendingSessionAction`；运行中输入或 deferred session action 不能绕过当前 work 的顺序。
- 自定义消息入口：为后续 extension/runtime command 支持 custom message；只接受完整 draft，通过 `SessionMutation` 或 `UserInput` batch 提交后决定是否触发 turn。
- 运行生命周期：启动 run、continue、post-run arbitration、失败消息构造、abort、phase 切换和 `session_settled` 状态归约。
- 运行输入 scope：持有固定 `workspace_id` / `cwd`；run 启动时捕获当前 cwd 的 `TurnResourceSnapshot` 并构建 `TurnState`，resource reload 只影响 future run，不会改写正在运行的后台 run。
- 模型与思考等级：支持 set/cycle model、模型认证检查、恢复失败 fallback、set/cycle thinking level、能力裁剪和持久化。
- 工具与提示词配置：持有 session-scoped `Tools` 子系统；在 turn start 汇合 captured `PromptResourceView`、tool/model/agent/environment/policy views 创建 immutable `PromptTurn`。MVP 在 `Turn` 中拒绝 active tools/模型可见 profile mutation；full version 若在安全点支持变化，必须使用同一 captured resources 重建并整体替换 `PromptCallProfile` 与 `ToolBatchInvoker`。
- 资源与扩展：在 user turn 启动时引用当前 `TurnResourceSnapshot`；该 snapshot pin 住 `CwdResourceSnapshot`，而 cwd snapshot pin 住对应 `RuntimeResourceSnapshot`。reload 后 future turn 使用新资源，运行中的 turn 不被 patch。
- 压缩与重试：支持手动压缩、自动压缩、context overflow 恢复、自动重试、取消压缩和取消重试。
- 会话树：支持 set session name、navigate tree、branch summary、fork selector 所需 user message 列表。
- shell 能力：后置支持 bash/shell 执行、输出流、结果记录、取消和 pending shell message flush；MVP 默认不开启。
- 导出与辅助：支持 session stats、context usage、HTML/JSONL 导出、last assistant text。

## 内部结构

```text
SessionRuntime actor
  ├─ command/run-effect mailbox
  ├─ WorkspaceId / cwd
  ├─ ResourceManager handle
  ├─ SessionHandle
  ├─ SessionPhase
  ├─ CurrentRun { PromptTurn / current-run context / RunTaskHandle }
  ├─ CurrentRunUsage / SessionUsageStats / ContextUsage
  ├─ ModelState
  ├─ ResourceState { last_seen_revision }
  ├─ CompactionState
  ├─ Command
  ├─ Tools
  ├─ QueueState
  ├─ PendingSessionActions
  └─ RuntimeHookRegistry        // future hook service view

RunTask                         // one per publicly started run
  ├─ Driver / Rig AgentRun segment(s)
  ├─ DriverTurnInput / run-local usage / limits
  ├─ owned SessionDriverHost
  └─ CancellationToken / RunLink
```

`SessionRuntime` 是产品编排层，不应把 Rig 类型泄漏给 UI，也不应把工具执行交给 UI。

`SessionRuntimeHandle` 是 `AgentRuntime` / `SessionManager` 联系 actor 的显式可克隆入口。它只发送 command、snapshot/read projection 和 shutdown request，不保存权威 session 状态，也不把 `&mut SessionRuntime` 或 `Arc<Mutex<SessionRuntime>>` 暴露给调用方。`dispatch()` 只等待 actor 对命令的接收/同步 guard 结果，不等待一次 Agent run terminal；最终状态仍通过 event/snapshot 观察。

`CurrentRun` 持有 actor-owned parent `CancellationToken` 和 attached `RunTaskHandle { run_id, join/completion }`；task 只得到 child token。`AbortRun` / shutdown 先 cancel parent，再按 terminal/commit 规则等待 completion；drop/replace `CurrentRun` 前必须确认旧 task 已结束或进入强制 crash policy，不能只丢 join handle 让 model/tool work 在后台继续。

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

`Turn + current_run = None` 是不可通过 `AbortRun` 取消的有界 admission/preflight 或 post-run finalization 窗口。进入 provider/model/tool/approval 等可能长时间等待的 Agent run work 前，`SessionRuntime` 必须先分配 host-global `RunId`、建立 `CurrentRun` 并发布 `run_started`。admission 中只允许 bounded in-memory validation、captured prompt assembly、required session commit 和同步 threshold 判定；如果 pre-run gate 命中，actor 必须立即离开该窗口并显式切换到 `SessionPhase::Compaction`，随后才允许 summary model / ProviderNative 外部等待。该 session-scoped compaction 有自己的 phase/event/cancel lifecycle，不创建 Agent `RunId`。session-level auto retry delay同样属于独立 `RetryBackoff` phase，并由 `AbortRetry` 取消。这里的 bounded 表示有限步骤且没有无界 lifecycle/external wait，不是硬实时延迟保证，writer commit 仍遵守不可中断与故障契约。未来 hook 若位于 admission 窗口，也必须有界并由自身 timeout/error policy 收尾，不能把它变成隐藏的长任务。

phase guard 还必须结合 `CurrentRunState` 和 command policy：`DecideToolApproval` 只在 `Turn + WaitingApproval` 合法，`ResumeRun` 只在 `Turn + Suspended` 合法；`AbortCompaction` 只在 `Compaction` 合法，`AbortRetry` 只在 `RetryBackoff` 合法。`Compact` 在 `Idle` 立即进入 `Compaction`，在 `Turn` / `RetryBackoff` 保存唯一 pending action，在 `Compaction` 返回 `CompactionAlreadyRunning`。

`SessionRuntime` 不实现 `wait_until_settled()` 来驱动内部顺序。要求 idle 的 command 通过 phase guard 原子检查；active work 后执行的动作保存为 typed pending action，并由 post-run arbitration 调度。这样避免 `wait → 其他 work 抢先启动 → act` 的 TOCTOU race。

`Turn` 可以包含连续多个 run；只要 immediate retry/continuation、pending action 或 overflow recovery 将立即继续，就不经过 `Idle`，也不发送 `session_settled`。完整转换矩阵见 [AgentRuntimeEvents](agent-runtime-events.md)。`BranchSummary` 不属于 MVP phase；未来若实现独立摘要模型任务，应定义明确 `ModelCallPurpose` 和 lifecycle，而不是预留幽灵 phase。

自动重试按 attempt 配对。当前 run 先发布唯一 `run_finished { failed }`，再由 post-run arbitration 从 `Turn` 直接切换到 `RetryBackoff` 并发布 `retry_auto_started { attempt }`；backoff 结束后切回 `Turn`，建立新的 `CurrentRun` 并发布 `run_started`。该 retry run terminal 后先发布 `run_finished`，再发布同 attempt 的 `retry_auto_finished { status: Succeeded | Failed | Aborted }`。失败且仍有下一 attempt 时直接再次进入 `RetryBackoff`，不能经过 `Idle`；只有 retry chain、overflow recovery、pending action 和立即 continuation 都不会继续时，才切回 `Idle` 并发布 `session_settled`。context overflow 使用 `Turn → Compaction → Turn` 的独立 recovery，不进入 `RetryBackoff`；provider 内部 fallback/retry 也不改变 session phase。

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
2. active `Steer` 在完整 assistant/tool turn 后的 `before_next_model_call`，或 Rig segment 原本将结束时的 `before_run_finish`，使用 `CurrentRun.prompt_turn` 展开，因此继续使用 active turn snapshot。
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

创建时机包括新 user turn、follow-up continuation、显式新 prompt 消费 `NextTurn` queue，以及 overflow recovery 后的新 continuation。`NextTurn` 自身不会自动启动 continuation。普通 resource reload 不 patch active `PromptTurn`。

如果运行中合法切换模型可见工具、agent profile 或其他会影响 system prompt 的 profile，`SessionRuntime` 在 `before_next_model_call` 安全点使用 active turn 的同一个 `PromptResourceView` 重新执行 `begin_turn()`，并整体替换 `PromptCallProfile`。不能分别 patch system prompt 和 active tool schemas。

MVP 中 Rig step 不调用 `ResourceManager`。完整 `PromptTurn` 只由 actor 的 `TurnState` / `CurrentRun` pin 住；run-scoped `SessionDriverHost` 只保留工具上下文需要的 captured turn resources，跨到 `Driver` 主输入的只有窄 `DriverTurnInput { model, prompt: PromptCallProfile, thinking_level, stream_options }`。未来 `StepResourceSnapshot` 也必须以 active `TurnResourceSnapshot` 为 parent，不能读取 ResourceManager current pointer。

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
3. `SetModel` / `CycleModel` 先计算并校验 next `ModelSelection`，提交 `SessionWriteBatch::session_mutation([ModelChangeDraft(next)])`；只有 commit 成功后才替换 `ModelState.selected` 并发 `session_model_changed`；它们不构造 provider client，也不读取 credentials。
4. `SetThinkingLevel` / `CycleThinkingLevel` 会按当前 `ModelCapabilities` 裁剪；不支持 thinking 的模型应降级为 `ThinkingLevel::Off` 或返回结构化错误。thinking level、active tools、session name 和其他需要恢复的 session mutation 都遵循同一顺序：计算 next state → commit `SessionMutation` batch → 替换 runtime-visible state → 发布 changed event。
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

`TurnState` 是 `SessionRuntime` 的内部 run snapshot。它 pin 住 resources、immutable `PromptTurn`、模型状态、工具视图、消息基线和 context usage，以保证 running run 不受 reload 或客户端 session selection 变化影响；它不是 `Driver` 的输入类型。

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

MVP 在 `Turn` 中拒绝模型、thinking、stream options 或 active-tools 切换，避免 `PromptCallProfile` 与 `ToolBatchInvoker` baseline 分裂。后续 full version 若允许运行中切换，`SessionRuntime` 必须先记录待应用事实，在 `DriverHost::before_next_model_call` 安全点使用 active captured resources 创建新的 immutable `PromptTurn`，并在同一 actor transaction 中整体替换 `CurrentRun.prompt_turn`、`NextModelCallPlan.prompt_profile` 与 future `ToolBatchInvoker`。旧 `PromptTurn` 不原地修改。资源 reload 不 patch 当前 `TurnState`；当前 run 的新旧 PromptTurn 都继续引用启动时捕获的 resources。provider 解析和 auth 注入仍只发生在 `ModelGateway`。

## Driver / Tools Coordination

并发模型以 [ADR 0021](../adr/0021-session-runtime-separates-actor-control-from-run-execution.md) 为权威。`SessionRuntime` actor 长期存活并持有 session 权威状态；每次公开启动的 run 由短期 `RunTask` 推进；`Driver` 基本无状态，通常推进一个 Rig `AgentRun`，active Steer 时可顺序 rollover segment；`DriverHost` 是 `RunTask` 内让 `Driver` 请求外部能力的 trait seam。

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
    inbox: mpsc::Receiver<SessionRuntimeMsg>,
}
```

Actor loop 必须持续消费两类有序 control message：上层经 `SessionRuntimeHandle` 提交的 session command，以及 active `RunTask` 经私有 `RunLink` 提交的 safe-point、tool-round commit candidate 和 terminal effect。provider/model/tool/approval 等无界等待不允许发生在 actor loop 内；`SessionWriter.commit(...)` 是唯一允许 actor 等待确定结果的不可取消稳定写入临界区。

`Tools` 仍是 session-owned 子系统，因为它持有 active tools、pending approvals、approval mode、grants、sandbox/mutation state 等会话中间状态；actor 保留 control/admin/decision 能力，run path 只得到绑定 committed tool/profile baseline 的 `ToolBatchInvoker`，不能把 `&mut Tools` 借给覆盖整个 run 的 future。`Driver` 只是执行协作者，不携带长期 session identity 或权威状态。

run 启动时的代码形态：

```rust
let run_id = self.allocate_run_id();
let turn_resources = turn_state.resources.clone();
self.current_run = Some(CurrentRun::starting(run_id, turn_state.clone()));
self.event_sink.publish_run_started(self.session_id, run_id).await?;

let request = DriveRequest {
    run_id,
    session_id: self.session_id,
    entry,
    turn: turn_state.driver_turn_input(),
    limits: self.drive_limits(),
};

let (run_link, actor_endpoint) = RunLink::pair(run_id);
self.attach_run_endpoint(actor_endpoint);

let host = SessionDriverHost {
    session_id: self.session_id,
    run_id,
    cwd: self.cwd.clone(),
    turn_resources,
    run_link,
    tools: self.tools.tool_batch_invoker(),
    model_gateway: Arc::clone(&self.services.model_gateway),
    progress: RunProgressSink::new(run_id),
    cancel: self.current_run.as_ref().unwrap().cancel.child_token(),
};

let task = RunTask::spawn(request, host);
self.current_run.as_mut().unwrap().attach_task(task);
```

`allocate_run_id()` 只在 runtime 已完成 admission、即将建立 `CurrentRun` 并调用 `drive_run()` 时执行；它使用 runtime-global id generator。`run_started` 必须在任何 provider/model/tool/approval wait 之前发布。preflight 不预分配 RunId，也不制造没有 `run_started` 的幽灵 run。

`RunTask` 内部拥有 `Driver` 和 mutable `SessionDriverHost`，Driver 私有持有当前 Rig `AgentRun` segment，因此 `DriverHost` trait 的 `&mut self` receiver 可以保留；禁止的是 host 内长期借用 actor-owned mutable state。Rig/driver future 若不是 `Send`，实现可以使用 local task 或 runtime-local executor，但仍必须与 actor mailbox 并发推进。

该 wrapper 实现下面的工具 seam：

```text
DriverHost::invoke_tool_batch(request, updates, cancel)
  → SessionDriverHost attaches ToolRunContext { session_id, run_id, cwd, turn resources, abort signal }
  → ToolBatchInvoker.invoke_batch(request, context, updates, cancel)
  → Ok(ToolBatchResult) | Err(Cancelled | Failed)
  → only Ok: RunLink.commit_tool_round(candidate)
  → SessionRuntime actor applies final hook/validation and commit admission
  → actor commits SessionWriteBatch::tool_round(...)
  → actor publishes message_assistant_finished + ordered message_tool_result_appended*
  → actor replies committed; host returns ToolBatchResult to Driver
```

`Tools` 返回内部工具结果和 update；update 经 `RunProgressSink` / control effect 由 actor 归约为 UI event 和 pending approval snapshot projection。`SessionRuntime` actor 从 `CurrentRun` 取出完整 assistant tool-call draft，把它与全部结果组装为一个 `ToolRound` batch；actor 可以应用 `ToolResultBeforeCommit` 并得到不同于 candidate 的最终 result。只有 `SessionHandle.commit(...)` 成功并把该最终 `ToolBatchResult` 回复 `RunTask` 后，host 才返回 Driver；Rig、公开 finished event 与 committed message 必须看到同一最终值。

生产实现选择 owned `SessionDriverHost` + `RunLink`，而不是 `impl DriverHost for SessionRuntime` 或 borrowed wrapper，原因是：

- actor 必须在 run 飞行期间继续处理 `DecideToolApproval`、`AbortRun`、queues、pending actions、snapshot 和 shutdown；内联 `await drive_run()` 会阻塞 mailbox。
- owned wrapper 限制 `DriverHost` 只能访问本次 run 需要的 handles/context，避免把整个 `SessionRuntime` 暴露给 driver seam。
- wrapper 将 `run_id`、turn resources、event correlation、checkpoint context 等 run-scoped 数据与 session-scoped state 分离。
- `DriveRequest` 只带 `DriverTurnInput`，而 `SessionDriverHost` 持有 turn resources；这同时收窄了 driver 主输入和 host 回调上下文。
- `RunLink` 把 safe-point、commit 和 terminal control 收敛到 actor 的单一线性化点；`RunTask` 不能提交任意 session command。
- wrapper 让 `Driver` 单测只需要 fake host，不需要构造真实 session runtime、tools、storage 和 event bus。

运行中 command 路径固定为：

```text
DecideToolApproval
  → SessionRuntimeHandle → actor validates phase/run/call/approval
  → actor-owned Tools.decide_approval(...) wakes the pending batch waiter

AbortRun
  → actor clears required queues/pending actions
  → actor cancels active RunTask token
  → terminal effect returns through RunLink

Steer / FollowUp / NextTurn
  → actor mutates the corresponding queue and publishes full queue snapshot
  → Steer is consumed after the current assistant/tool turn at before_next_model_call, or at before_run_finish when the Rig segment would otherwise end

Compact while Turn
  → actor stores one typed PendingSessionAction::Compact
  → RunTask continues unaffected until terminal arbitration

ClearQueue while Turn
  → actor clears steering/follow-up/next-turn/pending actions
  → actor publishes the full replacement queue snapshot

SetModel / SetThinkingLevel / SetStreamOptions / SetActiveTools while Turn (MVP)
  → actor returns the documented phase/command output rejection promptly
  → no state or PromptCallProfile/ToolBatchInvoker changes occur mid-run
```

`before_next_model_call` 是 actor transaction：它发生在当前 assistant response 及其完整工具批次结束后、下一次模型调用前。`RunTask` 发送 checkpoint 并等待 reply；actor 消费 steering intent、使用 active `PromptTurn` 展开、commit `UserInput`、发布 invoked/message facts，再把 committed messages 和完整 profile/context plan 返回。commit 失败时回复 failure 并让当前 run 进入 failed terminal path；Driver/Rig 不能看到仅存在内存的 steer。若 Rig segment 在无工具的 assistant turn 后原本将结束，`before_run_finish` 以相同 commit 规则最后检查一次 Steer；成功消费时返回 `ContinueWithSteering`，由 Driver 在同一 `RunId` 下 rollover 到新的 Rig segment。

高频 model/tool delta 通过独立 bounded/coalesced `RunProgressSink` 到达 actor；lifecycle/control effect 走优先的 `RunLink` control lane，并携带已提交 progress watermark。`run_finished`、message/tool finished、snapshot barrier 或 actor shutdown 前必须先归约到对应 watermark，不能出现 finished-before-delta 或 `last_event_sequence` 已覆盖事件而 snapshot projection 尚未更新。

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
  → includes usage in run_finished and matching agent_runtime_protocol::RuntimeSnapshot.loaded_sessions[*]
```

`UsageSummary` 是一次 run 内所有模型调用的消耗汇总，不是最后一次模型调用。一次 run 如果经历 `model -> tools -> model -> tools -> model`，`run_finished { usage }` 必须覆盖这几次模型调用。

`ContextUsageView` 表示下一次模型请求会占用多少上下文窗口。它应优先使用最近一次有效 assistant provider usage，再加上后续消息的本地估算；如果没有 provider usage，才估算整个模型可见上下文。

压缩只改变 `ContextUsageView.current_tokens`，不减少 `SessionStatsView.total_usage`。会话累计消耗是历史账本；当前上下文占用是窗口状态。

`usage_updated` 是实时 UI 事件，可以包含尚未形成稳定 session batch 的当前 host 内存统计。需要跨进程恢复的最终 usage 必须包含在对应 `AssistantFinal`、`ToolRound` 或其他稳定 batch 中，并在 `run_finished` 前提交。adapter reducer 重建时以对应 `agent_runtime_protocol::RuntimeSnapshot.loaded_sessions[*].session_stats` 和 `context_usage` 为权威恢复。

## RuntimeHooks

Hook 的边界、capability、typed result 和安全点规划见 [RuntimeHooks](runtime-hooks.md)。当前 MVP 不实现 hook system；后期启用时，`SessionRuntime` 只负责在自己拥有的会话状态机安全点调用 hook，并把 hook 结果应用到会话事实上。它不把 hook 暴露给 UI，也不让 hook 直接发布 `agent_runtime_protocol::Event`。

后期由 `SessionRuntime` 拥有的 hook 点：

- `BeforeAgentStart`：启动 run 前 transform/gate 结构化 current input 或 stream options。
- `PromptBuilt`：`PromptTurn` 形成 profile 后追加或 privileged replace system section；应用后必须重建并重新 fingerprint `PromptCallProfile`。
- `ContextProjection`：为 `CurrentRun` / `CurrentCall` 返回显式 `ContextMaterialContribution::Available/Unavailable`，或在 privileged capability 下替换模型可见 messages；最终必须回到 Prompt 校验。
- `BeforeNextModelCall` / `BeforeRunFinish`：run 级安全点，用于处理队列、暂停、follow-up、组合式 `NextModelCallPlan` 或 run 结束决策。
- `ToolResultBeforeCommit`：`Tools` 返回结果后、公开 `tool_call_finished` 和 `ToolRound` batch 组装前，对 tool-result draft 做受控干预；应用后必须重新校验 call id/order/redaction，公开 finished event 与 committed result 必须使用同一最终值。工具执行链路内部的 `ToolAfterExecute` 由 `Tools` 拥有。
- `SessionBeforeCompact`：压缩前取消、追加说明或提供完整 `CompactionResult`。
- `AfterSessionCommit`：稳定 batch commit 后做 observer 型备份、telemetry 或可重建 cache 同步；它是内部 hook，不产生公共 persistence event，也不能承担 `SessionIndex` / current leaf / message projection 的权威更新。

模型/provider 边界 hook 不属于 `SessionRuntime`：`BeforeModelCall`、`BeforeProviderPayload`、`AfterProviderResponse` 和 `ProviderUsageNormalized` 由 `ModelGateway` 在 `call_model(...)` 内拥有。`Driver` 不调用 hook。`SessionRuntime` 应按自己 hook point 的 error policy 处理失败：observer hook 失败进入 diagnostics；context 变换失败不得产生半写入 session entry。

## Compaction Orchestration

压缩由 `SessionRuntime` 编排，算法和 prompt helper 放在平级 [Compaction](compaction.md) 模块。`Driver` 不执行压缩，只把 usage、完成消息、Prompt projection/provider context-limit source 和 run result 归约给 `SessionRuntime`。

手动压缩流程：

1. 收到 `agent_runtime_protocol::AgentCommand::Compact { instructions }`。
2. 若 session 已 `idle`，立即进入第 4 步。若存在 active run、waiting approval、suspended run 或立即 retry chain，则保存唯一 `PendingSessionAction::Compact { command_id, instructions }`，发 `queue_updated`，不 abort、不改 phase、不清理任何 message queue。
3. 当前 work 的 terminal handling 已完成（正常完成包含 required stable batch commit；abort/failure 可以没有新 batch）且 terminal event 已发布后，如果没有立即 retry / overflow recovery，先移除 pending compact 并发 `queue_updated`，再切换到 `SessionPhase::Compaction`；该动作优先于 queued steering continuation / follow-up。若 `AbortRun`、`ClearQueue`、session close 或 shutdown 发生，则移除 pending compact，不启动压缩。重复 `Compact` 返回 `CompactAlreadyQueued`；已处于 compaction phase 返回 `CompactionAlreadyRunning`。
4. 读取当前 leaf 的 root-to-leaf `SessionEntry` path。
5. 调用 `compaction::prepare_compaction(path, settings)`，得到 `CompactionPreparation`。
6. 后期启用 hook system 时，触发 `RuntimeHookRegistry.invoke(SessionBeforeCompact)`；Hook 可以取消、patch instructions 或提供完整 `CompactionResult`。当前 MVP 直接进入下一步。
7. 如果未提供 hook result，则查询当前模型的 `CompactionCapabilities`，由 trigger、用户 preference 和 capability 解析 `CompactionMethod` plan。MVP `SummaryModel` 构造 `CompactionSummaryMaterial` 和 `ModelCallPurpose::CompactionSummary` 请求；后期 `ProviderNative` 由 ModelGateway adapter 调用专用 compact endpoint。两者都不进入 `Driver.drive_run()`。
8. 构造 `SessionWriteBatch::compaction(...)`，通过 `SessionHandle.commit(batch)` 原子提交 compaction entry 与 leaf update，再调用上下文构建能力重建 messages。
9. commit 成功后发出 `compaction_finished`，再由 post-compaction arbitration选择下一 phase：pre-run gate 完成后切回 `Turn` 并首次分配 Agent `RunId`；overflow recovery 或 queued steering/follow-up 将立即继续时切换到 `Turn`，分配新的 public `RunId` 并发布 `run_started`；需要延迟 retry 时进入 `RetryBackoff`；只有没有立即工作时才回到 `Idle` 并发布 `session_settled`。只有 active Steer 的 Rig segment rollover 复用原 `RunId`，所有 post-compaction continuation 都是新 run。`NextTurn` 可以在 settled 状态保留，任何 immediate continuation 都不能先经过 `Idle`。

run 前与自动压缩流程：

1. pre-run：最终 `UserInput` commit 后、分配 `RunId` 前，以当前模型 capability 和 context estimate 同步执行 threshold gate；命中后切换 `Turn → Compaction`，保护 current input、执行压缩，再切回 `Turn` 重建并复验一次，通过后才首次发布 `run_started`。
2. post-run：`Driver` 返回 `DriveResult::Completed` 后，`SessionRuntime` 提交最终 `AssistantFinal` batch并发布 `run_finished`，再从最新 assistant usage 或估算值计算 context usage。
3. post-run 达到阈值时执行 `run_auto_compaction(reason = Threshold, will_retry = false)`；不重跑刚完成的回答。如果队列里还有 queued steering continuation / follow-up，再从重建后的上下文继续。`NextTurn` 等下一次显式 prompt。

overflow recovery 流程：

1. 当前 run 必须先发布 `run_finished { status: failed }`；随后其 `DriveResult::Failed` 被识别为当前模型/work chain 的 `DriverError::ContextLimitExceeded`，来源可以是 `PromptProjection` 或 `Provider`，再进入 `Compaction` phase并发布 `compaction_started { reason: overflow }`。
2. 如果整个 work chain 尚未尝试 overflow recovery，记录该 budget 并执行 `run_auto_compaction(reason = Overflow, will_retry = true)`。
3. transient overflow failure 只进入 diagnostics，不写入 session；PromptProjection source 不产生 model-call usage，provider source 保留已有 attempt/usage；partial assistant 和旧 run 的未完成状态不进入重试上下文。
4. 压缩成功后必须先发布 `compaction_finished { will_retry: true }`，再用重建后的 session context 和新 `RunId` 启动 `DriveEntry::Continue { reason: ContextOverflowRecovery }`。
5. recovery 后再次超限、没有可压缩内容或 protected current input 本身过大时，返回 typed unrecoverable error；不进行第二次自动 compact。也不使用 `DriveEntry::Resume` 恢复旧 `AgentRun`，因为旧 serialized run 内含压缩前 history。

MVP `SummaryModel` 的压缩摘要消息进入 `TurnState.messages` / durable history，不进入 `PromptCallProfile.system_prompt`。后期 `ProviderNative` replacement 通过 provider-neutral handle 与普通 messages 分离，只有 `ModelGateway` adapter 能读取 opaque payload。启动新 prompt 时，Driver 应把当前完整 `PromptCallProfile`、兼容的压缩后 durable context 与受保护 current input 分开交给最终 projection；当前输入不能在同一启动边界上被摘要。

## 队列语义

目标队列能力使用 MiniCore 自己的 `PromptDelivery`、`QueueKind` 和 `QueueMode` 表达：

- steering queue：`PromptDelivery::Steer` 的模型可见输入；在当前 assistant response 及其完整工具批次结束后的最早 `before_next_model_call` 安全点注入；若 Rig segment 原本将结束，则在 `before_run_finish` 消费并 rollover。session idle 时直接启动 run；compaction、suspended 等暂时无安全点的状态保留队列，不能静默改写成 follow-up。
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

`QueueMode::All` / `OneAtATime` 只控制 steering/follow-up 每个安全点消费全部还是一条。运行时应在消息队列或 pending action 变化时发出完整 `queue_updated`。`AbortRun` 清除尚未消费的 steering/follow-up 和 pending manual compact，保留 `NextTurn`；`ClearQueue` 清除 steering/follow-up/next-turn 和 pending action 全部 queue state。已经完成 `UserInput` commit 的 steer 不再属于 queue，abort 不能从 durable history 删除它。

`SessionRuntime` 不持有 editor draft，也不返回 `returned_to_editor` 或 removed queue delta。queue 中的结构化 intent 不保证保留 raw slash text，不能被 runtime 解释为可恢复的编辑器文本；具体 adapter 如需恢复体验，只能使用自己的 submission history、editor undo state 和 event reducer 做 best-effort local restore。该行为不进入 runtime protocol、snapshot、session storage 或 reconnect contract。pending action 也不作为文本返回编辑器，因为它不是 `QueuedMessage`。

post-run arbitration 顺序固定为：

```text
terminal handling（required stable commit if any）+ terminal run facts
  → required overflow recovery / immediate retry chain
  → pending manual compact
  → threshold auto compaction（若 manual compact 已执行则跳过重复压缩）
  → queued steering continuation（仅当前没有可注入的 active run 时）
  → follow-up continuation
  → session_settled（next-turn queue 可以保持非空）
```

Steer 不要求在 provider streaming、模型请求或工具执行中途修改 Rig。完整实现由 `Driver` 在两个协议安全点让 `SessionRuntime` 检查 steering queue：完整 assistant/tool turn 后的 `before_next_model_call`，以及 Rig segment 原本将结束时的 `before_run_finish`。Rig 没有公开 history append 时，Driver 使用已 committed history 和 steer 创建同一 `RunId` 下的新 Rig segment；该 rollover 不能重置 run-level usage、limits、cancellation 或事件关联，也不能静默降级为 follow-up。

## Next Model Call Plan

同一个安全点可能同时发生 steer、RAG/memory context 和控制决策；full version 还可能加入 active tools/model profile 更新，因此返回组合式 plan，而不是互斥 enum：

```rust
pub struct NextModelCallPlan {
    pub control: NextModelCallControl,
    pub persistent_messages: Vec<MessageRecord>,
    pub prompt_profile: Option<PromptCallProfile>,
    pub current_run_context: Vec<ContextMaterialContribution>,
    pub current_call_context: Vec<ContextMaterialContribution>,
}
```

`persistent_messages` 是 active `PromptTurn.resolve_intent()` 展开的 steer，但只有完成 safe-point transaction 后才能放入 plan：消费结构化 steering intent → 展开最终 user message → commit `SessionWriteBatch::user_input(...)` → 若为 skill/template 则发布外层 `Event.run_id = Some(current_run_id)` 的 `skill_invoked` / `prompt_template_invoked` → 发布 `message_user_appended` → 加入 `persistent_messages`。commit 失败时当前 run 进入 failed terminal path，不能让 Driver/Rig 使用仅存在内存的 steer。`prompt_profile` 只能是用 active captured resources 重建后的完整 profile；`current_call_context` 只影响下一次调用。context owner 的获取失败也必须作为 `Unavailable` 保留，不能通过缺项吞掉 required failure。`SessionRuntime` 不直接构造最终 messages，它把 plan 返回给 Driver，由 Driver 调用 Prompt 的 final projection seam。

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
2. capture `TurnResourceSnapshot`，创建 `PromptTurn`，再让它展开目标 `PromptIntent`，得到 preliminary `ResolvedPromptInput` / `PromptCallProfile`。
3. 构建 preliminary `TurnState`，执行后期 bounded `BeforeAgentStart` / `PromptBuilt` 和最终 `RunBeforeStart(RunStartPlan)`；应用 typed result 后重新校验 current input、profile 和 limits。任何 current-input transform 都必须在持久化前完成。
4. 将最终 resolved current input 组装为 `SessionWriteBatch::user_input(...)` 并调用 `SessionHandle.commit(batch)`；成功后，如果 intent 是 skill/template invocation，先发布外层 `Event.run_id = None` 的 `skill_invoked` / `prompt_template_invoked`，再发布 `message_user_appended`；普通 prompt 直接发布 message event。随后构建不含重复 current input 的 durable history baseline 和最终 `TurnState`。
5. 在分配 `RunId` 前执行同步、best-effort pre-run context threshold gate。若命中，按 committed entry id 保护本次 current input，从 `Turn + current_run = None` 显式切换到 `Compaction` phase并调用统一 compaction orchestration；完成后切回 `Turn`、重建 `TurnState` 并只重新检查一次。仍超限或无可压缩历史时返回 typed error，不建立 `CurrentRun`。该 gate 只是 admission optimization，唯一权威的 call-size 判定仍是 Driver 每次调用 `prompt::project_model_call(...)` 的最终 projection validation。
6. 分配 host-global `RunId`、建立 `CurrentRun` 并发布 `run_started`；再将最终 `ResolvedPromptInput` 作为 `DriveEntry::Prompt`，把 `TurnState` 投影成 `DriverTurnInput { model, prompt, options }`，构造 owned `SessionDriverHost` 与 `DriveRequest`，启动短期 `RunTask` 后立即回到 actor mailbox loop。`RunTask` 内部调用 `Driver.drive_run()`。
7. 当前 assistant response 及其完整工具批次结束后、每次下一模型调用前，`RunTask` 通过 `RunLink` 请求 actor safe point；`SessionRuntime` actor 消费 steer、应用合法 profile 变化、收集 typed context materials，并回复 `NextModelCallPlan`。Rig segment 原本将结束时，`before_run_finish` 再执行一次 steering check。
8. `Driver` 对已 committed persistent messages 执行 Rig segment rollover，应用完整 replacement profile 后调用 `prompt::project_model_call(ModelCallProjectionInput { profile: &active_prompt_profile, ... })`，得到唯一 `ModelInputProjection`，再构造 `ModelCallRequest`。本地 projection context limit 或 provider `ContextOverflow` 都归约为带来源的 `DriverError::ContextLimitExceeded`。同一公开 run 的多个 Rig segment 沿用原 `RunId` 和外层预算。Driver 不持有 `PromptTurn` 或 resources；`BeforeModelCall` 和 provider payload hook 仍属于 `ModelGateway.call_model(...)`。
9. 工具调用通过 run-only `ToolBatchInvoker` 治理和执行；invoker 返回 normalized candidate 后，`RunTask` 通过 `RunLink` 提交完整 tool-round candidate。`SessionRuntime` actor 应用后期 `ToolResultBeforeCommit`（MVP 不启用）并重新校验，再发布最终 `tool_call_finished`，把完整 assistant tool-call message 与全部 tool results 按 `call_index` 组装为一个 `ToolRound` batch。commit 成功后，actor 先发布该 assistant message 的 `message_assistant_finished`，再按 `call_index` 发布 `message_tool_result_appended*`，然后把 actor-finalized `ToolBatchResult` 回复 host；Driver 才回填 Rig 并允许下一模型调用。
10. `RunTask` 返回正常 final assistant draft 后，通过 `RunLink` 提交 terminal effect；`SessionRuntime` actor 将消息组装为 `AssistantFinal` batch。commit 成功后发出 `message_assistant_finished`、最终 `usage_updated`，再发唯一终态 `run_finished { status: completed }`。随后按 post-run arbitration 处理 retry、pending manual compact、auto compaction 和 queued steering/follow-up continuation；只有这些动作都不会立即开始时，才切回 `idle` 并发出 `session_settled`。

稳定 batch 的数量由实际会话事实决定，不按 run 固定。正常 text-only `Completed` run 至少提交 `UserInput` 和 `AssistantFinal` 两个 batch。包含工具时，每个将被下一次模型调用消费的完整 tool-call/result round 都必须先提交一个 `ToolRound` batch；并行结果按 `call_index` 归约后共享该 batch。streaming delta、tool output delta、queue state、partial assistant、pending approval 和执行中的 tool round 不进入 writer。

`SessionRuntime` 是 batch 组装、commit 时机和 commit 后领域事件发布的唯一 owner。固定顺序是 `commit → 由对应 owner 应用 CommittedSessionBatch 到 runtime projection / required SessionIndex projection → 发布领域事件 → 调用可选 AfterSessionCommit observers`；observer 失败只进入 diagnostics，不能把已 committed 事实改判为失败或阻止领域事件。`Driver`、`Tools`、hook 和 UI adapter 都不能直接写 session。`commit()` 返回错误时，当前 run 进入 failed terminal path；不得继续下一模型调用，也不得把仅存在内存的 batch 当成 durable history。

abort、failure、session close 或 host shutdown 只保留已经成功 committed 的稳定单元。当前 partial assistant、waiting approval 或 incomplete tool round 被丢弃；如果已发布 `message_assistant_started`，仍要发布一次 `message_assistant_finished` 关闭 UI lifecycle，但该 partial 不进入 session。失败路径发出 `diagnostics_error` + `run_finished { status: failed }`，abort 路径发出 `run_finished { status: aborted }`；MVP 不生成 synthetic tool result，也不恢复中断中的 run。

`SessionRuntime` 不把 run cancellation token 传给 `SessionWriter`。同一 session 的 actor control ordering 决定竞态边界：若 actor 在 commit admission 前先观察到 abort/close/shutdown，完整但尚未提交的 candidate 仍可丢弃；若 actor 已调用 `commit()`，writer 获胜并必须得到确定结果。此后 actor 先阻止 RunTask 启动后续模型或工具工作，再等待该 commit 得到 `Ok` / `Err`：成功 batch 保留，失败 batch 不进入投影。若 final assistant 已成功 commit，则该 run 已跨过 completed terminal boundary，随后到达的 `AbortRun` 应观察为没有可 abort 的 active run；若完整 `ToolRound` 已 commit 但下一模型调用尚未开始，abort 可以保留该 round并以 aborted 结束当前 run。
