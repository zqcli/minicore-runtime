# Tools

`Tools` 是 `SessionRuntime` 内部的 session-scoped 工具子系统，对应未来的 `tools.rs` / `tools/`。它封装工具注册、active tools、prompt catalog、policy、approval、grants、执行协调、sandbox、mutation lock 和 executor implementations。它不是独立 runtime，也不是 UI 工具层。

工具有真实副作用，因此工具治理不能像技能那样只停留在 catalog 层。本项目采用：

```text
Driver
  consumes: DriverHost::invoke_tool_batch when Rig AgentRunStep::CallTools appears

SessionRuntime
  owns: Tools lifecycle/control, run/session state, event归约, persistence, queues
  creates: run-only ToolBatchInvoker from committed tool/profile baseline

Tools
  owns: ToolRegistry, ActiveToolSet, ToolPromptCatalog, ToolPolicy,
        ToolApprovalBroker, ToolApprovalGrantStore, ToolExecutionCoordinator,
        ToolExecutorRegistry, builtin/external executors, sandbox/mutation helpers
```

一句话：`Driver` 只驱动 Rig；`SessionRuntime` 协调当前 session/run 上下文；`Tools` 负责完整工具治理和执行。

## 设计决定

本项目不把工具执行交给 Rig 高阶 runner。我们使用 Rig 的 `AgentRun / AgentRunStep` sans-IO 路径，并在 `CallTools` step 中由 `Driver` 通过 `DriverHost::invoke_tool_batch(...)` 回到 `SessionRuntime`，再进入 `Tools`：

```text
Rig AgentRun
  owns: CallModel / CallTools / Done 状态机、turn counting、tool-result threading

Driver
  owns: 推进 AgentRun、调用 DriverHost seam、把 CallTools 转成 ToolBatchRequest、把结果喂回 AgentRun::tool_results(...)

SessionRuntime / SessionDriverHost
  SessionRuntime actor owns: phase、queues、pending writes、CurrentRun projection、public event reduction
  SessionDriverHost owns: run identity/context、ToolBatchInvoker、RunLink、progress sink、cancellation

Tools
  owns: 产品级工具治理和真实执行入口
```

这不是重新实现 Agent loop。Rig 仍决定下一步是什么；本项目只保留工具副作用的产品控制权。

## 与 Driver / SessionRuntime 的关系

调用链：

```text
LLM returns tool calls
  -> Rig AgentRunStep::CallTools { calls }
  -> Driver
  -> DriverHost::invoke_tool_batch(ToolBatchRequest)
  -> SessionRuntime / SessionDriverHost
  -> ToolBatchInvoker.invoke_batch(...)
  -> ToolExecutor
  -> Result<ToolBatchResult, ToolBatchError>
  -> SessionRuntime commits complete Ok result as ToolRound
  -> only committed ToolBatchResult returns to Driver
  -> AgentRun::tool_results(...)
```

`Driver` 不直接依赖 `Tools`，也不直接执行工具。它只知道 `DriverHost` 能返回一批 tool results。按 [ADR 0021](../adr/0021-session-runtime-separates-actor-control-from-run-execution.md)，生产实现必须使用 run-scoped owned `SessionDriverHost`；`SessionRuntime` actor 不直接实现 `DriverHost`，也不把 phase、queues、pending writes、event sink 或 `&mut Tools` 借给完整 run future。host 把 `ToolBatchRequest` 补上 session/run/cwd/turn context 后交给 run-only `ToolBatchInvoker`。

`Tools` 可以通过 `ToolUpdateSink` 返回内部工具更新，例如 proposed、approval requested、started、output delta 和 result-ready；这些不是 UI event。result-ready 是 `ToolAfterExecute` 后的候选值，仍需由 `SessionRuntime` 应用 `ToolResultBeforeCommit` 并重新校验；只有最终值才能发布为 `tool_call_finished` 并进入 `ToolRound`。

## 模块结构

建议代码布局：

```text
src/
  tools.rs
  tools/
    mod.rs
    subsystem.rs
    definition.rs
    registry.rs
    active.rs
    prompt.rs
    policy.rs
    approval.rs
    grants.rs
    planner.rs
    coordinator.rs
    executor.rs
    events.rs
    sandbox.rs
    mutation.rs
    builtin/
      mod.rs
      read.rs
      grep.rs
      find.rs
      ls.rs
      write.rs
      edit.rs
      apply_patch.rs
      bash.rs
```

