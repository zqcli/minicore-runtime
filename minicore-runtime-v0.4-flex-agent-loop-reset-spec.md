# MiniCore Runtime v0.4：Flex Agent Loop Reset 开发实施规格

## 0. 文档用途

本文档用于指导代码 Agent 对当前 `minicore-runtime` 进行一次明确的 breaking reset。

新的定位是：

> **MiniCore Runtime 是一次 Agent Loop 的 live execution manager。它负责一次运行的开始、模型请求、工具执行、运行中控制和结束，但不拥有 Session，不管理 Session 生命周期，也不负责 Session 持久化。**

本次重构只修改：

```text
minicore-runtime
```

明确不修改：

```text
minicore-agent
Provider 仓库
TUI / GUI / CLI / RPC
具体 Workspace
具体 Tool
具体 Model
具体 JSONL Store
```

其他仓库将在 Runtime v0.4 完成后单独迁移。

---

# 1. 决策背景

当前 v0.3 Runtime 同时承担：

```text
单 Session 生命周期
SessionSpec / SessionManifest
SessionLog
Conversation durable-first
expected head / AppendReceipt
Conversation validator
恢复与 unfinished Turn repair
Session health / Degraded
Transcript
Model / Tool Agent Loop
Cancellation / Interaction / Event
```

这使 Runtime 同时是：

```text
Agent 执行器
+
Session owner
+
Conversation ledger
+
Storage consistency coordinator
```

新的架构决策是：

```text
Session、History、JSONL、Profile 和长期配置
→ 上层 Agent 持有并管理

当前一次 Agent Loop 的运行状态
→ MiniCore Runtime 持有并管理
```

因此新 Runtime 不再证明：

```text
历史是一份可信账本
日志可以确定性重放
Tool 副作用与日志原子一致
整个 Session 使用固定执行契约
```

上层 Agent 可以：

```text
从 JSONL 重建 History
选择当前模型和 reasoning
创建 AgentLoop
接收 LoopReport
将本次新增内容写回 JSONL
```

MiniCore 不知道 JSONL，也不管理 JSONL。

---

# 2. 新架构的核心原则

## 2.1 单一 Session owner

完整 Session 只有上层 Agent 一个 owner。

Runtime 不再长期持有：

```text
完整 Session History
Session metadata
当前 Session model/profile
Session JSONL
Session loaded/unloaded 状态
```

## 2.2 单次运行对象

一个 `AgentLoop` 对应一次用户任务：

```text
初始 User Input
→ Model Request
→ Tool batch
→ Model Request
→ steer
→ Model Request
→ Final / Cancel / Failed
```

Loop 结束后不能再次提交普通用户消息。

下一次用户消息必须创建新的 `AgentLoop`。

## 2.3 一次 Loop 内可以有多个 Model Request

一个 Loop 内可以：

```text
Model A request
→ Tool batch
→ Model B request
→ Tool batch
→ Model C final
```

模型、reasoning、ToolSet、Policy 和 PromptProvider 可以在 request boundary 更新。

当前 in-flight Model Request 和由它产生的 Tool batch使用同一配置快照。

## 2.4 不恢复 live state

Runtime 不恢复：

```text
正在流式输出的 Model Request
正在执行的 Tool
pending steer
pending config update
pending Interaction
当前 Loop 的内存 delta
```

进程退出即丢失当前 Loop。

## 2.5 Event 只观察

`LoopEventStream` 是：

```text
bounded
best-effort
可丢失
不参与执行正确性
```

权威结果是：

```text
LoopHandle::wait()
AgentLoop::join()
LoopReport
```

## 2.6 局部执行必须自洽

放弃全 Session ledger 不代表不验证当前运行。

MiniCore 仍必须保证：

```text
一个 AgentLoop 只结束一次
一个 request 使用一个稳定配置快照
当前 Model Response 合法
当前 ToolCallId 不重复
ToolResult 与当前 ToolCall 匹配
一个 ToolCall 恰好生成一个 ToolResult
取消可以传播
Steer 和 Final 的竞争有明确线性化点
配置更新只在 request boundary 生效
```

---

# 3. 版本与兼容策略

## 3.1 版本

目标版本：

```text
minicore-runtime 0.4.0
```

这是明确的 breaking release。

## 3.2 冻结旧版

开发前创建：

```text
tag:
v0.3.0-durable-session-runtime

branch:
refactor/v0.4-flex-agent-loop
```

实际命名可以按仓库规范调整，但必须保存 v0.3 可维护基线。

## 3.3 不建立兼容层

最终代码中不得保留：

```text
type SessionRuntime = AgentLoop
type SessionHandle = LoopHandle
deprecated SessionLog adapter
v0.3/v0.4 feature flag
旧 create/load API
旧 Manifest reader
旧 Store compatibility module
```

原因：

```text
旧版与新版语义根本不同
兼容层会使代码规模重新增长
旧名称会继续造成 Session 所有权误解
```

## 3.4 旧日志迁移

Runtime v0.4 不读取 v0.3 SessionLog。

旧 JSONL 或 v0.3 log 的迁移属于上层 Agent。

Runtime 只接收：

```rust
Arc<[HistoryItem]>
```

---

# 4. 最终公开对象

v0.4 对上层公开的核心对象：

```text
AgentLoop
LoopHandle
LoopRequest
LoopOptions
LoopState
LoopStatus
LoopEventStream
LoopEventEnvelope
LoopEvent
LoopReport
LoopOutcome
LoopFailure

ExecutionConfig
ConfigRevision

HistoryItem
HistoryView
UserHistory
UserMessageKind
AssistantHistory
ToolResultHistory
SummaryHistory

Model
ModelDescriptor
ModelRequest
ModelEvent
ModelError

Tool
ToolSet
ToolPolicy
ToolContext
ToolInvocation
ToolOutput

PromptProvider
PromptRequest
PreparedPrompt
DefaultPromptProvider

Interaction
InteractionAnswer
```

不再公开：

```text
SessionRuntime
SessionHandle
TurnHandle
SessionSpec
SessionManifest
SessionBindings
SessionLog
AppendReceipt
ConversationLog
ConversationState
ConversationView
TranscriptPage
SessionHealth
Degraded
Recovery
```

---

# 5. 总体调用模型

```rust
let config = ExecutionConfig::new(
    model,
    ReasoningPreference::High,
    tools,
    policy,
    prompt,
)?;

let request = LoopRequest::new(
    history,
    UserInput::text("Fix the parser")?,
    config,
);

let mut agent_loop = AgentLoop::start(
    request,
    LoopOptions::default_checked()?,
)?;

let handle = agent_loop.handle();
let mut events = agent_loop.take_events()?;

let event_task = tokio::spawn(async move {
    while let Some(event) = events.recv().await {
        // TUI / Agent observation
    }
});

// Optional live control.
handle.steer(UserInput::text("Do not change config files")?)?;
handle.update(next_config)?;
handle.cancel();

let report = handle.wait().await?;
let joined = agent_loop.join().await?;

assert!(Arc::ptr_eq(&report, &joined));
event_task.await?;
```

上层 Agent：

```text
读取自己的 Session History
→ AgentLoop::start
→ 运行期间处理 steer/update/cancel
→ LoopReport
→ Agent 自己写 JSONL
→ Agent 更新自己的 History
```

---

# 6. Public API

## 6.1 `AgentLoop`

```rust
#[must_use = "dropping AgentLoop cancels the running loop"]
pub struct AgentLoop {
    // private
}
```

```rust
impl AgentLoop {
    pub fn start(
        request: LoopRequest,
        options: LoopOptions,
    ) -> Result<Self, LoopStartError>;

    pub fn id(&self) -> LoopId;

    pub fn handle(&self) -> LoopHandle;

    pub fn take_events(
        &mut self,
    ) -> Result<LoopEventStream, TakeEventsError>;

    pub async fn join(
        self,
    ) -> Result<Arc<LoopReport>, LoopJoinError>;

    pub async fn shutdown(
        self,
    ) -> Result<Arc<LoopReport>, LoopJoinError>;
}
```

语义：

### `start`

- 必须在已运行的 Tokio Runtime 内调用；
- 使用 `tokio::runtime::Handle::try_current()`；
- 不接受外部 runtime handle；
- 验证 `LoopRequest`、`ExecutionConfig` 和 `LoopOptions`；
- 创建一个 runner task；
- 立即返回 owner。

### `take_events`

- 只能成功调用一次；
- 未调用也不影响 Loop；
- Event consumer drop 不影响 Loop。

### `join`

