# Driver

`Driver` 是 `SessionRuntime` 内部的 Rig sans-IO 适配器。它只负责把 Rig `AgentRunStep` 转换成产品运行时可执行的 I/O 请求，并把 I/O 结果喂回 Rig。一次公开的 MiniCore run 通常推进一个 Rig `AgentRun`；active Steer 需要 continuation 时，可以在同一 `RunId` 下顺序推进多个 Rig `AgentRun` segment。

一句话边界：

```text
Driver drives Rig AgentRun.
It does not own model providers, tools, resources, queues, UI, or session persistence.
```

## 设计决定

本项目使用 Rig 的 `AgentRun / AgentRunStep` sans-IO 路径，不使用 Rig 高阶 runner 自动执行工具。

```text
AgentRun::next_step()
  ├─ CallModel: Driver 调 host.generate_model_turn(...)，再 AgentRun::model_response(...)
  ├─ CallTools: Driver 调 host.execute_and_commit_tool_round(...)，再 AgentRun::tool_results(...)
  └─ Done: Driver 返回 ConversationRunResult 给 SessionRuntime
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
- 聚合当前 `drive_conversation()` 内的 `ModelCallUsage` facts，返回 run-level `UsageSummary` 给 `SessionRuntime`。
- 把 cancellation token 传给 host I/O。

`Driver` 不拥有：

- provider registry、模型凭据、模型 fallback、auto retry。
- tool registry、active tools、tool policy、approval、sandbox、executor。
- resource loading、skill expansion、prompt template expansion、system prompt building。
- compaction、summary prompt、context trimming、session context reconstruction。
- steering/follow-up/next-turn queue storage。
- session usage stats、context usage、成本展示口径。
- session writer、stable batch commit、session tree。
- UI command/event transport。

## Rig 工具执行能力

| Rig 使用方式 | 工具执行者 | 本项目策略 |
| --- | --- | --- |
| 高阶 `Agent::prompt()` / `PromptRequest` / streaming prompt | Rig 内部通过 `ToolServerHandle::call_tool(...)` 执行 | 不作为主路径 |
| `AgentRun` / `AgentRunStep` sans-IO | 外部 driver 执行 `CallTools`，再调用 `AgentRun::tool_results(...)` | 主路径 |

本项目选择第二种。原因是桌面 Agent 需要工具审批、工作区沙箱、运行时事件、pending approval、abort、session writes、mutation queue 和暂停恢复。若直接交给 Rig 高阶 runner 自动执行工具，这些产品级控制会被迫塞进 Rig tool wrapper，边界会变浅。

## Run 编排职责

MiniCore 将底层 Agent 状态推进、I/O 适配和产品级状态分开：

```text
Rig AgentRun
  → owns protocol state machine

Driver
  → drives one or more sequential Rig AgentRun segments and maps I/O

SessionRuntime
  → owns queues, phase, persistence, tool state, model state, future run safe-point hooks

Tools
  → owns tool governance and execution

ModelGateway behind DriverHost::generate_model_turn
  → owns provider call, credentials, fallback policy, future provider hooks
```

## Public Interface

`Driver` 对 `SessionRuntime` 暴露一个主入口：

```rust
pub async fn drive_conversation(
    &self,
    request: ConversationDriveRequest,
    host: &mut dyn DriverHost,
    cancel: CancellationToken,
) -> ConversationRunResult;
```

不要暴露 `run_prompt()` / `continue_run()` 两个顶层方法。它们容易和 Rig 自身 prompt API、session continuation、queued follow-up 混淆。统一使用 `drive_conversation()`，并以 ordered `ConversationSeed` 表达本次 run 的已提交会话起点。

`drive_conversation()` 不再返回 `Result<ConversationRunResult, DriverError>`。普通 provider/tool/protocol 失败应归约为 `ConversationRunResult::Failed`，让 `SessionRuntime` 统一完成 diagnostics、UI lifecycle 收尾和 run terminal event；失败或 abort 时的 partial output 不进入 session writer。

`RunId` 绑定一次公开的 `drive_conversation()`，不绑定某个具体 Rig `AgentRun` 对象。Steer 在已完成 assistant/tool turn 的安全点被 commit 后，Driver 只接收 `CommittedConversationDelta` 并应用到 run-local `LiveConversation`；如需创建下一个 Rig segment，history/prompt split 只能发生在 `driver/rig.rs` 私有 adapter 内部。该 rollover 不重新分配 `RunId`，也不重置 run-level usage、limits、cancellation 或事件 correlation。Rig segment 不进入 `ConversationDriveRequest`、协议、snapshot 或 session storage。

```rust
pub struct ConversationDriveRequest {
    pub run_id: RunId,
    pub session_id: SessionId,
    pub turn: DriverTurnInput,
    pub seed: ConversationSeed,
    pub limits: DriveLimits,
}