`tools.rs` 可以 re-export 常用类型：

```rust
pub use subsystem::Tools;
pub use definition::{ToolDefinition, ToolExecutionMode, ToolName, ToolRisk, ToolSchema};
pub use registry::{RegisteredTool, ToolRegistry};
pub use active::ActiveToolSet;
pub use prompt::{ToolPromptCatalog, ToolPromptView, ToolProfileFingerprint};
pub use policy::{ToolPolicy, ToolPolicyConfig, ToolPolicyDecision, ToolPolicyInput};
pub use approval::{ApprovalDecisionOutcome, PendingToolApproval, ToolApprovalBroker};
pub use grants::{ApprovalGrantScope, ToolApprovalGrantStore, ToolApprovalMode};
pub use executor::{ToolExecutor, ToolExecutorRegistry, ToolInvocationResult};
```

## 文件职责

| 文件 | 职责 | 不负责 |
| --- | --- | --- |
| `tools.rs` | 顶层 public module 和常用类型 re-export | 具体执行逻辑 |
| `tools/mod.rs` | 子模块声明和内部 re-export | 业务逻辑 |
| `tools/subsystem.rs` | 定义 actor-owned `Tools` control facade；提供 `tool_batch_invoker`、`set_active_tools`、`prompt_view`、`decide_approval` 等深接口 | 不直接发布 UI event，不写 session storage |
| `tools/definition.rs` | 定义 `ToolDefinition`、`ToolName`、`ToolSchema`、`ToolRisk`、`ToolExecutionMode`、`ToolSource`、prompt metadata | 不保存当前注册表 |
| `tools/registry.rs` | 管理 `ToolRegistry`、`RegisteredTool`、工具冲突、source info、executor 绑定 | 不判断是否 active，不做审批 |
| `tools/active.rs` | 管理 `ActiveToolSet`，决定当前 session 哪些工具暴露给模型 | 不注册工具，不执行工具 |
| `tools/prompt.rs` | 从同一 active-set revision 原子投影 `ToolPromptView`：names、schemas、snippets、guidelines、fingerprint | 不构建完整 system prompt，不执行工具 |
| `tools/policy.rs` | 纯策略判断：allow、deny、require approval、rewrite args、force sequential、abort | 不等待 UI，不执行工具，不做 I/O preview |
| `tools/approval.rs` | 管 `ToolApprovalBroker`、`PendingToolApproval`、`ApprovalRequestId`、waiter lifecycle、duplicate/stale decision handling；UI-safe pending projection 由 actor `CurrentRun` 持有 | 不保存长期授权，不执行工具，不直接构造 snapshot |
| `tools/grants.rs` | 管 `ToolApprovalGrantStore`，支持 approve once、same call fingerprint、same tool in run/session/workspace、ttl/revoke | 不管理当前 pending approval |
| `tools/planner.rs` | 把 LLM tool call 规划成 `PreparedToolInvocation`：schema validate、args canonicalize、risk classify、sandbox check、preview build、execution mode resolve | 不真正执行工具 |
| `tools/coordinator.rs` | 执行已声明约束：parallel/sequential、approval wait、grant lookup、并发限制、按 `call_index` 稳定回填 | 不发明策略，不做具体工具副作用 |
| `tools/executor.rs` | 定义 `ToolExecutor` trait、`ToolExecutorRegistry`、`ToolInvocationResult`、错误归一化 | 不做 registry/policy/approval |
| `tools/events.rs` | 定义工具内部事件/更新类型，转换为 `ToolUpdateSink` 可消费的数据 | 不直接发 `agent_runtime_protocol::Event` |
| `tools/sandbox.rs` | 定义 `ToolSandboxView`、路径边界、读写域、进程/网络能力描述、sandbox check 结果 | 不做 UI approval |
| `tools/mutation.rs` | 定义 mutation lock、file mutation queue、`ToolMutationKey`，保证同文件或同资源 mutation 串行 | 不决定模型是否可调用工具 |
| `tools/builtin/read.rs` | `read` 工具定义和 executor | 不做全局 registry |
| `tools/builtin/grep.rs` | `grep` 工具定义和 executor | 不做全局 registry |
| `tools/builtin/find.rs` | `find` 工具定义和 executor | 不做全局 registry |
| `tools/builtin/ls.rs` | `ls` 工具定义和 executor | 不做全局 registry |
| `tools/builtin/write.rs` | `write` 工具定义和 executor；使用 mutation queue | 不绕过 policy/approval |
| `tools/builtin/edit.rs` | `edit` 工具定义和 executor；精确替换；使用 mutation queue | 不绕过 policy/approval |
| `tools/builtin/apply_patch.rs` | `apply-patch` 工具定义和 executor；patch 解析/应用；使用 mutation queue | 不绕过 policy/approval |
| `tools/builtin/bash.rs` | `bash` 工具定义和 executor；timeout、stdout/stderr streaming、cancel、cwd/sandbox | 不直接使用 provider/auth |