- 等待自然结束；
- 不主动取消；
- 返回与 `LoopHandle::wait()` 相同的 `Arc<LoopReport>`；
- 完成后确认 runner task 已退出。

### `shutdown`

- 调用 cancel；
- 等待 runner task；
- 返回最终 report；
- 不等待任何 Store 或 Session cleanup。

### `Drop`

- best-effort cancel；
- 不阻塞；
- 不 spawn cleanup task；
- 不保证异步 join。

## 6.2 `LoopHandle`

```rust
#[derive(Clone)]
pub struct LoopHandle {
    // private
}
```

```rust
impl LoopHandle {
    pub fn id(&self) -> LoopId;

    pub fn state(&self) -> LoopState;

    pub fn watch_state(
        &self,
    ) -> tokio::sync::watch::Receiver<LoopState>;

    pub fn steer(
        &self,
        input: UserInput,
    ) -> Result<(), SteerError>;

    pub fn update(
        &self,
        config: ExecutionConfig,
    ) -> Result<ConfigRevision, UpdateError>;

    pub fn answer(
        &self,
        interaction_id: InteractionId,
        answer: InteractionAnswer,
    ) -> Result<(), AnswerError>;

    pub fn cancel(&self) -> bool;

    pub fn is_finished(&self) -> bool;

    pub async fn wait(
        &self,
    ) -> Result<Arc<LoopReport>, LoopWaitError>;
}
```

`LoopHandle` 不持有：

```text
History owner
Store
Session metadata
Agent registry
```

## 6.3 `LoopRequest`

```rust
pub struct LoopRequest {
    pub history: Arc<[HistoryItem]>,
    pub input: UserInput,
    pub config: ExecutionConfig,
}
```

提供：

```rust
impl LoopRequest {
    pub fn new(
        history: Arc<[HistoryItem]>,
        input: UserInput,
        config: ExecutionConfig,
    ) -> Self;
}
```

`LoopId` 由 `AgentLoop::start` 生成。

不允许上层把本次新 UserInput预先放入 history。

## 6.4 `LoopOptions`

```rust
#[derive(Clone, Debug)]
pub struct LoopOptions {
    pub deadline: Option<tokio::time::Instant>,

    pub max_tool_rounds: u16,
    pub max_pending_steers: usize,

    pub event_capacity: usize,

    pub prompt_timeout: Duration,
    pub model_timeout: Duration,
    pub policy_timeout: Duration,
    pub tool_timeout: Duration,

    pub model_retry_attempts: u8,
    pub model_retry_base_delay: Duration,

    pub limits: LoopLimits,
}
```

提供：

```rust
impl LoopOptions {
    pub fn default_checked() -> Result<Self, LoopStartError>;

    pub fn validate(&self) -> Result<(), LoopStartError>;
}
```

约束：

```text
max_tool_rounds > 0
1 <= max_pending_steers <= 64
event_capacity > 0
所有 timeout > 0
retry attempts 有安全上限
deadline 在未来
LoopLimits 合法
```

不要实现 Builder。

Public 字段 + `validate()` 已足够。

---

# 7. Tokio 与任务模型

## 7.1 一个 Loop 一个 runner task

最终 Runtime 内每个 Loop 只创建：

```text
1 个 runner task
```

可由 Model / Tool adapter内部创建自己的任务，但 Core 不创建：

```text
SessionActor task
TurnRunner task
OpenGuard task
owner watcher
cleanup watcher
settlement task
```

## 7.2 不再使用 Actor + Runner 双层

当前 v0.3：

```text
SessionActor
↔ Runner protocol
↔ TurnRunner
```

v0.4：

```text
AgentLoop runner task
+
Arc<LoopControl>
```

控制操作通过共享的短临界区 state完成：

```text
steer queue
pending config
interaction answer slot
final seal
```

Cancel 使用 `CancellationToken`。

## 7.3 不使用 async Mutex

推荐：

```rust
std::sync::Mutex<ControlState>
```

锁内只允许：

```text
push/pop VecDeque
replace Arc<ExecutionConfig>
take oneshot sender
修改 bool/u64
```

锁内禁止：

```text
await
Model 调用
Tool 调用
Prompt 调用
Event send
JSON serialize
大 Vec clone
```

使用一个私有 helper处理 poisoned mutex：

```rust
fn lock_control(
    mutex: &Mutex<ControlState>,
) -> MutexGuard<'_, ControlState> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
```

不引入 `parking_lot`。

---

# 8. LoopControl

## 8.1 结构

```rust
struct LoopControl {
    id: LoopId,
    cancel: CancellationToken,

    options: Arc<LoopOptions>,

    state_tx: watch::Sender<LoopState>,
    completion_tx: watch::Sender<Option<Arc<LoopReport>>>,

    finished: AtomicBool,
    inner: Mutex<ControlState>,
}
```

```rust
struct ControlState {
    accepting_updates: bool,

    current_revision: ConfigRevision,
    next_revision: u64,

    pending_config: Option<PendingConfig>,
    pending_steers: VecDeque<UserInput>,

    interaction: Option<InteractionSlot>,
}
```

```rust
struct PendingConfig {
    revision: ConfigRevision,
    config: Arc<ExecutionConfig>,
}
```

```rust
struct InteractionSlot {
    id: InteractionId,
    expected: InteractionKind,
    reply: oneshot::Sender<InteractionAnswer>,
}
```

## 8.2 Config update coalescing

只保存最新：

```rust
pending_config: Option<PendingConfig>
```

多个 update 在下一个 request boundary 前到达：

```text
revision 1
revision 2
revision 3
```

只有 revision 3 被下一次 request应用。

这是明确的：

```text
latest update wins
```

不需要 config queue。

## 8.3 Steer 顺序

Steer 使用：

```rust
VecDeque<UserInput>
```

全部按接受顺序应用。

不合并，不重排，不去重。

## 8.4 Final seal

`accepting_updates` 同时控制：

```text
steer
update
answer 新 interaction之外的控制
```

最终完成前，runner在同一个 mutex临界区执行：

```text
如果 pending_steers 非空：
    取出 steer
    保持 accepting_updates=true
    继续下一 request

如果 pending_steers 为空：
    accepting_updates=false
    Loop 可以结束
```

这样解决：

```text
Runner检查无steer
→ steer被接受
→ Runner结束
```

的竞态。

---

# 9. History 模型

## 9.1 定位

`HistoryItem` 是：

```text
模型上下文的 typed history
+
LoopReport 的增量结果
```

它不是：

```text
严格 ledger
SessionLog entry
ACID transaction record
可信 replay proof
```

## 9.2 类型

```rust
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum HistoryItem {
    User(UserHistory),
    Assistant(AssistantHistory),
    ToolResult(ToolResultHistory),
    Summary(SummaryHistory),
}
```

## 9.3 User

```rust
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UserHistory {
    pub loop_id: LoopId,
    pub kind: UserMessageKind,
    pub input: UserInput,
}
```

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserMessageKind {
    Prompt,
    Steering,
}
```

初始 input：

```text
Prompt
```

运行中 steer：

```text
Steering
```

## 9.4 Assistant

```rust
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssistantHistory {
    pub loop_id: LoopId,
    pub request_index: u32,

    pub model: ModelRef,
    pub reasoning: ReasoningPreference,

    pub content: AssistantContent,
    pub finish_reason: ModelFinishReason,
    pub usage: Usage,
}
```

`AssistantContent` 尽量复用当前 Model Response中已经存在的 typed内容：

```text
Text
Reasoning
ToolCall
```

不得再引入第二套 Assistant parts。

## 9.5 ToolResult

```rust
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResultHistory {
    pub loop_id: LoopId,
    pub request_index: u32,

    pub call_id: ToolCallId,
    pub tool_name: ToolName,

    pub outcome: ToolResultOutcome,
    pub output: ToolOutput,
}
```

## 9.6 Summary

```rust
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SummaryHistory {
    pub content: BoundedText,
}
```

Core 不定义：

```text
through_seq
terminal boundary
latest summary
replaces before
```

这些属于 Agent 的 History管理。

PromptProvider决定如何理解 Summary。

## 9.7 不再存在的字段

删除：

```text
ConversationSeq
expected_head
created_at
TurnExecutionRecord
TurnTerminalEntry
SessionId
SessionInstanceId
summary through boundary
```

Agent 的 JSONL可以在外层添加：

```text
timestamp
sequence
session_id
config change
record version
```

## 9.8 `HistoryView`

```rust
#[derive(Clone, Copy)]
pub struct HistoryView<'a> {
    base: &'a [HistoryItem],
    appended: &'a [HistoryItem],
}
```

接口：

```rust
impl<'a> HistoryView<'a> {
    pub fn base(&self) -> &'a [HistoryItem];