pub struct DriverTurnInput {
    pub model: ModelSelection,
    pub context_profile: ModelContextProfile,
    pub thinking_level: ThinkingLevel,
    pub stream_options: StreamOptions,
}

pub struct ConversationSeed {
    pub messages: Vec<MessageRecord>, // committed order; current user exactly once
}
```

`DriverTurnInput` 是 `TurnState` 的窄投影，不是新的状态 owner。它只包含 `Driver` 推进 Rig、调用 `Prompt.assemble_model_context(...)` 和构造 `ModelCallRequest` 所需的模型选择、`ModelContextProfile` 与模型调用选项。它不能包含完整 `PreparedMessageTurn`、`Arc<TurnResourceSnapshot>`、`CwdResourceRevision`、`ContextUsageView`、session storage、queue state、`Tools` 内部状态、executor handle、policy/approval state 或 `ResourceManager` handle。`Driver` 内部的 `LiveConversation` 只能从 `ConversationSeed` 初始化，并且只通过 `CommittedConversationDelta` 追加/替换，不能应用 draft message。

`SessionRuntime` 仍然拥有完整 `TurnState` 和 committed-only `CommittedConversationState` 热视图。run 启动时它从 `TurnState` 投影出 `DriverTurnInput`，从 committed 热视图构造 `ConversationSeed`，并把两者放进 `ConversationDriveRequest`；同时把 `TurnState.resources` 留在 per-run `SessionDriverHost` 中，用于构造 `ToolRunContext`、safe point 决策和未来 `StepResourceSnapshot` parent。这样 `Driver` 的主输入 seam 不会泄漏资源 snapshot，也不会接收完整 `PreparedMessageTurn` / resources。

MVP 可以先不实现 resume admission，但保留 `ConversationRunResult::Suspended` 方向。Rig `AgentRun` 是 serializable，后续 approval pending、长工具或进程恢复会需要 suspend/resume seam；恢复请求仍由 `SessionRuntime` 先验证 committed state，再构造新的 `ConversationSeed`。

```rust
pub enum ConversationRunResult {
    Completed { final_message: MessageRecord, usage: UsageSummary },
    Aborted,
    Failed { error: DriverError },
    Suspended { reason: SuspendReason, serialized_run: SerializedAgentRun },
}
```

完整 tool rounds 已经在 `DriverHost::execute_and_commit_tool_round(...)` 返回前由 `SessionRuntime` 提交，不需要再次出现在 `ConversationRunResult`。`final_message` 是 run 正常完成后唯一尚待提交的最终稳定 assistant message；一次 assistant lifecycle 对应一个 message id。streaming 中断形成的 partial assistant 通过 `DriverEvent` / `CurrentRun` draft 关闭 UI lifecycle，不作为 `ConversationRunResult` 的可持久化 message 返回。

MVP 可以暂不产生 `Suspended`，但不要把接口设计死成只能 completed/failed/aborted。`Suspended` 不是 terminal result；它表示 `Driver` 已经在可恢复 checkpoint 停住，并把继续同一个未完成 AgentRun 所需的 `serialized_run` 交回 `SessionRuntime`。`SessionRuntime` 为它分配 `ResumeId`，只在当前 host 内存投影为 `CurrentRunState::Suspended { resume_id, reason }`，并发出 `run_suspended`，而不是 `run_finished { status: paused }`。

## Host Interface

`DriverHost` 是 `Driver` 与 `SessionRuntime` 之间的 seam。它让 driver 不直接持有 provider、`Tools`、queue 或 persistence。

```rust
pub enum ToolBatchHostError {
    Cancelled,
    Failed { error: RuntimeError },
}