## Public Interface

`Tools` 对 owner actor 暴露少量 control interface，并为 active `RunTask` 创建权限更窄的 `ToolBatchInvoker`。每个 loaded `SessionRuntime` 仍创建和销毁自己的 Tools state，不存在 workspace-global Tools service，也不存在一个同时拥有 invoke/admin/decision 权限的宽 handle。

```rust
pub struct Tools {
    registry: ToolRegistry,
    active: ActiveToolSet,
    prompt_catalog: ToolPromptCatalog,
    policy: ToolPolicy,
    approvals: Arc<ToolApprovalBroker>,
    grants: Arc<ToolApprovalGrantStore>,
    execution: Arc<ToolExecutionKernel>,
}

impl Tools {
    pub fn tool_batch_invoker(&self) -> ToolBatchInvoker;
    pub fn set_active_tools(&mut self, names: &[ToolName]) -> Result<ToolSetChange, ToolSubsystemError>;
    pub fn prompt_view(&self) -> ToolPromptView;
    pub fn prompt_catalog(&self) -> &ToolPromptCatalog;
    pub fn decide_approval(&self, command: ToolApprovalDecisionCommand) -> ApprovalDecisionOutcome;
    pub fn set_approval_mode(&mut self, mode: ToolApprovalMode);
    pub fn approval_mode(&self) -> ToolApprovalMode;
}

#[derive(Clone)]
pub struct ToolBatchInvoker {
    profile: Arc<ToolExecutionProfile>,
    approvals: Arc<ToolApprovalBroker>,
    grants: Arc<ToolApprovalGrantStore>,
    execution: Arc<ToolExecutionKernel>,
}

impl ToolBatchInvoker {
    pub async fn invoke_batch(
        &self,
        request: ToolBatchRequest,
        context: ToolRunContext,
        sink: ToolUpdateSink,
        cancel: CancellationToken,
    ) -> Result<ToolBatchResult, ToolBatchError>;
}

pub enum ToolBatchError {
    Cancelled,
    Failed { error: ToolSubsystemError },
}
```

`Ok(ToolBatchResult)` 只表示每个 requested call 都已有按 `call_index` 排列的 actual/error result，可以进入 `ToolRound` commit。unknown tool、policy denied、approval rejected、schema invalid 和普通 executor failure 都归约为 `Ok` 中的 error result；abort/cancel 返回 `Err(ToolBatchError::Cancelled)`，infra/hook invariant failure 返回 `Failed`。`Err` 路径中的已完成单项结果只可用于当前 host diagnostics/progress，不能拼成 partial `ToolRound`。

`ToolExecutionProfile` 是 invoker 创建时捕获的 immutable baseline，至少绑定 registry/executor bindings、active tool set、policy config、sandbox profile 和 prompt/tool profile fingerprint；grants 与 pending approvals 通过专用短锁 store/broker 访问，不把整个 `Tools` 放进 mutex。`ToolExecutionKernel` 封装 planner/coordinator/executor registry 和 mutation locks，可以被 invocation task 安全共享，但不能修改 actor-owned active/profile state。

`SessionRuntime` 仍是 lifecycle owner：它创建、保存和销毁每个 session 的 `Tools` 实例；它处理 `SetActiveTools`、`SetToolPolicy`、`DecideToolApproval` 等协议命令。模型可见 active-tool change 仍必须遵循“计算/校验 next state → session mutation commit → 应用 Tools state → 发布 changed event → 原子替换 `PromptCallProfile` 与 future `ToolBatchInvoker`”。MVP 在 `Turn` 中直接拒绝这类 mutation；后续支持 safe-point apply 时必须把两个 replacement 放进同一 actor transaction。

