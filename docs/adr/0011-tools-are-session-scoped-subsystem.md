# ADR 0011: Tools 是 SessionRuntime 内部的 Session-Scoped 子系统

状态：ownership部分被[ADR 0025](0025-loaded-session-uses-one-session-executor.md)替代
日期：2026-07-07

ADR 0025和当前[Tool子系统目标设计](../refactor/tool-subsystem.md)将Tools ownership改为MiniCoreRuntime-owned ToolService + Turn-pinned ToolSet，并由SessionExecutor通过ToolExecutionControl协调approval和execution-start记录。本文保留为pre-refactor历史决策。

## 背景

MiniCore 使用 Rig `AgentRun / AgentRunStep` 的 sans-IO 路径。Rig 负责模型何时请求工具、工具结果如何回填以及是否继续下一次模型调用；MiniCore 必须保留产品级工具治理，包括 active tools、policy、approval、sandbox、mutation lock、事件归约和 session persistence。

早期文档中使用 gateway 式门面描述工具执行，容易误导为 `Driver` 直接依赖某个工具执行实例，或工具执行是一层独立 runtime。随着 pending approval 恢复、批量 tool calls、并行/串行执行、grant 和 sandbox source of truth 继续细化，工具能力需要收敛为一个明确的 session-scoped 子系统。

## 决策

`Tools` 是 `SessionRuntime` 内部的 session-scoped 工具子系统。每个 `SessionRuntime` 持有自己的 `Tools` 实例，`Tools` 封装：

- `ToolRegistry` / `ActiveToolSet` / `ToolPromptCatalog`。
- `ToolPolicy`。
- `ToolApprovalBroker` 和 `PendingToolApproval`。
- `ToolApprovalGrantStore` 和 `ToolApprovalMode`。
- `ToolInvocationPlanner`。
- `ToolExecutionCoordinator`。
- `ToolExecutorRegistry`、builtin executors、sandbox 和 mutation queue。

`SessionRuntime` 负责协调 `Driver` 和 `Tools`：

```text
Rig AgentRunStep::CallTools
  -> Driver
  -> DriverHost::invoke_tool_batch(...)
  -> SessionRuntime / SessionDriverHost
  -> ToolBatchInvoker.invoke_batch(...)
  -> Result<ToolBatchResult, ToolBatchError>
  -> SessionRuntime commits complete ToolRound
  -> Driver receives committed ToolBatchResult or typed ToolBatchHostError
  -> AgentRun::tool_results(...)
```

`Driver` 不直接依赖 `Tools`。`DriverHost` 是 trait/interface seam，不是 `Driver` 的实例。按 ADR 0021，生产 `SessionRuntime` actor 不直接实现 `DriverHost`；它为每次 run 创建 owned `SessionDriverHost`，并只向 RunTask 提供 run-only `ToolBatchInvoker`。

按 [ADR 0021](0021-session-runtime-separates-actor-control-from-run-execution.md)，生产实现使用由短期 `RunTask` 持有的 owned `SessionDriverHost` wrapper；本 ADR 原先长期借用 `&mut Tools` / queues / `CurrentRun` 的草图已被修订：

```text
SessionRuntime actor::start_run
  -> build TurnState
  -> project DriverTurnInput into DriveRequest
  -> establish CurrentRun + publish run_started
  -> create SessionDriverHost { ToolBatchInvoker, RunLink, progress sink, turn_resources, ModelGateway handle, cancel, ... }
  -> spawn run-scoped RunTask { Driver, DriveRequest, SessionDriverHost }
  -> actor returns to mailbox loop
```

`SessionDriverHost` 不是额外 runtime，也不拥有 session state。它只保存 run-scoped context 和窄 capabilities；safe-point、完整 tool-round commit 和 terminal effect 通过私有 `RunLink` 回到 owner actor。`Tools` 仍由该 `SessionRuntime` 生命周期拥有；actor 保留 admin/decision 能力，active RunTask 只通过 immutable-profile `ToolBatchInvoker` 执行 batch，不能跨整个 `drive_run().await` 借用 `&mut Tools`。

`Tools` 不直接发布 UI event，不写 session storage，不调用 `ModelGateway`，不读取 `ResourceManager`，不构建最终 system prompt。所有工具内部 update 通过 `ToolUpdateSink` 返回，由 `SessionRuntime` 归约成 `agent_runtime_protocol::EventMsg::ToolCall(...)`、snapshot projection，并在完整 round 结束后组装 `SessionWritePurpose::ToolRound` batch。