    pub fn appended(&self) -> &'a [HistoryItem];

    pub fn len(&self) -> usize;

    pub fn is_empty(&self) -> bool;

    pub fn iter(
        &self,
    ) -> impl DoubleEndedIterator<Item = &'a HistoryItem>;
}
```

不在每个 request合并为新 `Vec`。

---

# 10. History 的信任边界

## 10.1 Base history

上层传入的 base history被视为：

```text
typed but host-trusted
```

Core 不验证：

```text
旧 ToolCall是否都有ToolResult
旧 Loop是否完成
旧 Model变化是否合法
旧 Summary是否覆盖正确范围
历史顺序是否来自可信日志
```

## 10.2 Core 只做资源上限检查

在 start 时扫描一次：

```text
item count
估算文本总 bytes
单个 item自身checked invariant
```

超限：

```text
LoopStartError::HistoryTooLarge
```

不构造复杂历史 validator。

## 10.3 Current loop delta

Core严格验证：

```text
本次 Model Response
本次 ToolCallId
本次 ToolResult
本次 Interaction
本次 final
```

Core返回的 `LoopReport.appended` 必须局部自洽。

---

# 11. Turn-local working state

避免使用“transaction”一词。

私有类型：

```rust
struct WorkingHistory {
    base: Arc<[HistoryItem]>,
    appended: Vec<HistoryItem>,
}
```

接口：

```rust
impl WorkingHistory {
    fn view(&self) -> HistoryView<'_>;

    fn append_user(
        &mut self,
        item: UserHistory,
    );

    fn append_assistant(
        &mut self,
        item: AssistantHistory,
    );

    fn append_tool_result(
        &mut self,
        item: ToolResultHistory,
    );

    fn into_appended(self) -> Arc<[HistoryItem]>;
}
```

含义：

```text
base
    由 Agent拥有并传入

appended
    当前 Loop在内存中产生

Loop结束
    appended进入LoopReport

Agent
    决定是否以及何时写JSONL
```

不保证：

```text
Tool副作用与Agent日志原子一致
进程崩溃后恢复appended
失败时回滚Tool副作用
```

---

# 12. ExecutionConfig

## 12.1 定位

`ExecutionConfig` 是：

> 一个可以在 request boundary 原子替换的 immutable 能力快照。

它不是：

```text
SessionEnvironment
SessionSpec
长期 Session 配置
```

## 12.2 类型

```rust
#[derive(Clone)]
pub struct ExecutionConfig {
    model: Arc<dyn Model>,
    descriptor: ModelDescriptor,

    reasoning: ReasoningPreference,

    tools: ToolSet,
    policy: Option<Arc<dyn ToolPolicy>>,

    prompt: Arc<dyn PromptProvider>,
}
```

构造：

```rust
impl ExecutionConfig {
    pub fn new(
        model: Arc<dyn Model>,
        reasoning: ReasoningPreference,
        tools: ToolSet,
        policy: Option<Arc<dyn ToolPolicy>>,
        prompt: Arc<dyn PromptProvider>,
    ) -> Result<Self, ExecutionConfigError>;

    pub fn model(&self) -> &Arc<dyn Model>;

    pub fn descriptor(&self) -> &ModelDescriptor;

    pub fn reasoning(&self) -> ReasoningPreference;

    pub fn tools(&self) -> &ToolSet;

    pub fn policy(&self) -> Option<&Arc<dyn ToolPolicy>>;

    pub fn prompt(&self) -> &Arc<dyn PromptProvider>;
}
```

字段保持 private。

不要使用 Builder。

## 12.3 构造时校验

只做当前快照需要的校验：

```text
Model::descriptor不panic
descriptor自身合法
reasoning被支持
tools非空时Model支持tools
ToolSet name/spec一致
Tool name不重复
policy可选
```

与 `LoopLimits` 有关的数量和byte上限在：

```text
AgentLoop::start
LoopHandle::update
```

检查。

## 12.4 Descriptor cache

`ExecutionConfig` 缓存 descriptor是合理的：

```text
它是该 immutable config对象的一部分
更新模型时创建新的ExecutionConfig
```

这不等于 SessionEnvironment。

不要在每个 request重新调用 `model.descriptor()`。

## 12.5 Config revision

```rust
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    Deserialize,
)]
pub struct ConfigRevision(u64);
```

初始 config：

```text
revision 0
```

每次 update：

```text
revision递增
```

RequestStarted Event记录实际使用的 revision。

---

# 13. Config 更新语义

## 13.1 接口

```rust
LoopHandle::update(
    &self,
    config: ExecutionConfig,
) -> Result<ConfigRevision, UpdateError>;
```

## 13.2 生效边界

更新：

```text
不影响当前in-flight Model Request
不影响当前Model Response产生的Tool batch
在下一次Model Request前生效
```

## 13.3 Latest wins

同一边界前多次 update：

```text
只使用最新config
```

## 13.4 Update 不保持 Loop 存活

如果当前 Model已生成 Final，且没有 pending steer：

```text
即使之前有一个accepted config update
Loop仍然可以结束
```

Config update本身不会强制再发模型请求。

长期配置由 Agent自己保存。

## 13.5 Update 与 steer

若同一边界同时有：

```text
pending config
pending steer
```

行为：

```text
应用最新config
追加全部steer
使用新config构建下一 request
```

## 13.6 更新范围

因为替换完整 `ExecutionConfig`，天然支持：

```text
模型
reasoning
ToolSet
ToolPolicy
PromptProvider
```

不要增加：

```text
set_model
set_reasoning
set_tools
set_policy
```

等零散 setter。

---

# 14. PromptProvider

## 14.1 替代的旧概念

最终删除 Runtime 内的：

```text
Session级 PromptBuilder
ContextProvider
durable CompactionStrategy
CompactionConfig
```

替换为一个明确接口：

```text
PromptProvider
```

## 14.2 理由

Session由 Agent持有后：

```text
AGENTS.md
Memory
RAG
Skills
Git Context
上下文裁剪
请求级压缩
压缩后提示注入
```

都属于“如何为当前 request生成 Model messages”。

Core无需理解这些产品概念。

## 14.3 Trait

```rust
pub trait PromptProvider: Send + Sync + 'static {
    fn prepare<'a>(
        &'a self,
        request: PromptRequest<'a>,
    ) -> PromptFuture<'a>;
}
```

```rust
pub type PromptFuture<'a> =
    Pin<Box<
        dyn Future<
                Output = Result<PreparedPrompt, PromptError>
            >
            + Send
            + 'a,
    >>;
```

## 14.4 PromptRequest

```rust
pub struct PromptRequest<'a> {
    pub loop_id: LoopId,
    pub request_index: u32,

    pub history: HistoryView<'a>,

    pub model: &'a ModelDescriptor,
    pub reasoning: ReasoningPreference,
    pub tools: &'a [ToolSpec],

    pub cancellation: CancellationToken,
    pub deadline: Instant,
}
```

## 14.5 PreparedPrompt

```rust
pub struct PreparedPrompt {
    pub messages: Vec<ModelMessage>,
}
```

不允许 PromptProvider返回：

```text
model
reasoning
tools
Tool实现
Session状态
```

这些由 Core从 ExecutionConfig构造 ModelRequest。

## 14.6 DefaultPromptProvider

Core提供一个简单实现：

```rust
pub struct DefaultPromptProvider {
    system_prompt: Option<BoundedText>,
}
```

```rust
impl DefaultPromptProvider {
    pub fn new(
        system_prompt: Option<BoundedText>,
    ) -> Self;
}
```

行为：

```text
可选System
HistoryItem按顺序投影
User Prompt/Steering → User message
Assistant → Assistant message
ToolResult → Tool message
Summary → System或User summary message（在文档固定一种）
```

推荐 Summary映射为：

```text
System message:
"Conversation summary:\n..."
```

Default实现：

```text
不读文件
不做RAG
不调用模型压缩
不缓存Session
```

## 14.7 Compaction 如何扩展

Runtime v0.4不保留独立 `CompactionStrategy`。

上层 Agent可以提供：

```text
CompactPromptProvider
```

其内部：

```text
读取HistoryView
估算目标模型窗口
必要时调用Summary Model
返回压缩后的messages
```

这属于 request-time prompt compaction。

长期 Summary 是否写入JSONL由 Agent决定。

## 14.8 不增加 Hook

不要再增加：

```text
before_prompt
after_prompt
before_compaction
after_compaction
transform_context
```

PromptProvider已经是稳定边界。

---

# 15. Request preparation 与 stale protection

Steer或config update可能在异步 PromptProvider执行期间到达。

Runner必须避免发送旧 Prompt。

## 15.1 算法

```text
loop:
    A. 从Control取出pending config和steers
    B. 更新current config
    C. 将steers写入WorkingHistory
    D. 使用当前config调用PromptProvider
    E. PromptProvider返回
    F. 再检查Control是否出现新的config/steer
       - 有：丢弃PreparedPrompt，回到A
       - 无：该request边界线性化
    G. 发起Model Request