并发实现必须满足 [ADR 0021](../adr/0021-session-runtime-separates-actor-control-from-run-execution.md)：

- `ToolBatchInvoker.invoke_batch(...)` 等待 approval 或 executor 时，actor-owned `Tools::decide_approval(...)`、pending projection 和 cancellation 必须仍可到达。
- 等待 decision 的 future 不得持有会阻止 reply path 的 mutex guard。
- `ToolApprovalBroker` 只在短临界区内登记/移除 `approval_id -> oneshot sender`；锁释放后 batch future 才等待 receiver。
- RunTask 只持有 `ToolBatchInvoker`，不能 set active tools、改变 approval mode、读取 pending projection或提交 approval decision。
- pending approval 必须先在 broker 登记 waiter，再通过 control-class `ToolUpdateSink::approval_pending(...)` 请求 actor；actor 更新 `CurrentRun` projection、发布 request event 并回复 ack 后，batch future 才进入 decision wait。actor reject/closed 时 invoker 必须移除 waiter 并失败，不能留下不可见 pending。resolved、abort、run terminal 或 session close 后从 projection 移除；duplicate/stale reply 不能重复执行工具。
- `ToolUpdateSink` 只提交内部 update；公共事件和 `RuntimeSnapshot` projection 仍由 `SessionRuntime` actor 归约。

`RuntimeSnapshot.loaded_sessions[*].current_run.pending_tool_approvals` 的 UI-safe source of truth 是 actor 的 `CurrentRun` projection。`ToolApprovalBroker` 保存冻结 args 和 waiter，是执行侧 source of truth；它不再提供另一套供 snapshot 直接读取的公开 view。pending update 必须先成功交给 actor 更新 projection，再发布 `tool_call_approval_requested` 并开始等待 decision，避免 event 已发而 snapshot 缺项。

`ToolPromptView` 必须一次性投影同一个 active-set revision：

```rust
pub struct ToolPromptView {
    pub active_names: Arc<[ToolName]>,
    pub schemas: Arc<[ToolSchema]>,
    pub snippets: Arc<[(ToolName, String)]>,
    pub guidelines: Arc<[String]>,
    pub fingerprint: ToolProfileFingerprint,
}
```

`prompt::begin_turn()` 只消费该 view，不能分别调用 `active_tool_schemas()`、`active_names()` 和 `prompt_catalog()` 后临时合并，否则并发或 safe-point 更新可能产生 system prompt/tool schema split-brain。

## 数据结构草案

```rust
pub enum ToolRisk {
    ReadOnly,
    FileMutation,
    ProcessExecution,
    Network,
    External,
}

pub enum ToolExecutionMode {
    Parallel,
    Sequential,
}

pub enum ToolSource {
    Builtin,
    StaticCustom { name: String },
    Extension { extension_id: ExtensionId },
    Mcp { server_id: McpServerId },
}

pub struct ToolDefinition {
    pub name: ToolName,
    pub label: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub prompt_snippet: Option<String>,
    pub prompt_guidelines: Vec<String>,
    pub risk: ToolRisk,
    pub execution_mode: ToolExecutionMode,
    pub source: ToolSource,
}

pub struct ToolBatchRequest {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub calls: Vec<ToolInvocation>,
}

pub struct ToolInvocation {
    pub call_index: ToolCallIndex,
    pub call_id: ToolCallId,
    pub name: ToolName,
    pub raw_args: serde_json::Value,
}

pub struct ToolInvocationResult {
    pub call_index: ToolCallIndex,
    pub call_id: ToolCallId,
    pub name: ToolName,
    pub content: Vec<MessageContent>,
    pub details: Option<serde_json::Value>,
    pub is_error: bool,
    pub terminate: bool,
}
```

公开协议不应使用裸 `usize`。内部数组索引可以转为 `usize`，但对外建议使用 `ToolCallIndex(u32)`。

## ToolPolicy / Planner / Approval

`ToolPolicy` 是纯策略判断器。它不等待 UI，不执行工具，也不发布事件；它只根据工具定义、prepared invocation、当前 session cwd 的 project trust、sandbox 结果和用户设置返回 `ToolPolicyDecision`。后期启用 hook system 时，`Tools` 可以把 hook overlay 归一化进 policy input。