工具安全语义分为两层：MiniCore 进程内 builtin executor 负责可测试的 path authorization 和安全文件操作；通用子进程的 filesystem/network/process-tree 限制必须由 OS-native 或 external sandbox adapter 强制。MVP 不启用 `bash`。后续请求 `Sandboxed` shell 时，effective backend capabilities 不满足策略必须 fail closed；显式 `FullAccessWithApproval` 只是无隔离的高风险执行模式，approval 不得被描述为 sandbox，也不能把缺失 enforcement 的请求静默降级到该模式。

## 理由

这种边界保留 Rig 的协议级状态机优势，同时让 MiniCore 掌握工具副作用的产品控制权。

模型 API 通常只控制模型是否可以一次返回多个 tool calls，例如 OpenAI / Mistral 的 `parallel_tool_calls`、Anthropic 的 `disable_parallel_tool_use`；它们不提供每个 tool call 的宿主本地执行策略。pi 的实践也把模型返回多个 calls 与本地执行策略分开：默认 parallel，session config 或任一 tool definition sequential 时整批串行，并用 file mutation queue 保护同文件写入。

因此 MiniCore 把 provider capability、LLM source order 和本地 execution policy 分开：`ModelGateway` 负责 provider 请求能力，`Driver` 负责 Rig step 驱动，`SessionRuntime` 注入 session/run/cwd/turn context，`Tools` 负责工具治理和执行。

`SessionDriverHost` 的取舍来自代码层面的 grilling：

- 如果直接写 `self.driver.drive_run(request, self, cancel).await` 或让 wrapper 长期借用 actor fields，除了自借用压力，还会让 per-session mailbox 在完整 run 期间无法处理 approval/abort/queue command。
- 如果直接 `impl DriverHost for SessionRuntime`，host 方法可以访问整个会话对象，并迫使 actor owner 与耗时 run future 共享 mutable access。owned wrapper 明确只暴露本次 run 需要的 handles/context。
- `run_id`、turn resources、event correlation、checkpoint context 等是 run-scoped 数据，不应被迫成为 `SessionRuntime` 的长期字段。`DriveRequest` 只携带 `DriverTurnInput`，turn resources 留在 wrapper 中，wrapper run 结束即 drop，生命周期更准确。
- `Driver` 单测可以使用 fake host，不需要构造真实 `SessionRuntime`、`Tools`、storage 或 event bus。

## 后果

正面后果：

- `Driver` 更深，只依赖 `DriverHost` seam，不持有 provider、tools、queue 或 persistence。
- `SessionRuntime` 是唯一能把工具 update 归约为 UI event，并通过 `SessionWriter` 提交完整 tool round 的 owner。
- 每个 loaded session 的 pending approval 可以通过对应 `RuntimeSnapshot.loaded_sessions[*].current_run.pending_tool_approvals` 恢复，同时冻结的 `prepared_args` 不进入 snapshot/event/log。
- 批量工具调用可以通过 `ToolCallIndex` 稳定回填，parallel 执行可乱序完成但对模型结果顺序稳定。
- `ToolPolicy` 可保持纯判断器；preview、canonicalization、sandbox check 由 planner 负责。
- approval mode 和 remembered grants 与 pending approval 分离，避免把长期授权塞进当前等待态。

约束和代价：

- `Tools` 不能被设计成全局服务；不同 session 的 active tools、approval mode、pending approvals 和 grants 默认隔离。
- `DriverHost::invoke_tool_batch(...)` 的 host 实现必须注入足够的 `ToolRunContext`，否则 `Tools` 不应自行读取外部 runtime state。
- `ToolBatchInvoker.invoke_batch(...)` 等待 approval 时，`SessionRuntime` actor 必须仍能通过 actor-owned `Tools::decide_approval(...)` 唤醒 broker；内部实现不得持锁等待 decision。
- 所有外部工具来源必须注册为 `RegisteredTool` 后走同一路径，不能绕过 registry/policy/approval/sandbox。
- 当前架构术语、模块名和文件规划统一使用 `Tools`，不再把 gateway 作为工具子系统名称。

## 未选择方案

未选择让 `Driver` 直接持有或调用 `Tools`。这会把 Rig step adapter 变成产品 runtime owner，增加借用、测试和暂停恢复复杂度。

未选择继续保留 gateway 式独立门面。随着 approval、grants、batch coordinator、sandbox 和 mutation lock 增加，gateway 名称过浅，不能表达工具子系统的完整 ownership。

未选择把工具执行交给 Rig high-level runner。这样会削弱审批、事件、session persistence、abort/resume 和 sandbox 控制。