```

## 15.2 边界定义

当 F 返回“无新的变化”时：

```text
本次request的config/history snapshot确定
```

F之后到达的 update/steer：

```text
进入下一request
```

不需要使网络调用与mutex原子化。

## 15.3 不立即取消PromptProvider

Steer/update到达时不取消正在执行的 PromptProvider。

它完成后被丢弃并重建。

原因：

```text
实现简单
无额外Notify/channel
Prompt通常比Model请求便宜
```

Cancel仍必须立即取消 PromptProvider。

---

# 16. Model Port

## 16.1 保留

继续保留当前 typed Model能力：

```text
Model
ModelDescriptor
ModelRequest
ModelEvent stream
ModelError
DeliveryState
RetryHint
Usage
ModelFinishReason
```

## 16.2 Model trait

整体形态保持：

```rust
pub trait Model: Send + Sync + 'static {
    fn descriptor(&self) -> ModelDescriptor;

    fn start<'a>(
        &'a self,
        request: ModelRequest,
        context: ModelCallContext,
    ) -> ModelStartFuture<'a>;
}
```

如果当前 trait返回引用，可保持现有形式；不要只为风格修改。

## 16.3 ModelCallContext

删除：

```text
SessionId
SessionInstanceId
TurnId
model round tied to Session
```

改为：

```rust
pub struct ModelCallContext {
    pub loop_id: LoopId,
    pub request_index: u32,

    pub cancellation: CancellationToken,
    pub deadline: Instant,
}
```

## 16.4 Provider continuation

Provider可以用：

```text
LoopId + request_index
```

管理当前 Loop内的 reasoning continuation。

当 Loop结束时，Core取消 root `CancellationToken`。

所有 request child token都会被取消，Provider可据此清理 continuation。

## 16.5 ModelDriver

保留 delivery-aware retry，但改为 request级函数或轻量对象：

```rust
pub(crate) async fn run_model(
    config: &ExecutionConfig,
    request: ModelRequest,
    context: ModelCallContext,
    options: &LoopOptions,
    events: &LoopEventSink,
) -> Result<CompletedModelResponse, ModelRunError>;
```

不再长期保存在 SessionEnvironment。

## 16.6 RequestStarted

发起请求前发布：

```rust
LoopEvent::RequestStarted {
    request_index,
    config_revision,
    model: ModelRef,
    reasoning: ReasoningPreference,
}
```

TUI/Agent可以确认热更新实际生效。

---

# 17. Model Response 局部验证

Core必须保留当前 response assembly和局部验证。

## 17.1 保证

```text
Stream必须有terminal
Text/Reasoning总大小有界
ToolCall数量有界
ToolCallId在当前response内唯一
Tool name合法
Tool arguments合法
ToolCall delta顺序正确
Usage合法
```

## 17.2 History写入

只有完整、合法的 Model Response才加入：

```text
HistoryItem::Assistant
```

如果 Stream中途失败：

```text
已发送的OutputDelta仍只是Event
不把partial Assistant加入LoopReport.appended
```

## 17.3 模型身份

AssistantHistory必须记录：

```text
当前request snapshot的ModelRef
当前reasoning
request_index
```

因此一个 Loop内可以可信描述多模型执行。

---

# 18. Tool 接口

## 18.1 保留

保留：

```text
Tool
ToolSpec
ToolSet
ToolContext
ToolInvocation
ToolOutput
ToolError
ToolPolicy
ToolDecision
ToolProgress
```

## 18.2 Tool snapshot

Model Request N使用 config snapshot N中的：

```text
ToolSpec列表
```

该 Model Response产生的整个 Tool batch必须使用同一 snapshot中的：

```text
Tool实现
ToolPolicy
Tool限制
```

即使运行期间收到 config update也不改变当前 batch。

## 18.3 执行顺序

继续使用：

```text
顺序执行
```

不在本次重构中增加并行 Tool。

## 18.4 ToolContext

只保留执行需要：

```rust
pub struct ToolContext {
    pub cancellation: CancellationToken,
    pub deadline: Instant,
    pub progress: ToolProgressSink,
}
```

不得加入：

```text
SessionRuntime
History mutator
Agent registry
Workspace service locator
Any / TypeMap
```

具体 Workspace或AgentHandle由 Tool构造时捕获。

## 18.5 Tool 结果

每个当前 response中的 ToolCall必须在 LoopReport中有一个 ToolResult。

正常：

```text
Tool执行结果
```

Policy deny：

```text
Denied ToolResult
```

取消：

```text
当前和未执行ToolCall生成Cancelled ToolResult
```

不可用 Tool：

```text
Unavailable/Failed ToolResult
```

不要留下当前 Loop内悬空 ToolCall。

## 18.6 Tool side effect

Core不保证：

```text
Tool副作用与Agent JSONL一致
```

README必须明确。

---

# 19. ToolPolicy 与 Interaction

## 19.1 保留原因

Interaction属于当前 live execution状态，不是持久化Session状态。

因此保留：

```text
Allow
Deny
RequireInteraction
```

## 19.2 WaitingForInput

Policy或Tool请求输入时：

```text
LoopState.status = WaitingForInput
LoopEvent::InteractionRequested
Runner等待LoopHandle::answer或cancel
```

## 19.3 Answer

```rust
LoopHandle::answer(
    interaction_id,
    answer,
)
```

仅接受当前 pending interaction。

错误：

```text
InteractionNotFound
WrongInteraction
LoopFinished
```

## 19.4 Steer 与 Waiting

第一版：

```text
WaitingForInput时steer返回SteerError::WaitingForInput
```

理由：

```text
避免把普通用户指导和结构化Interaction答案混合
```

Config update可以在 Waiting时接受，并在 Interaction完成后的下一request生效。

## 19.5 不恢复

Loop取消、结束或进程退出：

```text
pending Interaction丢失
```

---

# 20. Steer

## 20.1 语义

```text
当前 Model Request继续
当前 Tool batch继续
下一 Model Request前应用
属于当前Loop
```

Steer不创建新 Loop。

## 20.2 接受语义

```rust
LoopHandle::steer(...) -> Ok(())
```

只表示：

```text
当前Loop已在内存中接收
```

不表示：

```text
已持久化
已被模型消费
```

## 20.3 应用

在 request boundary：

```text
pending steers按顺序
→ HistoryItem::User(kind=Steering)
→ WorkingHistory.appended
→ PromptProvider看到
```

## 20.4 Multiple steer

同一边界全部应用。

不做：

```text
steer priority
steer edit/delete
steer id
steer Event
steer persistence
```

## 20.5 Queue full

返回：

```text
SteerError::QueueFull
```

已接受的 steer不丢失。

## 20.6 Final race

最终 Assistant无 ToolCall时：

```text
进入final seal
```

如果 seal前有 steer：

```text
应用steer
继续下一request
```

如果 seal先发生：

```text
后续steer返回NotActive
```

---

# 21. Loop 主算法

私有：

```rust
async fn run_loop(
    id: LoopId,
    request: LoopRequest,
    options: Arc<LoopOptions>,
    control: Arc<LoopControl>,
    events: LoopEventSink,
) -> Arc<LoopReport>;
```

伪代码：

```text
validate
publish Starting

working.append initial User(Prompt)
current_config = request.config
current_revision = 0
request_index = 0
tool_rounds = 0
usage = 0

