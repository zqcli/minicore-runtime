# Tools

`tools.rs` / `tools/` 是和 `skills.rs`、`resource_manager.rs`、`prompt.rs` 平级的工具能力模块。它提供工具定义、工具注册、工具策略、审批代理、工具网关、内置工具、外部工具适配、schema、prompt metadata 和 executor helper；它本身不是独立运行时。

工具有真实副作用，因此工具状态和工具治理不能像技能那样只停留在 catalog 层。本项目采用：

```text
Tools module
  provides: ToolDefinition, RegisteredTool, ToolPolicy, ToolApprovalBroker,
            ToolGateway, builtin/external providers, executor traits, helper types

SessionRuntime
  owns instances: ToolRegistry, ActiveToolSet, ToolPolicy, ToolApprovalBroker,
                  ToolGateway, pending tool calls

Driver
  consumes: DriverHost::invoke_tool when Rig AgentRunStep::CallTools appears
```

## 设计决定

本项目不把工具执行交给 Rig 高阶 runner。我们使用 Rig 的 `AgentRun / AgentRunStep` sans-IO 路径，并在 `CallTools` step 中由 `Driver` 通过 `DriverHost::invoke_tool` 进入 `ToolGateway`：

```text
Rig AgentRun
  owns: CallModel / CallTools / Done 状态机、turn counting、tool-result threading

Driver
  owns: 驱动 AgentRun、调用 DriverHost seam、把 CallTools 转成工具调用请求、把结果喂回 AgentRun::tool_results(...)

ToolGateway
  owns: 产品级工具治理和真实执行入口
```

这不是重新实现 Agent loop。Rig 仍决定下一步是什么；本项目只保留工具副作用的产品控制权。

`ToolPolicy` 和 `ToolApprovalBroker` 属于 `tools` public module，而不是新的顶层 runtime module。文件组织上建议拆成 `tools/policy.rs` 和 `tools/approval.rs`，再由 `tools.rs` re-export；调用方只看到 `crate::tools::ToolPolicy`、`crate::tools::ToolApprovalBroker` 和 `crate::tools::ToolGateway`。这样既保持工具领域边界集中，又避免一个巨大的 `tools.rs` 文件把策略、审批、执行和内置工具混在一起。

术语上要区分：

- `ToolPolicyDecision`：运行时策略判断结果，可能是 allow、deny、require approval、rewrite args 或 abort。
- `agent_runtime_protocol::ToolApprovalDecision`：用户对某个 pending approval 的协议回答，只能是 approve 或 reject。

## Rig 能力分析

Rig 有两条工具路径：

| Rig 使用方式 | 工具执行者 | 适用场景 | 本项目是否采用 |
| --- | --- | --- | --- |
| 高阶 `Agent::prompt()` / `PromptRequest` / streaming prompt | Rig 内部通过 `ToolServerHandle::call_tool(...)` 执行 | 简单 agent、无复杂审批、无暂停恢复 | 不作为主路径 |
| `AgentRun` / `AgentRunStep` sans-IO | 外部 driver 执行 `CallTools`，再调用 `AgentRun::tool_results(...)` | 桌面 Agent、审批、沙箱、事件、持久化、暂停恢复 | 主路径 |

Rig 文档把 `AgentRun` 定义为 sans-IO、steppable、serializable 状态机。`AgentRunStep::CallTools { calls }` 的含义是：driver 必须安排外部执行这些工具调用，并把结果喂回 `AgentRun::tool_results(...)`。

因此本项目的主路径是：

```text
AgentRun::next_step()
  ├─ CallModel: Driver 调 host.call_model(...)，然后 AgentRun::model_response(...)
  ├─ CallTools: Driver 调 host.invoke_tool(...)，host 内部走 ToolGateway，然后 AgentRun::tool_results(...)
  └─ Done: Driver 返回完成结果给 SessionRuntime
```

不要把产品工具直接交给 Rig `ToolServerHandle` 自动执行，否则审批、事件、pending 状态、session writes、workspace trust 和 mutation queue 都会被塞进 Rig wrapper，运行时边界会变浅。

## 来自 pi 的工具分层

pi coding-agent 也没有独立 `ToolRuntime`：

```text
dist/core/tools/
  ├─ read / write / edit / bash / grep / find / ls
  ├─ createAllToolDefinitions()
  └─ ToolDefinition factories

AgentSession
  ├─ _baseToolDefinitions
  ├─ _toolDefinitions
  ├─ _toolRegistry
  ├─ _toolPromptSnippets
  ├─ _toolPromptGuidelines
  ├─ _refreshToolRegistry()
  └─ setActiveToolsByName()

pi-agent-core agent-loop
  └─ protocol-level tool call loop
```

关键经验：

- `tools/` 提供工具定义和实现。
- `AgentSession` 持有工具注册表、活跃工具集合和工具提示词素材。
- `setActiveToolsByName()` 更新 active tools，并触发 system prompt 重建。
- LLM 通过两条通道了解工具：provider tool schema + system prompt 中的 snippets/guidelines。
- 工具执行必须可被产品层观察、拦截和记录。

