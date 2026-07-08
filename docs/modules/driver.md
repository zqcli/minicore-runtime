# Driver

`Driver` 是 `SessionRuntime` 内部的 Rig sans-IO 适配器。它只负责把 Rig `AgentRunStep` 转换成产品运行时可执行的 I/O 请求，并把 I/O 结果喂回 Rig。

一句话边界：

```text
Driver drives Rig AgentRun.
It does not own model providers, tools, resources, queues, UI, or session persistence.
```

## 设计决定

本项目使用 Rig 的 `AgentRun / AgentRunStep` sans-IO 路径，不使用 Rig 高阶 runner 自动执行工具。

```text
AgentRun::next_step()
  ├─ CallModel: Driver 调 host.call_model(...)，再 AgentRun::model_response(...)
  ├─ CallTools: Driver 调 host.invoke_tool_batch(...)，再 AgentRun::tool_results(...)
  └─ Done: Driver 返回 DriveResult 给 SessionRuntime
```

这不是重新实现 Agent loop。Rig 仍然拥有协议级状态机；`Driver` 只负责 I/O 分派、类型映射、事件映射和 cancellation propagation。

## 与 Rig 的边界

Rig 拥有：

- 何时 `CallModel`。
- 何时 `CallTools`。
- 何时 `Done`。
- turn counting 和 max turns。
- tool-call validation。
- pending tool call 与 tool result 的匹配。
- tool result message threading。
- protocol-level response construction and tool/result threading。
- final response construction。

`Driver` 拥有：

- 创建和推进 `AgentRun`。
- 分派 `CallModel` / `CallTools` / `Done`。
- Rig 类型与本项目类型之间的转换。
- 将 provider stream / tool update 转成 `DriverEvent`。
- 把 `ModelCallResult` 喂回 `AgentRun::model_response(...)`。
- 把 `ToolInvocationResult` 喂回 `AgentRun::tool_results(...)`。
- 聚合当前 `drive_run()` 内的 `ModelCallUsage` facts，返回 run-level `UsageSummary` 给 `SessionRuntime`。
- 把 cancellation token 传给 host I/O。

`Driver` 不拥有：

- provider registry、模型凭据、模型 fallback、auto retry。
- tool registry、active tools、tool policy、approval、sandbox、executor。
- resource loading、skill expansion、prompt template expansion、system prompt building。
- compaction、summary prompt、context trimming、session context reconstruction。
- steering/follow-up/next-turn queue storage。
- session usage stats、context usage、成本展示口径。
- session file writes、save point、session tree。
- UI command/event transport。

## Rig 工具执行能力

| Rig 使用方式 | 工具执行者 | 本项目策略 |
| --- | --- | --- |
| 高阶 `Agent::prompt()` / `PromptRequest` / streaming prompt | Rig 内部通过 `ToolServerHandle::call_tool(...)` 执行 | 不作为主路径 |
| `AgentRun` / `AgentRunStep` sans-IO | 外部 driver 执行 `CallTools`，再调用 `AgentRun::tool_results(...)` | 主路径 |

本项目选择第二种。原因是桌面 Agent 需要工具审批、工作区沙箱、运行时事件、pending approval、abort、session writes、mutation queue 和暂停恢复。若直接交给 Rig 高阶 runner 自动执行工具，这些产品级控制会被迫塞进 Rig tool wrapper，边界会变浅。

## 来自 pi agent-loop 的等价经验

pi `agent-loop` 的产品路径可以抽象成：

```text
runAgentLoop / runAgentLoopContinue
  → call model through streamFn
  → detect tool calls
  → execute tools through product tool definitions and hooks
  → append tool results
  → call prepareNextTurn safe point
  → drain steering/follow-up queues at safe points
```

本项目中这些职责拆为：

```text
Rig AgentRun
  → owns protocol state machine

Driver
  → drives steps and maps I/O

SessionRuntime
  → owns queues, phase, hooks, persistence, tool state, model state

Tools
  → owns tool governance and execution

ModelGateway behind DriverHost::call_model
  → owns provider call, credentials, provider hooks, fallback policy
```

## Public Interface

`Driver` 对 `SessionRuntime` 暴露一个主入口：

```rust
pub async fn drive_run(
    &self,
    request: DriveRequest,
    host: &mut dyn DriverHost,
    cancel: CancellationToken,
) -> DriveResult;
```

不要暴露 `run_prompt()` / `continue_run()` 两个顶层方法。它们容易和 Rig 自身 prompt API、session continuation、queued follow-up 混淆。统一使用 `drive_run()`，通过 `DriveEntry` 表达进入本次 run 的方式。

`drive_run()` 不再返回 `Result<DriveResult, DriverError>`。普通 provider/tool/protocol 失败应归约为 `DriveResult::Failed`，这样 `SessionRuntime` 仍能拿到已产生的 messages 做持久化、失败消息构造和 UI 收尾。

