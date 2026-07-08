# ADR 0011: Tools 是 SessionRuntime 内部的 Session-Scoped 子系统

状态：已接受
日期：2026-07-07

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
  -> Tools::invoke_batch(...)
  -> ToolBatchResult
  -> Driver
  -> AgentRun::tool_results(...)
```

`Driver` 不直接依赖 `Tools`。`DriverHost` 是 trait/interface seam，不是 `Driver` 的实例。`SessionRuntime` 可以直接实现 `DriverHost`，也可以创建 per-run `SessionDriverHost` wrapper 来实现。

推荐真实实现使用 per-run `SessionDriverHost` wrapper：

```text
SessionRuntime::start_run
  -> build DriveRequest / TurnState
  -> clone turn_resources from DriveRequest.turn_state
  -> create SessionDriverHost { tools: &mut self.tools, turn_resources, model_gateway, event_sink, queues, current_run, ... }
  -> let driver = Driver::new()
  -> driver.drive_run(request, &mut host, cancel)
```

直接 `impl DriverHost for SessionRuntime` 是合法的最小实现；`SessionDriverHost` 不是额外 runtime，也不拥有 session state。它只是一次 `drive_run()` 期间把 `SessionRuntime` 的一小片能力借给 `Driver` 的 adapter。

`Tools` 不直接发布 UI event，不写 session storage，不调用 `ModelGateway`，不读取 `ResourceManager`，不构建最终 system prompt。所有工具内部 update 通过 `ToolUpdateSink` 返回，由 `SessionRuntime` 归约成 `agent_runtime_protocol::EventMsg::ToolCall(...)`、snapshot projection 和 session writes。

## 理由

这种边界保留 Rig 的协议级状态机优势，同时让 MiniCore 掌握工具副作用的产品控制权。

模型 API 通常只控制模型是否可以一次返回多个 tool calls，例如 OpenAI / Mistral 的 `parallel_tool_calls`、Anthropic 的 `disable_parallel_tool_use`；它们不提供每个 tool call 的宿主本地执行策略。pi 的实践也把模型返回多个 calls 与本地执行策略分开：默认 parallel，session config 或任一 tool definition sequential 时整批串行，并用 file mutation queue 保护同文件写入。

因此 MiniCore 把 provider capability、LLM source order 和本地 execution policy 分开：`ModelGateway` 负责 provider 请求能力，`Driver` 负责 Rig step 驱动，`SessionRuntime` 注入 session/run/cwd/turn context，`Tools` 负责工具治理和执行。

`SessionDriverHost` 的取舍来自代码层面的 grilling：

- 如果直接写 `self.driver.drive_run(request, self, cancel).await`，容易同时借用 `self.driver` 和 `&mut self`，给 Rust borrow checker 制造自借用压力。让 `Driver` 保持无状态或浅状态，并在 run start 时临时创建或 clone 一个 driver，再调用 `driver.drive_run(request, &mut host, cancel)`，可以避开这个形态。
- 如果直接 `impl DriverHost for SessionRuntime`，host 方法可以访问整个会话对象，长期会让 safe point、工具调用和模型调用逻辑随手耦合到不相关状态。wrapper 明确只暴露本次 run 需要的字段。
- `run_id`、turn resources、event correlation、checkpoint context 等是 run-scoped 数据，不应被迫成为 `SessionRuntime` 的长期字段。wrapper run 结束即 drop，生命周期更准确。
- `Driver` 单测可以使用 fake host，不需要构造真实 `SessionRuntime`、`Tools`、storage 或 event bus。

## 后果

正面后果：

- `Driver` 更深，只依赖 `DriverHost` seam，不持有 provider、tools、queue 或 persistence。
- `SessionRuntime` 是唯一能把工具 update 归约为 UI event 和 session writes 的 owner。
- pending approval 可以通过 `RuntimeSnapshot.active_session.current_run.pending_tool_approvals` 恢复，同时冻结的 `prepared_args` 不进入 snapshot/event/log。
- 批量工具调用可以通过 `ToolCallIndex` 稳定回填，parallel 执行可乱序完成但对模型结果顺序稳定。
- `ToolPolicy` 可保持纯判断器；preview、canonicalization、sandbox check 由 planner 负责。
- approval mode 和 remembered grants 与 pending approval 分离，避免把长期授权塞进当前等待态。

约束和代价：

- `Tools` 不能被设计成全局服务；不同 session 的 active tools、approval mode、pending approvals 和 grants 默认隔离。
- `DriverHost::invoke_tool_batch(...)` 的 host 实现必须注入足够的 `ToolRunContext`，否则 `Tools` 不应自行读取外部 runtime state。
- 所有外部工具来源必须注册为 `RegisteredTool` 后走同一路径，不能绕过 registry/policy/approval/sandbox。
- 当前架构术语、模块名和文件规划统一使用 `Tools`，不再把 gateway 作为工具子系统名称。

## 未选择方案

未选择让 `Driver` 直接持有或调用 `Tools`。这会把 Rig step adapter 变成产品 runtime owner，增加借用、测试和暂停恢复复杂度。

未选择继续保留 gateway 式独立门面。随着 approval、grants、batch coordinator、sandbox 和 mutation lock 增加，gateway 名称过浅，不能表达工具子系统的完整 ownership。

未选择把工具执行交给 Rig high-level runner。这样会削弱审批、事件、session persistence、abort/resume 和 sandbox 控制。