本项目比 pi 多一个 `ToolGateway` 概念，用来把 Rig sans-IO `CallTools` step 接到产品级工具治理，但它仍然是 `SessionRuntime` 内部执行门面，不是独立 runtime。

## 与 Skills 的相似和不同

| 能力 | 平级模块 | 状态 owner | 调用路径 |
| --- | --- | --- | --- |
| Skills | `skills.rs` | `ResourceManager` 的 `CwdResourceSnapshot.resolved` 持有 effective `SkillCatalog` | `SessionRuntime` 从 captured `TurnResourceSnapshot` 展开成 user message |
| Tools | `tools.rs` / `tools/` | `SessionRuntime` 持有 `ToolRegistry` / `ActiveToolSet` | `Driver -> DriverHost::invoke_tool -> ToolGateway -> ToolExecutor` |

相同点：二者都是平级能力模块，不应命名成独立 runtime。

不同点：工具会产生本地副作用，所以需要 `ToolPolicy`、`ToolApprovalBroker`、`ToolGateway`、abort signal、事件和 session persistence。

## 模块结构

建议代码布局：

```text
src/
  tools.rs
  tools/
    definition.rs
    registry.rs
    policy.rs
    approval.rs
    gateway.rs
    providers.rs
    builtin/
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
pub use registry::{ToolRegistry, ActiveToolSet, ToolPromptCatalog};
pub use gateway::{ToolGateway, ToolInvocation, ToolInvocationResult};
pub use policy::{ToolPolicy, ToolPolicyConfig, ToolPolicyInput, ToolPolicyDecision};
pub use approval::{ToolApprovalBroker, ToolApprovalRequest, PendingToolApproval};
pub use providers::{BuiltinToolProvider, ExternalToolProvider};
```

## ToolGateway 位置

```text
SessionRuntime
  ├─ ToolRegistry          // 所有已知工具
  ├─ ActiveToolSet         // 暴露给模型的工具
  ├─ ToolPromptCatalog     // snippets / guidelines
  ├─ ToolPolicy            // allow / deny / rewrite / approval / abort
  ├─ ToolApprovalBroker    // pending approval
  ├─ ToolGateway           // DriverHost::invoke_tool 使用的执行门面
  └─ Driver
       └─ AgentRunStep::CallTools -> DriverHost::invoke_tool(...) -> ToolGateway.invoke(...)
```

`ToolGateway` 只通过 `SessionRuntime` 的 `DriverHost::invoke_tool` 实现被使用。UI、`ResourceManager`、`Prompt` 和 `SessionStorage` 都不直接执行工具。`ToolGateway` 可以接收 `SessionRuntime` 传入的工具更新 sink 上报进度，但不能绕过 `SessionRuntime` 自行发布 UI event。

`ToolGateway` 消费：

- `ToolRegistry`。
- `ActiveToolSet`。
- `ToolPolicy`。
- `ToolApprovalBroker`。
- `RuntimeHookRegistry`。
- `ToolExecutor`。
- `ToolUpdateSink` / `SessionRuntime` 内部工具更新 sink。
- `SessionRuntime` 提供的 abort signal / run context / pending write hooks。

## ToolPolicy And ToolApprovalBroker

`ToolPolicy` 是纯策略判断器。它不等待 UI，不执行工具，也不发布事件；它只根据工具定义、参数、workspace trust、sandbox 结果、用户设置和 hook overlay 返回一个 `ToolPolicyDecision`。

```rust
pub struct ToolPolicyInput {
    pub tool: ToolDefinition,
    pub invocation: ToolInvocation,
    pub prepared_args: serde_json::Value,
    pub workspace_trust: TrustLevel,
    pub sandbox: ToolSandboxView,
    pub hook_requirement: Option<ToolApprovalRequirement>,
}

pub enum ToolPolicyDecision {
    Allow,
    Deny { reason: String },
    RequireApproval { reason: String, preview: agent_runtime_protocol::ToolApprovalPreview },
    RewriteArgs { args: serde_json::Value },
    AbortRun { reason: String },
}
```

`ToolApprovalBroker` 是 pending approval 状态机。它只管理“正在等用户批准的工具调用”，通过 `SessionRuntime` 触发 `tool_call_approval_requested`，再等待 `agent_runtime_protocol::Command::DecideToolApproval`。用户回答的 payload 类型属于 `agent_runtime_protocol::ToolApprovalDecision`，因为下游 UI 只能依赖协议层；`ToolApprovalBroker` 只是消费这个 protocol decision 并恢复等待中的工具调用。

```rust
pub struct ToolApprovalRequest {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub reason: String,
    pub preview: agent_runtime_protocol::ToolApprovalPreview,
}

pub struct PendingToolApproval {
    pub request: ToolApprovalRequest,
    pub prepared_args: serde_json::Value,
    pub created_at: Timestamp,
}
```