pub struct CommittedToolRound {
    pub result: ToolBatchResult,
    pub delta: CommittedConversationDelta,
}

#[async_trait]
pub trait DriverHost {
    async fn emit_driver_event(&mut self, event: DriverEvent) -> Result<(), RuntimeError>;

    async fn generate_model_turn(
        &mut self,
        request: ModelCallRequest,
        sink: ModelStreamSink,
        cancel: CancellationToken,
    ) -> Result<ModelCallResult, ModelCallError>;

    async fn execute_and_commit_tool_round(
        &mut self,
        request: ToolBatchRequest,
        sink: ToolUpdateSink,
        cancel: CancellationToken,
    ) -> Result<CommittedToolRound, ToolBatchHostError>;

    async fn commit_pending_messages(
        &mut self,
        checkpoint: TurnCheckpoint,
    ) -> Result<NextConversationStep, RuntimeError>;

    async fn before_run_finish(
        &mut self,
        checkpoint: FinishCheckpoint,
    ) -> Result<FinishDecision, RuntimeError>;
}
```

命名意图：

- `generate_model_turn`：真实 provider 调用不属于 `Driver`；host 委托 `ModelGateway.generate_model_turn(...)`。
- `execute_and_commit_tool_round`：工具执行与 stable commit 都不属于 `Driver`；host 必须在返回前取得 committed result 和 delta。
- `commit_pending_messages`：在下一次模型调用前让 owner actor 消费并提交 Steer 等 pending message；无消息时仍可返回 context/control step。
- `before_run_finish`：Rig segment 原本将结束时的最后安全点，允许 `SessionRuntime` 决定 finish、用已 committed Steer rollover、suspend、retry 或 abort；FollowUp 由 `SessionRuntime` 在公开 run 结束后的 post-run arbitration 中启动新 run。

## SessionDriverHost Wrapper

并发与 ownership 以 [ADR 0021](../adr/0021-session-runtime-separates-actor-control-from-run-execution.md) 为权威。`DriverHost` 不是长期 runtime 对象；生产实现使用由单次 `RunTask` 持有的 owned `SessionDriverHost`。它不直接实现于 `SessionRuntime`，也不保存指向 actor-owned `Tools`、queues、`CurrentRun`、writer 或 event sink 的长期 mutable reference。

```rust
pub struct RunTask {
    request: ConversationDriveRequest,
    driver: Driver,
    host: SessionDriverHost,
}

impl RunTask {
    pub async fn run(self) {
        let RunTask { request, driver, mut host } = self;
        let run_id = request.run_id;
        let cancel = host.cancel.child_token();
        let result = driver.drive_conversation(request, &mut host, cancel).await;
        let after_progress = host.progress.watermark();
        let _ = host.run_link.drive_finished(run_id, result, after_progress).await;
    }
}

pub struct SessionDriverHost {
    session_id: SessionId,
    run_id: RunId,
    cwd: PathBuf,
    turn_resources: Arc<TurnResourceSnapshot>,
    tools: ToolBatchInvoker,
    model_gateway: Arc<ModelGateway>,
    run_link: RunLink,
    progress: RunProgressSink,
    cancel: CancellationToken,
}
```

MVP 中 `tools` 在整个 work chain 内固定。后续若启用 safe-point tool/profile mutation，actor 的私有 `RunLink` reply 可以同时携带 `NextConversationStep` 和 replacement `ToolBatchInvoker`；`SessionDriverHost::commit_pending_messages(&mut self, ...)` 必须先校验 `executor.fingerprint() == replacement_profile.tool_profile_fingerprint`，再原子替换自己的 tool executor，最后只把不含 tool governance state 的 step 返回 `Driver`。

`RunLink` 是 `RunTask` 回到 owner actor 的私有窄 seam，不是 `SessionRuntimeHandle`：

```rust
pub enum RunLinkError {
    StaleRun,
    Cancelled,
    RuntimeClosed,
    Failed { error: RuntimeError },
}