```rust
pub struct DriveRequest {
    pub run_id: RunId,
    pub session_id: SessionId,
    pub entry: DriveEntry,
    pub turn_state: TurnState,
    pub limits: DriveLimits,
}

pub enum DriveEntry {
    Prompt { messages: Vec<MessageRecord> },
    Continue { reason: ContinueReason },
    Resume { serialized_run: SerializedAgentRun },
}
```

MVP 可以先不实现 `Resume`，但类型上保留方向。Rig `AgentRun` 是 serializable，后续 approval pending、长工具或进程恢复会需要 pause/resume seam。

```rust
pub enum DriveResult {
    Completed { messages: Vec<MessageRecord>, usage: UsageSummary },
    Aborted { messages: Vec<MessageRecord> },
    Failed { error: DriverError, messages: Vec<MessageRecord> },
    Paused { reason: PauseReason, serialized_run: SerializedAgentRun },
}
```

MVP 可以暂不产生 `Paused`，但不要把接口设计死成只能 completed/failed。

## Host Interface

`DriverHost` 是 `Driver` 与 `SessionRuntime` 之间的 seam。它让 driver 不直接持有 provider、`Tools`、queue 或 persistence。

```rust
#[async_trait]
pub trait DriverHost {
    async fn emit_driver_event(&self, event: DriverEvent) -> Result<(), RuntimeError>;

    async fn call_model(
        &self,
        request: ModelCallRequest,
        sink: ModelStreamSink,
    ) -> Result<ModelCallResult, RuntimeError>;

    async fn invoke_tool_batch(
        &mut self,
        request: ToolBatchRequest,
        sink: ToolUpdateSink,
        cancel: CancellationToken,
    ) -> Result<ToolBatchResult, RuntimeError>;

    async fn before_next_model_call(
        &self,
        checkpoint: TurnCheckpoint,
    ) -> Result<NextModelCallDecision, RuntimeError>;

    async fn before_run_finish(
        &self,
        checkpoint: FinishCheckpoint,
    ) -> Result<FinishDecision, RuntimeError>;
}
```

命名意图：

- `call_model`：真实 provider 调用不属于 `Driver`。
- `invoke_tool_batch`：工具执行不属于 `Driver`；host 内部由 `SessionRuntime` 转发给 session-scoped `Tools` 子系统。
- `before_next_model_call`：比 `prepare_next_turn` 更准确，避免 Rig turn、user turn 和 session turn 混淆。
- `before_run_finish`：比 `drain_follow_up` 更深，允许 `SessionRuntime` 决定 finish、continue with follow-up、pause、retry 或 abort。

## Driver Events

`DriverEvent` 是 driver 内部事件，不是 UI 直接消费的 `agent_runtime_protocol::Event`。`SessionRuntime` 订阅并归约这些事件，再写 session、发 UI event、更新 snapshot。

driver 的 `RunFinished { result }` 只表示 Rig drive 已经结束；UI 侧唯一 run terminal event 是 `run_finished { status, ... }`，由 `SessionRuntime` 在完成必要的持久化、错误归类和后续队列判断后发出。

```rust
pub enum DriverEvent {
    RunStarted { run_id: RunId, session_id: SessionId },
    ModelCallStarted { run_id: RunId, turn: usize },
    ModelTextDelta { message_id: MessageId, delta: String },
    ModelCallFinished { run_id: RunId, turn: usize, usage: Option<ModelCallUsage> },
    AssistantMessageStarted { message_id: MessageId },
    AssistantMessageDelta { message_id: MessageId, delta: MessageDelta },
    AssistantMessageFinished { message: MessageRecord },
    ToolBatchStarted { calls: Vec<ToolCallView> },
    ToolCallStarted { call: ToolCallView },
    ToolCallDelta { call_id: ToolCallId, delta: String },
    ToolCallFinished { call_id: ToolCallId, result: ToolResultView, is_error: bool },
    ToolBatchFinished { results: Vec<ToolResultView> },
    BeforeNextModelCall { checkpoint: TurnCheckpoint },
    BeforeRunFinish { checkpoint: FinishCheckpoint },
    RunFinished { result: DriveResultSummary },
}
```

事件命名避免使用 `MessageStarted` 这类太泛的名字，尽量说明 assistant/model/tool 来源。

## Step Handlers

推荐内部函数命名：

```rust
async fn drive_agent_run(...)
async fn handle_agent_step(...)
async fn handle_call_model_step(...)
async fn handle_call_tools_step(...)
fn handle_done_step(...)
```

模型侧：

```rust
fn build_model_call_request(...)
async fn call_model_gateway(...)
fn assemble_model_turn(...)
fn feed_model_response_to_rig(...)
```

