# MiniCore Runtime v0.3：单 Session Kernel 与 SessionRuntime 重构实施规格

> 适用仓库：`zqcli/minicore-runtime`
> 适用基线分支：`refactor/v0.2-core-reset`
> 审查基线提交：`0df39ceddbd9cae3e2dbafed39bac4ef13a61e35`
> 目标版本：`0.3.0`
> 修订日期：`2026-08-22`
> 文档状态：最终实施版；取代此前以多 Session Runtime、重型 Snapshot/Observation 和 Subagent Core 原语为基础的 v0.3 草案
> 变更性质：允许 breaking change，不保留 v0.2 公共 API 兼容层
> 实施范围：只修改 `minicore-runtime`；不实现 Host、LocalWorkspace、具体 Store、具体 Model Provider、具体 Tool、RPC Agent、MCP、Memory、RAG、Skills、TUI、GUI 或 IPC

---

## 导航

- 0～3：实施规则、最终决策、Core 边界与目标目录；
- 4～8：Public API、配置、SessionRuntime 生命周期、状态与事件；
- 9～15：Conversation、SessionLog、重启修复、Model、Tool、Context、Compaction；
- 16～20：SessionActor/Agent Loop、Interaction、多实例并发、错误与扩展政策；
- 21～25：IDs、依赖、Public Surface、逐文件和逐方法修改；
- 26～30：测试、验收、实施阶段、文档、架构门禁与完成定义。

---

## 0. 给执行模型的强制要求

本文件是代码实施规格，不是讨论稿。执行模型必须按以下规则工作：

1. 开工前记录真实基线：

   ```bash
   git rev-parse HEAD
   git status --short
   cargo test --all-targets --locked
   cargo clippy --all-targets --all-features --locked -- -D warnings
   ```

2. 若实际 HEAD 不是上述审查基线，先记录差异并以当前分支真实代码为准；不得假定文件内容与本规格完全一致。
3. 每个实施阶段结束时都必须保持可编译、可测试；禁止先删除全部旧实现，再长期集中修复。
4. 不建立 v0.2 兼容 wrapper、deprecated alias、双 API、feature flag 兼容层或临时 Service Locator。
5. 不把被移出的具体能力重新包装成 Core 的“默认实现”。生产代码中不得保留具体 Workspace、Provider、JSONL、进程或文件工具。
6. 测试 fake/in-memory 实现只能放在 `#[cfg(test)]`、`tests/support` 或 dev-only module。
7. 所有执行型 Port 必须明确 cancellation、deadline、panic、错误、输出边界和并发语义。
8. Core 生产代码不得直接访问网络、文件系统、进程、环境变量、凭据或项目工作目录。
9. 不引入 `Any`、`TypeMap`、Service Locator、动态 Rust dylib、Plugin Manager、万能 Hook 或未约束的扩展上下文。
10. `SessionRuntime` 是单 Session owner；不得在 Core 中重新引入多 Session Registry、Session 列表或 Runtime-level supervisor。
11. 不实现 Subagent、AgentSpawner、Parent/Child Session、Agent graph 或专用 Subagent Event。
12. 最终必须更新 README、架构文档、API 示例、迁移说明、架构检查脚本与验收测试。
13. 若旧代码与本规格冲突，以本规格定义的最终 invariant、边界和 API 为准。
14. 除非本规格明确要求，否则不要增加新的 public trait 或 public enum。

---

## 1. 最终架构决策

### 1.1 MiniCore 的最终定位

MiniCore 是产品名，crate 名保持：

```text
minicore-runtime
```

v0.3 的核心 public owner 类型改为：

```text
SessionRuntime
```

最终定义：

> `minicore-runtime` 是一个单 Session Agent Execution Kernel。每个 `SessionRuntime` 实例只拥有一个已经创建或加载的 Session，维护该 Session 的串行 Agent Loop、Conversation 语义、取消、交互、持久化提交顺序和确定性关闭。

核心关系：

```text
一个 SessionRuntime
=
一个 loaded Session
=
一个 Session actor
=
同时最多一个 active Turn
```

多个 Session 不由 Core 管理，而由外部 Host 管理多个 `SessionRuntime`：

```text
Host process
└── one Tokio runtime
    ├── SessionRuntime(Session A)
    ├── SessionRuntime(Session B)
    └── SessionRuntime(Session C)
```

创建多个 `SessionRuntime` 只创建多个 Rust 对象和异步任务，不自动创建多个操作系统进程。

### 1.2 不得摇摆的设计决策

| 编号 | 最终决策 | 明确禁止 |
|---|---|---|
| D-01 | Core 只运行一个 loaded Session | Core 内多 Session Runtime、Registry、SessionManager |
| D-02 | `SessionRuntime` 是唯一 owner | Cloneable owner、多个对象共同负责 shutdown |
| D-03 | `SessionHandle` 是可 Clone 控制句柄 | Host 直接持有 actor channel、log、task、cancel slot |
| D-04 | `TurnHandle` 表示精确 Turn | Session-wide 模糊 cancel 作为主 API |
| D-05 | Host 管理 Session 集合 | Core 提供 list/create metadata/delete repository API |
| D-06 | Workspace 由外部 Tool/ContextProvider 捕获 | Core Workspace Port、`ToolContext.workspace` |
| D-07 | Model、Tool、Policy、Context、Compaction、SessionLog 通过 typed Port 注入 | Plugin Manager、Service Locator、动态 dylib ABI |
| D-08 | 一个 Session load 生命周期内 Bindings 不可变 | 运行中 hot plug/unplug、替换模型或工具集 |
| D-09 | 保留轻量 `SessionState` | 重型 Snapshot、Observation epoch/cursor/gap 协议 |
| D-10 | EventStream 是单消费者、best-effort 实时流 | 多订阅者 broadcast、Event 回放作为事实来源 |
| D-11 | pending Interaction 只存在于当前进程内存 | 跨重启恢复任意 Tool future 或权限问答 |
| D-12 | 重启只恢复 durable Conversation | continuation、后台 Job 或 Tool 堆栈恢复 |
| D-13 | Remote Agent 是普通 RPC Tool | Core Subagent 类型、父子状态机、Agent graph |
| D-14 | 不提供万能生命周期 Hook | `before_everything`、内部状态可变 Hook |
| D-15 | v0.3 直接 breaking | v0.2 双轨 API、旧 `Runtime` alias |

### 1.3 v0.2 到 v0.3 的总映射

| v0.2 | v0.3 |
|---|---|
| `Runtime` 管理多个 Session | 删除；Host 管理多个 `SessionRuntime` |
| `RuntimeConfig` 持有 data dir、ProviderRegistry、ToolRegistry | `KernelConfig + SessionSpec + SessionBindings + SessionRuntimeOptions` |
| `Runtime::create/load/list/delete_session` | 删除多 Session 路由；Host 打开/租用 `SessionLog`，`SessionRuntime::create/load` 只处理传入的单 Session log，list/delete 属于 Host/Repository |
| `Runtime::submit/answer/cancel` | `SessionHandle::submit/answer` + `TurnHandle::cancel` |
| `Runtime::snapshot/subscribe` | `SessionHandle::state/watch_state` + `SessionRuntime::take_events` |
| `Runtime::transcript(session_id, ...)` | `SessionHandle::transcript(...)` |
| `Runtime::shutdown` | `SessionRuntime::shutdown(self)` |
| `SessionManager`、load reservation、loaded map | 全部删除出 Core |
| `SessionSnapshot` + broadcast + ResyncRequired | `SessionState` + single-consumer `SessionEventStream` |
| `ProviderRegistry` + concrete providers | 一个 Session 直接绑定 `Arc<dyn Model>` |
| Runtime-global `ToolRegistry` | per-session immutable `ToolSet` |
| concrete `SessionStore` | Host 获取并注入独占 `Box<dyn SessionLog>` |
| `SessionConfig.workspace_root` | 删除；Workspace 完全外置 |
| `ToolContext.workspace` | 删除；Tool 构造时捕获 Workspace |
| `ToolContext::ask_user` | 删除；Tool 返回 `RequestInput` |
| string approval，例如 `"yes"` | typed `ApprovalDecision` |
| durable Interaction continuation | 内存 pending Interaction；重启时取消 unfinished Turn |
| Subagent 支撑原语 | 删除；RPC Agent 仅是普通 Tool |

---

## 2. Core 边界

### 2.1 Core 必须负责

`minicore-runtime` 只负责以下无法安全外置的语义：

- 一个 loaded Session 的生命周期所有权；
- Session 四态状态机；
- 一个 Session 内 Turn 严格串行；
- User → Model → Tool → Model → Final 的 Agent Loop；
- Model response 的结构验证；
- ToolCall 与 ToolResult 的匹配；
- ConversationEntry 的合法顺序；
- durable append 与内存状态发布的顺序；
- exact Turn cancellation；
- 当前进程内的 approval 与 Tool input interaction；
- Turn terminal 的唯一性；
- restart 时 unfinished Turn 的 repair；
- lightweight `SessionState`；
- single-consumer best-effort `SessionEventStream`；
- Port timeout、panic 和错误隔离；
- 确定性的 `SessionRuntime::shutdown(self)`。

### 2.2 Core 不负责

以下内容不得进入 production Core：

```text
Host Session list / metadata
Session repository implementation
LocalWorkspace / RemoteWorkspace
Filesystem Tool / Process Tool / Git Tool
OpenAI / Anthropic / other concrete Model Provider
JSONL / SQLite / PostgreSQL / remote Store implementation
Credentials / OAuth / endpoint configuration
MCP / Skills / Memory / RAG
Subagent / AgentSupervisor / parent-child graph
RPC Agent client or server
Plugin discovery / installation / hot reload
CLI / HTTP Server / IPC
TUI / GUI / IDE integration
Authentication / telemetry backend
Global provider rate limiting
Global process concurrency
Workspace write lease / Git worktree allocation
Workflow / DAG / background job system
```

### 2.3 判断功能是否进入 Core 的规则

一个职责只有满足以下至少一项，才考虑进入 Core：

1. 它定义 Session/Turn 的合法状态转移；
2. 它决定 ConversationEntry 的合法顺序；
3. 它必须与 durable commit 建立严格原子先后关系；
4. 它决定 cancellation 或 terminal 的基本语义；
5. 它防止 Model/Tool/Store 的非法结果污染 Conversation。

否则应作为外部 Port、Port decorator、Tool 或 Host 能力实现。

### 2.4 Workspace 的最终边界

Core 不定义 Workspace trait，不创建 Workspace，不知道本地根目录。

外部实现示例仅用于说明组合方式，不属于本仓库实施范围：

```rust
let workspace = Arc::new(LocalWorkspace::open(project_root)?);

let tools = ToolSet::builder()
    .register(ReadFileTool::new(workspace.clone()))?
    .register(WriteFileTool::new(workspace.clone()))?
    .register(ProcessTool::new(workspace.clone(), process_executor))?
    .build();

let context = Arc::new(ProjectContextProvider::new(workspace));
```

读取 `AGENTS.md` 的流程：

```text
SessionRuntime 的 TurnRunner
→ ContextProvider::provide
→ 外部 ProjectContextProvider
→ 外部 Workspace 读取 AGENTS.md
→ 返回 ContextBlock
→ Core PromptBuilder 组装 ModelRequest
```

必须删除：

- `SessionConfig.workspace_root`；
- `ToolContext.workspace`；
- `Runtime::prepare_session` 中的 Workspace 创建；
- 根据工具名推导 `WorkspaceAccess` 的逻辑；
- `src/workspace/**`；
- Cargo 中的 capability filesystem 依赖。

### 2.5 Remote Agent 的最终边界

Core 中不出现 `Subagent`、`AgentSpawner`、`ChildSession` 等 public 或 internal 类型。

外部 Host 可以实现普通 Tool：

```text
Model emits ToolCall(remote_agent)
→ Core calls RemoteAgentTool::execute
→ Tool 通过 RPC 启动或加载另一个完整 MiniCore
→ RPC 返回结果
→ Tool 返回 ToolOutput
→ Core durable append ToolResult
```

Core 只要求该 Tool 遵守普通 Tool 契约：

```text
Cancellation
Deadline
Progress
Bounded output
Typed error
No detached work owned by Core
```

不得为 Remote Agent 增加专用 SessionState、Event、ID、budget、resource link 或 parent-child relation。

---

## 3. 目标模块结构与依赖方向

### 3.1 推荐目录

最终 production source 推荐调整为：

```text
src/
├── lib.rs
├── config.rs
├── error.rs
├── ids.rs
├── time.rs
├── value.rs
│
├── session/
│   ├── mod.rs
│   ├── runtime.rs
│   ├── handle.rs
│   ├── turn_handle.rs
│   ├── actor.rs
│   ├── command.rs
│   ├── state.rs
│   ├── event.rs
│   ├── event_stream.rs
│   ├── interaction.rs
│   ├── bindings.rs
│   └── transcript.rs
│
├── agent/
│   ├── mod.rs
│   ├── runner.rs
│   ├── runner_protocol.rs
│   ├── turn_context.rs
│   └── retry.rs
│
├── conversation/
│   ├── mod.rs
│   ├── entry.rs
│   ├── state.rs
│   ├── validator.rs
│   ├── projection.rs
│   ├── log.rs
│   └── recovery.rs
│
├── model/
│   ├── mod.rs
│   ├── model.rs
│   ├── driver.rs
│   ├── request.rs
│   ├── response.rs
│   └── types.rs
│
├── tools/
│   ├── mod.rs
│   ├── tool.rs
│   ├── set.rs
│   ├── context.rs
│   ├── policy.rs
│   ├── progress.rs
│   └── types.rs
│
├── context/
│   ├── mod.rs
│   └── provider.rs
│
├── compaction/
│   ├── mod.rs
│   └── strategy.rs
│
├── prompt/
│   ├── mod.rs
│   └── builder.rs
│
└── storage/
    ├── mod.rs
    └── session_log.rs
```

允许保留少量当前文件名以降低一次提交的 rename 噪音，但最终职责必须与上述结构一致。

### 3.2 必须删除的 production 路径

```text
src/runtime/**
src/session/snapshot.rs
src/model/providers/**
src/model/transport.rs
src/model/registry.rs
src/tools/builtins/**
src/tools/process.rs
src/workspace/**
具体 JSONL / filesystem store 文件
```

### 3.3 依赖方向

```text
session::runtime
  └── session::actor
        ├── agent::runner_protocol
        ├── conversation::log
        ├── session::state
        └── session::event

agent::runner
  ├── model
  ├── tools
  ├── context
  ├── compaction
  ├── prompt
  └── conversation semantic types

conversation::log
  ├── conversation::validator/state
  └── storage::SessionLog
```

禁止：

- `model`、`tools`、`context`、`storage` 依赖 `session::actor`；
- Tool 获得 `SessionHandle`、`SessionRuntime` 或 actor sender；
- ContextProvider 获得可写 Conversation 句柄；
- `SessionEvent` 反向控制 actor；
- Core domain module 导入具体 provider、filesystem、process、RPC 或 JSONL 类型；
- 循环依赖通过把类型塞进 `lib.rs` 或 `Any` 规避。

---

## 4. Public API 总体设计

### 4.1 Public owner 与 handle

最终公开三个核心类型：

```rust
pub struct SessionRuntime;

#[derive(Clone)]
pub struct SessionHandle;

#[derive(Clone)]
pub struct TurnHandle;
```

语义：

| 类型 | 角色 | Clone | 负责 shutdown |
|---|---|---:|---:|
| `SessionRuntime` | 一个 loaded Session 的唯一 owner | 否 | 是 |
| `SessionHandle` | 控制同一 loaded Session | 是 | 否 |
| `TurnHandle` | 控制并等待一个 exact Turn | 是 | 否 |

Auto-trait 目标：

```text
SessionRuntime: Send，通常不要求 Sync
SessionHandle: Clone + Send + Sync
TurnHandle: Clone + Send + Sync
SessionEventStream: Send，不 Clone，不要求 Sync
```

### 4.2 `SessionRuntimeOptions`

新增 `src/session/runtime.rs` 或 `src/session/bindings.rs`：

```rust
pub struct SessionRuntimeOptions {
    kernel: KernelConfig,
    bindings: SessionBindings,
    task_runtime: tokio::runtime::Handle,
}
```

要求：

- 提供 `new(kernel, bindings, task_runtime)`；
- 提供只读 getter；
- 不实现 `Serialize` / `Deserialize`；
- `Debug` 不打印 Port 内部、凭据或完整 prompt；
- `SessionBindings` 在整个 loaded 生命周期内不可替换。

### 4.3 `SessionRuntime` API

```rust
impl SessionRuntime {
    pub async fn create(
        session_id: SessionId,
        spec: SessionSpec,
        log: Box<dyn SessionLog>,
        options: SessionRuntimeOptions,
    ) -> Result<Self, SessionOpenError>;

    pub async fn load(
        expected_session_id: SessionId,
        log: Box<dyn SessionLog>,
        options: SessionRuntimeOptions,
    ) -> Result<Self, SessionOpenError>;

    pub fn session_id(&self) -> SessionId;

    pub fn instance_id(&self) -> SessionInstanceId;

    pub fn handle(&self) -> SessionHandle;

    pub fn take_events(
        &mut self,
    ) -> Result<SessionEventStream, EventStreamTakenError>;

    pub async fn shutdown(self) -> Result<(), SessionShutdownError>;
}
```

#### `create` 契约

1. 校验 `KernelConfig`；
2. 校验 `SessionSpec`；
3. 校验 `SessionBindings` 与 spec 匹配；
4. 调用 `SessionLog::initialize(SessionManifest)`；
5. 要求 log 初始 head 为零；
6. 创建新的 `SessionInstanceId`；
7. 创建 state/event/command channels；
8. spawn actor supervisor task；
9. 只有 actor 已成功进入 Idle 后才返回；
10. 任一步失败必须关闭 log；关闭失败作为 secondary diagnostic，不覆盖主要错误；
11. spawn 到返回 owner 之间必须使用 internal `OpenGuard`；若 create/load future 被取消或 caller drop，guard 取消已启动 actor，防止 orphan task。