impl RunLink {
    async fn emit_lifecycle(&self, run_id: RunId, event: DriverEvent, after_progress: RunProgressSeq) -> Result<(), RunLinkError>;
    async fn commit_pending_messages(&self, run_id: RunId, checkpoint: TurnCheckpoint, after_progress: RunProgressSeq) -> Result<NextConversationStep, RunLinkError>;
    async fn before_run_finish(&self, run_id: RunId, checkpoint: FinishCheckpoint, after_progress: RunProgressSeq) -> Result<FinishDecision, RunLinkError>;
    async fn execute_and_commit_tool_round(&self, run_id: RunId, candidate: ToolRoundCandidate, after_progress: RunProgressSeq) -> Result<CommittedToolRound, ToolBatchHostError>;
    async fn drive_finished(&self, run_id: RunId, result: ConversationRunResult, after_progress: RunProgressSeq) -> Result<(), RunLinkError>;
}
```

`RunLink` request 必须验证 `run_id` 仍对应 actor 的 active `CurrentRun`；旧 RunTask 的 late effect 返回 typed `StaleRun` / `Cancelled`，不能修改后来 run，也不能被误归类成新 run 的普通 failure。它不能提交 `SubmitPrompt`、`SetModel`、`Compact` 或其他任意 session command。

`RunProgressSink` 是独立 bounded/coalesced progress lane，承载 text/tool output delta 等高频更新，并为每次成功提交返回单调 `RunProgressSeq`。SessionDriverHost 在发送 lifecycle/commit/terminal control request 时附带当前 watermark；actor 必须先归约 progress through watermark，再发布依赖它的 finished/terminal fact。external approval/abort command 不等待 progress drain即可先改变 control state/cancel token，但 lifecycle 收尾仍执行 barrier。

`SessionDriverHost` 的方法按下面方式实现：

- `emit_driver_event`：高频 delta 分类到 `RunProgressSink`，started/finished/usage/checkpoint 等 lifecycle 分类到 `RunLink` control lane；RunTask/Driver 不直接发布 UI event。
- `generate_model_turn`：直接调用 shared `ModelGateway.generate_model_turn(...)`，stream sink 把 delta 作为 run effect 交给 actor；host 不读取 provider/auth state。
- `commit_pending_messages` / `before_run_finish`：通过 `RunLink` request/reply，让 actor 在单一 ordering point 处理 queues、profile、context、abort/suspend 和 commit-gated steer。
- `execute_and_commit_tool_round`：通过 run-only `ToolBatchInvoker` 执行治理和工具工作；得到完整 normalized candidate 后交给 actor，只有 actor 回复 commit success、最终 `ToolBatchResult` 和 `CommittedConversationDelta` 后才把该最终值返回 Driver/Rig，并由 Driver 应用 delta 到 `LiveConversation`。

工具 round 事务固定为：

```text
RunTask / SessionDriverHost
  → ToolBatchInvoker.invoke_batch(...)
  → complete ToolBatchResult
  → RunLink.execute_and_commit_tool_round(run_id, candidate, progress_watermark)

SessionRuntime actor
  → validate active run + commit admission against abort
  → obtain pending assistant tool-call draft from CurrentRun
  → apply future ToolResultBeforeCommit and revalidate
  → commit_tool_round(SessionWriteBatch::tool_round(...))
  → apply_committed_messages(CommittedConversationDelta)
  → mark CurrentRun round committed
  → publish final tool_call_finished values
  → publish message_assistant_finished + ordered message_tool_result_appended*
  → reply final committed ToolBatchResult + CommittedConversationDelta

RunTask
  → return the actor-finalized ToolBatchResult to Driver
  → Driver applies CommittedConversationDelta to LiveConversation
  → AgentRun::tool_results(...)
```

若 actor 在 commit admission 前已观察到 abort/close/shutdown，返回 `ToolBatchHostError::Cancelled` 并丢弃 candidate；commit 一旦开始就不接收 run cancellation。`ToolBatchHostError` 必须原样保留 cancellation discriminant；Driver 把 `Cancelled` 归约为 aborted，把 `Failed` 归约为 failed，二者都不构造 partial `ToolRound`。

生产代码禁止 `self.driver.drive_conversation(request, self, cancel).await`、`Arc<Mutex<SessionRuntime>>` 包裹完整 run，或 `SessionDriverHost<'a> { tools: &'a mut Tools, queues: &'a mut QueueState, ... }`。这些形态会阻塞 per-session mailbox。纯 Driver 单测仍可使用无 commit 的 fake host，但不能作为生产 adapter。

选择 owned wrapper 的原因：