Approval 之后不允许再改 args；如果 hook 或 policy 在审批前 rewrite args，必须重新 schema validate、重新 sandbox check，并重新执行 `ToolPolicy.evaluate(...)`。

## 工具加载

所有工具来源先归一化成 `RegisteredTool`：

```rust
pub struct RegisteredTool {
    pub definition: ToolDefinition,
    pub executor: Arc<dyn ToolExecutor>,
    pub source: ToolSource,
}
```

工具来源：

```text
BuiltinToolProvider
  → read / grep / find / ls / write / edit / apply-patch / bash

StaticCustomToolProvider
  → 应用内静态注册工具

ExtensionToolProvider
  → 后续扩展提供的工具

McpToolProvider
  → 后续 MCP server tools
```

刷新流程：

```text
SessionRuntime.refresh_tool_registry()
  → load builtin tools
  → load static custom tools
  → load extension tools
  → load MCP tools
  → apply allowlist / denylist
  → resolve collisions
  → store ToolRegistry
  → update ToolPromptCatalog
  → preserve or compute ActiveToolSet
  → rebuild system prompt on next turn
```

外部工具不会直接调用 `ToolGateway`。它们只提供 `ToolDefinition + ToolExecutor`，进入 `ToolRegistry` 后走同一条执行路径。

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

pub struct ToolInvocation {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub call_id: ToolCallId,
    pub name: ToolName,
    pub raw_args: serde_json::Value,
}

pub struct ToolInvocationResult {
    pub call_id: ToolCallId,
    pub name: ToolName,
    pub content: Vec<MessageContent>,
    pub details: Option<serde_json::Value>,
    pub is_error: bool,
    pub terminate: bool,
}

// `agent_runtime_protocol::ToolApprovalPreview` owns the UI-visible preview shape.
// ToolPolicy constructs it; ToolApprovalBroker carries it while waiting.
```

## ToolGateway 流程

```text
Rig AgentRunStep::CallTools { calls }
  → Driver maps PendingToolCall -> ToolInvocation
  → DriverHost::invoke_tool(invocation)
  → ToolGateway.invoke(invocation)
      → ToolRegistry.get(name)
      → ActiveToolSet.contains(name)
      → prepare_arguments(raw_args)
      → validate_schema(prepared_args)
      → SessionRuntime event sink emits `tool_call_proposed`
      → RuntimeHookRegistry.invoke(ToolCallProposed)
      → RuntimeHookRegistry.invoke(ToolBeforePolicy)
          ├─ Deny -> error tool result
          ├─ RewriteArgs -> revalidate schema and re-run policy
          ├─ RequireApproval -> approval requirement overlay
          └─ AbortRun -> cancellation
      → ToolPolicy.evaluate(...)
          ├─ Allow
          ├─ Deny -> error tool result
          ├─ RewriteArgs -> revalidate
          ├─ RequireApproval -> ToolApprovalBroker waits for DecideToolApproval
          └─ AbortRun -> cancellation
      → after approval, freeze prepared_args
      → SessionRuntime event sink emits `tool_call_started`
      → ToolExecutor.execute(ctx, updates)
          └─ updates -> SessionRuntime event sink emits `tool_call_output_delta`
      → normalize output into ToolInvocationResult
      → RuntimeHookRegistry.invoke(ToolAfterExecute)
      → RuntimeHookRegistry.invoke(ToolResultBeforeAppend)
      → SessionRuntime event sink emits `tool_call_finished`
  → Driver maps ToolInvocationResult -> Rig tool result content
  → SessionRuntime appends tool-result message and emits `message_tool_result_appended`
  → AgentRun::tool_results(results)
```

错误原则：除 abort 外，未知工具、未启用工具、schema invalid、policy denied、approval rejected、executor failed 都应变成 error tool result，而不是让 run 崩溃。

## LLM 如何知道工具

保留 pi 的双通道：

1. Provider tool schema：active tools 的 `name`、`description`、`input_schema` 进入模型请求。
2. System prompt：active tools 的 `prompt_snippet` 和 `prompt_guidelines` 进入 `Available tools` 与 `Guidelines`。

`Prompt` 消费的是 `ToolPromptCatalog` 和 active tools，不消费 `ToolGateway`。

## 内置工具命名

内置工具应对齐 pi 的稳定名称：

- 只读：`read`、`grep`、`find`、`ls`。
- 文件变更：`write`、`edit`。
- 补丁变更：`apply-patch`。
- 进程执行：`bash`。

UI 本地化只能改变 `label`，不能改变 LLM 可调用的 canonical tool name。

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
- 不要让 `ToolGateway` 独立拥有状态；状态归 `SessionRuntime`，gateway 只是执行门面。
- 不要让外部工具绕过 registry/policy/approval。所有工具来源必须归一化成 `RegisteredTool`。
- 不要让 UI 执行工具。UI 只能发送审批决策。
- 不要把 tool snippets/guidelines 放进 `ResourceManager`。工具提示素材来自 session-owned active tools。