#### `load` 契约

1. 调用 `SessionLog::load_manifest`；
2. manifest 的 SessionId 必须等于 `expected_session_id`；
3. 校验 format version、SessionSpec 和 Bindings；
4. 分页 replay Conversation；
5. 逐条执行 semantic validation；
6. 若存在 unfinished Turn，执行 restart repair；
7. repair 成功并 durable 后才创建 actor；
8. 返回时状态必须是 `Idle + Healthy`；
9. 不恢复 Model stream、Tool future、approval 或 user question；
10. load 失败时不得留下可用 Handle。

#### `take_events` 契约

- EventStream 只能取出一次；
- 第二次调用返回 `EventStreamTakenError::AlreadyTaken`；
- EventStream 不 Clone；
- 未取出 EventStream 不影响 Agent 执行；
- Event queue 满不得阻塞 actor；
- `SessionState` 和 `TurnHandle` 才是控制正确性的依据。

#### `shutdown` 契约

- 消耗 `SessionRuntime` owner；
- 直接触发 owner/root cancellation，不向普通 command mailbox 排队；
- actor 拒绝新 submit，state 更新为 `Closing`；
- 取消 active Turn 和 pending Interaction；
- 等待 runner 收敛；
- 尽可能以一个 settlement batch durable 写入缺失 ToolResult 与 TurnTerminal；
- 调用 `SessionLog::close`；
- 在 `shutdown_timeout` 内等待 actor task 退出；
- 超时时 abort 仍由 Core 持有的 task、await abort completion，并返回 `SessionShutdownError::Timeout`；
- 关闭 EventStream 和 state sender；
- 成功返回是该 Session 所有 Core-owned cleanup 的 completion barrier。

### 4.4 `SessionHandle` API

```rust
impl SessionHandle {
    pub fn session_id(&self) -> SessionId;

    pub fn instance_id(&self) -> SessionInstanceId;

    pub async fn submit(
        &self,
        input: UserInput,
        options: TurnOptions,
    ) -> Result<TurnHandle, SessionError>;

    pub async fn answer(
        &self,
        interaction_id: InteractionId,
        answer: InteractionAnswer,
    ) -> Result<(), SessionError>;

    pub fn state(&self) -> SessionState;

    pub fn watch_state(&self) -> tokio::sync::watch::Receiver<SessionState>;

    pub async fn transcript(
        &self,
        after: Option<ConversationSeq>,
        limit: usize,
    ) -> Result<TranscriptPage, SessionError>;
}
```

要求：

- `SessionHandle` 只持有 command sender、state receiver、instance identity；
- 不持有 SessionLog；
- 不暴露 actor channel 类型；
- 不提供 `close()` 或 `shutdown()`，关闭权只属于 owner；
- actor 关闭后所有命令返回 `SessionError::Closed`；
- command queue 满返回 `SessionError::Backpressure`，不得无限 await；
- `state()` 返回 watch 中最新 clone；
- `watch_state()` 只是轻量 latest-state channel，不提供事件重放。

### 4.5 `TurnHandle` API

```rust
impl TurnHandle {
    pub fn session_id(&self) -> SessionId;

    pub fn instance_id(&self) -> SessionInstanceId;

    pub fn turn_id(&self) -> TurnId;

    pub fn cancel(&self) -> bool;

    pub fn is_finished(&self) -> bool;

    pub async fn wait(&self) -> Result<TurnOutcome, TurnWaitError>;
}
```

要求：

- `cancel()` 只触发该 exact Turn 的 CancellationToken；
- 首次成功请求返回 `true`，重复请求或已完成返回 `false`；
- drop `TurnHandle` 不取消 Turn；
- 多个 clone 可以同时 `wait()`；
- `wait()` 只有在 durable `TurnTerminal` 成功后才返回正常 `TurnOutcome`；
- durable outcome unknown 时返回 `TurnWaitError::DurabilityUnknown`；
- actor 异常退出且无 terminal 时返回 `TurnWaitError::RuntimeTerminated`。

`TurnOutcome` 直接对应已确认 durable 的 terminal，不建立第二套终态语义：

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnOutcome {
    pub turn_id: TurnId,
    pub terminal: TurnTerminal,
    pub usage: Usage,
}
```

### 4.6 删除的 Public API

必须删除并不得提供 alias：

```text
Runtime
RuntimeClient
RuntimeConfig
RuntimeError
SessionManager
SessionSummary.loaded
Runtime::open
Runtime::create_session
Runtime::load_session
Runtime::list_sessions
Runtime::delete_session
Runtime::close_session
Runtime::submit
Runtime::answer
Runtime::cancel
Runtime::snapshot
Runtime::subscribe
Runtime::transcript
Runtime::shutdown
SessionSnapshot
SnapshotHistory
ObservationFrame
ObservationCursor
ResyncRequired
```

---

## 5. 配置、SessionSpec 与 Bindings

### 5.1 `KernelConfig`

重写 `src/config.rs`，删除所有 data dir、registry、Workspace 和 coding instructions 字段。

目标：

```rust
#[derive(Clone, Debug)]
pub struct KernelConfig {
    pub command_capacity: usize,
    pub runner_capacity: usize,
    pub event_capacity: usize,
    pub shutdown_timeout: Duration,
    pub model_call_timeout: Duration,
    pub tool_call_timeout: Duration,
    pub policy_timeout: Duration,
    pub context_timeout: Duration,
    pub log_operation_timeout: Duration,
    pub retry_policy: RetryPolicy,
    pub limits: SemanticLimits,
}
```

建议默认值：

```text
command_capacity       64
runner_capacity        64
event_capacity         256
shutdown_timeout       30s
model_call_timeout     10m
tool_call_timeout      30m
policy_timeout         30s
context_timeout        30s
log_operation_timeout  30s
```

所有 capacity 范围：

```text
1..=4096
```

所有 timeout 必须大于零并小于明确上限。不要使用 `Duration::MAX`。

### 5.2 `SemanticLimits`

```rust
#[derive(Clone, Debug)]
pub struct SemanticLimits {
    pub max_user_input_bytes: usize,
    pub max_system_prompt_bytes: usize,
    pub max_context_blocks: usize,
    pub max_context_bytes: usize,
    pub max_tool_count: usize,
    pub max_tool_name_bytes: usize,
    pub max_tool_schema_bytes: usize,
    pub max_tool_input_bytes: usize,
    pub max_tool_output_bytes: usize,
    pub max_model_text_bytes_per_round: usize,
    pub max_model_reasoning_bytes_per_round: usize,
    pub max_tool_rounds: u16,
    pub max_transcript_page_size: usize,
    pub max_replay_page_size: usize,
}
```

要求：

- 所有 external Port 输出在进入 Conversation 前按这些限制验证；
- limit validation 集中在 checked constructor；
- 禁止各模块使用互相矛盾的 magic number；
- 具体默认值可以沿用 v0.2 已验证边界，但字段必须集中。

### 5.3 `SessionSpec`

`SessionSpec` 是 durable、adapter-neutral 的 Session 配置：

```rust
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionSpec {
    pub model: ModelRef,
    pub reasoning: ReasoningPreference,
    pub system_prompt: BoundedText,
    pub enabled_tools: BTreeSet<ToolName>,
    pub max_tool_rounds: u16,
    pub compaction: CompactionConfig,
}
```

不得包含：

```text
workspace path
HTTP endpoint
credential
Arc<dyn ...>
Tokio Handle
data directory
provider registry
tool implementation
Host metadata such as title/pinned/window state
```

### 5.4 `SessionManifest`

新增 durable manifest DTO：

```rust
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionManifest {
    pub format_version: u32,
    pub session_id: SessionId,
    pub created_at: Timestamp,
    pub spec: SessionSpec,
}
```

要求：

- `format_version` v0.3 初始值固定为 `3`；
- manifest 由 Core 定义语义，由外部 SessionLog adapter 决定编码；
- SessionId 和 spec 在一次 SessionRuntime 生命周期内不可修改；
- Core 不提供 manifest update API；
- v0.2 物理格式迁移属于外部 Store/Host，不在 Core 实现。

### 5.5 `SessionBindings`

新增 `src/session/bindings.rs`：

```rust
#[derive(Clone)]
pub struct SessionBindings {
    pub model: Arc<dyn Model>,
    pub tools: ToolSet,
    pub tool_policy: Option<Arc<dyn ToolPolicy>>,
    pub context: Option<Arc<dyn ContextProvider>>,
    pub compaction: Option<Arc<dyn CompactionStrategy>>,
}
```

Validation 必须检查：

1. `bindings.model.descriptor().model_ref == spec.model`；
2. model 支持 spec.reasoning；
3. `spec.enabled_tools` 全部存在于 ToolSet；
4. ToolSet 不得有重复 ToolName；
5. enabled tools 非空时必须存在 ToolPolicy；
6. compaction enabled 时必须存在 CompactionStrategy；
7. compaction disabled 时允许 strategy 存在，但 Core 不调用；
8. Tool schemas 和 names 满足 SemanticLimits；
9. Bindings 不 Serialize；
10. load 后 Bindings 不可替换。

### 5.6 `UserInput` 与 `TurnOptions`

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UserInput {
    Text(BoundedText),
}

#[derive(Clone, Debug)]
pub struct TurnOptions {
    pub deadline: Option<Instant>,
    pub max_tool_rounds: Option<u16>,
}
```

v0.3 不支持：

```text
per-turn model override
per-turn tool set replacement
per-turn workspace replacement
arbitrary metadata map
```

若未来需要更换模型，Host 应 shutdown 当前 `SessionRuntime`，更新外部 Session 配置或创建新 Session，再用新 Bindings load；不要在 v0.3 内加入 hot swap。

---

## 6. SessionRuntime 生命周期与所有权

### 6.1 为什么必须保留 SessionRuntime owner

Host 可以决定何时加载或卸载 Session，但不得直接管理：

```text
actor task
command channel
state sender
event sender
active Turn cancellation
runner task
SessionLog
writer lease
shutdown ordering
```

这些资源必须被一个不可 Clone 的 owner 封装，才能保证：

```text
停止接收 submit
→ 取消 active Turn
→ settle terminal
→ 关闭 SessionLog
→ 释放 writer lease
→ 关闭 channels
→ 结束 task
```

因此删除的是多 Session `Runtime`，不是单 Session 生命周期层。

### 6.2 `SessionRuntime` 内部建议结构

仅示意，字段保持 private：

```rust
pub struct SessionRuntime {
    session_id: SessionId,
    instance_id: SessionInstanceId,
    handle: SessionHandle,
    events: Option<SessionEventStream>,
    owner_cancel: CancellationToken,
    task: Option<JoinHandle<SessionActorExit>>,
}
```

要求：

- `SessionRuntime` 不实现 Clone；
- `SessionHandle` 不持有 `JoinHandle`；
- actor/supervisor task 自己拥有 `ConversationLog` 和 `Box<dyn SessionLog>`；
- owner drop 仅发送 owner cancellation，不 `mem::forget`；
- explicit shutdown 调用 `owner_cancel.cancel()`，随后 take 并 await actor task；
- shutdown 不依赖普通 command mailbox，因此不受 command backpressure 影响；
- stale SessionHandle 不得阻止 owner shutdown。

### 6.3 `Drop` 语义

`Drop for SessionRuntime`：

1. 发送 owner cancellation；
2. 不同步阻塞；
3. 不调用 `mem::forget`；
4. 不伪装成已完成 durable shutdown；
5. 若 Tokio runtime 仍存活，actor 应进入 best-effort cleanup；
6. 若 Tokio runtime 已销毁，不保证 log flush，文档必须要求 Host 显式 `shutdown().await`。

### 6.4 `SessionInstanceId`

新增非持久化 ID：

```rust
pub struct SessionInstanceId(...);
```

每次 `create` 或 `load` 都生成新值。

用途：

- 区分同一 durable Session 的不同加载实例；
- 让 Host 丢弃旧 event task 的迟到消息；
- 防止旧 SessionHandle 被误认为当前实例；
- 不进入 SessionManifest 或 ConversationEntry。

`SessionId` 是 durable identity；`SessionInstanceId` 是 loaded-instance identity。

### 6.5 Host 多 Session 并发要求

虽然 Host 不在本仓库实现，Core 必须满足：

- 多个 `SessionRuntime` 可以在同一个 Tokio runtime 中并发；
- production code 不使用全局 mutable singleton；
- `SessionLog` 每个实例独占；
- Model、Tool、Policy、Context 可以通过 Arc 跨 Session 共享；
- 一个 Session 的 cancellation 不影响另一个 Session；
- SessionId、TurnId、ToolCallId、InteractionId 的作用域明确；
- 测试必须同时启动至少两个 SessionRuntime 并验证真正并发。

---

## 7. SessionState：替代重型 Snapshot

### 7.1 Public 状态

删除 `SessionSnapshot`，新增轻量 authoritative current state：

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionState {
    pub session_id: SessionId,
    pub instance_id: SessionInstanceId,
    pub status: SessionStatus,
    pub health: SessionHealth,
    pub active_turn: Option<TurnId>,
    pub pending_interaction: Option<PendingInteraction>,
    pub conversation_seq: ConversationSeq,
    pub last_terminal: Option<TurnOutcome>,
}
```

### 7.2 四态状态机

```rust
pub enum SessionStatus {
    Idle,
    Running,
    WaitingForInput,
    Closing,
}
```

合法顶层转移：

```text
Idle → Running
Running → WaitingForInput
WaitingForInput → Running
Running → Idle
WaitingForInput → Idle
Idle → Closing
Running → Closing
WaitingForInput → Closing
```

不增加：

```text
Closed
RunningTool
CallingModel
Compacting
WaitingForSubagent
Retrying
```

`Closed` 由 state channel/command channel 关闭表达；model/tool/compaction 属于 Event，不是顶层状态。

### 7.3 Health

```rust
pub enum SessionHealth {
    Healthy,
    Degraded {
        diagnostic: DiagnosticSummary,
    },
}
```

规则：

- `Healthy` 才允许 submit；
- `Degraded` 仍允许读取 state/transcript 和执行 shutdown；
- append `UnknownOutcome`、Conflict/Corrupt、active Turn commit 无法继续或 actor 无法确定 durable head 时进入 Degraded；
- 不允许使用 private `unavailable: bool`；
- health 改变必须先更新 watch state，再 best-effort 发布 Event。

### 7.4 State watch

Actor 内部使用：

```rust
watch::Sender<SessionState>
```

Handle 持有：

```rust
watch::Receiver<SessionState>
```

规则：

- `send_replace` 更新最新状态；
- state 更新不得等待 UI；
- watch 可被 clone，但 Core 不提供多观察者恢复协议；
- Host/UI 可随时读取最新状态；
- pending Interaction 必须完整包含 UI 回答所需的受限字段。

---

## 8. SessionEventStream：单消费者实时事件

### 8.1 EventStream 语义

Event 只回答：

```text
刚刚发生了什么？
```

Event 不是：

- durable history；
- Session 当前事实来源；
- 控制命令；
- audit log；
- 跨重启 replay protocol。

### 8.2 Public Event

```rust
pub enum SessionEvent {
    TurnStarted {
        turn_id: TurnId,
    },

    ModelStarted {
        turn_id: TurnId,
        round: u16,
    },

    OutputDelta {
        turn_id: TurnId,
        channel: OutputChannel,
        delta: BoundedText,
    },

    ModelFinished {
        turn_id: TurnId,
        round: u16,
        usage: Usage,
    },

    ToolStarted {
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        tool_name: ToolName,
    },

    ToolProgress {
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        progress: ToolProgress,
    },

    ToolFinished {
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        result: ToolResultSummary,
    },

    InteractionRequested {
        interaction: PendingInteraction,
    },

    InteractionResolved {
        interaction_id: InteractionId,
        resolution: InteractionResolutionSummary,
    },

    HealthChanged {
        health: SessionHealth,
    },

    TurnFinished {
        turn_id: TurnId,
        outcome: TurnOutcome,
    },

    EventsDropped {
        count: u64,
    },
}
```

`SessionEventStream` wrapper 的每条消息必须额外携带或可查询：

```text
session_id
session_instance_id
```

可使用 envelope：

```rust
pub struct SessionEventEnvelope {
    pub session_id: SessionId,
    pub instance_id: SessionInstanceId,
    pub event: SessionEvent,
}

