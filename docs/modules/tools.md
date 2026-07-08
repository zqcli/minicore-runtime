# Tools

`Tools` 是 `SessionRuntime` 内部的 session-scoped 工具子系统，对应未来的 `tools.rs` / `tools/`。它封装工具注册、active tools、prompt catalog、policy、approval、grants、执行协调、sandbox、mutation lock 和 executor implementations。它不是独立 runtime，也不是 UI 工具层。

工具有真实副作用，因此工具治理不能像技能那样只停留在 catalog 层。本项目采用：

```text
Driver
  consumes: DriverHost::invoke_tool_batch when Rig AgentRunStep::CallTools appears

SessionRuntime
  owns: Driver, Tools, run/session state, event归约, persistence, queues
  coordinates: DriverHost implementation -> Tools::invoke_batch(...)

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
  owns: 当前 session/run/cwd/turn context、event sink、phase、queues、pending writes
  delegates: Tools::invoke_batch(...)

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
  -> Tools::invoke_batch(...)
  -> ToolExecutor
  -> ToolBatchResult
  -> Driver
  -> AgentRun::tool_results(...)
```

`Driver` 不直接依赖 `Tools`，也不直接执行工具。它只知道 `DriverHost` 能返回一批 tool results。`SessionRuntime` 实现 `DriverHost` 或创建 per-run `SessionDriverHost` wrapper，把 `ToolBatchRequest` 补上 session/run/cwd/turn context 后交给自己的 `Tools`。

`Tools` 可以通过 `ToolUpdateSink` 返回内部工具更新，例如 proposed、approval requested、started、output delta、finished；这些不是 UI event。`SessionRuntime` 负责把它们归约为 `agent_runtime_protocol::EventMsg::ToolCall(...)`、session writes、snapshot projection 和 save point。

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
pub use prompt::ToolPromptCatalog;
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
| `tools/subsystem.rs` | 定义 `Tools` 主结构；提供 `invoke_batch`、`set_active_tools`、`pending_approval_views`、`decide_approval`、`prompt_catalog` 等深接口 | 不直接发布 UI event，不写 session storage |
| `tools/definition.rs` | 定义 `ToolDefinition`、`ToolName`、`ToolSchema`、`ToolRisk`、`ToolExecutionMode`、`ToolSource`、prompt metadata | 不保存当前注册表 |
| `tools/registry.rs` | 管理 `ToolRegistry`、`RegisteredTool`、工具冲突、source info、executor 绑定 | 不判断是否 active，不做审批 |
| `tools/active.rs` | 管理 `ActiveToolSet`，决定当前 session 哪些工具暴露给模型 | 不注册工具，不执行工具 |
| `tools/prompt.rs` | 从 active tools 生成 provider tool schemas、prompt snippets、prompt guidelines | 不构建完整 system prompt |
| `tools/policy.rs` | 纯策略判断：allow、deny、require approval、rewrite args、force sequential、abort | 不等待 UI，不执行工具，不做 I/O preview |
| `tools/approval.rs` | 管 `ToolApprovalBroker`、`PendingToolApproval`、`ApprovalRequestId`、pending approval lifecycle、duplicate/stale decision handling | 不保存长期授权，不执行工具 |
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

`Tools` 对 `SessionRuntime` 暴露少量深接口：

```rust
pub struct Tools {
    registry: ToolRegistry,
    active: ActiveToolSet,
    prompt_catalog: ToolPromptCatalog,
    policy: ToolPolicy,
    approvals: ToolApprovalBroker,
    grants: ToolApprovalGrantStore,
    planner: ToolInvocationPlanner,
    coordinator: ToolExecutionCoordinator,
    executors: ToolExecutorRegistry,
}

impl Tools {
    pub async fn invoke_batch(
        &mut self,
        request: ToolBatchRequest,
        context: ToolRunContext,
        sink: ToolUpdateSink,
        cancel: CancellationToken,
    ) -> ToolBatchResult;

    pub fn set_active_tools(&mut self, names: &[ToolName]) -> ToolSetChange;
    pub fn active_tool_schemas(&self) -> Vec<ToolSchema>;
    pub fn prompt_catalog(&self) -> &ToolPromptCatalog;
    pub fn pending_approval_views(&self) -> Vec<agent_runtime_protocol::PendingToolApprovalView>;
    pub fn decide_approval(&mut self, command: ToolApprovalDecisionCommand) -> ApprovalDecisionOutcome;
    pub fn set_approval_mode(&mut self, mode: ToolApprovalMode);
    pub fn approval_mode(&self) -> ToolApprovalMode;
}
```