loop:
    if cancelled:
        finish Cancelled

    apply pending config and steers

    prepared = prepare_prompt(current_config, working)

    if config/steer arrived during prompt:
        continue loop

    publish RequestStarted
    publish state RunningModel

    response = run_model(...)

    if model error:
        finish Failed

    validate response
    working.append AssistantHistory
    usage += response.usage

    if response has ToolCalls:
        if tool_rounds >= max_tool_rounds:
            create Failed/Skipped ToolResults
            finish Failed(MaxToolRounds)

        tool_rounds += 1
        publish state RunningTools

        for each ToolCall in order:
            if cancelled:
                append Cancelled results for current/remaining
                finish Cancelled

            policy
            optional Interaction
            execute Tool with request config snapshot
            append ToolResult
            publish events

        request_index += 1
        continue loop

    match finish reason:
        Stop:
            final seal:
                if pending steer:
                    request_index += 1
                    continue loop
                else:
                    finish Completed

        Length:
            finish Failed(OutputLimit)

        Refused:
            finish Failed(Refused)

        ContentFiltered:
            finish Failed(ContentFiltered)

        Unknown:
            finish Failed(InvalidModelResponse)
```

## 21.1 Config update without ToolCall

如果 config update在最终 Model Request中到达，但没有 steer：

```text
Loop仍结束
```

Agent保存的新长期 config供下一 Loop使用。

## 21.2 Request index

初始 Model Request：

```text
0
```

每次真实发起新 Model Request递增。

因 Prompt stale而丢弃的准备过程：

```text
不递增
```

---

# 22. LoopState

## 22.1 类型

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoopState {
    pub loop_id: LoopId,
    pub status: LoopStatus,

    pub request_index: u32,
    pub config_revision: ConfigRevision,

    pub model: Option<ModelRef>,
    pub pending_interaction: Option<PendingInteraction>,
}
```

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoopStatus {
    Starting,
    RunningModel,
    RunningTools,
    WaitingForInput,
    Finishing,
    Finished,
}
```

## 22.2 不包含

```text
SessionId
Session health
Conversation sequence
last terminal
durability
loaded/unloaded
```

## 22.3 唯一事实源

Runner直接更新一个 watch sender。

不再维护：

```text
Actor private state
+
public SessionState projection
```

两份状态。

---

# 23. LoopEvent

## 23.1 Envelope

```rust
pub struct LoopEventEnvelope {
    pub dropped_before: u64,
    pub event: LoopEvent,
}
```

保留当前“下一条成功 Event携带丢失数”的简单模式。

## 23.2 Event

```rust
#[non_exhaustive]
pub enum LoopEvent {
    Started {
        loop_id: LoopId,
    },

    StateChanged {
        state: LoopState,
    },

    RequestStarted {
        loop_id: LoopId,
        request_index: u32,
        config_revision: ConfigRevision,
        model: ModelRef,
        reasoning: ReasoningPreference,
    },

    OutputDelta {
        loop_id: LoopId,
        request_index: u32,
        channel: OutputChannel,
        delta: BoundedText,
    },

    ToolStarted {
        loop_id: LoopId,
        request_index: u32,
        call_id: ToolCallId,
        tool_name: ToolName,
    },

    ToolProgress {
        loop_id: LoopId,
        request_index: u32,
        call_id: ToolCallId,
        progress: ToolProgress,
    },

    ToolFinished {
        loop_id: LoopId,
        request_index: u32,
        call_id: ToolCallId,
        outcome: ToolResultOutcome,
        output_bytes: usize,
    },

    InteractionRequested {
        loop_id: LoopId,
        interaction: PendingInteraction,
    },

    InteractionResolved {
        loop_id: LoopId,
        interaction_id: InteractionId,
    },

    Finished {
        loop_id: LoopId,
        outcome: LoopOutcomeSummary,
    },
}
```

## 23.3 不增加

```text
SteerQueued
SteerApplied
ConfigQueued
ConfigApplied
HistoryCommitted
DurabilityChanged
```

`RequestStarted` 已能说明 config实际生效。

## 23.4 Event backpressure

所有 Event发送：

```text
try_send
```

Event channel full：

```text
记录 dropped count
继续执行
```

Event receiver关闭：

```text
停止后续Event尝试
继续执行
```

---

# 24. LoopReport

## 24.1 类型

```rust
#[derive(Clone, Debug)]
pub struct LoopReport {
    pub loop_id: LoopId,
    pub outcome: LoopOutcome,

    pub appended: Arc<[HistoryItem]>,
    pub usage: Usage,

    pub requests: u32,
    pub tool_rounds: u16,
    pub final_config_revision: ConfigRevision,
}
```

## 24.2 Outcome

```rust
#[derive(Clone, Debug, PartialEq)]
pub enum LoopOutcome {
    Completed,
    Cancelled(CancelReason),
    Failed(LoopFailure),
}
```

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelReason {
    User,
    OwnerDropped,
    Shutdown,
    Deadline,
}
```

```rust
#[derive(Clone, Debug, PartialEq)]
pub struct LoopFailure {
    pub kind: LoopFailureKind,
    pub diagnostic: DiagnosticSummary,
}
```

```rust
#[non_exhaustive]
pub enum LoopFailureKind {
    Prompt,
    Model,
    InvalidModelResponse,
    OutputLimit,
    Refused,
    ContentFiltered,
    Policy,
    Interaction,
    MaxToolRounds,
    Internal,
}
```

Tool的普通执行失败通常表现为：

```text
ToolResultOutcome::Failed
```

然后模型可以继续。

只有无法保持当前执行闭环时才结束 Loop。

## 24.3 Report 在失败时仍返回

下列情况仍返回 LoopReport：

```text
Model失败
取消
Tool batch中断
Prompt失败
max rounds
refusal
```

`LoopWaitError` 只表示：

```text
runner task无法交付report
内部task被外部abort
completion channel异常
```

---

# 25. Completion

## 25.1 Shared report

`LoopControl` 使用：

```rust
watch::Sender<Option<Arc<LoopReport>>>
```

多个 `LoopHandle::wait()` 可以读取同一 `Arc<LoopReport>`。

不复制大 History。

## 25.2 发布顺序

Runner结束时：

```text
1. final seal
2. LoopState = Finishing
3. 构造LoopReport
4. LoopState = Finished
5. completion_tx.send(Some(report))
6. best-effort Finished Event
7. cancel root token，清理Provider临时状态
8. runner task返回report
```

权威顺序是 completion。

Finished Event可以丢失。

## 25.3 全 task panic

runner最外层使用一次：

```rust
AssertUnwindSafe(run_loop_inner(...))
    .catch_unwind()
```

内部 panic转成：

```text
LoopOutcome::Failed(Internal)
```

并尽量发布 report。

Model/Tool/Prompt/Policy自身仍分别做 panic isolation。

不要建立复杂 panic recovery framework。

---

# 26. Error 模型

## 26.1 `LoopStartError`

```rust
#[non_exhaustive]
pub enum LoopStartError {
    NoTokioRuntime,
    InvalidOptions,
    InvalidConfig,
    InvalidInput,
    HistoryTooLarge,
    IdGeneration,
}
```

## 26.2 `SteerError`

```rust
#[non_exhaustive]
pub enum SteerError {
    InvalidInput,
    QueueFull,
    WaitingForInput,
    NotActive,
}
```

## 26.3 `UpdateError`

```rust
#[non_exhaustive]
pub enum UpdateError {
    InvalidConfig,
    NotActive,
}
```

## 26.4 `AnswerError`

```rust
#[non_exhaustive]
pub enum AnswerError {
    InteractionNotFound,
    WrongInteraction,
    NotActive,
}
```

## 26.5 `LoopWaitError`

```rust
#[non_exhaustive]
pub enum LoopWaitError {
    CompletionClosed,
}
```

## 26.6 `LoopJoinError`

```rust
#[non_exhaustive]
pub enum LoopJoinError {
    TaskCancelled,
}
```

## 26.7 删除的错误

删除所有：

```text
SessionOpenError
SessionCreateError
SessionLoadError
SessionShutdownError
SessionLogError
ConversationCommitError
TranscriptError
RecoveryError
DurabilityUnknown
DurabilityUnavailable
SessionDegraded
AppendReceipt mismatch
Log conflict/corrupt
```

---

# 27. LoopLimits

## 27.1 目标

保留运行时安全上限，删除持久化语义上限。

建议从当前 `SemanticLimits` 中保留：

```rust
pub struct LoopLimits {
    pub max_history_items: usize,
    pub max_history_bytes: usize,

    pub max_user_input_bytes: usize,

    pub max_model_text_bytes: usize,
    pub max_model_reasoning_bytes: usize,
    pub max_tool_calls_per_response: usize,

    pub max_tool_name_bytes: usize,
    pub max_tool_schema_bytes: usize,
    pub max_tool_arguments_bytes: usize,
    pub max_tool_output_bytes: usize,

    pub max_prompt_messages: usize,
}
```

字段以当前代码已有实际需求为准，避免重复限制。

## 27.2 删除

删除与以下有关的 limits：