#[derive(Clone, Debug)]
pub struct ToolResultSummary {
    pub outcome: ToolResultOutcome,
    pub content_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InteractionResolutionSummary {
    Approved,
    Denied,
    InputProvided,
}
```

Event summary 不包含完整 Tool output、用户输入答案、Tool arguments 或 provider raw error；这些内容分别属于 durable Conversation、Host UI state 或 internal tracing。

### 8.3 输出 channel

```rust
pub enum OutputChannel {
    Text,
    Reasoning,
}
```

不要将 stdout/stderr、Subagent、Git、Browser 做成 Core channel；具体 Tool progress 由 `ToolProgress` 表达。

### 8.4 Event delivery

内部使用 bounded `mpsc` 单消费者 channel。

要求：

- actor 使用 `try_send`，EventStream 不能反压执行；
- queue 满时丢弃当前 event 并累加 dropped count；
- 下次可发送时优先 best-effort 发送 `EventsDropped`；
- `OutputDelta` 和 `ToolProgress` 明确允许丢失；
- `InteractionRequested` 即使 Event 丢失，UI 仍可从 SessionState.pending_interaction 获取；
- `TurnFinished` 即使丢失，调用方仍可从 TurnHandle.wait 和 SessionState 获取；
- 不实现 `ResyncRequired`、Gap、cursor、revision、epoch；
- 不实现 broadcast subscriber list。

### 8.5 发布顺序

必须满足：

1. `TurnStarted`：UserMessage durable 后；
2. `ToolStarted`：AssistantMessage containing ToolCall durable、policy allow 后；
3. `ToolFinished`：对应 ToolResult durable 后；
4. `InteractionRequested`：SessionState 已进入 WaitingForInput 后；
5. `InteractionResolved`：answer 已被 actor 接受并 state 恢复 Running 后；
6. `TurnFinished`：TurnTerminal durable、SessionState 更新、TurnHandle completion 设置后；
7. `HealthChanged`：SessionState health 先更新。

### 8.6 删除旧观察实现

删除：

```text
SessionSnapshot
SnapshotHistory
SessionObservation broadcast
subscribe()
ResyncRequired
first snapshot event
lag recovery tests
ObservationEpoch / Cursor / Revision / Gap
```


## 9. Conversation Semantic Model

### 9.1 目标文件

```text
src/conversation/entry.rs
src/conversation/validator.rs
src/conversation/projection.rs
src/conversation/log.rs
src/conversation/recovery.rs
src/conversation/transcript.rs
```

### 9.2 Durable ConversationEntry

v0.3 保留最小 durable semantic model：

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ConversationEntry {
    UserMessage(UserMessageEntry),
    AssistantMessage(AssistantMessageEntry),
    ToolResult(ToolResultEntry),
    Summary(SummaryEntry),
    TurnTerminal(TurnTerminalEntry),
}
```

不持久化：

```text
SessionState
SessionEvent
OutputDelta
ToolProgress
PendingInteraction
ApprovalRequest
InteractionAnswer
ModelStarted/Finished
ToolStarted
```

### 9.3 UserMessageEntry

```rust
pub struct UserMessageEntry {
    pub seq: ConversationSeq,
    pub turn_id: TurnId,
    pub input: UserInputRecord,
    pub execution: TurnExecutionRecord,
    pub created_at: Timestamp,
}
```

`TurnExecutionRecord` 至少记录：

- effective model selection；
- effective reasoning；
- effective budget 的 durable、相对值；
- SessionSpec identity/version 如有；
- 不记录 monotonic Instant。

### 9.4 AssistantMessageEntry

```rust
pub struct AssistantMessageEntry {
    pub seq: ConversationSeq,
    pub turn_id: TurnId,
    pub model: ModelSelection,
    pub text: Option<BoundedText>,
    pub reasoning: Option<BoundedText>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub finish_reason: ModelFinishReason,
    pub created_at: Timestamp,
}
```

要求：

- 整条 Model response 完成验证后一次 append；
- provisional delta 不 append；
- ToolCallId 在 Session Conversation 内唯一；
- arguments 必须是 bounded valid JSON；
- tool name 必须 enabled 且存在；
- response 不能同时处于非法 finish state；
- AssistantMessage append 成功后才允许执行 Tool。

### 9.5 ToolResultEntry

```rust
pub struct ToolResultEntry {
    pub seq: ConversationSeq,
    pub turn_id: TurnId,
    pub tool_call_id: ToolCallId,
    pub tool_name: ToolName,
    pub outcome: ToolResultOutcome,
    pub content: BoundedText,
    pub created_at: Timestamp,
}
```

```rust
pub enum ToolResultOutcome {
    Success,
    Failed,
    Denied,
    Cancelled,
    InputProvided,
}
```

要求：

- 必须对应之前 unresolved ToolCall；
- name 与 ToolCall 一致；
- 每个 ToolCall 恰好一个 ToolResult；
- denied、cancelled、input-provided 都是正常 durable ToolResult；
- 具体 approval 原文不写入 prompt-visible content，除非外部 Tool 自己把必要结果写入 ToolOutput；
- restart repair 生成 `Cancelled`。

### 9.6 SummaryEntry

```rust
pub struct SummaryEntry {
    pub seq: ConversationSeq,
    pub through: ConversationSeq,
    pub summary: BoundedText,
    pub created_at: Timestamp,
}
```

Summary 只影响 prompt projection，不删除 source entries。

### 9.7 TurnTerminalEntry

```rust
pub struct TurnTerminalEntry {
    pub seq: ConversationSeq,
    pub turn_id: TurnId,
    pub terminal: TurnTerminal,
    pub usage: Usage,
    pub created_at: Timestamp,
}
```

```rust
pub enum TurnTerminal {
    Completed,
    Failed { diagnostic: DiagnosticSummary },
    CancelledByUser,
    CancelledByShutdown,
    CancelledByRestart,
    BudgetExceeded,
}
```

### 9.8 Validator invariant

`ConversationValidator` 必须验证：

1. seq 连续；
2. 一个 Turn 从 UserMessage 开始；
3. 同一时刻最多一个未终结 Turn；
4. 一个 Turn 只能有一个 terminal；
5. terminal 后不能追加该 Turn 的普通 entry；
6. AssistantMessage 的 turn_id 必须匹配 active Turn；
7. ToolResult 必须匹配 unresolved ToolCall；
8. ToolCallId 不重复；
9. ToolResult 不重复；
10. 下一次 Model response 前，前一 AssistantMessage 的所有 ToolCall 都有 result；
11. Summary boundary 只能落在 completed semantic boundary；
12. 大小限制在 commit 前已经满足；
13. stored DTO 反序列化后再次执行 semantic validation。

### 9.9 ConversationLog 内部事务

Runner、actor settlement 和 recovery 只能构造未分配序号的 draft；只有 `ConversationLog` 可以分配 `ConversationSeq` 与 durable timestamp：

```rust
pub(crate) enum UnsequencedEntry {
    UserMessage(UserMessageDraft),
    AssistantMessage(AssistantMessageDraft),
    ToolResult(ToolResultDraft),
    Summary(SummaryDraft),
    TurnTerminal(TurnTerminalDraft),
}
```

Draft 不包含 `seq`；Core 在 `append_validated` 内根据当前 head 生成最终 `ConversationEntry`。任何 Runner 都不得自行猜测下一个 seq。

```rust
pub(crate) struct ConversationState {
    validator: ConversationValidator,
    projection: ConversationProjection,
    head: ConversationSeq,
}

pub(crate) struct ConversationLog {
    inner: Box<dyn SessionLog>,
    state: ConversationState,
    closed: bool,
}
```

append 流程：

```text
clone validator/projection candidate
→ validate whole batch
→ SessionLog.append(expected_head, batch)
→ verify AppendReceipt
→ commit candidate validator/projection/head
→ update SessionState
→ best-effort publish Event
```

Store append 失败时不得先更新内存 projection。

### 9.10 Transcript

`SessionHandle::transcript` 通过 actor 串行调用 ConversationLog/SessionLog：

- limit 受 `SemanticLimits.max_transcript_page_entries` 限制；
- 返回 durable entries 的 public transcript DTO；
- 不返回 pending interaction；
- 不保证包含丢失的 OutputDelta；
- active Turn 尚未 durable 的 provisional assistant text 不在 transcript 中；
- degraded 状态下，如果 log 不可继续调用，可从内存已确认 projection 返回 bounded 已提交部分，并在 page 中标记 `complete: false`。

### 9.11 Restart recovery

load 时只恢复 durable history，不恢复 execution continuation。

若最后一个 Turn 没有 terminal：

1. 找出该 Turn 所有 unresolved ToolCall；
2. 按 AssistantMessage 和 call 原顺序生成 cancelled ToolResult；
3. append `TurnTerminal::CancelledByRestart`；
4. 一次 batch commit；
5. load 后状态为 Idle；
6. 不重发旧 InteractionRequested；
7. 不调用旧 Tool；
8. 不恢复 model stream；
9. 不恢复 retry sleep。

若 repair append 返回 UnknownOutcome，load 失败并返回 `SessionOpenError::RecoveryUncertain`，不得启动 actor。

---

## 10. SessionLog Port 与 durable contract

### 10.1 为什么只保留 SessionLog

v0.3 不保留 repository-level `ConversationStore`。

原因：

- Host 已经负责 Session 列表、创建 metadata、删除和 load 策略；
- Core 只需要操作一个已经获取独占 lease 的 Session；
- list/create/delete repository API 会重新把多 Session 管理带回 Core；
- 一个 `SessionRuntime` 只应持有一个 `SessionLog`。

因此 public Store Port 只有：

```text
SessionLog
```

Host/adapter 如何得到该 log 不属于本 crate。

### 10.2 Trait

新增或重写 `src/storage/session_log.rs`：

```rust
pub trait SessionLog: Send + 'static {
    fn initialize<'a>(
        &'a mut self,
        manifest: SessionManifest,
    ) -> LogFuture<'a, ConversationSeq>;

    fn load_manifest<'a>(
        &'a mut self,
    ) -> LogFuture<'a, SessionManifest>;

    fn read_page<'a>(
        &'a mut self,
        after: Option<ConversationSeq>,
        limit: usize,
    ) -> LogFuture<'a, ConversationPage>;

    fn append<'a>(
        &'a mut self,
        expected_head: ConversationSeq,
        entries: Vec<ConversationEntry>,
    ) -> LogFuture<'a, AppendReceipt>;

    fn close<'a>(
        &'a mut self,
    ) -> LogFuture<'a, ()>;
}
```

建议 future alias：

```rust
pub type LogFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, SessionLogError>> + Send + 'a>>;
```

### 10.3 为什么使用 `&mut self`

`SessionLog` 由 Session actor 独占，因此：

- trait 只要求 `Send`，不要求 `Sync`；
- 所有 log 操作天然串行；
- adapter 可以安全持有事务、文件句柄或 lease；
- Core 不需要对 log 再包 `Arc<Mutex<_>>`；
- 不允许 runner 和 actor 并发写 log。

### 10.4 初始化与 load

所有具体 adapter 还必须保证：`Drop` 不阻塞，并能释放进程内句柄/lease ownership；`close()` 是 graceful flush/close contract，而不是释放所有权的唯一途径。

`initialize`：

- 只允许对未初始化 log 调用；
- 成功表示 manifest 已 durable；
- 返回 head 必须为零；
- 已初始化返回 `AlreadyInitialized`；
- 不得静默覆盖。

`load_manifest`：

- 返回 Core 定义的 SessionManifest；
- 不执行 legacy migration；
- format version 不支持由 Core 返回 SessionOpenError；
- adapter 物理损坏映射 `Corrupt`。

### 10.5 分页 replay

```rust
pub struct ConversationPage {
    pub entries: Vec<ConversationEntry>,
    pub next_after: Option<ConversationSeq>,
    pub observed_head: ConversationSeq,
}
```

要求：

- `entries.len() <= requested limit`；
- entry seq 严格递增；
- `next_after` 等于最后 entry seq；
- 到尾部时 `next_after = None`；
- exclusive lease 下多页读取的 head 必须稳定；
- Core 仍逐条执行 semantic validator；
- page size 使用 `KernelConfig.limits.max_replay_page_size`。

### 10.6 Append receipt

```rust
pub struct AppendReceipt {
    pub previous_head: ConversationSeq,
    pub new_head: ConversationSeq,
    pub appended: usize,
}
```

成功必须表示：

> 传入的非空 batch 已作为一个原子 append 全部 durable，并且之后重新打开同一 SessionLog 时可以按相同顺序读回。

失败语义：

- adapter 能证明 batch 完全未提交时，返回明确的 `Unavailable` / `Conflict` / `Corrupt` 等错误；
- adapter 不能证明是否发生部分或全部提交时，必须返回 `UnknownOutcome`；
- 不允许以普通错误声称“部分 entries 已成功”；
- Core 不实现 partial-batch compensation。

禁止返回：

```text
success but not durable
queued
accepted for later
best effort
```

### 10.7 Append 顺序

所有 append 必须通过 `conversation::log::ConversationLog::append_validated`：

```text
1. 根据当前 ConversationState 分配连续 seq
2. 在临时 state 上验证完整 batch
3. 调用 SessionLog::append(expected_head, batch)
4. 校验 AppendReceipt
5. 仅成功后 commit 内存 ConversationState
6. 更新 SessionState.conversation_seq
7. 发布相应 Event 或完成 TurnHandle
```

Store append 前不得更新 authoritative state。

### 10.8 Append cancellation、timeout 与失败状态

SessionLog append 与普通 Model/Tool 不同：

- 一旦调用 adapter，Core 不得因调用者 future 被 drop 就假定未提交；
- timeout、panic、channel disconnect 或 adapter 无法证明未提交时，结果映射为 `UnknownOutcome`；
- `UnknownOutcome` 后 Core 不得继续 append，SessionHealth 立即变为 Degraded；
- active Turn waiter 返回 `TurnWaitError::DurabilityUnknown`；
- active Turn 期间出现 `Unavailable`、`Conflict`、`Corrupt` 或无法完成 commit 的其他已知失败时，同样停止 runner并进入 Degraded；
- 已知未提交的 active-Turn failure 返回 `TurnWaitError::DurabilityUnavailable`，不得伪造 Failed terminal；下次 load 通过 restart repair 收口 open Turn；
- UserMessage 尚未提交时的 retryable `Unavailable` 可以直接使 submit 失败并保持 Idle/Healthy；`Conflict`、`Corrupt` 和 `UnknownOutcome` 仍必须 Degraded；
- Degraded 后只允许 state、已确认 transcript 和 shutdown，不允许新 submit 或任何新的 append；
- `close()` 仍可调用，但不得借 shutdown 猜测未知 append 的结果。

### 10.9 `SessionLogError`

```rust
pub enum SessionLogErrorKind {
    NotInitialized,
    AlreadyInitialized,
    Conflict,
    Corrupt,
    Unavailable,
    UnknownOutcome,
    Closed,
    Internal,
}
```

Error 必须包含 safe diagnostic，禁止默认暴露数据库连接串、路径隐私或 raw record。

### 10.10 `ConversationLog`

新增 `src/conversation/log.rs`，它是 Core 内部 coordinator：

`ConversationLog` 的唯一结构定义见 9.9；本节只规定其 Port 协调方法。

主要方法：

```rust
impl ConversationLog {
    pub(crate) async fn initialize(...);

    pub(crate) async fn load_and_validate(...);

    pub(crate) async fn append_validated(
        &mut self,
        entries: Vec<UnsequencedEntry>,
    ) -> Result<CommittedBatch, ConversationCommitError>;

    pub(crate) async fn transcript(
        &mut self,
        after: Option<ConversationSeq>,
        limit: usize,
    ) -> Result<TranscriptPage, ConversationCommitError>;

    pub(crate) fn projection(&self) -> PromptProjection;

    pub(crate) fn recovery_plan(&self) -> Option<RecoveryPlan>;

    pub(crate) async fn close(&mut self) -> Result<(), ConversationCommitError>;
}
```

禁止 actor、runner 或 prompt builder 直接调用 `SessionLog::append`。

---

## 11. Restart recovery 与 crash-consistency

### 11.1 明确保证

MiniCore v0.3 保证：

> durable Conversation crash-consistent；未完成执行在下一次 load 时被明确终止。

不保证：

```text
Model stream continuation
Tool future continuation
pending approval continuation
pending user question continuation
background job continuation
```

### 11.2 Recovery plan

新增 `src/conversation/recovery.rs`：

```rust
pub(crate) struct RecoveryPlan {
    pub turn_id: TurnId,
    pub unresolved_tools: Vec<PendingToolCall>,
    pub terminal: TurnTerminal,
}
```

`ConversationState::recovery_plan()`：

- 无 open turn：返回 None；
- 有 open turn：为每个 unresolved ToolCall 生成 cancelled ToolResult；
- terminal 固定为 `Cancelled { reason: RestartRepair }`；
- 工具结果按 AssistantMessage 中 ToolCall 顺序生成；
- 不调用 Tool、Model、Policy 或 ContextProvider。

### 11.3 Load repair 顺序

```text
load manifest
→ replay and validate all entries
→ build RecoveryPlan
→ append cancelled ToolResults + TurnTerminal as one atomic `SessionLog::append` batch
→ commit state
→ spawn SessionActor
→ expose Idle state
```

如果 repair append：

- `Unavailable`：load 失败，可重试；
- `UnknownOutcome`：load 失败并返回 durability unknown；
- `Conflict`：load 失败，说明 exclusive lease contract 被破坏；
- `Corrupt`：load 失败，不自动覆盖或截断。

### 11.4 Shutdown repair

正常 shutdown 不是 restart repair。

shutdown 时若 active Turn 存在：

- cancellation reason 使用 `SessionShutdown`；
- runner 尽量为当前 pending ToolCall 生成 cancelled ToolResult；
- actor durable append terminal；
- 只有进程异常退出、无法正常 settle 时，下次 load 才使用 `RestartRepair`。

---

## 12. Model Port

### 12.1 删除 Registry 和具体 Provider

必须删除：

```text
ProviderRegistry
ModelResolver
provider installation
credential source
endpoint policy
HTTP transport
OpenAI Responses implementation
Anthropic Messages implementation
```

一个 loaded Session 直接通过 `SessionBindings.model` 持有：

```rust
Arc<dyn Model>
```

### 12.2 Model trait

重写 `src/model/provider.rs` 为 `src/model/model.rs`：

```rust
pub trait Model: Send + Sync + 'static {
    fn descriptor(&self) -> &ModelDescriptor;

    fn start<'a>(
        &'a self,
        request: ModelRequest,
        context: ModelCallContext,
    ) -> ModelStartFuture<'a>;
}
```

```rust
pub type ModelStartFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ModelStream, ModelError>> + Send + 'a>>;

pub type ModelStream =
    Pin<Box<dyn Stream<Item = Result<ModelEvent, ModelError>> + Send + 'static>>;
```

### 12.3 Descriptor

```rust
pub struct ModelDescriptor {
    pub model_ref: ModelRef,
    pub context_window: u64,
    pub supported_reasoning: BTreeSet<ReasoningPreference>,
    pub supports_tools: bool,
}
```

禁止 descriptor 包含 credential 或 endpoint secret。

### 12.4 `ModelCallContext`

```rust
pub struct ModelCallContext {
    pub session_id: SessionId,
    pub instance_id: SessionInstanceId,
    pub turn_id: TurnId,
    pub round: u16,
    pub cancellation: CancellationToken,
    pub deadline: Instant,
}
```

不提供：

```text
SessionHandle
Workspace
Store
ToolSet mutable handle
Runtime/Host access
```

### 12.5 Model events

保留现有能表达 streaming response 的 typed events，但必须由 internal assembler 消费：

```text
TextDelta
ReasoningDelta
ToolCallStart
ToolCallArgumentsDelta
ToolCallEnd
Usage
Finish
```

Core 只在完整 response 通过 validation 后构造 AssistantMessageEntry。

### 12.6 `ModelDriver`

将当前 `model/gateway.rs` 收敛为 internal `model/driver.rs`：

职责：

- 调用一个绑定好的 `Arc<dyn Model>`；
- 应用 model timeout、cancellation 和 retry；
- catch panic；
- assembler 组装 streaming response；
- 检查字节限制；
- 将 delta 转为 RunnerProgress；
- 返回 validated `ModelResponse`。

不负责：

```text
Provider lookup
credential resolution
HTTP request construction
session management
conversation append
```

### 12.7 Retry

Core 只对 Model 调用应用 retry。

`ModelError` 必须携带 delivery state：

```rust
pub enum DeliveryState {
    NotStarted,
    Started,
    Unknown,
}
```

自动 retry 仅允许：

```text
retryable == true
AND delivery == NotStarted
AND deadline/budget 未耗尽
```

`Started` 或 `Unknown` 不自动 retry，避免重复模型 side effect 或重复 billing。

Tool、Policy、Context、SessionLog 不使用通用自动 retry。

### 12.8 Panic 与 cancellation

- Model future 和 stream poll panic 必须被 catch；
- panic 转 `ModelErrorKind::Panicked`；
- cancellation 优先于 retry sleep；
- cancellation 后停止继续消费 stream；
- adapter 自身产生的后台请求清理由 Model 契约负责；
- Core 不强制 provider 实现 abort handle，但文档要求 no detached Core-owned work。

---

## 13. Tool、ToolSet、Policy 与 Interaction

### 13.1 删除具体 Tool

删除 production：

```text
ask_user
read_file
write_file
list_directory
run_command
process adapter
```

测试使用 FakeTool。

### 13.2 Tool trait

新增或重写 `src/tools/tool.rs`：

```rust
pub trait Tool: Send + Sync + 'static {
    fn spec(&self) -> &ToolSpec;

    fn execute<'a>(
        &'a self,
        invocation: ToolInvocation,
        context: ToolContext,
    ) -> ToolFuture<'a>;
}
```

```rust
pub type ToolFuture<'a> = Pin<
    Box<dyn Future<Output = Result<ToolExecutionOutcome, ToolError>> + Send + 'a>
>;
```

### 13.3 ToolSpec

```rust
pub struct ToolSpec {
    pub name: ToolName,
    pub description: BoundedText,
    pub input_schema: serde_json::Value,
}
```

不增加 Core workspace/read/write/network capability 字段。

具体 Tool 在构造时已经捕获所需外部能力；其安全策略由 ToolPolicy 和外部 adapter 自己保证。

### 13.4 ToolInvocation

```rust
pub struct ToolInvocation {
    pub session_id: SessionId,
    pub instance_id: SessionInstanceId,
    pub turn_id: TurnId,
    pub tool_call_id: ToolCallId,
    pub tool_name: ToolName,
    pub arguments: serde_json::Value,
}
```

arguments 在构造前已通过：

- JSON completion；
- input byte limit；
- ToolName lookup；
- ToolCall uniqueness。

### 13.5 ToolContext

重写 `src/tools/context.rs`：

```rust
pub struct ToolContext {
    pub cancellation: CancellationToken,
    pub deadline: Instant,
    pub progress: ToolProgressSink,
}
```

删除：

```text
Workspace
InteractionClient
ask_user()
RuntimeClient
SessionHandle
Any/TypeMap
credentials
```

Tool 所需 Workspace、RPC client、ProcessExecutor、MCP client 等必须由 Tool struct 自己捕获。

### 13.6 Tool execution outcome

```rust
pub enum ToolExecutionOutcome {
    Completed(ToolOutput),
    RequestInput(ToolInputRequest),
}
```

`Completed`：正常进入 ToolResult。

`RequestInput`：

- Tool 本次 future 已结束；
- Core 不保存或恢复该 future；
- Core 创建内存 pending Interaction；
- 用户回答后，Core 将答案直接编码为该 ToolCall 的 ToolResult；
- Core 不再次调用原 Tool。

Tool 在返回 RequestInput 前不得执行依赖用户答案的不可逆副作用；这是 Tool 契约。

### 13.7 ToolOutput

```rust
pub struct ToolOutput {
    pub content: BoundedText,
}
```

ToolOutput 不自行指定 success/failed terminal；成功返回代表 status Success，`Err(ToolError)` 代表 Failed。

### 13.8 ToolSet

将 `src/tools/registry.rs` 改为 `src/tools/set.rs`：

```rust
#[derive(Clone)]
pub struct ToolSet {
    tools: Arc<BTreeMap<ToolName, Arc<dyn Tool>>>,
}
```

API：

```rust
impl ToolSet {
    pub fn builder() -> ToolSetBuilder;
    pub fn get(&self, name: &ToolName) -> Option<Arc<dyn Tool>>;
    pub fn specs_for(&self, enabled: &BTreeSet<ToolName>) -> Vec<ToolSpec>;
    pub fn contains(&self, name: &ToolName) -> bool;
}
```

要求：

- immutable；
- duplicate name build error；
- 不支持运行中 register/unregister；
- 不存在 Runtime-global ToolRegistry；
- Host 可以为不同 Session 构造不同 ToolSet。

### 13.9 ToolPolicy

```rust
pub trait ToolPolicy: Send + Sync + 'static {
    fn decide<'a>(
        &'a self,
        request: ToolPolicyRequest,
    ) -> ToolPolicyFuture<'a>;
}

pub type ToolPolicyFuture<'a> = Pin<
    Box<dyn Future<Output = Result<ToolDecision, ToolPolicyError>> + Send + 'a>
>;

pub struct ToolPolicyRequest {
    pub invocation: ToolInvocation,
    pub spec: ToolSpec,
    pub cancellation: CancellationToken,
    pub deadline: Instant,
}
```

```rust
pub enum ToolDecision {
    Allow,
    Deny {
        reason: BoundedText,
    },
    RequireApproval {
        request: ApprovalRequest,
    },
}
```

`ToolPolicyRequest.invocation` 中的 arguments 和 identity 在整个审批流程中不可变；`deadline` 由 Core 按 `policy_timeout` 与 Turn deadline 取较早值。

Policy 不得：

- 修改 arguments；
- 执行 Tool；
- 直接打开 UI；
- 获得 actor/session internals。

Policy timeout/panic/error 必须 fail closed，生成 Denied ToolResult 和 safe diagnostic。

### 13.10 Approval

```rust
pub struct ApprovalRequest {
    pub prompt: BoundedText,
    pub risk: ApprovalRisk,
}

pub enum ApprovalDecision {
    AllowOnce,
    Deny,
}
```

v0.3 不支持 `AllowForSession`，因为这要求 Core 管理 policy state。Host 可以提供 stateful ToolPolicy，但一次 interaction 的 Core decision 只有 AllowOnce/Deny。

Flow：

```text
ToolCall durable
→ ToolPolicy::decide
→ RequireApproval
→ actor 建立 PendingInteraction
→ SessionState WaitingForInput
→ SessionHandle::answer(Approval)
→ AllowOnce: actor 恢复 Running，runner 执行原 ToolInvocation
→ Deny: runner 生成 Denied ToolResult
```

Tool arguments 和 identity 在等待期间不可修改，因此不需要第二次 policy 调用。

### 13.11 Tool input

```rust
pub struct ToolInputRequest {
    pub prompt: BoundedText,
    pub choices: Vec<BoundedText>,
    pub answer_kind: ToolInputAnswerKind,
}

pub enum ToolInputAnswerKind {
    Text,
    SingleChoice,
}
```

回答：

```rust
pub enum ToolInputAnswer {
    Text(BoundedText),
    Choice { index: usize },
}
```

Core 使用固定 canonical JSON 形成 ToolResult：

```json
{"answer":"..."}
```

或：

```json
{"choice_index":0,"choice":"..."}
```

不要让 Tool 提供任意 continuation token 或 resume callback。

### 13.12 PendingInteraction

新增 `src/session/interaction.rs`：

```rust
pub struct PendingInteraction {
    pub interaction_id: InteractionId,
    pub turn_id: TurnId,
    pub tool_call_id: ToolCallId,
    pub tool_name: ToolName,
    pub kind: InteractionKind,
}

pub enum InteractionKind {
    Approval(ApprovalRequest),
    ToolInput(ToolInputRequest),
}

pub enum InteractionAnswer {
    Approval(ApprovalDecision),
    ToolInput(ToolInputAnswer),
}
```

要求：

- 只存在于 actor 内存和 SessionState；
- 不进入 Conversation；
- 同时最多一个 pending Interaction；
- answer kind 必须严格匹配 pending kind；
- interaction_id 不匹配返回 `InteractionMismatch`；
- answer 只能消费一次；
- shutdown/cancel 时 pending interaction 被取消；
- restart 后不恢复。

### 13.13 ToolProgress

```rust
#[derive(Clone, Debug)]
pub struct ToolProgress {
    pub message: Option<BoundedText>,
    pub completed: Option<u64>,
    pub total: Option<u64>,
}
```

`ToolProgressSink` 由 Core 构造、字段 private、可以 Clone：

```rust
#[derive(Clone)]
pub struct ToolProgressSink {
    inner: Arc<dyn ToolProgressEmitter>,
}

impl ToolProgressSink {
    pub fn emit(&self, progress: ToolProgress) -> bool;
}
```

`emit` 返回 `true` 仅表示 event 已进入 best-effort queue，不表示 durable；返回 `false` 表示 queue full、stream closed 或 progress 被丢弃。

`ToolProgressSink::emit`：

- 同步、非阻塞；
- best effort；
- 校验 message 大小；
- 不能改变 Tool 结果；
- 不能携带任意 `serde_json::Value` 或服务对象；
- Remote Agent 若需要显示详细 child UI，由 Host/RPC 层处理，不扩展 Core progress schema。

### 13.14 Tool cancellation 与 panic

- 每个 ToolCall 获得 child CancellationToken；
- Turn cancel 会取消当前 Tool token；
- Tool timeout 同样取消 token；
- Tool future panic 转 failed ToolResult；
- Tool error 不直接失败 Turn，默认形成 failed ToolResult 并让模型下一轮处理；
- 只有 Core invariant、budget、durability 或 model-level failure 才直接终止 Turn；
- Tool 不得在 future 返回后留下 Core 认为仍受其所有的后台任务。

---

## 14. ContextProvider 与 Prompt 组装

### 14.1 ContextProvider

新增 `src/context/provider.rs`：

```rust
pub trait ContextProvider: Send + Sync + 'static {
    fn provide<'a>(
        &'a self,
        request: ContextRequest,
    ) -> ContextFuture<'a>;
}
```

```rust
pub struct ContextRequest {
    pub session_id: SessionId,
    pub instance_id: SessionInstanceId,
    pub turn_id: TurnId,
    pub model_round: u16,
    pub conversation: ConversationView,
    pub remaining_context_budget: u64,
    pub cancellation: CancellationToken,
    pub deadline: Instant,
}
```

### 14.2 Context 返回值

```rust
pub struct ContextBundle {
    pub blocks: Vec<ContextBlock>,
}

pub struct ContextBlock {
    pub source: ContextSourceId,
    pub slot: ContextSlot,
    pub priority: i16,
    pub content: BoundedText,
}

pub enum ContextSlot {
    ProjectInstructions,
    RetrievedKnowledge,
    TurnContext,
}
```

限制：

- ContextProvider 不能返回任意 Chat/Model role message；
- 不能伪造 AssistantMessage、ToolCall 或 ToolResult；
- 不能直接修改 Conversation；
- blocks 数量和总字节受 SemanticLimits 限制；
- Core 按固定 slot 顺序、`priority` 降序、`source` 升序稳定排序；
- duplicate source 可拒绝或稳定覆盖，必须在实现中固定一种规则；推荐拒绝 duplicate source。

### 14.3 调用时机

每次 Model round 前调用一次 ContextProvider：

```text
conversation projection
→ context provide
→ validate ContextBundle
→ prompt build
→ model call
```

外部 composite provider 可以在内部组合：

```text
AGENTS.md
Memory
RAG
Git summary
Skills
```

Core 只调用一个 optional provider，避免在 Kernel 中实现 provider fan-out、并发和 partial failure 策略。

### 14.4 Context failure

- timeout、panic、error 默认终止当前 Turn；
- 不 append AssistantMessage；
- actor append Failed TurnTerminal；
- 若产品需要 best-effort context，应在外部 composite provider 内实现；
- Context cancellation 继承 Turn cancellation。

### 14.5 PromptBuilder

重写 `src/prompt/builder.rs`：

固定顺序：

```text
1. Kernel invariant instructions（仅 Core 必要协议）
2. SessionSpec.system_prompt
3. ContextSlot::ProjectInstructions
4. ContextSlot::RetrievedKnowledge
5. ContextSlot::TurnContext
6. durable Conversation prompt projection
7. enabled Tool specs
```

要求：

- 不读取文件；
- 不包含 global coding_instructions；
- 不调用 Provider；
- 纯函数或近似纯函数；
- 同样输入产生字节级稳定顺序；
- context/model/tool limits 在 call 前完成；
- PromptBuilder 不持有 SessionRuntime/Handle。

---

## 15. CompactionStrategy

### 15.1 Port

新增 `src/compaction/strategy.rs`：

```rust
pub trait CompactionStrategy: Send + Sync + 'static {
    fn compact<'a>(
        &'a self,
        request: CompactionRequest,
    ) -> CompactionFuture<'a>;
}
```

```rust
pub struct CompactionRequest {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub candidate: CompactionCandidate,
    pub target_tokens: u64,
    pub cancellation: CancellationToken,
    pub deadline: Instant,
}

pub struct CompactionProposal {
    pub through_seq: ConversationSeq,
    pub summary: BoundedText,
}
```

外部 strategy 可捕获另一个 Model，但 Core 不知道。

### 15.2 Core validation

proposal 必须满足：

- through_seq 存在；
- boundary 位于已 terminal Turn 尾部；
- 不跨越当前 open Turn；
- 不回退到已有 Summary boundary 之前；
- summary 大小受限；
- append Summary 前再次检查当前 head 未改变。

### 15.3 触发

`SessionSpec.compaction`：

```rust
pub enum CompactionConfig {
    Disabled,
    Enabled {
        trigger_tokens: u64,
        target_tokens: u64,
    },
}
```

规则：

- prompt 仍能放入 context 时的 proactive compaction 失败可记录 diagnostic 后继续；
- prompt 已无法构造时的 forced compaction 失败必须终止 Turn；
- compaction strategy 不直接写 Store；
- Core 负责 append Summary；
- compaction 不删除 durable transcript。

### 15.4 删除当前 concrete compactor

删除或重写当前 `prompt/compaction.rs` 中：

```text
具体模型摘要调用
provider lookup
直接 conversation mutation
```

保留 boundary selection/validation 中属于 Core invariant 的部分，移动到 `conversation` 或 `compaction` internal helper。


## 16. SessionActor 与 Agent Loop

### 16.1 Actor command

目标 `src/session/command.rs`：

```rust
pub(crate) enum SessionCommand {
    Submit {
        input: UserInput,
        options: TurnOptions,
        reply: oneshot::Sender<Result<TurnHandle, SessionError>>,
    },
    Answer {
        interaction_id: InteractionId,
        answer: InteractionAnswer,
        reply: oneshot::Sender<Result<(), SessionError>>,
    },
    Transcript {
        after: Option<ConversationSeq>,
        limit: usize,
        reply: oneshot::Sender<Result<TranscriptPage, SessionError>>,
    },
}
```

Turn cancel 和 owner shutdown 都不通过普通 command mailbox 排队：`TurnHandle` 直接触发 exact Turn token，`SessionRuntime` 直接触发 root token。

### 16.2 SessionActor private state

```rust
pub(crate) struct SessionActor {
    session_id: SessionId,
    instance_id: SessionInstanceId,
    config: KernelConfig,
    spec: SessionSpec,
    bindings: SessionBindings,
    conversation: ConversationLog,
    commands: mpsc::Receiver<SessionCommand>,
    state_tx: watch::Sender<SessionState>,
    events: InternalEventSink,
    root_cancel: CancellationToken,
    active: Option<ActiveTurn>,
    health: SessionHealth,
}
```

Actor loop 必须同时驱动：

```text
root cancellation
active runner critical events
active runner exit
public command mailbox
```

优先级要求：

1. root cancellation 不能被满 command queue 饿死；
2. Runner 的 commit/suspend event 不能被持续 transcript/answer/submit command 饿死；
3. command 处理不得在 actor 内执行 Model/Tool 等长任务；
4. 所有 `SessionLog` 操作仍由 actor 串行 await；
5. runner channel 意外关闭且无 Finish 时按 internal runner failure settlement；
6. actor 不持有跨 await 的标准互斥锁 guard。

可以使用带明确优先级的 `tokio::select!`，也可以手写 bounded-drain loop；测试必须覆盖 command flood 下 commit 和 shutdown 仍能推进。

### 16.3 ActiveTurn

```rust
pub(crate) struct ActiveTurn {
    turn_id: TurnId,
    cancellation: CancellationToken,
    completion: SharedTurnCompletion,
    effective: EffectiveTurnOptions,
    phase: ActiveTurnPhase,
    pending_interaction: Option<PendingInteractionState>,
    runner_events: mpsc::Receiver<RunnerEvent>,
    runner: Option<JoinHandle<TurnRunnerExit>>,
}
```

`ActiveTurnPhase` 仅 internal，不成为 public SessionStatus：

```rust
pub(crate) enum ActiveTurnPhase {
    Starting,
    RunningModel,
    RunningTool,
    WaitingForInput,
    Settling,
}
```

### 16.4 Submit 顺序

`SessionActor::handle_submit` 必须执行：

1. health 必须 Healthy；
2. public status 必须 Idle；
3. reply receiver 仍打开；
4. 校验 UserInput；
5. 计算 effective model/reasoning/budget；
6. 校验当前绑定 Model 的 descriptor/capability；
7. 生成 TurnId 和 exact cancellation token；
8. 构造 `UserMessageDraft`；
9. 由 `ConversationLog` 分配 seq 并 durable append UserMessage；
10. 创建 SharedTurnCompletion；
11. 将 state 设置 Running；
12. 发送 TurnHandle；
13. 若发送失败，立即触发 cancellation 并进入 settlement；
14. publish best-effort TurnStarted；
15. spawn TurnRunner。

在 UserMessage durable 前不得发布 Running/TurnStarted。

### 16.5 TurnRunner 输入

```rust
pub(crate) struct TurnRunnerRequest {
    session_id: SessionId,
    instance_id: SessionInstanceId,
    turn_id: TurnId,
    spec: SessionSpec,
    effective: EffectiveTurnOptions,
    bindings: SessionBindings,
    projection: PromptProjection,
    cancellation: CancellationToken,
    critical_tx: mpsc::Sender<RunnerEvent>,
    events: InternalEventSink,
}
```

Runner 不拥有 SessionLog，不直接 append Conversation。

### 16.6 RunnerEvent

```rust
pub(crate) enum RunnerEvent {
    CommitAssistant {
        draft: AssistantMessageDraft,
        reply: oneshot::Sender<Result<CommitAck, RunnerCommitError>>,
    },
    CommitToolResult {
        draft: ToolResultDraft,
        reply: oneshot::Sender<Result<CommitAck, RunnerCommitError>>,
    },
    CommitSummary {
        draft: SummaryDraft,
        reply: oneshot::Sender<Result<CommitAck, RunnerCommitError>>,
    },
    Suspend {
        suspension: TurnSuspension,
    },
    Finish {
        outcome: RunnerOutcome,
    },
}

pub(crate) struct CommitAck {
    pub head: ConversationSeq,
    pub projection: Arc<PromptProjection>,
}
```

所有会影响下一轮 Model Context 的 entry 必须得到 actor durable ack 后，Runner 才能继续。

### 16.7 Agent Loop

```text
build Context
→ build Prompt
→ use bound Model
→ stream Model response
→ emit provisional delta Event
→ validate complete response
→ request actor durable AssistantMessage commit
→ receive CommitAck and replace local PromptProjection
→ if final without tools: Finish Completed
→ if tools: sequential tool round
→ request actor durable ToolResult commits
→ receive CommitAck after every result
→ when compaction is needed, request actor durable Summary commit
→ next Context/Model round
```

每一轮检查：

- cancellation；
- wall-time deadline；
- model attempt budget；
- tool call budget；
- max tool rounds；
- Session health response from actor ack。

### 16.8 Tool round 顺序

v0.3 保持 v0.2 的确定性顺序执行：

- 同一 AssistantMessage 的 ToolCall 按返回顺序；
- 不在 Core 中自动并行 ToolCall；
- 一个 call suspend 时，后续 call 暂停；
- interaction resolved 后从内存 continuation 继续剩余 calls；
- 每个 result durable 后才执行下一个 call；
- cancellation 时对全部 unresolved calls 生成 cancelled ToolResult。

### 16.9 execute_one_tool

顺序：

1. 查找 enabled Tool；
2. validate arguments；
3. 调用 ToolPolicy；
4. Deny/error/timeout：生成 denied ToolResult；
5. RequireApproval：创建 in-memory suspension；
6. Allow：发布 ToolStarted best-effort；
7. 构造 ToolContext；
8. 执行 Tool，catch panic + deadline + cancellation；
9. Completed：验证 output，生成 ToolResult；
10. RequestInput：创建 in-memory suspension；
11. actor durable commit result；
12. durable ack 后发布 ToolFinished best-effort。

### 16.10 Terminal settlement

只有 `SessionActor::settle_turn` 可以 append TurnTerminal。

输入来源：

```text
Runner Completed
Runner Failed
user cancel
shutdown
budget exceeded
actor/log failure
interaction cancel
runner panic
```

顺序：

1. 停止/等待 runner；
2. 为 unresolved ToolCall 构造 cancelled/failed result drafts；
3. 构造唯一 `TurnTerminalDraft`；
4. 将 missing ToolResults 与 terminal 作为一个 settlement batch 调用 `append_validated`；
5. 只有整个 batch durable 后更新 Conversation projection；
6. 更新 SessionState：Idle 或 Degraded/Closing；
7. 完成 SharedTurnCompletion；
8. publish TurnFinished best-effort；
9. 清除 active。

TurnHandle 不能在 terminal commit 前完成。

如果 settlement batch commit 失败：

- 不发布 `TurnFinished`；
- 不构造不存在的 durable terminal；
- 已知未提交返回 `DurabilityUnavailable`，未知结果返回 `DurabilityUnknown`；
- SessionState 清除 active/pending，转为 `Idle + Degraded`（shutdown 路径保持 `Closing + Degraded`）；
- `conversation_seq` 保持最后一个已确认 head；
- 下次重新打开 log 后，由 restart recovery 修复 durable open Turn。
---

## 17. Interaction：仅进程内挂起，不跨重启恢复

### 17.1 Public 类型只定义一次

Public 类型统一使用 13.10～13.12 定义的：

```text
ApprovalRequest
ApprovalDecision
ToolInputRequest
ToolInputAnswer
PendingInteraction
InteractionKind
InteractionAnswer
```

不得在 `session/interaction.rs`、`tools/types.rs` 和 `agent/runner_protocol.rs` 分别定义语义相同的第二套类型。推荐归属：

```text
session/interaction.rs：PendingInteraction、InteractionKind、InteractionAnswer

tools/policy.rs：ApprovalRequest、ApprovalDecision

tools/types.rs：ToolInputRequest、ToolInputAnswer
```

### 17.2 Internal suspension protocol

`ToolPolicy::RequireApproval` 或 `ToolExecutionOutcome::RequestInput` 出现后，Runner 可以挂起自身，但不保留任意 Tool future。

在 `src/agent/runner_protocol.rs` 定义：

```rust
pub(crate) struct TurnSuspension {
    pub turn_id: TurnId,
    pub tool_call_id: ToolCallId,
    pub tool_name: ToolName,
    pub kind: InteractionKind,
    pub resume: oneshot::Sender<Result<InteractionAnswer, SuspensionError>>,
}
```

Actor 保存：

```rust
pub(crate) struct PendingInteractionState {
    pub public: PendingInteraction,
    pub resume: oneshot::Sender<Result<InteractionAnswer, SuspensionError>>,
}

pub(crate) enum SuspensionError {
    Cancelled,
    DeadlineExceeded,
    StaleTurn,
    InvalidState,
    RuntimeClosed,
}
```

Runner 创建 `(resume_tx, resume_rx)`，通过 critical `RunnerEvent::Suspend` 发送 `resume_tx`，然后使用 `tokio::select!` 等待：

```text
resume_rx
Turn cancellation
runner deadline
actor channel closed
```

禁止：

```text
保存 Tool future
保存任意 closure
保存 Any/opaque continuation
将 resume sender 暴露给 Host/UI
将 pending interaction 持久化
```

### 17.3 Actor 注册 suspension

`SessionActor::handle_runner_suspend` 必须：

1. 验证 event 属于当前 `active.turn_id`；
2. 验证当前 phase 允许 suspension；
3. 验证不存在另一个 pending interaction；
4. 生成当前 `SessionInstanceId` 内唯一的 `InteractionId`；
5. 构造 immutable `PendingInteraction`；
6. 将 `PendingInteractionState` 放入 `ActiveTurn`；
7. 将 public state 更新为 `WaitingForInput`；
8. `send_replace` state 完成后，再 best-effort 发布 `InteractionRequested`；
9. actor 继续处理 Answer、Transcript、root cancellation 和 runner exit，不阻塞 actor loop。

若注册失败，actor 必须通过 `resume` 返回 typed `SuspensionError`，不得让 Runner 永久等待。

### 17.4 Answer 流程

`SessionActor::handle_answer`：

1. Session 必须处于 `WaitingForInput`；
2. `interaction_id` 必须等于当前 pending id；
3. answer variant 必须匹配 `InteractionKind`；
4. 文本、choice index 和大小必须通过 validation；
5. atomically take `PendingInteractionState`，防止重复回答；
6. state 先改为 `Running` 并清除 pending；
7. 将 typed answer 发送给 `resume` channel；
8. 发送失败表示 Runner 已结束，actor 转入 settlement，不恢复 pending；
9. best-effort 发布 `InteractionResolved`；
10. command reply 返回成功或明确的 stale/closed 错误。

用户回答不会直接修改 Conversation；Runner 根据回答生成对应 ToolResult，再走 actor durable commit。

### 17.5 Tool approval 流程

```text
Assistant ToolCall 已 durable
→ ToolPolicy::decide
→ RequireApproval
→ RunnerEvent::Suspend(Approval)
→ actor state WaitingForInput
→ SessionHandle::answer(ApprovalDecision)
→ AllowOnce：Runner 执行冻结的 exact ToolInvocation
→ Deny：Runner 构造 Denied ToolResult
→ actor durable commit ToolResult
→ 继续 tool round/model round
```

约束：

- arguments、ToolCallId、ToolName 在等待期间不可修改；
- `AllowOnce` 仅对当前 exact ToolCall 有效；
- 不支持字符串 `"yes"` / `"allow"`；
- `AllowForSession` 等持久策略状态由外部 stateful ToolPolicy 管理，不由 Core interaction 返回。

### 17.6 Tool input 流程

```text
Tool returns RequestInput
→ Tool future 已结束
→ RunnerEvent::Suspend(ToolInput)
→ actor state WaitingForInput
→ 用户提交 ToolInputAnswer
→ Runner 使用固定 canonical JSON 编码答案
→ outcome = InputProvided
→ actor durable commit ToolResult
→ 继续剩余 ToolCall 或下一 Model round
```

Core 不再次调用原 Tool。Tool 在返回 `RequestInput` 前不得执行依赖该答案的不可逆副作用。

### 17.7 Cancel、shutdown 与 runner exit

等待 interaction 时：

- `TurnHandle::cancel` 取消 exact Turn token；
- Runner 的 `select!` 立即结束等待；
- actor 收到 Runner exit 后移除 pending interaction；
- 当前 unresolved ToolCall 生成 Cancelled ToolResult；
- user cancel terminal 为 `CancelledByUser`；
- owner shutdown terminal 为 `CancelledByShutdown`；
- actor shutdown 时若仍持有 resume sender，向其发送 `SuspensionError::Cancelled` 后释放。

任何路径都不得留下等待中的 oneshot sender 或 runner task。

### 17.8 Restart 语义

进程退出时 pending interaction 丢失。

下次 load：

- 从 durable Conversation 发现 unresolved ToolCall；
- 生成 Cancelled ToolResult；
- append `CancelledByRestart` terminal；
- 状态回到 Idle；
- 不发布旧 `InteractionRequested`；
- 用户不能回答旧 InteractionId。

这是明确产品契约，不得在 v0.3 中实现 interaction resume。

---

## 18. 多 SessionRuntime 并发支持

### 18.1 Core 的支持目标

虽然 Core 不管理多个 Session，但必须允许 Host 在同一进程中安全创建多个：

```rust
let a = SessionRuntime::load(...).await?;
let b = SessionRuntime::load(...).await?;

let turn_a = a.handle().submit(...).await?;
let turn_b = b.handle().submit(...).await?;

let (result_a, result_b) = tokio::join!(turn_a.wait(), turn_b.wait());
```

### 18.2 必须避免的全局状态

Production Core 禁止：

- global loaded Session map；
- static mutable provider registry；
- global cancellation token；
- global event sender；
- global Conversation lock；
- process-wide runtime lock；
- hidden singleton executor；
- 所有 Session 共享一个 command actor。

### 18.3 可共享能力

Host 可以共享：

```text
Arc<dyn Model>
Arc<dyn ToolPolicy>
Arc<dyn ContextProvider>
Arc<dyn CompactionStrategy>
Arc<dyn Tool>（若实现并发安全）
```

Core 不复制大型 client；`SessionBindings` 主要持有 `Arc`。

### 18.4 不可共享能力

每个 SessionRuntime 必须独立：

```text
SessionLog
SessionInstanceId
SessionActor
command mailbox
state watch sender
EventStream sender
root cancellation
Conversation projection
active Turn
Turn completion
```

### 18.5 Workspace 冲突不由 Core 解决

两个 SessionRuntime 的 Tool 可以捕获同一 Workspace，但并发写冲突属于 Host/Workspace adapter。

Core 不实现：

```text
WorkspaceLeaseManager
file lock
worktree allocator
git index lock
```

### 18.6 全局限流不由 Core 解决

Host 或共享 Model decorator 可以使用 semaphore：

```rust
struct LimitedModel {
    semaphore: Arc<Semaphore>,
    inner: Arc<dyn Model>,
}
```

`LimitedModel` 实现普通 `Model` Port；Core 只执行每 Session 的单 Turn 和预算限制。

### 18.7 stale instance 防护

- 每次 create/load 生成随机 `SessionInstanceId`；
- SessionHandle/TurnHandle/Event/State 均带 instance id；
- shutdown 后旧 command sender 关闭；
- Host 重新 load 同一 SessionId 后，旧 event task 可通过 instance id 丢弃迟到数据；
- 不使用 `Arc::ptr_eq` 作为 public identity。

---

## 19. Error、Diagnostic 与失败边界

### 19.1 Public operation errors

公开错误按操作分层：

```text
SessionOpenError
SessionError
TurnWaitError
SessionShutdownError
SessionLogError
ModelError
ToolError
ToolPolicyError
ContextError
CompactionError
```

`SessionError` 只用于 loaded Session 控制操作，例如 Submit、Answer、Transcript；不要恢复一个覆盖多 Session 管理的 `RuntimeError`。

最低 public variant 集合：

```rust
#[non_exhaustive]
pub enum SessionOpenError {
    InvalidConfiguration(DiagnosticSummary),
    InvalidManifest(DiagnosticSummary),
    SessionIdMismatch { expected: SessionId, actual: SessionId },
    BindingMismatch(DiagnosticSummary),
    Log(SessionLogError),
    RecoveryUncertain(DiagnosticSummary),
    ActorStartFailed(DiagnosticSummary),
}

#[non_exhaustive]
pub enum SessionError {
    Closed,
    Busy { active_turn: TurnId },
    Degraded(DiagnosticSummary),
    Backpressure,
    InvalidInput(DiagnosticSummary),
    InteractionNotFound,
    InteractionKindMismatch,
    InteractionAlreadyResolved,
    TranscriptUnavailable(DiagnosticSummary),
}

#[non_exhaustive]
pub enum TurnWaitError {
    DurabilityUnknown(DiagnosticSummary),
    DurabilityUnavailable(DiagnosticSummary),
    RuntimeTerminated(DiagnosticSummary),
}

#[non_exhaustive]
pub enum SessionShutdownError {
    Timeout(DiagnosticSummary),
    Durability(DiagnosticSummary),
    LogClose(DiagnosticSummary),
    ActorTerminated(DiagnosticSummary),
}
```

Public error 可以是 struct + kind，而不一定机械采用上述 enum 形态；但可观察语义和区分度不得降低。

### 19.2 `DiagnosticSummary`

```rust
pub struct DiagnosticSummary {
    pub code: DiagnosticCode,
    pub category: DiagnosticCategory,
    pub message: BoundedText,
    pub retryable: bool,
}
```

```rust
#[non_exhaustive]
pub enum DiagnosticCategory {
    Configuration,
    Model,
    Tool,
    Policy,
    Context,
    Compaction,
    Storage,
    Cancellation,
    Internal,
}
```

### 19.3 稳定错误码

至少定义：

```text
invalid_configuration
invalid_session_manifest
session_closed
session_busy
session_degraded
command_backpressure
interaction_not_found
interaction_kind_mismatch
model_mismatch
model_timeout
model_malformed_response
model_unavailable
context_failed
policy_denied
policy_failed
tool_not_found
tool_timeout
tool_failed
turn_budget_exceeded
log_conflict
log_corrupt
log_unknown_outcome
runtime_terminated
shutdown_timeout
```

### 19.4 错误与 durable 结果

| 错误发生点 | durable 行为 |
|---|---|
| Submit 前 validation | 不写 Conversation |
| UserMessage append failure | 不开始 Turn，不返回正常 TurnHandle |
| Model failure | append Failed TurnTerminal |
| Tool failure | append Failed ToolResult，通常继续下一轮 Model |
| Policy deny/fail closed | append Denied ToolResult |
| Context failure | append Failed TurnTerminal |
| forced compaction failure | append Failed TurnTerminal |
| user cancel/shutdown | 补 cancelled ToolResult 后 append对应 Cancelled terminal |
| active Turn append known failure | 不伪造 terminal；Session Degraded；Turn waiter 返回 DurabilityUnavailable；reload repair |
| SessionLog UnknownOutcome | 不猜测；Session Degraded；Turn waiter返回 DurabilityUnknown |
| actor/internal task panic | 尽力停止；不得声称不存在的 durable terminal |

### 19.5 Raw source error 与敏感数据

- raw adapter error 只进入 internal tracing；
- Public error 不暴露 credential、完整 prompt、HTTP body、环境变量或绝对路径；
- Tool/Model error message 必须 bounded；
- `Debug` 不打印 Port trait object 内部；
- SessionLog adapter 应自行清洗数据库连接串和物理路径。

### 19.6 Panic isolation

必须隔离：

```text
Model
Tool
ToolPolicy
ContextProvider
CompactionStrategy
SessionLog
```

建议：

- Model/Tool/Policy/Context/Compaction future 在 runtime-owned child task 或 `catch_unwind` wrapper 中执行；
- panic 映射 typed error；
- SessionLog panic/timeout按 UnknownOutcome 处理，Session Degraded；
- Core 自身 panic 仍是 bug，但 owner Drop/shutdown 不得遗留永久 task owner。

---

## 20. Extension 形式、Hook 政策与安全边界

### 20.1 Extension 形式

默认扩展方式：

```text
Rust Trait
+ checked request/response DTO
+ constructor injection
+ external crate
```

未来外部实现示例：

```text
minicore-provider-openai
minicore-provider-anthropic
minicore-store-jsonl
minicore-store-sqlite
minicore-tools-fs
minicore-tools-process
minicore-context-project
minicore-mcp
```

本规格不创建这些 crate。

### 20.2 不实现 Plugin Manager

Core 不实现：

```text
plugin discovery
install/uninstall
runtime hot reload
dylib ABI
WASI host
plugin child-process lifecycle
RPC schema negotiation
```

Host 以后可将动态插件或 MCP server 包装成普通 `Tool`、`Model` 或 `ContextProvider`。

### 20.3 不提供万能 Hook

禁止：

```text
BeforeTurn
AfterTurn
BeforeModel
AfterModel
BeforeTool
AfterTool
on_everything
```

需求映射：

| 需求 | 正确机制 |
|---|---|
| 模型前加入项目规则 | `ContextProvider` |
| 工具前权限 | `ToolPolicy` |
| 模型统计 | Model decorator / SessionEvent |
| 工具统计 | Tool decorator / SessionEvent |
| Store audit | SessionLog decorator |
| Tool 参数变换 | Tool wrapper |
| 全局配额和并发 | Host/shared adapter |
| 单 Turn deadline/tool-round 限制 | `TurnOptions` + `SessionSpec` |

### 20.4 不提供 TurnAdmissionPolicy

v0.3 不增加 TurnAdmissionPolicy：

- Host 是多 Session admission owner；
- 单 Session Core 已有 Busy、Healthy、bounds、deadline 和 max tool rounds；
- 租户额度、全局并发、维护模式属于 Host；
- 避免增加 Hook-like Port。

### 20.5 Capability 边界

Tool/Context/Model 在构造时捕获能力：

- ReadFileTool 只访问其持有的 Workspace；
- ProcessTool 只访问其持有的 ProcessExecutor；
- RemoteAgentTool 只访问其持有的 RPC client；
- Model 只访问其持有的 credential/HTTP client；
- ContextProvider 只访问其持有的 project source。

Core 不向 Port 注入 Host、SessionRuntime、SessionHandle、Service Locator 或任意 capability map。

---

## 21. IDs、时间与序列

### 21.1 Public IDs

保留/新增：

```text
SessionId              durable
SessionInstanceId      per load, non-durable
TurnId                 durable in Conversation
ToolCallId             durable in Conversation
InteractionId          process-local, non-durable
ConversationSeq        durable log sequence
ModelRef                durable in SessionManifest/Conversation metadata
ToolName
ContextSourceId
```

EventStream 不需要 durable cursor、ObservationEpoch 或 EventSequence 协议。

### 21.2 ID 生成

- SessionId 通常由 Host 在创建 Repository record 时生成；Core 只校验；
- SessionInstanceId 每次 create/load 由 Core 生成；
- TurnId/ToolCallId 由 Core 生成或验证唯一；
- InteractionId 只要求当前 SessionInstance 唯一；
- 所有 parser 有长度和字符集边界；
- Debug/Display 不泄露业务 payload。

### 21.3 时间

- durable DTO 使用 serializable `Timestamp`；
- internal timeout/deadline 使用 `Instant`；
- 不持久化 `Instant`；
- time source 集中封装，测试使用 paused Tokio time 或 internal test clock；
- Clock 不作为 public Extension Port。

### 21.4 Checked value

新增 `src/value.rs`，集中定义不可变的 checked value：

```rust
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct BoundedText(String);
```

契约：

- checked constructor 同时执行 UTF-8 byte size 和调用点 `SemanticLimits` 检查；
- serde deserialize 至少执行一个 crate 级 absolute hard cap，之后 Conversation/Config validator 再执行用途相关 limit；
- 不实现 `DerefMut` 或暴露可变内部 String；
- 提供 `as_str()`、`into_string()`；
- Model delta assembler、Tool output、Context block、policy prompt、diagnostic 都通过同一检查入口；
- Tool arguments 的 `serde_json::Value` 使用集中 `validate_json_size`，不得每个模块自行序列化计算不同边界。

---

## 22. Cargo.toml 与依赖清理

### 22.1 Package

```toml
[package]
name = "minicore-runtime"
version = "0.3.0"
edition = "2024"
rust-version = "1.85"
```

### 22.2 必须删除的 production dependency

基于 v0.2：

```text
cap-std
cap-primitives
fs4
reqwest
```

### 22.3 Tokio feature 清理

删除 production 不再使用的：

```text
process
io-util
macros   # library production 若无使用
```

测试所需 `rt-multi-thread`、`test-util` 留在 dev-dependencies。

### 22.4 预期保留

可保留：

```text
getrandom
serde
serde_json
thiserror
time
tokio: rt, sync, time
tokio-util: rt
futures-util
```

每个 normal dependency 必须对应 Core 语义职责。

### 22.5 Lint

继续：

```toml
[lints.rust]
unsafe_code = "forbid"

[lints.clippy]
await_holding_lock = "deny"
await_holding_invalid_type = "deny"
```

Architecture script 额外禁止 production `std::fs`、`std::process`、`tokio::process`、`reqwest` 和 blocking I/O。

---

## 23. `src/lib.rs` Public Surface

### 23.1 推荐 export

```rust
pub mod compaction;
pub mod config;
pub mod context;
pub mod conversation;
pub mod error;
pub mod ids;
pub mod model;
pub mod session;
pub mod storage;
pub mod tools;
pub mod value;

pub use value::BoundedText;

pub use config::{
    CompactionConfig,
    KernelConfig,
    RetryPolicy,
    SemanticLimits,
    SessionManifest,
    SessionSpec,
    TurnOptions,
    UserInput,
};

pub use conversation::{
    ConversationEntry,
    ConversationSeq,
    TranscriptPage,
    TurnTerminal,
};

pub use ids::{
    InteractionId,
    SessionId,
    SessionInstanceId,
    ToolCallId,
    TurnId,
};

pub use session::{
    InteractionAnswer,
    InteractionKind,
    PendingInteraction,
    SessionEvent,
    SessionEventEnvelope,
    SessionEventStream,
    SessionHandle,
    SessionHealth,
    SessionRuntime,
    SessionRuntimeOptions,
    SessionState,
    SessionStatus,
    TurnHandle,
    TurnOutcome,
};

pub use storage::{
    AppendReceipt,
    ConversationPage,
    SessionLog,
    SessionLogError,
};

pub use tools::{
    ApprovalDecision,
    ApprovalRequest,
    Tool,
    ToolDecision,
    ToolExecutionOutcome,
    ToolInputAnswer,
    ToolInputRequest,
    ToolPolicy,
    ToolSet,
};
```

具体 Port traits 从其模块公开：

```text
model::Model
tools::Tool / ToolPolicy / ToolSet
context::ContextProvider
compaction::CompactionStrategy
storage::SessionLog
```

### 23.2 不得 export

```text
SessionActor
ActiveTurn
ConversationLog
RunnerEvent
SessionCommand
TurnRunnerRequest
JoinHandle
channel sender
internal validator/state
provider transport
workspace
concrete tool/store/model
```

### 23.3 Enum 演进

观察型 enum 可以 `#[non_exhaustive]`：

```text
SessionEvent
DiagnosticCategory
ModelFinishReason
```

ConversationEntry、SessionStatus、InteractionAnswer 等语义变化在 0.x 阶段视为 breaking。

---

## 24. 逐文件修改方案

本节按 v0.2 当前文件和 symbol 描述。若实际 HEAD 有轻微差异，按职责和类型定位，不得因为文件名不同而保留旧架构。

### 24.1 `Cargo.toml`

修改：

- package version 改为 `0.3.0`；
- 保持 edition 2024、Rust 1.85；
- 删除 `cap-std`、`cap-primitives`、`fs4`、`reqwest`；
- 删除 Tokio `process` 和不再使用的 `io-util` production feature；
- `rt-multi-thread`、`test-util` 只放 dev-dependencies；
- 保持 `unsafe_code = "forbid"` 和现有 Clippy deny；
- 运行 `cargo tree -e normal`，确认无 concrete HTTP/filesystem/process dependency。

### 24.2 `src/lib.rs`

删除 public export：

```text
Runtime
RuntimeConfig
RuntimeClient
SessionManager
SessionSnapshot
ProviderRegistry
concrete providers
builtin tools
Workspace
WorkspaceAccess
```

新增/保留 public export：

```text
SessionRuntime
SessionRuntimeOptions
SessionHandle
TurnHandle
KernelConfig
SessionManifest
SessionSpec
SessionBindings
SessionState
SessionEventStream
SessionLog
Model
Tool
ToolPolicy
ContextProvider
CompactionStrategy
```

`agent`、actor、runner protocol、ConversationLog 等保持 private。

### 24.3 `src/config.rs`

删除：

```text
RuntimeConfig
data_dir
provider registry
tool registry
workspace_root
global coding instructions
```

新增/重写：

```text
KernelConfig
SemanticLimits
SessionSpec
SessionManifest
TurnOptions
CompactionConfig
RetryPolicy
```

必须提供：

```rust
impl KernelConfig {
    pub fn new(...) -> Result<Self, ConfigError>;
    pub fn validate(&self) -> Result<(), ConfigError>;
}

impl SessionSpec {
    pub fn new(...) -> Result<Self, ConfigError>;
    pub fn validate(&self, limits: &SemanticLimits) -> Result<(), ConfigError>;
}

impl SessionManifest {
    pub const FORMAT_VERSION: u32 = 3;
    pub fn validate(&self, limits: &SemanticLimits) -> Result<(), ConfigError>;
}
```

所有 validation 为纯函数，无 I/O。

### 24.4 新增/重写 `src/session/bindings.rs`

定义：

```rust
#[derive(Clone)]
pub struct SessionBindings {
    pub model: Arc<dyn Model>,
    pub tools: ToolSet,
    pub tool_policy: Option<Arc<dyn ToolPolicy>>,
    pub context: Option<Arc<dyn ContextProvider>>,
    pub compaction: Option<Arc<dyn CompactionStrategy>>,
}
```

提供：

```rust
impl SessionBindings {
    pub fn new(...) -> Self;

    pub fn validate(
        &self,
        spec: &SessionSpec,
        limits: &SemanticLimits,
    ) -> Result<(), SessionBindingError>;
}
```

Validation：

- bound model_ref 与 SessionSpec.model 相等；
- model 支持 reasoning；
- enabled tool 全部存在；
- enabled tools 非空时 policy 必须存在；
- compaction enabled 时 strategy 必须存在；
- ToolSpec 名称/schema/描述满足 limits。

Bindings 不 Serialize，不允许 loaded 后替换。

### 24.5 新增/重写 `src/session/runtime.rs`

实现 public `SessionRuntimeOptions`：

```rust
pub struct SessionRuntimeOptions {
    kernel: KernelConfig,
    bindings: SessionBindings,
    task_runtime: tokio::runtime::Handle,
}
```

方法：

```rust
pub fn new(
    kernel: KernelConfig,
    bindings: SessionBindings,
    task_runtime: Handle,
) -> Result<Self, SessionOpenError>;
```

实现 `SessionRuntime`：

```rust
pub async fn create(
    session_id: SessionId,
    spec: SessionSpec,
    log: Box<dyn SessionLog>,
    options: SessionRuntimeOptions,
) -> Result<Self, SessionOpenError>;

pub async fn load(
    expected_session_id: SessionId,
    log: Box<dyn SessionLog>,
    options: SessionRuntimeOptions,
) -> Result<Self, SessionOpenError>;

pub fn session_id(&self) -> SessionId;
pub fn instance_id(&self) -> SessionInstanceId;
pub fn handle(&self) -> SessionHandle;
pub fn take_events(&mut self) -> Result<SessionEventStream, EventStreamTakenError>;
pub async fn shutdown(self) -> Result<(), SessionShutdownError>;
```

内部 helper：

```rust
async fn open_new(...);
async fn open_existing(...);
async fn spawn_actor(...);
async fn cleanup_failed_open(...);

struct OpenGuard { /* cancel + task until disarmed */ }
```

要求：

- owner 不 Clone；
- actor ready handshake 后才返回；
- create/load future 被取消时 OpenGuard 必须取消已 spawn 的 actor；
- failed open 关闭 log；
- shutdown 消耗 owner；
- Drop 只 cancel，不 block、不 forget。

### 24.6 删除 `src/runtime/**`

全部删除：

```text
Runtime
RuntimeInner
RuntimeClient
RuntimeSupervisor
SessionManager
loaded session map
load reservation
runtime-level Store ownership
runtime-level shutdown_all
```

不得留下同名 wrapper 转发到 SessionRuntime。

Host 的 `HashMap<SessionId, SessionRuntime>` 不在本 crate 实现。

### 24.7 `src/session/handle.rs`

实现：

```rust
pub fn session_id(&self) -> SessionId;
pub fn instance_id(&self) -> SessionInstanceId;
pub async fn submit(...) -> Result<TurnHandle, SessionError>;
pub async fn answer(...) -> Result<(), SessionError>;
pub fn state(&self) -> SessionState;
pub fn watch_state(&self) -> watch::Receiver<SessionState>;
pub async fn transcript(...) -> Result<TranscriptPage, SessionError>;
```

实现细节：

- 持有 bounded command sender、watch receiver、IDs；
- 使用 `try_send` 或有界 admission，queue full 返回 Backpressure；
- 不直接调用 Model、Tool、SessionLog；
- channel closed 返回 Closed；
- Debug 不打印内部 sender。

### 24.8 `src/session/turn_handle.rs`

定义内部 shared completion，例如：

```rust
pub(crate) enum TurnCompletionState {
    Running,
    Finished(Arc<TurnOutcome>),
    DurabilityUnknown(Arc<DiagnosticSummary>),
    RuntimeTerminated,
}
```

实现：

```rust
pub fn cancel(&self) -> bool;
pub fn is_finished(&self) -> bool;
pub async fn wait(&self) -> Result<TurnOutcome, TurnWaitError>;
```

要求：

- exact cancellation token；
- 多 clone waiter；
- drop 不 cancel；
- terminal durable 后 publish Finished；
- stale handle 不连接新 instance。

### 24.9 `src/session/actor.rs`

重写 private state：

```rust
pub(crate) struct SessionActor {
    session_id: SessionId,
    instance_id: SessionInstanceId,
    config: KernelConfig,
    spec: SessionSpec,
    bindings: SessionBindings,
    conversation: ConversationLog,
    commands: mpsc::Receiver<SessionCommand>,
    state_tx: watch::Sender<SessionState>,
    events: InternalEventSink,
    root_cancel: CancellationToken,
    active: Option<ActiveTurn>,
    health: SessionHealth,
}
```

删除：

```text
Workspace
Runtime/SessionManager back-reference
Snapshot publisher
broadcast sender
hidden unavailable bool
InteractionClient
```

必须实现：

```rust
async fn run(mut self) -> SessionActorExit;
async fn handle_idle_command(&mut self, command: SessionCommand);
async fn handle_active_command(&mut self, command: SessionCommand);
async fn handle_runner_event(&mut self, event: RunnerEvent);
async fn handle_submit(...);
async fn settle_turn(...);
async fn begin_shutdown(...);
fn publish_state(&mut self, next: SessionState);
fn emit_best_effort(&mut self, event: SessionEvent);
fn mark_degraded(&mut self, diagnostic: DiagnosticSummary);
```

所有 durable append 只通过 `ConversationLog`。`ActiveTurn` 必须持有 per-turn runner event receiver；actor loop 同时 select root cancellation、runner events/exit 和 public commands，且不得让 command flood 饿死 commit 或 shutdown。

### 24.10 `src/session/command.rs`

目标：

```rust
pub(crate) enum SessionCommand {
    Submit { input, options, reply },
    Answer { interaction_id, answer, reply },
    Transcript { after, limit, reply },
}
```

删除：

```text
Snapshot
Subscribe
Load/Delete Session
ReplaceBindings
Runtime-level close
```

Turn cancel 和 owner shutdown 不走 command queue。

### 24.11 `src/session/state.rs`

定义：

```text
SessionStatus
SessionHealth
SessionState
StateInvariantError
```

集中方法验证：

```text
Idle => no active turn/no pending interaction
Running => active turn/no pending interaction
WaitingForInput => active turn + pending interaction
Closing => no new submit
Degraded => no new submit
```

v0.3 不在 `SessionState` 中保存 active Model/Tool activity；实时 activity 只通过 best-effort Event 表达。

删除 hidden `unavailable`。

### 24.12 删除 `src/session/snapshot.rs`

删除全部 Snapshot 类型、builder、history 和 test。

必要字段移入 `SessionState`。

### 24.13 `src/session/event.rs`

定义：

```text
SessionEventEnvelope
SessionEvent
OutputChannel
summary DTO
```

Envelope 只包含：

```text
SessionId
SessionInstanceId
SessionEvent
```

TurnId 保存在需要它的具体 `SessionEvent` variant 中，不在 envelope 重复维护。

删除：

```text
ResyncRequired
Snapshot frame
Observation cursor/epoch/revision
Runtime event
Subagent event
```

### 24.14 `src/session/event_stream.rs`

将 broadcast 改为 bounded mpsc single receiver。

实现：

```rust
pub async fn recv(&mut self) -> Option<SessionEventEnvelope>;
pub fn try_recv(&mut self) -> Result<SessionEventEnvelope, TryRecvError>;
```

可实现 `Stream`，不实现 Clone/subscribe。

内部 `InternalEventSink`：

- `try_send`；
- queue full 累计 dropped count；
- 下一次有空间时尝试发 EventsDropped；
- 不阻塞 actor/runner。

### 24.15 `src/session/interaction.rs`

定义：

```text
PendingInteraction
InteractionKind
InteractionAnswer
ApprovalRequest
ApprovalDecision
ToolInputRequest
ToolInputAnswer
```

删除：

```text
InteractionClient
resume token
durable interaction entry
string approval
```

所有值 bounded；answer kind 必须匹配。

### 24.16 `src/session/transcript.rs`

可保留文件位置或移到 `conversation/transcript.rs`。

要求：

- 只返回 durable ConversationEntry；
- after/limit checked；
- 不返回 Event/delta；
- adapter page 与内存 projection head 一致；
- max page size 来自 SemanticLimits。

### 24.17 `src/agent/runner.rs`

保留 `agent` 命名或重命名 `turn` 均可；本规格以现有 `agent` 降低 rename 噪音。

重写：

```rust
async fn run_turn(request: TurnRunnerRequest) -> TurnRunnerExit;
async fn run_model_round(...);
async fn execute_tool_round(...);
async fn execute_one_tool(...);
async fn wait_for_interaction(...);
```

变化：

- 不拥有 SessionLog；
- 不直接 append Conversation；
- 不直接完成 TurnHandle；
- 不持有 Workspace；
- 不调用 ToolContext::ask_user；
- Model 直接来自 bindings.model；
- 通过 RunnerEvent 请求 actor commit/suspend/finish；
- sequential ToolCall；
- cancellation/deadline/max rounds。

### 24.18 新增/重写 `src/agent/runner_protocol.rs`

定义：

```text
RunnerEvent
CommitAck
RunnerCommitError
TurnSuspension
RunnerOutcome
TurnRunnerExit
```

`CommitAssistant`、`CommitToolResult`、`CommitSummary` 都传递 unsequenced draft 并使用 oneshot ack；Runner 收到 durable `CommitAck { head, projection }` 后才继续。

`TurnSuspension` 持有唯一 resume sender；actor 将其保存在 `PendingInteractionState`，Answer/cancel/shutdown 只能消费一次。Runner 等待 resume 时必须同时监听 Turn cancellation 和 deadline。

Critical event 必须 await；delta/progress 通过 InternalEventSink best effort。

### 24.19 `src/agent/turn_context.rs`

删除：

```text
Workspace
InteractionClient
Runtime reference
```

包含：

```text
session/instance/turn IDs
SessionSpec/Bindings clone
PromptProjection
CancellationToken
deadline/max tool rounds
runner protocol sender
event sink
```

### 24.20 `src/agent/retry.rs`

仅处理 Model retry。

实现：

```rust
fn classify_model_retry(error: &ModelError, attempt: u32, policy: &RetryPolicy) -> RetryDecision;
async fn cancellable_backoff(...);
```

禁止：

```text
Tool retry
Policy/Context retry
SessionLog append retry after UnknownOutcome
retry Started/Unknown model delivery
```

### 24.21 `src/storage/conversation.rs` → `src/conversation/*`

拆分：

```text
entry.rs
state.rs
validator.rs
projection.rs
log.rs
recovery.rs
```

`ConversationLog` 由 actor 独占：

```rust
pub(crate) async fn initialize(...);
pub(crate) async fn load_and_validate(...);
pub(crate) async fn append_validated(
    &mut self,
    batch: Vec<UnsequencedEntry>,
) -> Result<CommittedBatch, ConversationCommitError>;
pub(crate) async fn transcript(...);
pub(crate) fn projection(&self) -> PromptProjection;
pub(crate) fn recovery_plan(&self) -> Option<RecoveryPlan>;
pub(crate) async fn close(...);
```

`UnsequencedEntry` 及各 draft 类型保持 `pub(crate)`；`ConversationLog` 是唯一 seq/timestamp 分配者。

删除：

```text
Snapshot generation
durable Interaction
JSONL/path/file lock logic
```

### 24.22 `src/storage/store.rs` → `src/storage/session_log.rs`

删除 concrete：

```text
SessionStore
runtime.lock
sessions directory
JSONL scanner
worker thread
fs4 lock
list/create/delete Session API
```

定义 public：

```text
SessionLog trait
LogFuture
ConversationPage
AppendReceipt
SessionLogError
```

SessionLog 方法：

```text
initialize(SessionManifest)
load_manifest()
read_page(after, limit)
append(expected_head, entries)
close()
```

Trait 使用 `&mut self`、`Send`，不要求 Sync。

### 24.23 `src/model/provider.rs` → `src/model/model.rs`

删除 provider/credential/endpoint 概念。

定义 `Model` trait、ModelStartFuture、ModelStream、ModelDescriptor。

`ModelDescriptor.model_ref` 必须与 SessionSpec.model 校验。

### 24.24 `src/model/gateway.rs` → `src/model/driver.rs`

保留：

```text
stream assembly
bounds
cancellation
timeout
retry
panic isolation
response validation
```

删除：

```text
ProviderRegistry lookup
credential resolution
HTTP/provider branching
```

### 24.25 删除 `src/model/registry.rs`

不替换为 ModelResolver。

每个 SessionBindings 直接持有一个 `Arc<dyn Model>`。

### 24.26 删除 `src/model/providers/**`、transport、credential

全部移出 production source和 Core tests。

Core 只用 FakeModel 测试 semantic contract。

### 24.27 `src/tools/types.rs`

重写/新增：

```text
ToolSpec
ToolInvocation
ToolExecutionOutcome
ToolOutput
BoundedText-based ToolOutput
ToolResultOutcome
ToolInputRequest/Answer
ApprovalRequest/Decision
ToolProgress
```

删除 builtin/process/workspace-specific DTO。

### 24.28 `src/tools/context.rs`

仅包含：

```text
CancellationToken
Deadline
ToolProgressSink
```

删除所有 service/capability lookup。

### 24.29 `src/tools/registry.rs` → `src/tools/set.rs`

实现 immutable ToolSet/Builder。

- duplicate name error；
- `Arc<BTreeMap<...>>` 可共享；
- no hot register；
- no Runtime global registry。

### 24.30 `src/tools/policy.rs`

实现 async typed ToolPolicy。

删除：

```text
AllowConfiguredTools hardcode
ToolDecision::Ask
string answer matching
policy UI callback
```

Policy panic/timeout/error fail closed。

### 24.31 删除 `src/tools/builtins/**`

删除 ask_user/read/write/list/run_command production implementation。

FakeTool 只在 tests/support。

### 24.32 删除 `src/tools/process.rs`

删除进程执行、环境、cwd、output pump。

### 24.33 删除 `src/workspace/**`

删除 capability root、WorkspaceAccess、relative path implementation和 public export。

### 24.34 新增 `src/context/provider.rs`

实现 ContextProvider、ContextRequest、ContextBundle/Block/Slot。

不实现 AGENTS.md/local provider。

### 24.35 `src/prompt/builder.rs`

重写为 deterministic builder：

```text
Core protocol instructions
SessionSpec.system_prompt
Context blocks
Conversation projection
Tool specs
```

不读文件、不调用 Model、不持有 Host。

### 24.36 `src/prompt/compaction.rs` → `src/compaction/strategy.rs`

定义 CompactionStrategy Port 和 DTO。

保留 boundary validation，删除 concrete model summary。

### 24.37 `src/error.rs`

按第 19 节重写。

删除 RuntimeError；保留 adapter error 的 safe mapping。

### 24.38 `src/ids.rs`

新增 SessionInstanceId、ContextSourceId；删除 Snapshot/Observation专用 ID。

### 24.39 `src/time.rs` / `timestamp.rs`

集中 Timestamp 构造和测试 clock；不散落 SystemTime 调用。

### 24.40 新增 `src/value.rs`

定义 `BoundedText` 和 centralized JSON-size validation；若 v0.2 的 bounded helper 位于 `wire`，只迁移与 transport 无关的 checked-value 逻辑，不迁移 Wire API。

### 24.41 删除旧 `src/wire/**`（若仍存在）

Core 不提供 JSON command router、HTTP/IPC DTO 或 legacy bootstrap。

### 24.42 scripts 与 CI

更新：

```text
scripts/check.sh
scripts/check-msrv.sh
scripts/check-architecture.sh/.py
.github/workflows/*
```

检查 no Runtime/SessionManager/Snapshot/Workspace/provider/builtin/concrete store。

---

## 25. 现有方法级迁移表

### 25.1 Runtime 层

| v0.2 symbol | 操作 | v0.3 目标 |
|---|---|---|
| `Runtime::open` | 删除 | Host 打开 log；调用 `SessionRuntime::create/load` |
| `Runtime::create_session` | 删除 | Host/Repository 准备 ID/log；`SessionRuntime::create` 初始化 manifest |
| `Runtime::load_session` | 删除 | Host 获取独占 log；`SessionRuntime::load` |
| `Runtime::list_sessions` | 删除 | Host/Repository |
| `Runtime::delete_session` | 删除 | Host/Repository |
| `Runtime::submit` | 删除 | `SessionHandle::submit` |
| `Runtime::answer` | 删除 | `SessionHandle::answer` |
| `Runtime::cancel` | 删除 | `TurnHandle::cancel` |
| `Runtime::snapshot` | 删除 | `SessionHandle::state/watch_state` |
| `Runtime::subscribe` | 删除 | `SessionRuntime::take_events` |
| `Runtime::transcript` | 移动 | `SessionHandle::transcript` |
| `Runtime::close_session` | 删除 | Host remove owner并 `shutdown` |
| `Runtime::shutdown` | 删除 | Host 对每个 SessionRuntime shutdown |
| `Runtime::prepare_session` | 删除/拆分 | Host prepares log；Core validates spec/bindings |
| `workspace_access` | 删除 | Workspace 外置 |
| `RuntimeInner::drop` | 删除 | SessionRuntime Drop cancel only，无 retention |

### 25.2 SessionManager

| v0.2 职责 | v0.3 |
|---|---|
| loaded HashMap | Host |
| load reservation | Host + SessionLog lease |
| ManagedSession | 删除 |
| LoadedSessionId | SessionInstanceId（单 owner内） |
| exact remove callback | Host owner removal |
| delete only unloaded | Host policy |

### 25.3 SessionActor

| 当前 method/职责 | 修改 |
|---|---|
| `SessionActor::new` | 接收 config/spec/bindings/ConversationLog/channels/instance ID |
| `handle_idle_command` | Submit/Transcript/Shutdown；Answer invalid |
| `handle_active_command` | Answer/Transcript/Shutdown；Submit Busy |
| `active_step` | select command、RunnerEvent、root cancellation |
| `handle_submit` | 按第 15.4 节 durable-first 顺序 |
| `handle_interaction` | process-local suspension，不 append Interaction |
| `handle_runner_event` | CommitAssistant/ToolResult/Suspend/Finish |
| `finish_active` | 替换为唯一 `settle_turn` |
| `append_terminal` | 只允许 settlement 调用 |
| `close_session` | 替换为 owner shutdown path |
| `snapshot` | 删除 |
| `refresh_projection` | ConversationLog 内部 |
| hidden unavailable | SessionHealth::Degraded |

### 25.4 TurnRunner

| 当前 method/职责 | 修改 |
|---|---|
| `run_turn` | Context→Model→Tool loop，返回 RunnerOutcome |
| prompt build | ContextProvider + PromptProjection |
| `execute_tool_calls` | 保持顺序，取消/limit检查 |
| `execute_one_tool` | typed Policy + Tool outcome |
| approval string compare | 删除 |
| ToolContext construction | 无 Workspace/InteractionClient |
| direct Conversation append | 删除，RunnerEvent commit ack |
| public Event send | 只通过 InternalEventSink best effort |
| terminal append/completion | 删除，actor settlement |

### 25.5 Conversation/Store

| 当前 symbol/职责 | 修改 |
|---|---|
| `ConversationLog::append` | stage validation → SessionLog append → commit state |
| `ConversationLog::snapshot` | 删除 |
| prompt view | `conversation::projection` |
| restart repair | 保留 semantic plan，取消 unfinished Turn |
| JSONL scan/tail repair | 外部 adapter |
| SessionStore worker/lock | 删除 |
| session.json | SessionManifest 通过 SessionLog adapter编码 |
| list/create/delete | Host Repository |

### 25.6 Model/Tool/Event

| 当前 symbol | 修改 |
|---|---|
| ProviderRegistry | 删除；SessionBindings.model |
| ModelGateway | ModelDriver，无 provider lookup |
| concrete providers | 删除 |
| ToolRegistry | immutable ToolSet |
| `ToolContext::ask_user` | 删除；Tool returns RequestInput |
| InteractionClient | 删除 |
| `ToolDecision::Ask` | RequireApproval |
| builtin tools/process | 删除 |
| Workspace | 删除 |
| SessionSnapshot | 删除 |
| broadcast subscribe | single mpsc take_events |
| ResyncRequired | 删除 |
| Text/Reasoning delta | best-effort OutputDelta |

---

## 26. 测试结构与 Fake Port

### 26.1 目标目录

```text
tests/
├── support/
│   ├── fake_model.rs
│   ├── fake_tool.rs
│   ├── fake_policy.rs
│   ├── fake_context.rs
│   ├── fake_compaction.rs
│   ├── fake_session_log.rs
│   ├── harness.rs
│   └── task_tracker.rs
├── api_compile.rs
├── session_runtime_acceptance.rs
├── conversation_contract.rs
├── session_log_contract.rs
├── interaction_contract.rs
├── event_contract.rs
├── lifecycle_contract.rs
├── port_isolation.rs
├── multi_instance_concurrency.rs
└── restart_recovery.rs
```

### 26.2 FakeSessionLog

支持脚本化：

```text
uninitialized/initialized manifest
initial entries
paged replay
append success/conflict/unknown outcome/delay/panic
read corruption
close failure
operation recording
```

记录：

```text
initialize/load/read/append/close order
expected heads
batches
max simultaneous mutable operation
close count
```

### 26.3 FakeModel

支持：

```text
text/reasoning delta
tool call stream
final response
malformed response
missing finish
started delivery then error
not-started retryable error
delay/panic/cancellation observation
usage
```

### 26.4 FakeTool

至少：

```text
EchoTool
FailTool
WaitForCancelTool
PanicTool
ProgressTool
RequestInputTool
OversizedOutputTool
```

### 26.5 FakePolicy

脚本化 Allow/Deny/RequireApproval/Delay/Panic/Error，并记录 exact invocation。

### 26.6 FakeContext 与 FakeCompaction

支持 ordered blocks、overflow、delay、panic、error和 invalid summary boundary。

### 26.7 TaskTracker

跟踪：

```text
actor task
runner task
model/tool child task
alive/completed/cancelled/aborted
```

用于 no detached work 和 shutdown 验收。

### 26.8 禁止真实 adapter

Core default tests：

- 不访问网络；
- 不访问真实文件系统；
- 不启动进程；
- 不需要 credential；
- 不依赖 OS-specific Workspace behavior。

---

## 27. 强制验收矩阵

### Public API 与生命周期

| ID | 验收内容 |
|---|---|
| AT-K01 | Public API 不存在 Runtime、RuntimeClient、SessionManager |
| AT-K02 | SessionRuntime 不 Clone；SessionHandle/TurnHandle 可 Clone |
| AT-K03 | create 初始化 manifest、空 Conversation并返回 Idle/Healthy |
| AT-K04 | create 对已初始化 log 失败且无 task leak |
| AT-K05 | load 校验 manifest SessionId/spec/bindings |
| AT-K06 | take_events 第二次返回 AlreadyTaken |
| AT-K07 | shutdown 关闭 active turn、log和全部 task |
| AT-K08 | shutdown 后旧 handle 返回 Closed |
| AT-K09 | reload 同 SessionId 生成不同 SessionInstanceId |
| AT-K10 | stale handle/turn/event 不影响新 instance |

### Turn 与状态机

| ID | 验收内容 |
|---|---|
| AT-K11 | submit 仅在 UserMessage durable 后返回 TurnHandle |
| AT-K12 | UserMessage append failure 不调用 Model、不进入 Running |
| AT-K13 | 第二次 submit 返回 Busy + active TurnId |
| AT-K14 | TurnHandle cancel exact且幂等 |
| AT-K15 | drop TurnHandle 不 cancel |
| AT-K16 | 多 clone wait 同一 outcome |
| AT-K17 | 四态及 health invariant完整 |
| AT-K18 | ordinary Model/Tool error 不使 Session Degraded |
| AT-K19 | log UnknownOutcome 使 Degraded并拒绝 submit |
| AT-K20 | command queue full 返回 Backpressure而非无限等待 |

### Conversation/Log

| ID | 验收内容 |
|---|---|
| AT-K21 | malformed/partial Model response不进入 Conversation |
| AT-K22 | ToolCall/ToolResult严格匹配且结果唯一 |
| AT-K23 | 多 ToolCall按原序执行和提交 |
| AT-K24 | Conversation state只在 durable AppendReceipt后更新 |
| AT-K25 | ToolFinished在 ToolResult durable后尝试发布 |
| AT-K26 | TurnHandle completion/TurnFinished在 terminal durable后 |
| AT-K27 | terminal exactly once |
| AT-K28 | expected-head conflict导致 Degraded |
| AT-K29 | transcript只包含 durable entries |
| AT-K30 | invalid Summary boundary被拒绝且不append |

### Restart recovery

| ID | 验收内容 |
|---|---|
| AT-K31 | unfinished无 ToolCall Turn追加 CancelledByRestart terminal |
| AT-K32 | unresolved ToolCall按稳定顺序补 Cancelled result |
| AT-K33 | pending approval不恢复 |
| AT-K34 | pending ToolInput不恢复 |
| AT-K35 | 已 terminal历史load不追加repair |
| AT-K36 | repair UnknownOutcome使load失败且不spawn actor |
| AT-K37 | seq gap/multiple terminal/unmatched result load失败 |

### Interaction

| ID | 验收内容 |
|---|---|
| AT-K38 | RequireApproval进入 WaitingForInput并更新 SessionState |
| AT-K39 | AllowOnce执行冻结的 exact arguments |
| AT-K40 | Deny不调用 Tool并提交 Denied result |
| AT-K41 | 不接受 `yes/allow` 文本替代 typed approval |
| AT-K42 | RequestInput后 Tool future已结束，answer直接形成 InputProvided result |
| AT-K43 | interaction ID/kind mismatch和重复answer被拒绝 |
| AT-K44 | waiting时cancel清除pending并settle Cancelled |

### State/Event

| ID | 验收内容 |
|---|---|
| AT-K45 | SessionRuntime返回前 state watch已有初始值 |
| AT-K46 | slow/no event consumer不阻塞 Turn完成 |
| AT-K47 | event queue full累计并尝试发布 EventsDropped |
| AT-K48 | InteractionRequested event丢失仍可从 state回答 |
| AT-K49 | TurnFinished event丢失仍可从 TurnHandle/transcript获得结果 |
| AT-K50 | 每个 event包含正确 SessionId/InstanceId/TurnId |

### Port/cancellation/panic

| ID | 验收内容 |
|---|---|
| AT-K51 | Context blocks按 slot/priority/source稳定排序 |
| AT-K52 | Context error/timeout/panic形成 Failed terminal |
| AT-K53 | Model Started/Unknown delivery不自动retry |
| AT-K54 | NotStarted retry sleep响应cancellation |
| AT-K55 | Tool timeout/panic形成 Failed result且actor存活 |
| AT-K56 | Policy error/timeout/panic fail closed |
| AT-K57 | ToolProgress queue full不阻塞 Tool |
| AT-K58 | cancellation传播 Model/Tool/Policy/Context/Compaction/interaction wait |
| AT-K59 | shutdown后 TaskTracker无 Core-owned live task |
| AT-K60 | Drop无 mem::forget/block_on，测试进程可退出 |

### Boundary/并发

| ID | 验收内容 |
|---|---|
| AT-K61 | src无 std::fs/std::process/reqwest/cap_std/fs4 |
| AT-K62 | src无 Workspace、builtin Tool、concrete Provider/Store |
| AT-K63 | src无 Subagent/AgentSpawner/parent-child graph |
| AT-K64 | src无 SessionSnapshot/ObservationFrame/ResyncRequired |
| AT-K65 | 同一 Tokio runtime并发两个 SessionRuntime，彼此取消/状态隔离 |
| AT-K66 | 两个 SessionRuntime可共享同一 Arc<dyn Model>/Policy/Context |
| AT-K67 | 每个 SessionLog只被其 actor串行可变调用 |
| AT-K68 | create/load future在actor ready前被取消，不遗留actor或log owner |
| AT-K69 | command queue已满时，SessionRuntime::shutdown仍通过root cancellation完成 |
| AT-K70 | Runner只提交unsequenced draft，ConversationLog是唯一seq/timestamp分配者 |
| AT-K71 | settlement将缺失ToolResult和TurnTerminal作为一个atomic append batch提交 |
| AT-K72 | active Turn append已知失败使Degraded，wait返回DurabilityUnavailable且不伪造terminal |
| AT-K73 | Event summary不携带完整Tool output、arguments、interaction answer或raw adapter error |

---

## 28. 实施阶段与提交顺序

### P0：基线和架构门禁

1. 记录 HEAD、fmt/test/clippy。
2. 新增 forbidden symbol/dependency检查。
3. 新增新的 API compile test skeleton。
4. 保存 v0.2 acceptance baseline。

完成：旧代码仍绿，门禁能识别未来目标。

### P1：Semantic types、Manifest、Bindings

1. KernelConfig/SemanticLimits。
2. SessionSpec/SessionManifest。
3. SessionBindings direct Model。
4. SessionInstanceId、SessionState、Interaction DTO。
5. 新 error types。

完成：纯 validation tests通过。

### P2：SessionLog 与 Conversation

1. 定义 SessionLog trait。
2. 拆分 Conversation entry/state/validator/projection/log/recovery。
3. FakeSessionLog。
4. create/load manifest和restart repair tests。

完成：AT-K21～K37核心部分通过。

### P3：Model、Tool、Context、Compaction Ports

1. direct Model trait/driver。
2. Tool/ToolSet/ToolPolicy/ToolContext。
3. RequestInput/typed approval。
4. ContextProvider/PromptBuilder。
5. CompactionStrategy。
6. Fake Ports与panic/cancel tests。

完成：Fake Model→Tool→Model闭环可运行。

### P4：SessionRuntime owner/Handle/Actor

1. SessionRuntimeOptions。
2. SessionRuntime create/load/take_events/shutdown。
3. SessionHandle/TurnHandle。
4. Actor command/state/event。
5. partial-open cleanup、ready handshake、Drop fallback。

完成：AT-K01～K20和K45基本通过。

### P5：Agent Loop 与 Interaction

1. Runner/RunnerProtocol。
2. actor commit ack。
3. sequential tools。
4. typed approval和ToolInput。
5. centralized settlement。
6. cancellation/deadline/max rounds。

完成：AT-K38～K44、K51～K58通过。

### P6：删除 Snapshot/broadcast 与旧 Runtime

1. 删除 SessionSnapshot/Observation协议。
2. event改 single mpsc。
3. 删除 Runtime/SessionManager。
4. 更新所有调用和tests。

完成：新 Public API唯一。

### P7：删除 concrete adapters和依赖

1. 删除 providers/transport/registry。
2. 删除 builtins/process/workspace。
3. 删除 concrete Store/JSONL。
4. Cargo dependency/feature清理。
5. architecture gate通过。

### P8：文档、矩阵与发布收口

1. README、contracts、migration、release note。
2. AT-K01～K73。
3. Rust 1.85/stable、Linux/macOS/Windows。
4. fmt/clippy/doc/architecture。
5. code size和public surface review。

---

## 29. 文档更新要求

### 29.1 `README.md`

首段必须明确：

> MiniCore Runtime is an embeddable single-session Agent Execution Kernel. One `SessionRuntime` owns exactly one loaded Session. A Host manages multiple SessionRuntime instances and all concrete storage, model, tool, workspace, and product capabilities.

包含：

- 一个 SessionRuntime = 一个 loaded Session；
- Host 多实例同进程示意；
- create/load API；
- state/watch + take_events；
- explicit shutdown；
- Port 列表；
- restart repair；
- non-goals。

删除旧描述：

```text
Runtime manages multiple Sessions
snapshot-first observation
built-in providers/tools/workspace/JSONL
```

### 29.2 API 示例

示例展示 Host 已经得到 `Box<dyn SessionLog>` 和外部 Port：

```rust
let options = SessionRuntimeOptions::new(
    KernelConfig::default_checked()?,
    SessionBindings::new(model, tools, Some(policy), Some(context), compaction),
    tokio::runtime::Handle::current(),
)?;

let mut session = SessionRuntime::load(
    session_id,
    opened_log,
    options,
).await?;

let handle = session.handle();
let mut events = session.take_events()?;
let turn = handle.submit(UserInput::text("Inspect the repository")?, TurnOptions::default()).await?;

let event_task = tokio::spawn(async move {
    while let Some(event) = events.recv().await {
        render(event);
    }
});

let outcome = turn.wait().await?;
session.shutdown().await?;
let _ = event_task.await;
```

示例不引用本仓库不存在的 LocalWorkspace/OpenAI/JSONL类型。

### 29.3 Contracts docs

新增：

```text
docs/contracts/session-runtime-lifecycle.md
docs/contracts/session-state.md
docs/contracts/event-stream.md
docs/contracts/conversation.md
docs/contracts/session-log.md
docs/contracts/model.md
docs/contracts/tool-policy-interaction.md
docs/contracts/cancellation.md
docs/contracts/extensions.md
```

### 29.4 Host boundary

新增 `docs/integration/host-boundary.md`，说明：

```text
Host owns HashMap<SessionId, SessionRuntime>
multiple instances share one Tokio runtime
Session Repository/list/delete/lease are external
workspace/global limits are external
SessionRuntime does not create processes
```

### 29.5 Migration

新增 `docs/migrations/v0.2-to-v0.3.md`：

| v0.2 | v0.3 |
|---|---|
| Runtime | per-loaded-session SessionRuntime |
| Runtime list/delete/按ID路由 | Host Repository/Supervisor；单 Session create/load 改为 SessionRuntime + pre-opened SessionLog |
| Runtime::submit(id) | SessionHandle::submit |
| snapshot/subscribe | state/watch + take_events |
| ProviderRegistry | SessionBindings.model |
| JSONL Store | external SessionLog |
| Workspace/builtins | external adapters |
| durable interaction | process-local + restart cancellation |

### 29.6 Release note

`docs/release-v0.3.md`：

- decisions D-01～D-15；
- breaking changes；
- AT-K01～K73结果；
- dependency/code-size变化；
- Rust/OS matrix；
- known limitations；
- no compatibility layer。

---

## 30. 架构门禁、完成定义与执行指令

### 30.1 Architecture gate

#### Forbidden files/modules

```text
src/runtime/
src/session/snapshot.rs
src/workspace/
src/model/providers/
src/model/registry.rs
src/tools/builtins/
src/tools/process.rs
concrete JSONL/store implementation
```

#### Forbidden production symbols

```text
pub struct Runtime
RuntimeClient
SessionManager
SessionSnapshot
ObservationFrame
ObservationCursor
ObservationEpoch
ResyncRequired
Workspace
WorkspaceAccess
InteractionClient
AgentSpawner
Subagent
ModelResolver
```

只检查 production `src/**/*.rs`，文档/migration字符串可排除。

#### Forbidden imports/deps

```text
reqwest
cap_std
cap_primitives
fs4
std::fs
std::env
std::net
std::process
tokio::net
tokio::process
```

#### DAG

生产模块无 SCC > 1；禁止 Port 依赖 actor/runtime internals。

#### Size

初始门禁：

```text
production Rust total <= 20,000 lines
single production file <= 1,200 lines
single public Port file <= 500 lines
```

不可为通过门禁随意扩大阈值。

### 30.2 完成定义

#### 边界

- [ ] Core 只运行一个 loaded Session。
- [ ] Host 多 Session职责未进入 Core。
- [ ] Workspace、具体 Store/Provider/Tool已删除。
- [ ] 无 Subagent、Plugin Manager、Service Locator、万能 Hook。

#### API

- [ ] SessionRuntime unique owner。
- [ ] SessionHandle/TurnHandle能力边界正确。
- [ ] create/load/take_events/shutdown完整。
- [ ] Runtime/SessionManager/Snapshot旧API无 alias。

#### 生命周期

- [ ] failed open或open future取消均无task/log owner leak。
- [ ] explicit shutdown是cleanup barrier。
- [ ] Drop无block_on/mem::forget。
- [ ] stale instance无法控制reload instance。
- [ ] shutdown后无Core-owned live task。

#### 状态/事件

- [ ] 四态+Health invariant。
- [ ] initial watch state可立即读取。
- [ ] single event consumer、bounded、lossy、不反压。
- [ ] Event丢失不影响interaction/terminal正确性。

#### Conversation/Log

- [ ] only User/Assistant/ToolResult/Summary/Terminal durable entries。
- [ ] append expected-head + durable-first。
- [ ] UnknownOutcome→Degraded，无自动retry。
- [ ] restart unfinished→CancelledByRestart。
- [ ] actor独占 `Box<dyn SessionLog>`。

#### Agent Loop

- [ ] Fake Context→Model→Tool→Model→Final闭环。
- [ ] sequential tool calls。
- [ ] typed approval，无字符串协议。
- [ ] RequestInput不恢复任意 Tool continuation。
- [ ] terminal exactly once、settlement atomic batch、durable before completion。
- [ ] cancellation覆盖全部 Port。

#### 工程

- [ ] AT-K01～K73通过。
- [ ] Rust 1.85/stable通过。
- [ ] Linux/macOS/Windows通过。
- [ ] fmt/clippy/doc/architecture通过。
- [ ] README/contracts/migration/release完成。

### 30.3 已知限制

```text
pending Interaction跨重启恢复
active Model/Tool continuation恢复
多观察者一致性/Event replay
Host Session list/idle eviction/shutdown_all
跨进程 writer lease实现
全局模型/工具调度
workspace写冲突
concrete Store durability
remote Agent orchestration
plugin ABI
per-turn model override/hot model swap
```

这些是有意边界，不是 v0.3 未完成缺陷。

### 30.4 可直接交给代码模型的执行指令

```text
请基于 zqcli/minicore-runtime 的 refactor/v0.2-core-reset 分支，严格按照《MiniCore Runtime v0.3：单 Session Kernel 与 SessionRuntime 重构实施规格》执行 breaking refactor。

最终必须满足：

1. MiniCore 是产品/crate 名，Public owner 为 SessionRuntime。
2. 一个 SessionRuntime unique owner 只拥有一个 loaded Session。
3. Host 在 Core 外管理 HashMap<SessionId, SessionRuntime>、Repository、writer lease、全局限流和 shutdown_all。
4. Public API 为 SessionRuntime、SessionRuntimeOptions、SessionHandle、TurnHandle。
5. 删除 Runtime、RuntimeClient、SessionManager 和所有按 SessionId 路由的 Core facade。
6. 删除 SessionSnapshot、Observation cursor/epoch/gap/Resync；使用 watch<SessionState> 和单消费者 bounded lossy SessionEventStream。
7. 重启不恢复 Turn、Tool future、approval 或 ToolInput；load 将 unfinished Turn repair 为 CancelledByRestart。
8. Conversation durable entries 只保留 UserMessage、AssistantMessage、ToolResult、Summary、TurnTerminal。
9. Core 只接收一个独占 Box<dyn SessionLog>；SessionLog 负责 manifest、page read、append和close；不定义多 Session Repository API。
10. 每个 SessionBindings 直接绑定 Arc<dyn Model>，不保留 ProviderRegistry/ModelResolver。
11. Workspace 不进入 Core；具体 Tool/ContextProvider构造时自行捕获 Workspace。
12. 删除 concrete JSONL、filesystem/process tools、OpenAI/Anthropic providers、HTTP transport和相关依赖。
13. 扩展仅通过 Model、Tool/ToolPolicy/ToolSet、ContextProvider、CompactionStrategy、SessionLog typed ports注入。
14. 不实现万能 Hook、Plugin Manager、Service Locator 或 Subagent专用类型。
15. ToolPolicy使用 Allow/Deny/RequireApproval；approval使用 typed AllowOnce/Deny。
16. Tool可返回 RequestInput；answer直接成为 ToolResult，不恢复任意 Tool continuation。
17. Runner不直接append Conversation或完成 TurnHandle；actor独占 ConversationLog并执行 durable commit/terminal settlement。
18. ToolFinished、TurnFinished和TurnHandle completion遵守 durable-first ordering。
19. SessionLog UnknownOutcome必须使 Session Degraded；active Turn的已知append failure同样停止执行并返回DurabilityUnavailable，不伪造terminal。
20. 多个 SessionRuntime必须能在同一进程、同一 Tokio runtime并发运行，可共享 Arc<dyn Model>/Policy/Context，但状态、log和取消域独立。
21. 按 P0～P8分阶段实现，每阶段保持 check/test/clippy通过。
22. 最终提交 README、contracts、migration、release note、architecture gate，并确保 AT-K01～K73、Rust 1.85、stable、fmt、clippy、doc全部通过。

请先记录基线 HEAD和现有测试，再按逐文件修改清单执行。不得创建 v0.2 compatibility wrapper。
```

---

## 附录 A：Host 集成参考边界（非本仓库实现任务）

```rust
pub struct SessionSupervisor {
    loaded: HashMap<SessionId, LoadedSessionSlot>,
}

pub struct LoadedSessionSlot {
    runtime: SessionRuntime,
    handle: SessionHandle,
    last_accessed: Instant,
}
```

典型流程：

```text
Host通过Repository列出metadata
→ 获取Session A独占writer lease并打开SessionLog
→ 构造workspace-bound tools/context和shared model
→ SessionRuntime::load(A)
→ 插入Host HashMap
→ 对Session B重复，同一Tokio runtime
→ A/B Turn并发
→ unload A：从map移除owner并await shutdown
→ later重新打开A并load durable history
```

Host额外治理：

```text
同SessionId重复load
跨进程writer lease
shared workspace写冲突
model rate limit/process concurrency
idle eviction
UI event routing
shutdown_all
```

---

## 附录 B：最终最小闭环

```text
Host获得空FakeSessionLog
→ SessionRuntime::create写入SessionManifest
→ initial state Idle
→ submit UserInput
→ UserMessage durable
→ FakeModel返回Assistant ToolCall
→ ToolPolicy Allow
→ FakeTool返回ToolOutput
→ ToolResult durable
→ FakeModel返回final Assistant text
→ AssistantMessage durable
→ TurnTerminal Completed durable
→ TurnHandle::wait返回Completed
→ SessionState回Idle
→ transcript包含完整durable chain
→ SessionRuntime::shutdown关闭log和全部task
```

另一个 SessionRuntime 可以同时执行相同闭环，二者仅共享显式注入的 `Arc` Port，不共享 mutable Session state。