```rust
pub struct ToolPolicyInput {
    pub tool: ToolDefinition,
    pub invocation: PreparedToolInvocation,
    pub cwd_trust: TrustLevel,
    pub sandbox: ToolSandboxView,
    pub hook_requirement: Option<ToolApprovalRequirement>, // future RuntimeHooks overlay
    pub grant: Option<ApprovalGrantMatch>,
}

pub enum ToolPolicyDecision {
    Allow,
    Deny { reason: String },
    RequireApproval { reason: String },
    RewriteArgs { args: serde_json::Value },
    ForceSequential { reason: String },
    AbortRun { reason: String },
}
```

`ToolApprovalPreview` 不由 `ToolPolicy` 构造。diff preview、patch preview、bash command preview 等可能需要 I/O 或 path canonicalization，应由 `ToolInvocationPlanner` 在 policy 前准备；policy 只决定是否需要 approval 和原因。

`ToolApprovalBroker` 只管理当前 pending execution waiter：保存已经 finalized/frozen 的 `prepared_args`，等待 actor 处理 `agent_runtime_protocol::AgentCommand::DecideToolApproval` 后 resolve。它不发布 `tool_call_approval_requested`，也不拥有 UI-safe snapshot projection；request event 与 `CurrentRun.pending_tool_approvals` 都由 `SessionRuntime` actor 在 ack pending registration 时产生。长期授权归 `ToolApprovalGrantStore`。

```rust
pub struct PendingToolApproval {
    pub approval_id: ApprovalRequestId,
    pub request: ToolApprovalRequest,
    pub call_index: ToolCallIndex,
    pub prepared_args: serde_json::Value,
    pub created_at: Timestamp,
}

pub enum ApprovalDecisionOutcome {
    Accepted,
    AlreadyResolved { original_decision: agent_runtime_protocol::ToolApprovalDecision },
    StaleRun,
    StaleCall,
    NotFound,
}
```

`PendingToolApprovalView` 是 UI-safe projection，进入所属 loaded session 的 `RuntimeSnapshot.loaded_sessions[*].current_run.pending_tool_approvals`。它不能包含 `prepared_args`、executor handle、sandbox internals 或 hook-private context。

## Approval Modes And Grants

需要支持每次询问、批准一次、后续记忆授权和 session 无需授权模式：

```rust
pub enum ToolApprovalMode {
    AskEveryTime,
    UseRememberedGrants,
    AutoAllow { max_risk: ToolRisk },
    AutoDeny { reason: String },
}

pub enum ToolApprovalDecision {
    ApproveOnce,
    ApproveGrant { scope: ApprovalGrantScope, ttl: Option<Duration> },
    Reject { reason: Option<String> },
}

pub enum ApprovalGrantScope {
    SameCallFingerprint,
    SameToolInRun,
    SameToolInSession,
    SameToolInWorkspace,
}
```

`ToolApprovalGrantStore` 的 key 至少应包含：

```rust
pub struct ApprovalGrantKey {
    pub tool_name: ToolName,
    pub cwd: PathBuf,
    pub risk: ToolRisk,
    pub args_fingerprint: Option<String>,
    pub sandbox_profile: Option<String>,
}
```

`SameCallFingerprint` 必须使用 canonical args hash，避免批准一个低风险参数后放行同工具的高风险参数。`SameToolInWorkspace` 在 MVP 不得由 UI 或 policy 签发：当前 grant key 含 cwd/sandbox，它不能诚实表达跨 cwd workspace grant；移除 cwd 又会跨越 per-cwd trust/sandbox。该 scope 保留为后续类型占位，待单独 ADR 定型。`AutoAllow { max_risk }` 只跳过 UI approval，不跳过 active tool check、schema validate、trust/sandbox、hard deny、mutation queue、audit log 和 diagnostics。

## 执行语义

主流模型 API 通常只控制“模型能否一次吐多个 tool calls”，例如 OpenAI / Mistral 的 `parallel_tool_calls`、Anthropic 的 `disable_parallel_tool_use`。LLM tool call 对象通常只表达 call id、tool name、arguments 和 source order；它不可靠地表达本地执行策略。