```text
Session manifest
Conversation page
Transcript cursor
Append batch durability
Summary boundary
Session ID metadata
Store page contract
```

## 27.3 扫描成本

History大小只在 `AgentLoop::start` 扫描一次。

每个 request不重复统计完整 History；PromptProvider自己负责目标模型窗口。

---

# 28. 从当前代码删除的模块

以下模块在最终 v0.4 中应删除。

## 28.1 Storage

删除：

```text
src/storage/
src/conversation/session_log.rs
```

删除所有：

```text
SessionLog trait
AppendReceipt
SessionLogErrorKind
```

## 28.2 Durable Conversation

删除：

```text
src/conversation/log.rs
src/conversation/state.rs
src/conversation/validator.rs
src/conversation/recovery.rs
src/conversation/load.rs
src/conversation/view.rs
src/conversation/projection.rs
```

其中可复用的纯 DTO 移入：

```text
src/history.rs
```

## 28.3 Session config

删除：

```text
src/config/session_spec.rs
SessionManifest
SessionSpec
TurnExecutionRecord
```

## 28.4 Session Runtime

删除：

```text
src/session/runtime.rs
src/session/runtime_open.rs
src/session/runtime_shutdown.rs
src/session/handle.rs
src/session/turn_handle.rs
src/session/actor/*
src/session/bindings.rs
```

由：

```text
src/agent_loop/*
```

替代。

## 28.5 SessionEnvironment

删除：

```text
src/agent/environment.rs
ValidatedSessionBindings
SessionEnvironment
```

由：

```text
ExecutionConfig
```

替代。

## 28.6 Runner commit protocol

删除：

```text
CommitAssistant
CommitToolResult
CommitSummary
CommittedUpdate
RunnerCommitError
settlement
Transcript barrier
Conversation ack
```

Runner直接写入自己的 `WorkingHistory.appended`。

## 28.7 Compaction durable semantics

删除：

```text
CompactionConfig
CompactionCandidate with through boundaries
CompactionProposal through_seq
durable Summary commit
```

请求级压缩通过 `PromptProvider` 实现。

---

# 29. 可复用的当前实现

不要因为 breaking reset 重写所有底层能力。

应优先迁移和复用：

```text
Model request/response DTO
Model event stream assembly
delivery-aware retry
Tool / ToolSet / ToolSpec
Tool driver中的timeout/cancel/panic
ToolPolicy
Interaction DTO
Tool progress
BoundedText / checked strings
ID生成
Event dropped_before机制
Prompt消息DTO
Usage
```

原则：

> 删除 Session 和 durability 耦合，不重写已正确工作的局部执行组件。

---

# 30. 目标目录结构

```text
src/
├── lib.rs
├── ids.rs
├── error.rs
├── execution.rs
├── history.rs
├── interaction.rs
├── limits.rs
├── prompt.rs
│
├── agent_loop/
│   ├── mod.rs
│   ├── handle.rs
│   ├── control.rs
│   ├── runner.rs
│   ├── state.rs
│   └── event.rs
│
├── model/
│   ├── mod.rs
│   ├── model.rs
│   ├── request.rs
│   ├── response.rs
│   └── driver.rs
│
└── tools/
    ├── mod.rs
    ├── tool.rs
    ├── set.rs
    ├── policy.rs
    ├── context.rs
    ├── progress.rs
    └── driver.rs
```

目标是：

```text
少量较完整模块
而不是大量20～50行文件
```

如果 `agent_loop/runner.rs` 超过约 900 行，可以仅按：

```text
model.rs
tools.rs
```

拆分。

不要提前拆：

```text
lifecycle/
orchestration/
pipeline/
services/
ports/
domain/
application/
```

---

# 31. 文件级改造方案

## 31.1 `src/lib.rs`

最终只公开：

```text
AgentLoop / LoopHandle
Loop request/options/state/event/report
ExecutionConfig
History
Model
Tool
Policy
Prompt
Interaction
IDs
Errors
```

删除所有 Session/Storage export。

不提供 deprecated alias。

## 31.2 `src/ids.rs`

删除或停止公开：

```text
SessionId
SessionInstanceId
TurnId
ConversationSeq
```

新增：

```rust
pub struct LoopId(...);
```

保留：

```text
ToolCallId
InteractionId
```

`LoopId::new()` 使用当前安全 ID生成方式。

## 31.3 `src/execution.rs`

新增：

```text
ExecutionConfig
ConfigRevision
ExecutionConfigError
构造与getter
limits validation helper
```

从当前：

```text
bindings.rs
agent/environment.rs
```

迁移可复用逻辑。

## 31.4 `src/history.rs`

新增：

```text
HistoryItem
HistoryView
UserHistory
AssistantHistory
ToolResultHistory
SummaryHistory
WorkingHistory（crate-private）
History size估算
Default prompt projection helper
```

不增加历史 validator。

## 31.5 `src/prompt.rs`

合并或替换当前：

```text
context.rs
compaction.rs
prompt/*
```

包含：

```text
PromptProvider
PromptRequest
PromptFuture
PreparedPrompt
PromptError
DefaultPromptProvider
```

## 31.6 `src/agent_loop/mod.rs`

包含：

```text
AgentLoop owner
LoopRequest
LoopOptions
start
take_events
join
shutdown
Drop
```

## 31.7 `src/agent_loop/control.rs`

包含：

```text
LoopControl
ControlState
PendingConfig
InteractionSlot
BoundaryChanges
take_boundary
final_seal
publish_completion
```

不要公开。

## 31.8 `src/agent_loop/handle.rs`

包含：

```text
LoopHandle
state/watch
steer
update
answer
cancel
wait
```

所有方法只操作：

```text
LoopControl
```

## 31.9 `src/agent_loop/runner.rs`

包含：

```text
run_loop
WorkingHistory
request preparation
request boundary
model/tool loop
final seal
report construction
```

复用 model/tool driver。

## 31.10 `src/agent_loop/state.rs`

包含：

```text
LoopState
LoopStatus
state publish helper
```

## 31.11 `src/agent_loop/event.rs`

包含：

```text
LoopEvent
LoopEventEnvelope
LoopEventStream
LoopEventSink
drop accounting
```

从当前 Session Event实现迁移。

## 31.12 `src/model/model.rs`

保留 Model trait。

调整 `ModelCallContext` IDs。

## 31.13 `src/model/driver.rs`

删除 SessionEnvironment依赖。

改为 request级执行函数。

保留：

```text
panic isolation
timeout
cancellation
delivery-aware retry
stream bounds
terminal validation
```

## 31.14 `src/tools/driver.rs`

从当前 `agent/tool_driver.rs` 迁移。

删除：

```text
SessionSpec
SessionEnvironment
Conversation draft
Actor commit
```

输入：

```text
request config snapshot
ToolCall
Loop options
Loop event sink
```

输出：

```text
ToolResultHistory
```

## 31.15 `src/error.rs`

重写为：

```text
LoopStartError
LoopWaitError
LoopJoinError
SteerError
UpdateError
AnswerError
LoopFailure
```

保留 Model/Tool/Prompt自己的 domain error。

删除 durability映射。

## 31.16 `Cargo.toml`

版本：

```text
0.4.0
```

移除只服务于 Storage/Session的依赖。

保留：

```text
tokio
tokio-util
futures-util
serde
serde_json
thiserror
```

以实际使用为准。

仍：

```text
rust-version = 1.85
unsafe_code = forbid
```

---

# 32. 实施阶段

## Phase 0：基线与分支

提交：

```text
chore: preserve the durable v0.3 runtime baseline
```

工作：

- 记录当前 HEAD；
- 建 v0.3 tag；
- 建 v0.4 branch；
- 运行完整旧测试；
- 记录旧生产代码行数。

## Phase 1：新 DTO 和能力接口

提交：

```text
feat(v0.4): introduce loop history and execution configuration
```

新增：

```text
LoopId
HistoryItem
HistoryView
ExecutionConfig
PromptProvider
LoopOptions
LoopReport
```

此阶段允许旧 API暂时仍存在，但新代码不得依赖旧 Storage。

## Phase 2：实现单任务 AgentLoop

提交：

```text
feat(v0.4): run one agent loop without session ownership
```

实现：

```text
AgentLoop
LoopHandle
LoopControl
LoopState
LoopEvent
run_loop
```

先跑通：

```text
User → Model → Final
User → Model → Tool → Model → Final
cancel
Interaction
```

## Phase 3：动态 config

提交：

```text
feat(v0.4): apply execution updates at request boundaries
```

实现：