- actor 在 model/tool/approval wait 期间仍可处理 `DecideToolApproval`、`AbortRun`、steer/follow-up/next-turn、pending compact、snapshot 和 shutdown。
- `run_id`、turn resources、event correlation、checkpoint context 和 cancellation 的生命周期准确落在一次 `RunTask` 内。
- `ConversationDriveRequest` 继续只携带窄 `DriverTurnInput` 和 ordered `ConversationSeed`；resources 不泄漏进 Driver 主输入。
- Driver 单测只依赖 fake `DriverHost`；SessionRuntime 并发测试则通过显式 `SessionRuntimeHandle` 和 fake RunTask/RunLink effect 覆盖。

## Driver Events

`DriverEvent` 是 driver 内部事件，不是 UI 直接消费的 `agent_runtime_protocol::Event`。`run_started` 不来自 DriverEvent：`SessionRuntime` actor 在建立 `CurrentRun` 后、启动 `RunTask` 前直接发布它，保证任何 model/provider/tool wait 都已有公开 RunId。RunTask 通过 `RunLink` 上报这些内部事件，actor 归约 streaming/current-run projection，组装并提交稳定 session batches，再发 UI event、更新 snapshot。

driver 的 `RunFinished { result }` 只表示 Rig drive 已经结束；UI 侧唯一 run terminal event 是 `run_finished { status, ... }`，由 `SessionRuntime` 在完成必要的 stable batch commit、错误归类和后续队列判断后发出。`ConversationRunResult::Suspended` 不应归约为 `run_finished`；它由 `SessionRuntime` 分配 `ResumeId`、仅在同一 host 生命周期的内存中保存 resume state，并发出非终态 `run_suspended`。

```rust
pub enum DriverEvent {
    ModelCallStarted { run_id: RunId, turn: usize },
    ModelTextDelta { message_id: MessageId, delta: String },
    ModelCallFinished { run_id: RunId, turn: usize, usage: Option<ModelCallUsage> },
    AssistantMessageStarted { message_id: MessageId },
    AssistantMessageDelta { message_id: MessageId, delta: MessageDelta },
    AssistantMessageFinished { message: MessageRecord },
    BeforeNextModelCall { checkpoint: TurnCheckpoint },
    BeforeRunFinish { checkpoint: FinishCheckpoint },
    RunFinished { result: ConversationRunResultSummary },
}
```