`SessionRuntime` 仍是 lifecycle owner：它创建、保存和销毁每个 session 的 `Tools` 实例；它处理 `SetActiveTools`、`SetToolPolicy`、`DecideToolApproval` 等协议命令，并调用 `Tools` 的对应接口。

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

`ToolPolicy` 是纯策略判断器。它不等待 UI，不执行工具，也不发布事件；它只根据工具定义、prepared invocation、workspace trust、sandbox 结果、用户设置和 hook overlay 返回 `ToolPolicyDecision`。

```rust
pub struct ToolPolicyInput {
    pub tool: ToolDefinition,
    pub invocation: PreparedToolInvocation,
    pub workspace_trust: TrustLevel,
    pub sandbox: ToolSandboxView,
    pub hook_requirement: Option<ToolApprovalRequirement>,
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

`ToolApprovalBroker` 只管理当前 pending approval：冻结 `prepared_args`，触发 `tool_call_approval_requested`，等待 `agent_runtime_protocol::Command::DecideToolApproval`。长期授权归 `ToolApprovalGrantStore`。

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

`PendingToolApprovalView` 是 UI-safe projection，进入 `RuntimeSnapshot.active_session.current_run.pending_tool_approvals`。它不能包含 `prepared_args`、executor handle、sandbox internals 或 hook-private context。

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

`SameCallFingerprint` 必须使用 canonical args hash，避免批准一个低风险参数后放行同工具的高风险参数。`AutoAllow { max_risk }` 只跳过 UI approval，不跳过 active tool check、schema validate、trust/sandbox、hard deny、mutation queue、audit log 和 diagnostics。

## 执行语义

主流模型 API 通常只控制“模型能否一次吐多个 tool calls”，例如 OpenAI / Mistral 的 `parallel_tool_calls`、Anthropic 的 `disable_parallel_tool_use`。LLM tool call 对象通常只表达 call id、tool name、arguments 和 source order；它不可靠地表达本地执行策略。

外部实现参考：

- pi `pi-agent-core` 默认 `toolExecution = "parallel"`。
- pi 在 `config.toolExecution === "sequential"` 或 batch 中任一 tool definition `executionMode === "sequential"` 时整批串行。
- pi parallel 模式下 prepare 按输入顺序进行，allowed tools 通过 `Promise.all(...)` 并发执行；完成事件可乱序，但返回给模型的 tool results 保持原始输入顺序。
- pi 的 `write` / `edit` 等 mutation tools 通过 file mutation queue 做同文件互斥，而不是要求所有工具全局串行。
- OpenAI / Mistral 的 `parallel_tool_calls`、Anthropic 的 `disable_parallel_tool_use` 主要控制模型是否可以一次返回多个 tool calls。
- Gemini 常见接口主要暴露 `functionCallingConfig.mode` / `toolChoice` 这一层能力，未提供每个 tool call 的宿主本地执行策略。

因此 MiniCore 把 provider request capability、LLM source order 和本地 execution policy 分开处理：ModelGateway 负责告诉 provider 是否允许模型一次返回多个 calls；`Tools` 负责本地批量工具调用的治理和执行。

MiniCore 的本地执行语义对齐 pi：

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
- `ToolPolicy` / sandbox / hook 的降级结果。
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
- hook `RewriteArgs` 后必须重新 schema validate、重新 canonicalize、重新 sandbox check、重新 policy evaluate。

executor 不能自行扩大 sandbox。executor-local 检查只能比 `ToolSandboxView` 更严格，不能更宽松。

## Tools::invoke_batch 流程

```text
Rig AgentRunStep::CallTools { calls }
  → Driver maps PendingToolCall -> ToolBatchRequest { calls: ToolInvocation { call_index, ... } }
  → DriverHost::invoke_tool_batch(request)
  → SessionRuntime / SessionDriverHost attaches ToolRunContext
  → Tools::invoke_batch(request, context, sink, cancel)
      → for each call in call_index order:
          → registry lookup
          → active tool check
          → prepare/canonicalize args
          → schema validate
          → sandbox/trust check
          → build approval preview if needed
          → RuntimeHookRegistry.invoke(ToolBeforePolicy)
          → ToolPolicy.evaluate(...)
          → grant lookup / approval mode
          → ToolApprovalBroker waits if required
          → approval 后冻结 prepared_args
      → ToolExecutionCoordinator executes allowed prepared invocations
          → SessionRuntime sink receives tool_call_started / output_delta / finished
          → ToolExecutor.execute(ctx, updates, cancel)
          → normalize output into ToolInvocationResult
          → RuntimeHookRegistry.invoke(ToolAfterExecute)
          → RuntimeHookRegistry.invoke(ToolResultBeforeAppend)
      → sort ToolInvocationResult by call_index
      → return ToolBatchResult
  → Driver maps ToolBatchResult -> Rig tool result content
  → AgentRun::tool_results(results)