外部实现可以提供并发与顺序治理的设计参考，但不构成兼容承诺。例如：

- pi `pi-agent-core` 默认 `toolExecution = "parallel"`。
- pi 在 `config.toolExecution === "sequential"` 或 batch 中任一 tool definition `executionMode === "sequential"` 时整批串行。
- pi parallel 模式下 prepare 按输入顺序进行，allowed tools 通过 `Promise.all(...)` 并发执行；完成事件可乱序，但返回给模型的 tool results 保持原始输入顺序。
- pi 的 `write` / `edit` 等 mutation tools 通过 file mutation queue 做同文件互斥，而不是要求所有工具全局串行。
- OpenAI / Mistral 的 `parallel_tool_calls`、Anthropic 的 `disable_parallel_tool_use` 主要控制模型是否可以一次返回多个 tool calls。
- Gemini 常见接口主要暴露 `functionCallingConfig.mode` / `toolChoice` 这一层能力，未提供每个 tool call 的宿主本地执行策略。

因此 MiniCore 把 provider request capability、LLM source order 和本地 execution policy 分开处理：ModelGateway 负责告诉 provider 是否允许模型一次返回多个 calls；`Tools` 负责本地批量工具调用的治理和执行。

MiniCore 的本地执行语义由以下规则独立定义：

```text
默认 Parallel。
如果 session config 强制 Sequential：整批串行。
如果 batch 中任一 ToolDefinition.execution_mode == Sequential：整批串行。
否则：prepare 按 call_index 顺序；execute 并发；progress/finish event 可按完成顺序；tool result 回填按 call_index 顺序。
```

`ToolExecutionCoordinator` 只执行已声明约束，不发明策略。约束来源包括：

- LLM source order：`call_index`。
- provider request option：是否允许模型一次返回多个 tool calls。
- `ToolDefinition.execution_mode`。
- session config / approval mode。
- `ToolPolicy` / sandbox 的降级结果；后期可加入 hook 降级结果。
- executor-local locks，例如 file mutation queue。

后续若需要细粒度 mutation lock，可扩展：

```rust
pub enum ToolExecutionMode {
    Parallel,
    Sequential,
    Mutating { lock_key: ToolMutationKey },
}
```

但 `lock_key` 应由 tool definition、planner 或 executor 提供，不能由 coordinator 猜测。

## Sandbox Source Of Truth

`ToolSandboxView` 是工具安全边界的 source of truth。UI approval 只能表达用户意愿，不能替代 sandbox。所有工具在 executor 前必须拿到 planner 计算出的 sandbox verdict。

```rust
pub struct ToolSandboxView {
    pub cwd: PathBuf,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub denied_roots: Vec<PathBuf>,
    pub process: ProcessSandboxPolicy,
    pub network: NetworkSandboxPolicy,
    pub env: EnvSandboxPolicy,
}

pub enum ProcessSandboxPolicy {
    Disabled,
    AllowListed { commands: Vec<String> },
    WorkspaceShell { timeout: Duration },
}

pub enum NetworkSandboxPolicy {
    Disabled,
    AllowListed { hosts: Vec<String> },
}

pub enum ToolSandboxVerdict {
    Allowed,
    Denied { reason: String },
    RequiresApproval { reason: String },
    ForceSequential { reason: String },
}
```

路径规则：

- planner 必须先把所有 path 参数相对 `ToolRunContext.cwd` 解析为 canonical path，再做 sandbox check。
- path 必须落在 `read_roots` 才能读取，落在 `write_roots` 才能写入；`denied_roots` 优先级最高。
- 不存在的待创建文件必须 canonicalize 它最近存在的父目录，并校验目标路径不会通过 symlink 跳出 `write_roots`。
- 路径比较必须使用平台语义；大小写不敏感文件系统不能只用字节前缀判断。
- bash 的 `cwd` 必须落在允许的 workspace cwd 内；bash 默认不能访问 network，除非 `NetworkSandboxPolicy` 明确允许。
- 后期 hook `RewriteArgs` 后必须重新 schema validate、重新 canonicalize、重新 sandbox check、重新 policy evaluate。

executor 不能自行扩大 sandbox。executor-local 检查只能比 `ToolSandboxView` 更严格，不能更宽松。

## Tool Batch Invocation 流程