工具 proposed/approval/started/output/result-ready updates 来自 `Tools` / `ToolUpdateSink`，不重复包装成 `DriverEvent`；最终公开 `tool_call_finished` 由 `SessionRuntime` 在 `ToolResultBeforeCommit` 和 `ToolRound` commit 成功后发布。事件命名避免使用 `MessageStarted` 这类太泛的名字，尽量说明 assistant/model/tool 来源。`DriverEvent::AssistantMessageFinished` 只表示 provider/Rig 已形成完整 assistant draft，不能被直接转发为公共 `message_assistant_finished`：tool-call assistant 必须等对应 `ToolRound` commit，final assistant 必须等 `AssistantFinal` commit；abort/failure 才以非持久化 finished 关闭已 started lifecycle。

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
fn apply_next_conversation_step(...)
fn assemble_model_context(input: AssembleModelContextInput<'_>)
fn build_model_call_request(assembled, model_options)
async fn generate_model_turn(...)
fn assemble_model_turn(...)
fn feed_model_response_to_rig(...)
```

工具侧：

```rust
fn map_pending_tool_call(...)
async fn execute_and_commit_tool_round(...)
fn map_tool_result_for_rig(...)
fn feed_tool_results_to_rig(...)
```

安全点：

```rust
async fn commit_pending_messages(...)
async fn check_before_run_finish(...)
```

不要使用这些名字：

```text
prepare-next-turn 风格命名 // turn 语义混淆；SessionRuntime 侧使用 prepare_message_turn
execute_tool_call      // 暗示 driver 执行工具
save_message           // 持久化不属于 driver
build_system_prompt    // prompt 构建不属于 driver
reload_tools           // 工具生命周期不属于 driver
```

## CallModel Step

```text
AgentRunStep::CallModel { private_rig_step }
  → apply current NextConversationStep
  → Prompt.assemble_model_context(AssembleModelContextInput {
        profile: &active_model_context_profile,
        committed_conversation: &live_conversation,
        transient_context,
        output_contract: active_output_contract.as_ref(), // MVP Agent run 为 None
        purpose: ModelCallPurpose::AgentRun,
    })
  → validate AssembledModelContext
      → ContextLimitExceeded: do not call ModelGateway; return ConversationRunResult::Failed
        { DriverError::ContextLimitExceeded { source: PromptAssembly, ... } }
  → build_model_call_request(assembled, request.turn model/options)
  → host.generate_model_turn(request, ModelStreamSink, cancel)
      → provider/model gateway handles credentials, future hooks, payload, fallback
      → ModelStreamSink emits model/assistant deltas as DriverEvent
  → assemble_model_turn(ModelCallResult)
  → AgentRun::model_response(model_turn)
```

`Driver` 可以构造 `ModelCallRequest`，但不直接持有 provider registry 或 credentials。`DriverHost::generate_model_turn(...)` 必须保留 typed `ModelCallError`：`kind == Cancelled` 归约为 `ConversationRunResult::Aborted`，`kind == ContextOverflow` 归约为 `DriverError::ContextLimitExceeded { source: Provider, ... }`，其他 kind 按 retry/failure policy 处理，不能先擦成 generic `RuntimeError`。本地 `PromptError::ContextLimitExceeded` 归约到同一个 recovery class，但保留 `PromptAssembly` 来源和估算值；两者都使用 `ConversationRunResult::Failed`，不新增平行 terminal variant。后期如果启用 provider request/payload hooks，也只能在 `host.generate_model_turn(...)` 后面的 `ModelGateway` 中执行；driver 只传递必要上下文。

`ModelCallRequest` 必须是 MiniCore-owned provider-neutral 类型。`Driver` 初始化 run-local `active_model_context_profile = request.turn.context_profile.clone()`、`live_conversation = LiveConversation::from_seed(request.seed)` 和 MVP `active_output_contract: Option<OutputContract> = None`；safe point 若返回 replacement profile，只能整体替换该值。每次 CallModel 时，Driver 把 `&active_model_context_profile`、`&live_conversation` 和 safe-point context materials 交给 `Prompt.assemble_model_context(...)`，得到已校验的 `AssembledModelContext`；随后把整个 assembled context 作为 `ModelCallRequest.input`，并从 `DriverTurnInput` 复制 `ModelSelection`、thinking level 和 stream options。Driver 不能拆开或重组 assembled fields、读取 `TurnState.resources`、解析 `ProviderRegistry`、读取 `AuthStore`、构造 provider client、持有 base URL，也不能把 `rig::providers::*` 类型放进 Prompt interface 或 request。Agent loop 路径固定使用 `ModelCallPurpose::AgentRun`，`max_output_tokens` 默认由 output contract 或模型设置决定。完整请求结构见 [ModelGateway](model-gateway.md)。

MVP Agent run 的 `active_output_contract` 固定为 `None`；`AssembleModelContextInput.output_contract` 先保留 provider-neutral 能力，供后续结构化输出或 required-tool-choice 场景使用。未来若启用，它必须来自 MiniCore-owned typed call policy，而不是从 `PreparedMessageTurn`、provider payload 或未验证的 Rig 字段侧取；Rig 到该类型的具体映射由后续 Driver integration spike 决定。

## CallTools Step

```text
AgentRunStep::CallTools { calls }
  → map PendingToolCall[] -> ToolBatchRequest { calls: ToolInvocation { call_index, ... } }
  → host.execute_and_commit_tool_round(request, ToolUpdateSink)
      → SessionDriverHost calls run-only ToolBatchInvoker.invoke_batch(...)
      → Tools handles registry/active/policy/approval/grants/sandbox/coordinator/executor
      → RunLink asks SessionRuntime actor to execute_and_commit_tool_round(...) as one complete ToolRound batch
  → apply CommittedToolRound.delta to LiveConversation
  → map CommittedToolRound.result -> Rig tool results
  → AgentRun::tool_results(results)
```

`Driver` 必须保证 Rig 要求的协议：

- 每个 pending tool call 都要有一个 tool result。
- tool result 的 call id 必须匹配 pending call。
- 结果可以按执行完成顺序返回，但 feed back 前要满足 Rig/provider 对 tool result message 的要求。
- 非 abort 的工具错误应作为 error tool result 回填，而不是让 run 非正常崩溃。
- host 只有在完整 assistant tool-call/result round 通过 `SessionWriter.commit(...)` 成功后才能返回；commit 失败归约为 `ConversationRunResult::Failed`，不得进入下一次 `CallModel`。

## Safe Points

`Driver` 不直接拥有队列，但它提供两个安全点给 `SessionRuntime`：

```text
commit_pending_messages
  在当前 assistant turn 的 model response 和完整 tool batch 已经结束、tool results 已回填后、下一次 CallModel 前触发。
  SessionRuntime 返回组合式 NextConversationStep，可同时携带 committed steering delta、整体替换 ModelContextProfile、patch model options、提供 typed context materials，或 abort/suspend。

before_run_finish
  在 Rig segment 即将 Done 时触发。
  SessionRuntime 可决定结束、消费刚到达的 Steer 并 rollover、suspend、retry 或 abort。
```

建议类型：

```rust
pub struct NextConversationStep {
    pub control: NextModelCallControl,
    pub committed_delta: Option<CommittedConversationDelta>,
    pub model_context_profile: Option<ModelContextProfile>,
    pub model_patch: Option<ModelSelection>,
    pub thinking_level: Option<ThinkingLevel>,
    pub stream_options: Option<StreamOptions>,
    pub transient_context: Vec<ContextMaterialContribution>,
}

pub enum NextModelCallControl {
    Continue,
    Abort { reason: String },
    Suspend { reason: SuspendReason },
}

pub enum FinishDecision {
    Finish,
    ContinueWithCommittedDelta(CommittedConversationDelta),
    Retry { reason: RetryReason },
    Abort { reason: String },
    Suspend { reason: SuspendReason },
}
```

`NextConversationStep` 是一次 safe-point transaction：Driver 只应用 `committed_delta` 到 `LiveConversation`，再整体替换可选 `ModelContextProfile` 和 model options，最后把 `transient_context` 交给 `Prompt.assemble_model_context(...)`；scope 保存在每个 material 内。Rig 0.40.0 没有公开的 mid-run history append 时，`driver/rig.rs` 私有 adapter 可用 `LiveConversation` 生成下一个 segment 需要的 Rig history/prompt split；该 split 不进入公共 Driver seam。step 不能携带 resources、context provider handle、queue/storage 或 tool governance state；system prompt 与 tool schemas 不允许作为两个独立 patch 字段。future replacement executor 通过 `RunLink` 的私有 host reply 与 step 同时返回，由 `SessionDriverHost` 在把 step 交给 Driver 前校验 `executor.fingerprint() == model_context_profile.tool_profile_fingerprint` 并完成本地替换。MVP 可以先让 transient context 为空，但不应退回互斥 decision enum。

## Error And Abort Semantics

- prompt assembly / provider context limit：统一返回 `ConversationRunResult::Failed { DriverError::ContextLimitExceeded { source, ... } }`，由 `SessionRuntime` 执行一次有界 compaction recovery；来源保留为 `PromptAssembly` 或 `Provider`，未完成 assistant draft 不持久化。
- 其他 provider error：返回 `ConversationRunResult::Failed`，由 `SessionRuntime` 决定 auto retry 和 diagnostics；未完成 assistant draft 不持久化。
- tool error：优先转换为 error tool result 并喂回 Rig，除非是 abort。
- cancellation：`cancel` 必须传播到 `host.generate_model_turn`、`host.execute_and_commit_tool_round`、approval wait 和 stream sink。
- out-of-protocol Rig error：返回 `ConversationRunResult::Failed`；此前已 committed 的稳定 rounds 保留，当前未提交单元丢弃。
- stream prematurely ended：构造结构化 driver error，不让 UI 卡在 running 状态。

## 不应承担

`Driver` 不应：

- 自己选择默认模型。
- 自己读取 API key。
- 自己执行 tool executor。
- 自己等待 UI approval。
- 自己读取资源或构建 system prompt。
- 自己执行 compaction 或摘要模型调用。
- 自己展开 `/skill <name>`、兼容 `/skill:name` 或 prompt template。
- 自己保存 session entry。
- 自己管理 active tools 或 queues。
- 暴露 Rig 类型给 UI adapter。