```text
LoopHandle::update
ConfigRevision
RequestStarted model/reasoning
same snapshot for Tool batch
latest update wins
```

## Phase 4：Steer

提交：

```text
feat(v0.4): steer active loops at request boundaries
```

实现：

```text
LoopHandle::steer
queue
prompt stale rebuild
final seal
multiple steer
```

## Phase 5：删除 durable Session

提交：

```text
refactor(v0.4): remove session storage and durable conversation ownership
```

删除：

```text
SessionRuntime
SessionLog
Manifest
ConversationLog
Recovery
Degraded
Transcript
Runner commit protocol
SessionEnvironment
```

此提交后旧 API不再编译。

## Phase 6：Prompt收敛

提交：

```text
refactor(v0.4): replace context and durable compaction with prompt providers
```

删除：

```text
ContextProvider
CompactionStrategy
PromptBuilder的Session耦合
```

加入：

```text
DefaultPromptProvider
```

## Phase 7：代码整理

提交：

```text
refactor(v0.4): converge modules around the agent loop boundary
```

完成：

```text
目录移动
错误删除
文档更新
architecture gate更新
dead code删除
依赖清理
```

## Phase 8：发布验收

提交：

```text
test(v0.4): close the flexible agent loop contract
```

完成全部新测试和迁移文档。

---

# 33. 核心测试

## 33.1 基础

### MC4-001 Text final

```text
User
→ Model Text
→ Stop
→ Completed
```

### MC4-002 Tool loop

```text
User
→ Model ToolCall
→ ToolResult
→ Model Final
```

### MC4-003 Sequential tools

一个 response中多个 ToolCall按顺序执行。

### MC4-004 Max rounds

超过上限：

```text
Loop Failed(MaxToolRounds)
当前ToolCall均有对应结果或Assistant不进入history
```

采用一种确定规则并文档化。

## 33.2 流式和 Event

### MC4-005 Text delta

Model结束前收到 OutputDelta。

### MC4-006 Reasoning delta

Reasoning与Text channel分开。

### MC4-007 Event full

Event channel满不阻塞Loop。

### MC4-008 Event receiver drop

不取或中途drop EventStream，Loop正常完成。

### MC4-009 dropped_before

下一条成功Event报告丢失数。

## 33.3 Cancel

### MC4-010 Model cancel

取消in-flight Model。

### MC4-011 Tool cancel

取消in-flight Tool。

### MC4-012 Pending tool results

取消Tool batch后，本次Assistant的所有ToolCall在LoopReport中都有终态ToolResult。

### MC4-013 Owner drop

Drop AgentLoop取消运行。

### MC4-014 shutdown

shutdown取消并join。

## 33.4 Interaction

### MC4-015 Require interaction

进入WaitingForInput。

### MC4-016 Answer

正确ID恢复执行。

### MC4-017 Wrong answer

错误ID/kind被拒绝。

### MC4-018 Cancel waiting

取消后Loop结束。

## 33.5 Steer

### MC4-019 During model

当前request完成后，下一request看到steer。

### MC4-020 During tool

整个当前Tool batch完成后，下一request看到steer。

### MC4-021 During prompt

PromptProvider执行时到达steer，旧PreparedPrompt不发送。

### MC4-022 Multiple steer

按接受顺序出现在History。

### MC4-023 Queue full

明确返回QueueFull。

### MC4-024 Final race steer wins

Steer先于seal，Loop继续。

### MC4-025 Final race seal wins

Seal先于steer，steer返回NotActive。

### MC4-026 Waiting

WaitingForInput时steer被拒绝。

## 33.6 Config update

### MC4-027 During model

Model A当前request完成；下一request使用Model B。

### MC4-028 During tools

Model A产生的Tool batch使用A快照；下一request使用B。

### MC4-029 Multiple updates

A→B→C在边界前更新，只使用C。

### MC4-030 Update with steer

下一request同时使用新config和steer。

### MC4-031 Update after final seal

返回NotActive。

### MC4-032 Update does not extend final

只有config update没有steer，Loop正常结束。

### MC4-033 ToolSet update

旧response只能使用旧ToolSet；下一request显示新Tool specs。

### MC4-034 PromptProvider update

下一request使用新的PromptProvider。

## 33.7 History

### MC4-035 Mixed models

Base history包含多个ModelRef，可以启动。

### MC4-036 Inconsistent old history

旧历史ToolCall缺结果时，Core不做全局拒绝；DefaultPromptProvider按其规则投影或返回PromptError。

### MC4-037 History limits

超限在start失败。

### MC4-038 Appended delta

Report只含当前Loop新增Item，不复制base history。

### MC4-039 Steering history

Applied steer出现在Report。

### MC4-040 Failure history

失败/取消仍返回已完成的当前Loop delta。

## 33.8 Model response

### MC4-041 Missing terminal

Failed InvalidModelResponse。

### MC4-042 Duplicate ToolCall ID

Failed InvalidModelResponse。

### MC4-043 Partial stream failure

Event中可能有delta，Report不含partialAssistant。

### MC4-044 Delivery-aware retry

只有NotStarted+Retryable重试。

### MC4-045 Model switch continuation

切换Model后新Model收到干净request；旧Model临时状态在Loop结束时可由cancel token清理。

## 33.9 Runtime

### MC4-046 No Tokio runtime

start返回NoTokioRuntime。

### MC4-047 Multiple waiters

多个LoopHandle wait得到同一Arc Report。

### MC4-048 Join and wait

AgentLoop::join与LoopHandle::wait结果一致。

### MC4-049 Single event take

第二次take_events失败。

### MC4-050 No orphan task

join/shutdown后没有Core-owned task。

---

# 34. Property 与竞态测试

不需要引入模型检查框架。

使用：

```text
oneshot
Notify
Barrier
Semaphore
deterministic fake Model/Tool/Prompt
```

覆盖以下相对顺序：

```text
steer vs final seal
update vs request boundary
cancel vs interaction answer
cancel vs Tool completion
owner drop vs Model completion
Event receiver close vs finish
```

禁止依赖长 `sleep` 证明顺序。

允许短 timeout防测试永远挂起。

---

# 35. 删除的旧测试

删除只验证以下旧语义的测试：

```text
SessionLog AppendReceipt
expected head conflict
durability unknown
Session degraded
transcript consistency
manifest load
restart repair
open future cancellation cleanup
Store close barrier
Conversation sequence
Summary boundary
durable settlement
```

不要将这些测试机械改名后保留。

它们属于 v0.3产品。

保留和迁移：

```text
Model stream assembly
Tool执行
Tool policy
Interaction
Cancellation
Event dropping
Prompt投影
response bounds
retry
```

---

# 36. 架构门禁

更新当前 architecture gate。

新门禁只检查真正边界：

## 禁止 Runtime 中出现

```text
SessionLog
SessionManifest
JSONL
SQLite
PostgreSQL
Workspace实现
OpenAI / Anthropic
HTTP client
Session repository
Profile
RPC
TUI
```

## 允许

```text
AgentLoop
HistoryItem
Model / Tool / Prompt interfaces
Loop state/event/report
```

不要维护完整文件名 allow-list。

---

# 37. 文档

## 37.1 README 新定位

README第一段：

> MiniCore Runtime is a small Rust execution core for one live agent loop. It runs model/tool iterations, streaming, cancellation, interaction, steering, and request-boundary configuration updates. It does not own sessions, persistence, transcripts, providers, or workspaces.

## 37.2 必须说明

```text
一个AgentLoop只能运行一次
Session由Host拥有
History由Host传入
LoopReport由Host保存
Event不是权威结果
Tool副作用与Host日志不原子
update在下一Model Request生效
当前Tool batch使用产生它的request snapshot
steer在下一Model Request生效
config update不保持Loop存活
Runtime需要Tokio context
```

## 37.3 API示例

至少提供：

```text
simple text loop
tool loop
stream events
steer
model update
cancel
multi-turn MemoryAgent example（放examples，不进入Core API）
```

## 37.4 `MemoryAgent` 示例

可以在：

```text
examples/memory_agent.rs
```

展示：

```rust
struct MemoryAgent {
    history: Vec<HistoryItem>,
    config: ExecutionConfig,
}
```

它反复创建 `AgentLoop`。

该类型不得放入 library production module。

这样证明：

```text
薄AgentLoop可以组成简单多轮Agent
```

而不让Runtime重新拥有Session。

---

# 38. 代码规模要求

目标不是精确LOC，但重构必须产生真实收敛。

最终生产代码应明显删除：

```text
Storage
Recovery
Durability
Conversation ledger
Session open/load
Actor/Runner commit协议
```