```text
Rig AgentRunStep::CallTools { calls }
  → Driver maps PendingToolCall -> ToolBatchRequest { calls: ToolInvocation { call_index, ... } }
  → DriverHost::invoke_tool_batch(request)
  → SessionRuntime / SessionDriverHost attaches ToolRunContext
  → ToolBatchInvoker.invoke_batch(request, context, sink, cancel)
      → for each call in call_index order:
          → registry lookup
          → active tool check
          → prepare/canonicalize args
          → schema validate
          → sandbox/trust check
          → build approval preview if needed
          → future RuntimeHookRegistry.invoke(ToolBeforePolicy)
          → ToolPolicy.evaluate(...)
          → grant lookup / approval mode
          → apply allowed rewrites、重新 schema/sandbox 校验并 finalized/freeze prepared_args + preview
          → ToolApprovalBroker waits if required
      → ToolExecutionCoordinator executes allowed prepared invocations
          → SessionRuntime sink receives tool_call_started / output_delta
          → ToolExecutor.execute(ctx, updates, cancel)
          → normalize output into ToolInvocationResult
          → future RuntimeHookRegistry.invoke(ToolAfterExecute)
      → sort ToolInvocationResult by call_index
      → return Ok(ToolBatchResult) only when every call has a result
      → return Err(Cancelled | Failed) for incomplete batch
  → SessionRuntime may invoke ToolResultBeforeCommit on result drafts
  → SessionRuntime revalidates call ids/order/redaction and emits tool_call_finished
  → SessionRuntime commits complete ToolRound batch
  → Driver maps ToolBatchResult -> Rig tool result content
  → AgentRun::tool_results(results)
```

错误原则：除 abort/cancel 外，未知工具、未启用工具、schema invalid、policy denied、approval rejected、executor failed 都应变成 error tool result，而不是让 run 崩溃。assistant tool-call message 与整批实际/error tool results 只有在全部结果归约完成后才作为一个 `ToolRound` 写入会话，因此 committed history 不会出现 unresolved tool call。

## LLM 如何知道工具

模型通过两个原子一致的通道获知工具：

1. Provider tool schema：active tools 的 `name`、`description`、`input_schema` 进入模型请求。
2. System prompt：active tools 的 `prompt_snippet` 和 `prompt_guidelines` 进入 `Available tools` 与 `Guidelines`。

`Prompt` 消费的是单次 `Tools.prompt_view()`，不消费 `ToolBatchInvoker.invoke_batch`、approval/policy、sandbox 或 executor state。`ResourceManager` 不拥有工具 schemas/snippets/guidelines；工具提示素材来自 session-owned `Tools`。`PromptCallProfile` 把该 view 产生的 system sections 与 provider schemas 原子绑定。

## 内置工具命名

MiniCore 的 canonical 内置工具名固定为：

- 只读：`read`、`grep`、`find`、`ls`。
- 文件变更：`write`、`edit`。
- 补丁变更：`apply-patch`。
- 进程执行：`bash`。

UI 本地化只能改变 `label`，不能改变 LLM 可调用的 canonical tool name。

## 功能覆盖表