```

错误原则：除 abort/cancel 外，未知工具、未启用工具、schema invalid、policy denied、approval rejected、executor failed 都应变成 error tool result，而不是让 run 崩溃。任何已经进入会话历史的 assistant tool call，最终都必须有对应 tool result，避免下次 provider 请求出现 unresolved tool call。

## LLM 如何知道工具

保留 pi 的双通道：

1. Provider tool schema：active tools 的 `name`、`description`、`input_schema` 进入模型请求。
2. System prompt：active tools 的 `prompt_snippet` 和 `prompt_guidelines` 进入 `Available tools` 与 `Guidelines`。

`Prompt` 消费的是 `Tools.prompt_catalog()` 和 active tool names，不消费 `Tools::invoke_batch` 或 executor state。`ResourceManager` 不拥有工具 snippets/guidelines；工具提示素材来自 session-owned `Tools`。

## 内置工具命名

内置工具应对齐 pi 的稳定名称：

- 只读：`read`、`grep`、`find`、`ls`。
- 文件变更：`write`、`edit`。
- 补丁变更：`apply-patch`。
- 进程执行：`bash`。

UI 本地化只能改变 `label`，不能改变 LLM 可调用的 canonical tool name。

## 功能覆盖表

| 能力 | 设计位置 | 说明 |
| --- | --- | --- |
| LLM 返回 tool call 后可执行 | `Driver -> DriverHost -> SessionRuntime -> Tools::invoke_batch` | Driver 不直接执行工具 |
| Driver 不直接依赖 Tools | `DriverHost` | Driver 只调用 host seam |
| SessionRuntime 协调 Driver 和 Tools | `SessionRuntime` / `SessionDriverHost` | 注入 session/run/cwd/turn context |
| Tools 封装完整工具行为 | `tools/subsystem.rs` | registry、active、policy、approval、grant、executor 都在 Tools 内部 |
| 乱序执行 | `tools/coordinator.rs` | parallel 模式下 executor 可乱序完成 |
| 顺序执行 | `ToolExecutionMode::Sequential` / session config | 整批串行，后续可细化 lock-based serial |
| 结果按 LLM 原始顺序回填 | `call_index` + `coordinator.rs` | tool result 回填前按 `call_index` 排序 |
| 批量 tool calls | `ToolBatchRequest` / `invoke_batch` | 一次处理 Rig `CallTools { calls }` |
| 工具批准能力 | `approval.rs` | pending approval 状态机 |
| UI 从 snapshot 恢复 pending approval | `pending_approval_views()` | 只暴露 UI-safe view |
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
| abort/cancel 传播 | `invoke_batch(..., cancel)` | approval wait 和 executor 都要响应 cancel |
| unresolved tool call 避免会话带毒 | `coordinator.rs` + session persistence | 每个已持久化 tool call 最终必须有 tool result |
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