工具侧：

```rust
fn map_pending_tool_call(...)
async fn invoke_tools(...)
fn map_tool_result_for_rig(...)
fn feed_tool_results_to_rig(...)
```

安全点：

```rust
async fn check_before_next_model_call(...)
async fn check_before_run_finish(...)
```

不要使用这些名字：

```text
prepare_next_turn      // turn 语义混淆
execute_tool_call      // 暗示 driver 执行工具
save_message           // 持久化不属于 driver
build_system_prompt    // prompt 构建不属于 driver
reload_tools           // 工具生命周期不属于 driver
```

## CallModel Step

```text
AgentRunStep::CallModel { prompt, history, turn }
  → build_model_call_request(prompt, history, turn, turn_state)
  → host.call_model(request, ModelStreamSink)
      → provider/model gateway handles credentials, hooks, payload, fallback
      → ModelStreamSink emits model/assistant deltas as DriverEvent
  → assemble_model_turn(ModelCallResult)
  → AgentRun::model_response(model_turn)
```

`Driver` 可以构造 `ModelCallRequest`，但不直接持有 provider registry 或 credentials。provider request/payload hooks 也应在 `host.call_model(...)` 后面的 model gateway 中执行，driver 只传递必要上下文。

`ModelCallRequest` 必须是 MiniCore-owned provider-neutral 类型。`Driver` 只能从 `TurnState.model.selection` 复制 `ModelSelection { provider_id, model_id }`，并填入 messages、system prompt、active tool schemas、thinking level 和 stream options；它不能解析 `ProviderRegistry`、读取 `AuthStore`、构造 provider client、持有 base URL，也不能把 `rig::providers::*` 类型放进 request。完整请求结构见 [ModelGateway](model-gateway.md)。

## CallTools Step

```text
AgentRunStep::CallTools { calls }
  → map PendingToolCall[] -> ToolBatchRequest { calls: ToolInvocation { call_index, ... } }
  → host.invoke_tool_batch(request, ToolUpdateSink)
      → SessionRuntime forwards to Tools::invoke_batch(...)
      → Tools handles registry/active/policy/approval/grants/sandbox/coordinator/executor
  → map ToolBatchResult -> Rig tool results
  → AgentRun::tool_results(results)
```

`Driver` 必须保证 Rig 要求的协议：

- 每个 pending tool call 都要有一个 tool result。
- tool result 的 call id 必须匹配 pending call。
- 结果可以按执行完成顺序返回，但 feed back 前要满足 Rig/provider 对 tool result message 的要求。
- 非 abort 的工具错误应作为 error tool result 回填，而不是让 run 非正常崩溃。

## Safe Points

`Driver` 不直接拥有队列，但它提供两个安全点给 `SessionRuntime`：

```text
before_next_model_call
  在一次 model response 和 tool results 已经回填后、下一次 CallModel 前触发。
  SessionRuntime 可决定注入 steering messages、patch TurnState、切换模型/思考等级/active tools，或继续原样运行。

before_run_finish
  在 Rig 即将 Done 时触发。
  SessionRuntime 可决定结束、注入 follow-up 继续运行、暂停、retry 或 abort。
```

建议类型：

```rust
pub enum NextModelCallDecision {
    Continue,
    PatchTurnState(TurnStatePatch),
    InjectMessages(Vec<MessageRecord>),
    Abort { reason: String },
    Pause { reason: PauseReason },
}

pub enum FinishDecision {
    Finish,
    ContinueWithMessages(Vec<MessageRecord>),
    Retry { reason: RetryReason },
    Abort { reason: String },
    Pause { reason: PauseReason },
}
```

MVP 可以只实现 `Continue` / `Finish` / `ContinueWithMessages`，但函数名和 enum 应给后续扩展留下空间。

## Error And Abort Semantics

- provider error：返回 `DriveResult::Failed`，由 `SessionRuntime` 决定是否写失败 assistant message、auto retry 或 compaction recovery。
- tool error：优先转换为 error tool result 并喂回 Rig，除非是 abort。
- cancellation：`cancel` 必须传播到 `host.call_model`、`host.invoke_tool_batch`、approval wait 和 stream sink。
- out-of-protocol Rig error：返回 `DriveResult::Failed`，并附带当前已归约 messages。
- stream prematurely ended：构造结构化 driver error，不让 UI 卡在 running 状态。

## 不应承担

`Driver` 不应：

- 自己选择默认模型。
- 自己读取 API key。
- 自己执行 tool executor。
- 自己等待 UI approval。
- 自己读取资源或构建 system prompt。
- 自己执行 compaction 或摘要模型调用。
- 自己展开 `/skill:name` 或 prompt template。
- 自己保存 session entry。
- 自己管理 active tools 或 queues。
- 暴露 Rig 类型给 UI adapter。