| 能力 | 设计位置 | 说明 |
| --- | --- | --- |
| LLM 返回 tool call 后可执行 | `Driver -> DriverHost -> ToolBatchInvoker.invoke_batch -> SessionRuntime commit` | Driver 不直接执行工具 |
| Driver 不直接依赖 Tools | `DriverHost` | Driver 只调用 host seam |
| SessionRuntime 协调 Driver 和 Tools | `SessionRuntime` / `SessionDriverHost` | 注入 session/run/cwd/turn context |
| Tools 封装完整工具行为 | `tools/subsystem.rs` | registry、active、policy、approval、grant、executor 都在 Tools 内部 |
| 乱序执行 | `tools/coordinator.rs` | parallel 模式下 executor 可乱序完成 |
| 顺序执行 | `ToolExecutionMode::Sequential` / session config | 整批串行，后续可细化 lock-based serial |
| 结果按 LLM 原始顺序回填 | `call_index` + `coordinator.rs` | tool result 回填前按 `call_index` 排序 |
| 批量 tool calls | `ToolBatchRequest` / `invoke_batch` | 一次处理 Rig `CallTools { calls }` |
| 工具批准能力 | `approval.rs` | pending approval 状态机 |
| UI 从 snapshot 恢复 pending approval | `CurrentRun.pending_tool_approvals` actor projection | 不直接读取 broker waiter/冻结 args |
| approval 后参数冻结 | `PendingToolApproval.prepared_args` | UI 不能修改 args |
| duplicate/stale approval 处理 | `ApprovalRequestId` + `ApprovalDecisionOutcome` | 防止双击重复执行 |
| session 无需授权模式 | `ToolApprovalMode::AutoAllow` | session-scoped，仍受 hard deny/sandbox/audit 约束 |
| 某一种 tool 批准一次后续免批 | `ToolApprovalGrantStore` + `SameToolInSession` | 建议限制 risk/sandbox/args fingerprint |
| session 批准一次后续免批 | `ApprovalGrantScope::SameToolInSession` 或 explicit `AutoAllow` | 高风险工具不应默认开放全 session grant |
| approve once | `ToolApprovalDecision::ApproveOnce` | 只当前 pending call 生效 |
| same args fingerprint grant | `SameCallFingerprint` | 使用 canonical args hash |
| grant TTL / revoke | `grants.rs` | 后续 UI 可加 revoke |
| 无审批模式仍执行安全治理 | `policy.rs` / `sandbox.rs` / `coordinator.rs` | AutoAllow 只跳过 UI approval |
| active tool check | `active.rs` + `planner.rs` | 未启用工具返回 error tool result |
| schema validation | `planner.rs` | executor 前完成 |
| preview 构造 | `planner.rs` | 不放在纯 `ToolPolicy` |
| sandbox source of truth | `sandbox.rs` | 定义路径、读写域、进程/网络边界 |
| 文件 mutation 串行 | `mutation.rs` + builtin mutation tools | 同文件/同资源 lock |
| read/grep/find/ls 可并行 | `ToolExecutionMode::Parallel` | 默认 parallel |
| bash 可限制顺序/沙箱/timeout/cancel | `builtin/bash.rs` + `sandbox.rs` | bash 默认可按 policy 降级 sequential |
| executor 实现包含在 Tools | `tools/builtin/*` | builtin executors 在 Tools 子系统内 |
| MCP / extension tools 后续扩展 | `registry.rs` / `executor.rs` | 注册为 `RegisteredTool` 后走同一路径 |
| tool progress / output streaming | `events.rs` + `ToolUpdateSink` | Tools 不直接发 UI event |
| 工具错误不崩 run | `executor.rs` / `coordinator.rs` | 非 abort 转 error tool result |
| abort/cancel 传播 | `invoke_batch(..., cancel)` | approval wait 和 executor 都要响应 cancel；未完成 batch 不持久化 |
| unresolved tool call 避免会话带毒 | `coordinator.rs` + `SessionWriter` | 完整 assistant tool-call/result round 作为一个 batch 提交；incomplete round 不持久化 |
| provider parallel tool calls 兼容 | `ModelGateway` 控制，`Tools` 处理 batch | 模型是否吐多个 calls 与本地执行分离 |

## MVP 策略

MVP 默认只启用只读工具：

- `read`
- `grep`
- `find`
- `ls`

后续再启用高风险工具：

- `write`
- `edit`
- `apply-patch`
- `bash`

高风险工具必须接入审批、工作区沙箱、abort signal 和 mutation queue。UI 审批只是 `ToolPolicy` 的输入，不是安全边界。

## 设计约束

- 不要把工具执行交给 Rig 高阶 runner；那会削弱审批、事件、暂停恢复和 session persistence 的控制。
- 不要把 `Tools` 命名成 `ToolRuntime`；runtime 在本项目中表示拥有生命周期和调度权的运行单元。
- 不要让 `Driver` 直接依赖 `Tools`；`Driver` 只通过 `DriverHost::invoke_tool_batch(...)` 请求工具结果。
- 不要让 `Tools` 直接发布 UI event 或写 session storage；所有 UI event 和持久化仍由 `SessionRuntime` 归约。
- 不要让外部工具绕过 registry/policy/approval。所有工具来源必须归一化成 `RegisteredTool`。
- 不要让 UI 执行工具。UI 只能发送审批决策。
- 不要把 tool snippets/guidelines 放进 `ResourceManager`。工具提示素材来自 session-owned `Tools`。