粗略目标：

```text
生产代码净减少 25%～40%
测试代码净减少或重写 20%～35%
Public type数量明显下降
```

新增的：

```text
AgentLoop
Control
History
ExecutionConfig
Steer/update
```

不得重新长成同等规模。

## 38.1 明确禁止的过度设计

禁止新增：

```text
LoopManager
LoopRegistry
ExecutionService
ControlPlane
HistoryRepository
ConfigResolver
PortRegistry
PluginManager
HookBus
Middleware stack
Command enum + actor，仅为代替短Mutex
Event ACK
Event replay
Task supervisor framework
```

## 38.2 允许的必要复杂度

允许：

```text
一个runner task
一个Control mutex
一个watch state
一个watch completion
一个bounded Event channel
一个CancellationToken
一个Interaction oneshot
```

---

# 39. 验收矩阵

## 39.1 边界

| ID | 验收 |
|---|---|
| V4-001 | Runtime不拥有Session |
| V4-002 | Runtime不解析JSONL |
| V4-003 | Runtime不公开SessionLog |
| V4-004 | Runtime不公开Manifest |
| V4-005 | Runtime不提供Transcript |
| V4-006 | Runtime没有Degraded/durability状态 |
| V4-007 | AgentLoop单次使用 |
| V4-008 | 多轮由上层多次创建AgentLoop |
| V4-009 | 不提供旧API兼容层 |
| V4-010 | MiniCore crate不依赖具体Provider/Workspace |

## 39.2 接口

| ID | 验收 |
|---|---|
| V4-011 | Model接口保留 |
| V4-012 | Tool/ToolSet接口保留 |
| V4-013 | ToolPolicy/Interaction保留 |
| V4-014 | PromptProvider替代Context/Compaction |
| V4-015 | AgentLoop start/handle/events/join/shutdown可用 |
| V4-016 | LoopHandle steer/update/answer/cancel/wait可用 |
| V4-017 | LoopReport返回当前增量 |
| V4-018 | HistoryItem可序列化但Core不持久化 |

## 39.3 动态配置

| ID | 验收 |
|---|---|
| V4-019 | 初始config revision为0 |
| V4-020 | update只在下一request生效 |
| V4-021 | 当前Tool batch使用旧snapshot |
| V4-022 | latest update wins |
| V4-023 | model/reasoning可热更新 |
| V4-024 | ToolSet/Policy/Prompt可原子热更新 |
| V4-025 | RequestStarted报告实际config |
| V4-026 | update不强制增加request |
| V4-027 | 无静默能力降级 |

## 39.4 Steer

| ID | 验收 |
|---|---|
| V4-028 | model中steer下一request生效 |
| V4-029 | tool中steer在batch后生效 |
| V4-030 | prompt中steer导致rebuild |
| V4-031 | 多steer有序 |
| V4-032 | queue有界 |
| V4-033 | final race线性化 |
| V4-034 | applied steer进入LoopReport |
| V4-035 | 未应用steer不持久化 |
| V4-036 | WaitingForInput拒绝steer |

## 39.5 当前执行正确性

| ID | 验收 |
|---|---|
| V4-037 | 当前Model Response严格验证 |
| V4-038 | 当前ToolCallId唯一 |
| V4-039 | 当前ToolCall都有ToolResult |
| V4-040 | Tool按顺序执行 |
| V4-041 | Cancel传播到Model/Prompt/Tool |
| V4-042 | 一个Loop只完成一次 |
| V4-043 | invalid response不进入History |
| V4-044 | failure/cancel仍返回Report |
| V4-045 | max rounds生效 |

## 39.6 状态与事件

| ID | 验收 |
|---|---|
| V4-046 | Starting/Model/Tools/Waiting/Finishing/Finished正确 |
| V4-047 | Event full不阻塞 |
| V4-048 | Event drop报告dropped_before |
| V4-049 | Event consumer可缺失 |
| V4-050 | Finished Event非权威 |
| V4-051 | wait/join返回同一Report |
| V4-052 | 多waiter安全 |
| V4-053 | shutdown后无Core task |

## 39.7 History

| ID | 验收 |
|---|---|
| V4-054 | Base history由Host拥有 |
| V4-055 | Core不做全局history ledger验证 |
| V4-056 | History超限安全失败 |
| V4-057 | Report不复制base history |
| V4-058 | 同一Loop可记录多个Model |
| V4-059 | Summary没有durable boundary语义 |
| V4-060 | DefaultPrompt可运行简单Agent |

## 39.8 工程

| ID | 验收 |
|---|---|
| V4-061 | Rust 1.85通过 |
| V4-062 | stable通过 |
| V4-063 | Linux/macOS/Windows通过 |
| V4-064 | cargo fmt通过 |
| V4-065 | clippy -D warnings通过 |
| V4-066 | rustdoc -D warnings通过 |
| V4-067 | unsafe_code=forbid |
| V4-068 | architecture gate通过 |
| V4-069 | 旧durable模块不存在 |
| V4-070 | README与实际边界一致 |

---

# 40. 验证命令

每个提交：

```bash
cargo fmt --all -- --check
cargo test --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
```

仓库门禁：

```bash
./scripts/check.sh
./scripts/check-architecture.sh
```

如果脚本名称不同，使用仓库实际命令。

代码规模：

```bash
tokei src
tokei tests
```

或：

```bash
cloc src tests
```

记录：

```text
v0.3基线
v0.4最终
生产/测试行数变化
```

---

# 41. 最终交付报告

开发 Agent 必须报告：

```text
实际起始HEAD
v0.3保存tag
v0.4最终HEAD
提交列表
删除文件
新增文件
Public API变化
删除的Public API
Model/Tool API变化
History格式
update语义
steer语义
Interaction语义
Event语义
代码行数变化
测试结果
CI结果
V4-001～V4-070结果
已知限制
```

必须明确：

```text
旧Session日志迁移未包含
minicore-agent尚未迁移
Tool副作用和Host日志不原子
当前Loop不可恢复
```

---

# 42. 完成定义

只有满足以下全部条件，v0.4 reset才算完成：

```text
Runtime没有Session create/load/close语义
Runtime没有SessionLog/Manifest/Transcript
Runtime没有durability/degraded/recovery
Runtime没有SessionEnvironment
一个AgentLoop对应一次用户任务
AgentLoop内部可完成多个Model/Tool request
Model/Tool/Policy接口保留
PromptProvider成为上下文和请求级压缩边界
LoopHandle支持steer
LoopHandle支持request-boundary config update
一个request和其Tool batch使用相同config snapshot
History由Host输入
LoopReport返回当前Loop增量
Event保持best-effort
Cancel/Interaction/Tool局部语义正确
旧v0.3 API不留兼容层
代码规模明显下降
V4-001～V4-070全部通过
```

最终架构：

```text
Host / minicore-agent
├── Session
├──完整History
├──JSONL
├──当前ExecutionConfig
└──Option<AgentLoop>

minicore-runtime
└──AgentLoop
   ├──当前WorkingHistory delta
   ├──当前Model Request
   ├──当前Tool batch
   ├──pending steer
   ├──pending config
   ├──cancellation
   ├──LoopEvent
   └──LoopReport
```

---

# 43. 给代码 Agent 的最终执行提示

请基于当前 `minicore-runtime` 的 `refactor/v0.3-simplify` 代码实施本规格。

要求：

1. 先保存 v0.3 durable runtime基线；
2. 在独立 v0.4 breaking branch开发；
3. 只修改 minicore-runtime；
4. 不修改 minicore-agent；
5. 不实现JSONL；
6. 不实现Session列表和生命周期；
7. 删除SessionEnvironment，而不是在其上叠加mutable override；
8. 删除SessionLog/Manifest/Conversation durable协议；
9. 复用当前Model/Tool局部执行代码；
10. 采用一个runner task + 一个短临界区Control，不再保留Actor/Runner双层；
11. `ExecutionConfig`完整原子替换，不增加零散setter；
12. update在下一Model Request生效；
13. 当前Tool batch固定使用产生它的request snapshot；
14. steer在下一Model Request生效；
15. Prompt期间出现更新必须丢弃旧Prompt并重建；
16. final与steer通过同一Control锁完成线性化；
17. 当前Loop内ToolCall/ToolResult仍严格自洽；
18. Event不能参与正确性；
19. 不提供旧API兼容层；
20. 不新增插件、Manager、Repository、HookBus或Task Supervisor；
21. 每个Phase独立提交并保持测试通过；
22. 最终提交完整报告和V4验收结果。
