# AgentRuntimeEvents

`AgentRuntimeEvents` 描述 `agent_runtime_protocol::Event` 的事件生命周期。它不是 UI 状态管理方案，也不是 session 持久化格式；它是运行时向 UI adapter 发布事实的唯一通道。

设计参考：

- Rust 常见风格：内部类型和 enum variant 使用 `PascalCase`，序列化到 JSON 时常用 `snake_case`。
- pi-agent-core `AgentEvent`：`agent_start`、`turn_start`、`message_start/update/end`、`tool_execution_start/update/end`、`turn_end`、`agent_end`。
- pi coding-agent `AgentSessionEvent`：在核心事件外增加 `queue_update`、`compaction_start/end`、`auto_retry_start/end`、`thinking_level_changed`、`session_info_changed`，并在 session 层处理持久化和 extension events。
- Codex `Submission Queue / Event Queue`：用户提交进入 submission queue，agent 输出进入 event queue；`Event` 带 submission id；Rust enum variant 用 `PascalCase`，wire event type 用 `snake_case`，长任务使用 `Begin / Delta / End` 或 `ItemStarted / ItemCompleted` 配对。

## 推荐结论

1. `agent_runtime_protocol::Event` 是 UI 收到的一条完整事件记录，不是内部 hook，也不是 session log。
2. `agent_runtime_protocol::Event.msg` 使用分组 enum，例如 `agent_runtime_protocol::EventMsg::Run(RunEvent::Started)`。
3. UI/wire 事件类型使用 flat `snake_case`，例如 `run_started`、`tool_call_output_delta`。
4. 所有 UI 事件都携带顺序号、时间、workspace/session/run/command correlation；这些 metadata 属于 `agent_runtime_protocol::Event` 外层记录。
5. `SessionRuntime` 是把内部事件归约成 UI 事件的中心；`DriverEvent`、`Tools` update、后期 `RuntimeHooks` typed result 不能直接泄漏给 UI。
6. 长生命周期对象必须有明确 started/delta/finished 配对，例如 assistant message、tool call、run、compaction、resource reload。
7. 流式 delta 不是持久化事实；`persistence_save_point` 才是“此前相关 session writes 已经落盘”的 durable barrier。
8. `session_settled` 表示 UI 可认为该 session 当前没有立即继续的 run/compaction/retry、pending session action 或 queued continuation，并且 phase 已经回到 `idle`。
9. 同步命令接收用 `CommandAck` 表达；异步执行结果和用户可见命令结果用 `agent_runtime_protocol::Event` 表达。
10. `session_focus_changed` 是 runtime-visible event，表示默认会话目标改变；它不表示 session loaded、running 或 closed。

## Naming Convention

事件命名分两层：

```text
Rust internal:
  agent_runtime_protocol::EventMsg::Family(FamilyEvent::Transition)

UI / wire:
  family_subject_transition
```

示例：

| Rust 内部事件 | Wire event type |
| --- | --- |
| `agent_runtime_protocol::EventMsg::Run(RunEvent::Started)` | `run_started` |
| `agent_runtime_protocol::EventMsg::Run(RunEvent::Suspended)` | `run_suspended` |
| `agent_runtime_protocol::EventMsg::Run(RunEvent::Resumed)` | `run_resumed` |
| `agent_runtime_protocol::EventMsg::Run(RunEvent::Finished)` | `run_finished` |
| `agent_runtime_protocol::EventMsg::Message(MessageEvent::AssistantTextDelta)` | `message_assistant_text_delta` |
| `agent_runtime_protocol::EventMsg::ToolCall(ToolCallEvent::ApprovalRequested)` | `tool_call_approval_requested` |
| `agent_runtime_protocol::EventMsg::Persistence(PersistenceEvent::SavePoint)` | `persistence_save_point` |

不推荐把内部 module 名写进事件名：

```text
// 不推荐
SessionRuntimeRunStarted
ToolsToolCallStarted
ResourceManagerResourcesChanged
```

推荐事件名表达公开生命周期族，而不是实现归属：

```text
// 推荐
run_started
tool_call_started
resources_changed
```

owner module 通过文档表格说明，不进入 wire event type。

### Transition Words

| 词 | 用途 | 示例 |
| --- | --- | --- |
| `started` | 生命周期开始 | `run_started`、`tool_call_started` |
| `suspended` | 可恢复暂停，不是终态 | `run_suspended` |
| `resumed` | 从可恢复暂停继续 | `run_resumed` |
| `finished` | 生命周期终态，可携带 status | `run_finished`、`compaction_finished` |
| `delta` | 流式增量 | `message_assistant_text_delta`、`tool_call_output_delta` |
| `changed` | 配置、资源或名称变更 | `session_model_changed`、`resources_changed` |
| `updated` | 替换式状态更新 | `queue_updated`、`usage_updated` |
| `appended` | transcript/session view 追加 | `message_user_appended`、`command_output_appended` |
| `requested` | 等待外部动作 | `tool_call_approval_requested`、`command_interaction_requested` |

本项目统一使用 `started/finished`，不混用 `begin/end/complete`。`finished` 只覆盖 `completed`、`failed`、`aborted` 等终态，具体结果放在对应 event msg 的 `status` 字段。可恢复暂停使用 `run_suspended` / `run_resumed`，不能表达为 `run_finished { status: paused }`。

## Event And EventMsg

`agent_runtime_protocol::EventStream` 传输完整的 `agent_runtime_protocol::Event`。它借鉴 Codex 的 `Event { id, msg }` 形态：外层事件记录负责“这条事实属于谁、排在第几、由哪个命令引起、重连时是否已经见过”；`msg` 只描述业务事实本身。

```text
agent_runtime_protocol::Event
  event_id
  sequence
  timestamp
  workspace_id?
  session_id?
  run_id?
  command_id?
  msg: agent_runtime_protocol::EventMsg
```

```text
agent_runtime_protocol::Event
  owns: routing, ordering, correlation, replay/reconnect cursor

agent_runtime_protocol::EventMsg
  owns: business fact visible to UI
```

推荐 wire 形态保持 Codex-like 外层事件 + 内层消息：

```json
{
  "event_id": "evt_...",
  "sequence": 42,
  "session_id": "ses_...",
  "run_id": "run_...",
  "msg": {
    "type": "run_started",
    "run_id": "run_...",
    "session_id": "ses_..."
  }
}
```

稳定 event type 位于 `msg.type`；外层 `agent_runtime_protocol::Event` 字段只负责 routing、ordering、correlation 和 reconnect cursor。

字段规则：

| 字段 | 谁生成 | 是否稳定 | 主要用途 |
| --- | --- | --- | --- |
| `event_id` | `AgentRuntime` event bus | 全局唯一 | 日志、去重、测试断言、telemetry correlation |
| `sequence` | `AgentRuntime` event bus | 单 runtime 单调递增 | UI reducer 顺序、snapshot 水位、重连过滤 |
| `timestamp` | `AgentRuntime` event bus | 事件发出时间 | UI 展示、排序辅助、耗时计算 |
| `workspace_id` | runtime routing layer | 可空 | workspace 级状态归属 |
| `session_id` | runtime routing layer | 可空 | session 级状态归属 |
| `run_id` | `SessionRuntime` | 可空 | 一次 agent run 的 correlation |
| `command_id` | `dispatch()` | 可空 | 关联用户命令与后续异步事件 |
| `msg` | `AgentRuntime` / `SessionRuntime` | 不可空 | 业务事实 |

`sequence` 和 `event_id` 不互相替代：

- `sequence` 是本地顺序号，适合 UI 重连和 reducer 顺序检查。
- `event_id` 是唯一标识，适合跨日志、telemetry 和测试定位。

### Routing Profiles

| 事件层级 | `workspace_id` | `session_id` | `run_id` | `command_id` |
| --- | --- | --- | --- | --- |
| runtime diagnostics | `None` 或相关 workspace | `None` | `None` | 可空 |
| workspace resources | 必填 | `None` | `None` | reload 命令 id |
| session list/create/delete | 必填 | 目标 session 可填 | `None` | 触发命令 id |
| open session | 必填 | 新 session 必填 | `None` | 触发命令 id |
| session phase/queue/config | 必填 | 必填 | 通常 `None` | 触发命令 id 或 `None` |
| run lifecycle | 必填 | 必填 | 必填 | 启动 run 的命令 id |
| assistant message stream | 必填 | 必填 | 必填 | 启动 run 的命令 id |
| tool call | 必填 | 必填 | 必填 | 启动 run 的命令 id；审批响应另有自己的 command id |
| compaction | 必填 | 必填 | 可空 | 手动 compact 命令 id；自动压缩可空 |
| command result | 可空或相关 workspace | 可空或目标 session | 通常 `None` | 触发输出或交互请求的命令 id |
| persistence save point | 必填 | 必填 | 可空 | 导致写入的命令 id 或 run command id |
| settled | 必填 | 必填 | `None` | 通常可空 |

自动行为不一定有 `command_id`：例如 threshold auto-compaction、auto-retry。此时 `agent_runtime_protocol::Event` 依赖 `workspace_id` / `session_id` / `run_id` 关联上下文。MVP 的资源更新通过显式 reload / startup ensure 进入 `ResourceManager`，不设计文件监听器或热更新事件源。

### agent_runtime_protocol::Event 生成点

推荐只有两个地方生成 UI 可见的 `agent_runtime_protocol::Event`：

```text
AgentRuntime event bus
  assigns event_id + sequence + timestamp to workspace/runtime/session-list events

SessionRuntime event sink
  submits session/run/tool/message events to AgentRuntime event bus
  AgentRuntime event bus assigns event_id + sequence + timestamp
```

`Driver`、`Tools`、`ResourceManager`、`Compaction` 不直接生成 UI 可见的 `agent_runtime_protocol::Event`。后期 `RuntimeHooks` 只返回 typed result。它们的结果由 `SessionRuntime` / `AgentRuntime` 归约。

### RuntimeSnapshot 水位

`agent_runtime_protocol::RuntimeSnapshot.last_event_sequence` 是 UI 在同一 host 生命周期内看到的权威状态水位：

```text
RuntimeSnapshot contains state reduced through sequence = N
event stream contains events N+1, N+2, ...
UI ignores events <= N
```

如果 UI reducer/subscriber 发现事件断号，说明内存 ring buffer 不足或同一 host 生命周期内的订阅中断过久：

```text
expected sequence = N + 1
received sequence > N + 1
  → discard local incremental assumptions
  → request fresh RuntimeSnapshot
  → continue from new snapshot.last_event_sequence
```

session JSONL 不是 runtime event log，`RuntimeSnapshot` 也不是单独持久化的文件。不能要求 UI 通过 session entries 重放 `message_assistant_text_delta`、`tool_call_output_delta` 或 approval waiting 状态；这些瞬时状态必须从 runtime 内存投影出的 `RuntimeSnapshot` 或新事件恢复。active session 当前 run 的 approval waiting 状态投影在 `RuntimeSnapshot.active_session.current_run.pending_tool_approvals`。关闭窗口后，下一次打开 workspace 时重新生成 `RuntimeSnapshot`，默认没有 active session。

MVP 不支持 UI adapter 失败/断线但 `AgentRuntime` 作为独立 daemon 继续运行再被重连的故障模型。UI host 和 runtime 同生命周期；因此 reconnect contract 只覆盖同一程序上下文内的初始化、late subscribe、reducer/subscriber 重建和 sequence gap recovery。`RuntimeSnapshot` 不需要为非 active/background session 提供完整恢复投影；若未来支持独立 runtime server、多窗口共享 runtime 或 daemon 模式，再引入 all-loaded-session snapshot 或 scoped event cursor。

## Command Ack

`agent_runtime_protocol::CommandAck` 只说明命令是否被运行时接收，不返回助手文本，也不返回 command text 的查询或展示结果。事件文档只解释它和异步事件的关系，结构定义以 [AgentRuntimeProtocol](agent-runtime-protocol.md) 为准。

原则：

- runtime 无法接收或无法路由的命令，例如 runtime 已关闭、workspace 不存在、目标 session id 无效，返回 `CommandAck { accepted: false }` 或 `RuntimeError`。
- 对 `ExecuteCommandText` / `ExecuteCatalogCommand`，unknown command、参数非法、phase 不允许这类用户级错误，优先返回 `CommandAck { accepted: true }`，再发 `command_output_appended { severity: Error }`，让下游 CLI/TUI/GUI 的 message panel 或等价输出呈现一致。
- 已接受命令的后续成功、失败、审批、流式输出，都通过 `agent_runtime_protocol::Event`。
- UI 不应把 `CommandAck` 当成最终状态，只能当成“命令进入运行时”的确认。

## Event Families

| Family | Wire event type 示例 | 生命周期 | 是否可由 RuntimeSnapshot 重建 |
| --- | --- | --- | --- |
| session lifecycle | `session_created`、`session_opened`、`session_closed`、`session_focus_changed`、`session_tree_changed` | 离散事实 | 是 |
| run lifecycle | `run_started`、`run_suspended`、`run_resumed`、`run_finished` | started -> suspended/resumed* -> terminal | 是，当前 run 只能部分重建 |
| message lifecycle | `message_user_appended`、`message_assistant_started`、`message_assistant_text_delta`、`message_assistant_finished`、`message_tool_result_appended` | appended 或 started -> delta* -> finished | finished 后是，delta 不是 |
| tool call lifecycle | `tool_call_proposed`、`tool_call_approval_requested`、`tool_call_started`、`tool_call_output_delta`、`tool_call_finished` | proposed -> approval? -> started -> delta* -> finished | finished 后是，delta 不是 |
| queue lifecycle | `queue_updated` | 每次队列变化发完整队列摘要 | 是 |
| usage lifecycle | `usage_updated` | 模型调用、run 结束、压缩或 prompt material 改变后替换 usage/context view | 是 |
| resource lifecycle | `resources_reload_started`、`resources_changed` | reload started -> committed revision | 是 |
| command catalog | `command_catalog_changed` | catalog revision changed | 是 |
| command result | `command_output_appended`、`command_interaction_requested`、`command_interaction_resolved` | 用户命令产生 UI-safe 输出、display-neutral interaction 请求或交互结果 | 部分；取决于是否持久化 UI-only panel item |
| compaction lifecycle | `compaction_started`、`compaction_finished` | started -> finished | 是 |
| retry lifecycle | `retry_auto_started`、`retry_auto_finished` | started -> finished | 是 |
| diagnostics | `diagnostics_runtime_changed`、`diagnostics_error`、`diagnostics_warning` | 离散或替换式 | 部分 |
| persistence | `persistence_save_point` | durable barrier | 是 |
| idle lifecycle | `session_settled` | terminal convenience event | 是 |

## Event Sources

| Source | 可以发 UI `agent_runtime_protocol::Event` 吗 | 说明 |
| --- | --- | --- |
| `AgentRuntime` | 可以 | workspace、session open/close/focus/list/delete、runtime diagnostics、resource reload 入口、command catalog 和 command result。 |
| `SessionRuntime` | 可以 | 单会话 phase、run、queue、usage/context usage、message persistence、compaction、retry、save point。 |
| `Driver` | 不直接发 UI event | 只发 `DriverEvent` 给 `SessionRuntime` 归约。 |
| `ModelGateway` | 不直接发 UI event | 通过 `ModelStreamSink` / `DriverEvent` 上报模型流和 usage。 |
| `Tools` | 不直接绕过 `SessionRuntime` | 可以使用 `SessionRuntime` 传入的工具更新 sink，但所有 UI 事件仍由会话运行时归约并拥有 correlation 和 phase。 |
| `ResourceManager` | 不直接发 UI event | 管理 `ResourceSnapshotStore`、返回 reload/capture 结果和诊断；`AgentRuntime` 发布 `resources_changed`，`SessionRuntime` 在后续 user turn 捕获新 `TurnResourceSnapshot` 并重建 prompt。 |
| `CommandSurface` / `CommandManager` | 不直接发 UI event | 返回 command catalog、parse/suggest/resolve 结果和 UI-safe command output；`AgentRuntime` 发布 `command_catalog_changed`、`command_output_appended`、`command_interaction_requested` 并执行映射后的 `AgentCommand` / session command handler。 |
| `Compaction` | 不发 UI event | 只返回准备结果、摘要 prompt 和 result type；`SessionRuntime` 发事件。 |
| `RuntimeHooks` | 后期能力；不直接发 UI event | Hook 只返回 typed decision / patch / replacement；Hook 错误进入 diagnostics 或 `diagnostics_error`。 |

这比“每个模块都能 emit”更深：事件 routing、顺序、持久化和 RuntimeSnapshot 投影集中在 `AgentRuntime` / `SessionRuntime`，UI 不需要知道内部模块图。

## Event And State Ownership

事件所有权遵循一个原则：谁拥有对应状态机，谁负责把状态变化归约成 `agent_runtime_protocol::Event`。底层模块可以返回结果或内部事件，但不能自行对 UI 发布权威事件。

### Ownership Matrix

| 层级 | 持有/管理的状态 | 负责发布或归约的 UI wire events | 只消费/返回，不拥有的事件 |
| --- | --- | --- | --- |
| UI Adapter | UI 本地渲染状态、输入框草稿、滚动位置、选中面板、临时 optimistic affordance | 不发布 runtime event；只发 `agent_runtime_protocol::AgentCommand` | 消费 `agent_runtime_protocol::Event` 和 `agent_runtime_protocol::RuntimeSnapshot` |
| `AgentRuntime` | event bus、`sequence`、subscription、workspace、`WorkspaceServices`、runtime diagnostics、command output dispatch | 生成所有 UI 可见 `agent_runtime_protocol::Event` metadata；发布 `session_*`、`resources_*`、`command_catalog_changed`、`command_output_appended`、`command_interaction_requested`、`diagnostics_runtime_changed` 等应用级事件 | 消费 `SessionRuntime` 提交的 `agent_runtime_protocol::EventMsg` 和 routing ids；调用 `SessionManager` 协调会话生命周期 |
| `SessionRuntime` | `SessionPhase`、current run、queues、model state、`Tools` state、fixed workspace cwd、run-captured `TurnResourceSnapshot`、compaction/retry state、pending session writes、RuntimeSnapshot projection state | 发布/归约绝大多数 `run_*`、`message_*`、`tool_call_*`、`queue_updated`、`compaction_*`、`retry_*`、`persistence_save_point`、`session_settled` | 消费 `DriverEvent`、`Tools` progress/update、`SessionHandle` 写入结果；后期消费自己拥有安全点的 hook result |
| `Driver` | Rig `AgentRun` 推进中的临时 protocol state、step handling、driver-local counters/limits | 不发布 UI event；发内部 `DriverEvent` | 消费 `DriverHost` 的 model/tool/safe-point 结果 |
| `ModelGateway` | provider/model selection execution context、credentials resolution、future provider payload hooks、fallback/retry metadata、usage/error normalization | 不发布 UI event；通过 stream sink 返回 model delta/usage/failure | 被 `SessionRuntime` / `DriverHost` 调用；完整边界见 [ModelGateway](model-gateway.md) |
| `Tools` | session-scoped 工具注册、active tools、prompt catalog、policy、approval、grants、sandbox、mutation locks、execution coordination、executor update forwarding | 通过 `SessionRuntime` event sink 归约为 `tool_call_*`；自身不拥有 UI event metadata | 消费 tool definitions/policy/approval/executor；后期消费工具治理 hook；返回 `ToolBatchResult` |
| `ToolExecutor` | 单个工具执行过程和底层副作用句柄 | 不发布 UI event；只返回 result 和 progress chunk | 被 `Tools` 调用 |
| `ResourceManager` | `ResourceSnapshotStore`、current `RuntimeResourceSnapshot`、current `CwdResourceSnapshot`s、overlay policy、resource diagnostics、reload/capture results | 不发布 UI event；返回 reload result / capture result | `AgentRuntime` 发布 `resources_reload_started` / `resources_changed`；`SessionRuntime` 在 future turn 捕获 `TurnResourceSnapshot` |
| `CommandSurface` / `CommandManager` | command catalog materialization、name conflict diagnostics、phase policy、command text parse result、suggestion、execution resolve | 不发布 UI event；返回 catalog、suggestion、resolved invocation 或 UI-safe command result | `AgentRuntime` 发布 `command_catalog_changed` 和 command output events；`SessionRuntime.command` 执行 session-scoped command |
| `Skills` | 无生命周期状态；只提供 metadata/catalog parsing/format helpers | 不发布事件 | 被 `ResourceManager` / `SessionRuntime` 调用 |
| `Prompt` | 无生命周期状态；纯构建输入到输出 | 不发布事件 | 被 `SessionRuntime` 调用 |
| `Compaction` | 无运行生命周期状态；只提供准备、cut point、summary prompt、result helper | 不发布事件 | `SessionRuntime` 持有 compaction lifecycle 并发 `compaction_*` |
| `SessionHandle` | 单会话领域操作 facade、上下文重建结果 | 不发布 UI event；返回 entry id/context | `SessionRuntime` 根据返回结果发 `message_*`、`persistence_save_point` 等 |
| `SessionStorage` | 单会话 metadata、append-only entries、leaf、path-to-root index | 不发布事件 | 被 `SessionHandle` 调用 |
| `SessionManager` | persistent session catalog、`LoadedSessionRuntimes`、focused session、create/open/list/delete/fork/focus/close、storage adapter | 通常不直接发事件；由 `AgentRuntime` 发 session lifecycle 事件 | 被 `AgentRuntime` 调用；创建/关闭/查找 `SessionRuntimeHandle`，但不拥有单会话运行状态机 |
| `RuntimeHookRegistry` | 后期内部 hook handler 集合和 hook 执行结果 | 默认不发布 UI event | hook 影响后的事实由对应 owner 应用，再由 `SessionRuntime` / `AgentRuntime` 归约成事件 |

### AgentRuntime Ownership

`AgentRuntime` 拥有应用级事件通道和 workspace 事实，并通过 `SessionManager` 协调 session lifecycle。

持有状态：

```text
EventBus
  ├─ next_sequence
  ├─ subscribers
  ├─ optional ring buffer
  └─ last emitted timestamp/id metadata

WorkspaceServices
  ├─ SessionManager / SessionIndex
  ├─ CommandManager
  ├─ RuntimeHookRegistry / future hook service
  ├─ RuntimeDiagnostics
  ├─ ResourceManager
  │   ├─ ResourceSnapshotStore
  │   │   ├─ current runtime -> RuntimeResourceSnapshot
  │   │   └─ key: (workspace_id, cwd) -> CwdResourceSnapshot
  │   └─ ResourceOverlayPolicy
  ├─ ProviderRegistry / AuthStore
  └─ ModelGateway
```

负责发布的 wire events：

```text
session_created
session_opened
session_closed
session_imported
session_deleted
session_focus_changed
diagnostics_runtime_changed
resources_reload_started
resources_changed
command_catalog_changed
command_output_appended
command_interaction_requested
command_interaction_resolved
diagnostics_warning
diagnostics_error
```

负责生成 UI 可见事件记录：

```text
agent_runtime_protocol::EventMsg
  → AgentRuntime event bus
  → Event { event_id, sequence, timestamp, routing ids, msg }
  → agent_runtime_protocol::EventStream
```

不持有：

```text
current run internals
pending tool approval
assistant streaming buffer
session phase
pending session writes
Rig AgentRun
```

### SessionRuntime Ownership

`SessionRuntime` 是单会话产品状态机 owner。凡是与“这个会话正在发生什么”有关的事件，默认由它发布或归约。

持有状态：

```text
SessionPhase
CurrentRun
PendingSessionWrites
QueueState
ModelState
ResourceState view
Tools / ToolRegistry / ActiveToolSet
CompactionState
RetryState
RuntimeHookRegistry / future hook service
RuntimeSnapshot projection state
SessionHandle
```

负责发布/归约的 wire events：

```text
session_phase_changed
session_name_changed
session_tree_changed
session_model_changed
session_thinking_level_changed
session_tools_changed
session_active_tools_changed
session_stream_options_changed
session_settled
run_started
run_suspended
run_resumed
run_finished
message_user_appended
message_assistant_started
message_assistant_text_delta
message_assistant_finished
message_tool_result_appended
tool_call_proposed
tool_call_approval_requested
tool_call_started
tool_call_output_delta
tool_call_finished
queue_updated
usage_updated
persistence_save_point
skill_invoked
prompt_template_invoked
compaction_started
compaction_finished
retry_auto_started
retry_auto_finished
diagnostics_warning
diagnostics_error
```

消费的内部输入：

```text
DriverEvent
Tools progress/update
ModelGateway stream result
SessionHandle append/build_context result
future RuntimeHooks typed result
ResourceManager captured TurnResourceSnapshot
```

不持有：

```text
global event sequence
workspace service lifecycle outside current session
raw provider credentials
low-level JSONL write implementation
UI rendering state
```

### Driver Ownership

`Driver` 只拥有一次 Rig sans-IO drive 的协议推进，不拥有产品事件生命周期。

持有状态：

```text
Rig AgentRun local variable
current AgentRunStep
DriveRequest { turn: DriverTurnInput }
DriveLimits
driver-local usage/message accumulation
cancel token observation
```

内部事件：

```rust
DriverEvent::RunStarted
DriverEvent::ModelCallStarted
DriverEvent::ModelTextDelta
DriverEvent::ModelCallFinished { usage: Option<ModelCallUsage> }
DriverEvent::AssistantMessageStarted
DriverEvent::AssistantMessageDelta
DriverEvent::AssistantMessageFinished
DriverEvent::ToolBatchStarted
DriverEvent::ToolCallStarted
DriverEvent::ToolCallDelta
DriverEvent::ToolCallFinished
DriverEvent::ToolBatchFinished
DriverEvent::BeforeNextModelCall
DriverEvent::BeforeRunFinish
DriverEvent::RunFinished
```

这些事件不直接进入 UI。`SessionRuntime` 决定哪些转换成 `agent_runtime_protocol::Event`，何时写 session，何时发 `persistence_save_point` 和 `run_finished`。

### Tools Ownership

`Tools` 是 `SessionRuntime` 内部的 session-scoped 工具子系统，拥有工具注册、active tools、prompt catalog、policy、approval、grants、sandbox、mutation locks、execution coordination 和 executor implementations。它不直接发布 UI event，也不写 session storage；所有工具内部 update 都通过 `SessionRuntime` 传入的 sink 归约。

持有状态：

```text
ToolRegistry lifecycle
ActiveToolSet lifecycle
ToolPromptCatalog
ToolPolicy
ToolApprovalBroker / pending approvals
ToolApprovalGrantStore / approval modes
ToolExecutionCoordinator
ToolExecutorRegistry
prepared arguments
schema validation result
policy decision
approval wait handle
executor progress forwarding
normalized ToolInvocationResult
```

通过 `SessionRuntime` 归约的 wire events：

```text
tool_call_proposed
tool_call_approval_requested
tool_call_started
tool_call_output_delta
tool_call_finished
```

不持有：

```text
session phase
run terminal status
tool result message persistence
UI event metadata / sequence
```

### ResourceManager Ownership

`ResourceManager` 拥有资源 current pointers 和 reload/recompose pipeline，但不拥有 UI 事件通道。

持有状态：

```text
ResourceSnapshotStore
  ├─ current runtime -> Arc<RuntimeResourceSnapshot>
  └─ (workspace_id, cwd) -> Arc<CwdResourceSnapshot>
ResourceOverlayPolicy
ResourceResolver / loaders
Resource diagnostics
Reload/capture result types
```

相关 UI wire events 由 `AgentRuntime` 或 `SessionRuntime` 发：

```text
resources_reload_started
resources_changed
diagnostics_runtime_changed
```

### SessionManager Ownership

`SessionManager` 拥有工作区内 session lifecycle 事实，但不拥有 UI event metadata。它协调两类状态：

```text
Persistent session catalog
  ├─ create/open/list/delete/fork
  ├─ SessionHandle creation
  └─ storage adapter selection

LoadedSessionRuntimes
  ├─ loaded SessionRuntimeHandle map
  ├─ focused session id
  ├─ open/close/focus runtime lifecycle
  └─ shutdown_all / optional idle unload
```

相关 UI wire events 仍由 `AgentRuntime` 发布：

```text
session_created
session_opened
session_closed
session_deleted
session_focus_changed
session_tree_changed
```

`SessionManager` 不能把 `LoadedSessionRuntimes` 扩展成 Agent loop owner：phase、queue、current run、tool state、usage stats、compaction/retry 和 save point 仍属于 `SessionRuntime`。

### SessionHandle / SessionStorage Ownership

`SessionHandle` 和 `SessionStorage` 拥有可恢复历史，不拥有实时 UI event。

`SessionHandle` 持有/提供：

```text
append_message
append_compaction
append_model_change
append_active_tools_change
move_to
build_session_context
```

`SessionStorage` 持有：

```text
SessionMetadata
SessionEntry[]
current leaf id
entry id index / path-to-root reconstruction support
```

相关 UI wire events 由 `SessionRuntime` 发：

```text
message_user_appended
message_tool_result_appended
session_tree_changed
persistence_save_point
compaction_finished
```

### UI Adapter Ownership

UI adapter 只拥有渲染状态，不拥有运行时事实。

可持有：

```text
input draft
scroll position
selected session id
expanded/collapsed message ids
pending local command affordance
current rendered snapshot copy
```

不能持有为权威状态：

```text
session messages authority
tool approval authority
active tools authority
model credentials
resource catalog
session file state
run terminal status
```

UI 的权威输入只有：

```text
agent_runtime_protocol::RuntimeSnapshot
agent_runtime_protocol::Event stream
CommandAck
```

## Global Ordering Rules

1. 同一个 `AgentRuntime` 的 `sequence` 必须严格单调递增。
2. 同一个 `run_id` 只能有一个 `run_started`，并且必须有且只有一个 terminal `run_finished`。
3. `run_suspended` / `run_resumed` 只能发生在 `run_started` 之后、terminal `run_finished` 之前；它们不是终态，不能替代 `run_finished`。
4. `message_assistant_text_delta` 必须发生在对应 `message_assistant_started` 和 `message_assistant_finished` 之间。
5. `tool_call_output_delta` 必须发生在对应 `tool_call_started` 和 `tool_call_finished` 之间。
6. `tool_call_approval_requested` 只能发生在 `tool_call_proposed` 之后、`tool_call_started` 之前。
7. `persistence_save_point` 只能在相关 session writes 成功后发出。
8. `session_settled` 只能在 phase 为 `idle`，当前没有 active run/compaction/retry、pending session action，且没有同步马上要启动的 continuation 时发出。
9. `diagnostics_error` 不替代 terminal event。run 内失败需要 `diagnostics_error` + `run_finished { status: failed }`；abort 需要 `run_finished { status: aborted }`。
10. `resources_changed` 表示新 resource revision 已经原子替换；失败 reload 不应污染旧 revision。
11. UI reducer 必须能从任意 `agent_runtime_protocol::RuntimeSnapshot` 加之后续事件恢复一致状态。

## Lifecycle State Machines

下面列出不同层级的状态机。它们不是都要暴露成 enum；只有 UI 需要稳定观察的状态才进入 `agent_runtime_protocol::Event` 或 `agent_runtime_protocol::RuntimeSnapshot`。

### 1. Command Lifecycle

```text
Created in UI
  → Dispatched
  → Accepted
      └─ async agent_runtime_protocol::Event* follows
  → Rejected
      └─ no async work should start
```

事件/返回值：

- `CommandAck { accepted: true }`：命令已进入运行时，后续看事件。
- `CommandAck { accepted: false }`：运行时无法接收或路由该命令，或结构化命令在 preflight 中被同步拒绝。对 `ExecuteCommandText` / `ExecuteCatalogCommand` 的用户级错误，优先用 `command_output_appended { severity: Error }` 呈现。
- `RuntimeError`：transport、runtime fatal 或命令无法被解释。

不推荐给每个命令都发 `command_started/command_finished`，因为多数命令的业务状态已有更深事件表达，例如 `run_started`、`resources_reload_started`、`session_opened`。

### 2. Command Text Lifecycle

Command text 是 `agent_runtime_protocol::AgentCommand::ExecuteCommandText` 的业务处理过程；`/...` slash command 只是常见文本语法。它使用 materialize / parse / resolve / handler 模型，但不把每个阶段都暴露成 UI lifecycle event：

```text
Accepted ExecuteCommandText
  → materialize current catalog
  → parse raw text such as "/..."
  → resolve as trusted handler / protocol intent / runtime query / prompt input / rejection
  → execute through backend owner
  → emit command output or display-neutral interaction request
```

对应事件：

- `command_catalog_changed`：catalog projection 变化，不表示某个 command invocation 正在执行。
- `command_output_appended`：用户命令产生一条 message panel 输出，例如 `/status`、`/usage`、解析错误或 `/model` 设置完成提示。
- `command_interaction_requested`：用户命令请求 UI 展示 display-neutral 候选或收集输入，例如 `/model`、`/thinking`、`/sessions`。runtime 不指定具体 picker、popup、menu、form 或 detail view 组件。
- `command_interaction_resolved`：后续事件。只有 runtime 跟踪 pending interaction 或实现 `SubmitInteraction` 时，才需要在 UI 关闭或提交某个 interaction 后发布。

不推荐新增 `command_started` / `command_finished`。真正业务状态仍由更深事件表达：`/reload` 产生 `resources_reload_started` / `resources_changed`，`/compact` 产生 `compaction_started` / `compaction_finished`，`/skill <name>`（兼容 `/skill:name`）产生 `skill_invoked` / `message_user_appended` / `run_started` / `run_finished`。

`CommandResultEvent` 是用户命令结果的 UI-safe 表达，不是业务事实的唯一来源。UI reducer 应同时消费业务事件更新状态，并消费 command result events 更新 message panel 或打开交互控件。

### 3. Runtime Lifecycle

```text
Uninitialized
  → OpeningWorkspace
  → Ready
  → RebuildingServices
  → Ready
  → ShuttingDown
  → Closed
```

对应事件：

- `diagnostics_runtime_changed`：runtime 级诊断变化。
- `diagnostics_error { scope: Runtime }`：不可归属到具体 session 的错误。
- `session_opened` / `resources_changed`：通常标志 runtime 已经完成某个 workspace/session 的可用切换。

MVP 可以不暴露 `runtime_lifecycle_changed`，因为 UI 通常通过 workspace/session/resource 事件感知可用性。

### 4. Workspace Lifecycle

```text
NoWorkspace
  → Opening
  → Open
  → ReloadingResources
  → Open
  → Closing
  → NoWorkspace
```

对应事件：

- `resources_reload_started`
- `resources_changed`
- `diagnostics_runtime_changed`

资源 reload 失败不应进入 `NoWorkspace`，而是保持旧 revision，并通过 diagnostics 表达失败。

### 5. Session Catalog And Loaded Runtime Lifecycle

```text
CatalogOnly
  → CreatingSession / ImportingSession
  -- session_created/session_imported --> PersistedSession

PersistedSession
  → OpeningRuntime
  -- session_opened --> LoadedRuntime

LoadedRuntime
  → ClosingRuntime
  -- session_closed --> PersistedSession

PersistedSession
  → DeletingSession
  -- session_deleted --> Deleted
```

对应事件：

- `session_created`
- `session_opened`
- `session_closed`
- `session_deleted`
- `session_imported`

`session_opened` 表示运行时已创建 `SessionRuntime` 并可进入 `RuntimeSnapshot.active_session`；不表示当前正在跑 agent。

多 session 同时 loaded 时，打开新 session 不要求关闭旧 session。`session_closed` 只表示 runtime 从 `LoadedSessionRuntimes` 卸载，不表示从 catalog 删除。

### 6. Session Focus Lifecycle

Focused session 是 UI 或默认命令的当前目标。它是 runtime-visible state，和 loaded/running 不同。

```text
NoFocusedSession
  -- session_focus_changed(None -> session_id) --> Focused(session_id)

Focused(old_session_id)
  -- session_focus_changed(old_session_id -> new_session_id) --> Focused(new_session_id)

Focused(session_id)
  -- session_focus_changed(session_id -> None) --> NoFocusedSession
```

对应事件：

- `session_focus_changed`

`session_opened` 通常可以紧跟 `session_focus_changed`，但二者语义不同：前者表示 runtime loaded，后者表示默认目标改变。后台 running session 失去 focus 时不应被关闭或中止。

### 7. Session Phase Lifecycle

`SessionPhase` 是单个 `SessionRuntime` 的互斥工作状态。

```text
Idle
  -- session_phase_changed(turn) --> Turn
  -- session_phase_changed(idle) --> Idle

Idle
  -- session_phase_changed(compaction) --> Compaction
  -- session_phase_changed(idle) --> Idle

Idle
  -- session_phase_changed(retry) --> Retry
  -- run_started --> Turn
  -- session_phase_changed(idle) --> Idle
```

对应事件：

- `session_phase_changed`
- `session_settled` 只能在 phase 回到 `idle` 且不会立即继续时发出。

`run_started` 不替代 `session_phase_changed(turn)`：phase 是产品级互斥状态，run 是一次 Rig drive / agent 工作单元。

### 8. Run Lifecycle

```text
Pending
  -- run_started --> Running
      ├─ ModelCalling
      ├─ ToolCalling
      ├─ WaitingApproval
      ├─ Suspended
      └─ Finishing
  -- run_suspended(resume_id, reason) --> Suspended
  -- run_resumed(resume_id) --> Running
  -- run_finished(status) --> Finished(completed | failed | aborted)
```

UI 需要的 run lifecycle event：

- `run_started`
- `run_suspended { resume_id, reason }`
- `run_resumed { resume_id }`
- `run_finished { status }`

`run_suspended` 是可恢复暂停，不是终态。它表示 `Driver` / `SessionRuntime` 已在协议安全 checkpoint 停住，并持有 resume state；恢复后继续同一个未完成 run 的 continuation。`run_finished` 仍是唯一 terminal run event，不存在 `run_finished { status: paused }`。

普通中间状态通过 message/tool/approval 事件体现，不额外暴露 `run_model_calling` 或 `run_waiting_tool`，避免状态重复。`WaitingApproval` 可以只通过 `tool_call_approval_requested` 和 `RunView.pending_tool_approvals` 表达；只有需要挂起并等待显式 resume 时，才进入 `CurrentRunState::Suspended`。

### 9. Model Call Lifecycle

Model call 是 `Driver` 内部生命周期，不直接暴露给 UI。

```text
Prepared
  → Streaming
  → Completed
  → MappedToAssistantMessage
```

内部事件：

- `DriverEvent::ModelCallStarted`
- `DriverEvent::ModelTextDelta`
- `DriverEvent::ModelCallFinished`

UI 事件：

- `message_assistant_started`
- `message_assistant_text_delta`
- `message_assistant_finished`
- `usage_updated`

不把 provider payload、credentials、raw response 作为 UI event。

### 10. User Message Lifecycle

User message 通常不是流式对象。

```text
AcceptedByCommand
  → Expanded(skill/template)
  → future hook expansion/patch if hook system is enabled
  -- message_user_appended --> AppendedToSession
  -- persistence_save_point --> DurableAtSavePoint
  → IncludedInRunContext
```

对应事件：

- `skill_invoked` / `prompt_template_invoked`：如果输入来自技能或模板。
- `message_user_appended`
- `persistence_save_point`

`message_user_appended` 表示 UI 可以渲染消息；`persistence_save_point` 表示它已经进入可恢复边界。

### 11. Assistant Message Lifecycle

Assistant message 是流式对象。

```text
NotStarted
  -- message_assistant_started --> Started
  -- message_assistant_text_delta* --> StreamingText
  -- message_assistant_finished --> Finished
  → AppendedToSession
  -- persistence_save_point --> DurableAtSavePoint
```

对应事件：

- `message_assistant_started`
- `message_assistant_text_delta*`
- `message_assistant_finished`
- `persistence_save_point`

`message_assistant_finished` 关闭 UI 流式块，但不等于已经落盘。落盘边界看 `persistence_save_point`。

### 12. Tool Call Lifecycle

Tool call 是模型请求的产品侧执行对象。

```text
Proposed
  -- tool_call_proposed --> PolicyEvaluating
  -- tool_call_approval_requested --> WaitingApproval
      ├─ Approved
      └─ Rejected
  -- tool_call_started --> Started
  -- tool_call_output_delta* --> OutputStreaming
  -- tool_call_finished --> Finished(success | error)
  -- message_tool_result_appended --> ToolResultMessageAppended
  -- persistence_save_point --> DurableAtSavePoint
```

对应事件：

- `tool_call_proposed`
- `tool_call_approval_requested`
- `tool_call_started`
- `tool_call_output_delta*`
- `tool_call_finished`
- `message_tool_result_appended`
- `persistence_save_point`

policy denied、approval rejected、schema invalid、unknown tool 都走 `tool_call_finished { is_error: true }`，并生成 error tool result message；不直接让 run failed。

### 13. Approval Lifecycle

Approval 是 tool call 的子状态，不是 UI callback。

```text
NotRequired

Required
  -- tool_call_approval_requested --> WaitingUserDecision
      ├─ approved by agent_runtime_protocol::AgentCommand::DecideToolApproval
      ├─ rejected by agent_runtime_protocol::AgentCommand::DecideToolApproval
      └─ cancelled by abort
```

对应事件/命令：

- `tool_call_approval_requested`
- `agent_runtime_protocol::AgentCommand::DecideToolApproval`
- approved 后进入 `tool_call_started`
- rejected 后进入 `tool_call_finished { is_error: true }`
- abort 后 run 进入 `run_finished { status: aborted }`

审批状态不能由 UI 私有保存为权威状态；同一 host 生命周期内的订阅/状态重建后，从 `RuntimeSnapshot.active_session.current_run.pending_tool_approvals` 恢复。`tool_call_approval_requested` 是一次事件，pending approval 是 `ToolApprovalBroker` 持有的当前运行状态；snapshot 只暴露 UI-safe view，不暴露冻结的 prepared args。

### 14. Queue Lifecycle

消息队列和 pending session actions 都是 session runtime 状态，不是持久化 transcript。`PendingSessionAction::Compact` 是结构化 post-run action，不是模型可见消息。

```text
Empty
  -- queue_updated --> NonEmpty
  → DrainingAtSafePoint
  -- queue_updated --> Empty or NonEmpty
```

对应事件：

- `queue_updated { follow_up, steering, next_turn, pending_actions }`

`queue_updated` 每次发送完整队列摘要，而不是 delta。这样 UI reducer 不需要理解运行时 drain 细节。

### 15. Compaction Lifecycle

Compaction 是 session context projection，不是普通 run。

```text
Requested while idle
  -- session_phase_changed(compaction) --> PreparingCutPoint
  -- compaction_started --> PreparingSummary
  → future SessionBeforeCompact hook if hook system is enabled
  → Summarizing
  → AppendingCompactionEntry
  → RebuildingSessionContext
  -- compaction_finished --> Finished(aborted? failed? will_retry?)
  -- persistence_save_point --> DurableAtSavePoint
  → session_settled or run_started

Requested while active work
  -- queue_updated(pending compact) --> Deferred
  → current work terminal facts + persistence save point
  → required retry / overflow recovery chain completes
  -- queue_updated(remove pending compact) --> Dispatching
  -- session_phase_changed(compaction) --> PreparingCutPoint
```

对应事件：

- `session_phase_changed { phase: compaction }`
- `compaction_started`
- `compaction_finished`
- `persistence_save_point`
- `session_phase_changed { phase: idle }`
- `session_settled` 或紧接 `run_started`

摘要模型调用不发 `message_assistant_started`，因为它不是用户会话里的 assistant 回复。

### 16. Auto Retry Lifecycle

Auto retry 是 run failure 后的恢复流程。

```text
Idle
  -- retry_auto_started --> ScheduledBackoff
  → RetryStarting
  -- run_started --> Running
  -- run_finished --> RunTerminal
  -- retry_auto_finished --> RetryFinished(success | failed | aborted)
```

对应事件：

- `retry_auto_started`
- `run_started`
- `run_finished`
- `retry_auto_finished`

如果 retry 会马上启动新 run，前一个 failed run 后不应发 `session_settled`，否则 UI 会短暂误判为空闲。

### 17. Usage And Context Usage Lifecycle

Usage lifecycle 描述模型调用消耗、run 汇总、会话累计 stats 和当前上下文占用 view 的更新。它不是持久化屏障，也不是成本账单。

```text
StatsStable
  -- model_call_finished --> RunUsageUpdated
  -- usage_updated --> StatsStable

StatsStable
  -- run_finished --> RunUsageFinalized
  -- usage_updated --> StatsStable

StatsStable
  -- compaction_finished/resources_changed/session_tools_changed --> ContextUsageRecomputed
  -- usage_updated --> StatsStable
```

对应事件：

- `usage_updated`
- `run_finished { usage }`

`usage_updated` 可以在模型调用结束、run 结束、压缩结束、资源或工具变更后发出。它表示 UI usage/context view 已更新；如果对应 usage facts 需要恢复，仍必须等待 `persistence_save_point`。`SessionStatsView.total_usage` 是累计模型调用消耗，压缩不会让它下降；`ContextUsageView.current_tokens` 是下一次模型请求的上下文窗口占用，压缩后通常会下降。

### 18. Resource Reload Lifecycle

Resource reload 是 workspace/cwd 级生命周期。

```text
Stable(cwd, revision=N)
  -- resources_reload_started --> Reloading
      ├─ Commit(revision=N+1)
      └─ KeepOldRevisionWithDiagnostics(revision=N)
  -- resources_changed --> Stable
```

对应事件：

- `resources_reload_started`
- `resources_changed`
- `command_catalog_changed`，如果 skills、prompt templates、extension commands 或 builtin availability 因 reload 改变

`resources_changed` 必须携带当前有效 `workspace_id`、`cwd` 和 revision。失败 reload 也可以发 `resources_changed`，但 revision 仍是旧值，diagnostics 描述失败。

`command_catalog_changed` 不是资源持久化屏障；它只是 command catalog projection 的替换式更新。UI autocomplete / command palette 应以最新 `RuntimeSnapshot.command` 或该事件中的 catalog 为准。

### 19. Persistence Lifecycle

Persistence lifecycle 描述 session writes，而不是文件系统细节。

```text
Clean
  → PendingSessionWrites
  → Flushing
  -- persistence_save_point --> Clean
```

对应事件：

- `persistence_save_point { had_pending_mutations }`

`persistence_save_point` 是 UI 可以信任的 durable barrier。不要让 UI 根据 `message_assistant_finished` 推断已保存。

### 20. Session Tree Lifecycle

Session tree 代表当前 leaf 和分支视图。

```text
LeafStable
  → Navigating
  -- session_tree_changed --> LeafChanged
  → ContextRebuilt
  → LeafStable

LeafStable
  → Forking
  -- session_created --> NewSessionCreated
  -- session_opened --> SessionOpened
```

对应事件：

- `session_tree_changed`
- `session_created`
- `session_opened`
- `persistence_save_point`，如果 leaf move 以 append entry 记录。

导航 session tree 不能删除历史，只移动当前 leaf。

### 21. Diagnostics Lifecycle

Diagnostics 通常是替换式状态。

```text
NoDiagnostics
  -- diagnostics_runtime_changed --> DiagnosticsPresent
  -- diagnostics_runtime_changed --> DiagnosticsChanged
  -- diagnostics_runtime_changed --> NoDiagnostics
```

对应事件：

- `diagnostics_runtime_changed { diagnostics }`
- `resources_changed { diagnostics }`
- `diagnostics_warning`
- `diagnostics_error`

`diagnostics_error` 是一次事实通知；`diagnostics_runtime_changed` 是当前诊断集合的权威替换。

### 22. Hook Lifecycle

Hook 是后期内部生命周期，默认不进 `AgentRuntimeProtocol`。当前 MVP 不实现 hook system；完整规则见 [RuntimeHooks](runtime-hooks.md)。

```text
NotRunning
  → RunningHook
  → Completed / Failed / Cancelled
  → EffectAppliedToRuntimeFact
```

内部输入/输出：

- future `RuntimeHookRegistry.invoke(HookPoint)`
- hook typed decision / patch / replacement

UI 只看后期 hook 影响后的事实，例如 tool denied、message replaced、compaction cancelled 或 diagnostics changed。后续如果要展示 hook 运行详情，应设计独立 `hook_started/hook_finished`，不要复用内部 hook point 或 typed result。

### 23. Reconnect Lifecycle

Reconnect 是 UI adapter 在同一 host 生命周期内的消费流程。它不是 UI 进程失败后重新连接仍在后台运行的独立 runtime daemon。

```text
Connected(sequence=N)
  → Disconnected
  → Resubscribe
  → RuntimeSnapshot(last_event_sequence=M)
  → Apply events with sequence > M
  → Connected
```

如果发现 sequence gap：

```text
Expected K, received K+n
  → request fresh RuntimeSnapshot
  → drop buffered assumptions
```

这也是为什么 RuntimeSnapshot 必须包含当前 adapter 视图所需的权威状态，而不是只包含 session messages。MVP 只承诺 active session / 当前视图的恢复语义；非 active/background session 的完整运行态不属于 UI reconnect contract。

## Submit Prompt Lifecycle

```text
UI
  → dispatch(SubmitPrompt)
  ← CommandAck { command_id, accepted: true }

SessionRuntime
  → session_phase_changed { phase: turn }
  → message_user_appended
  → persistence_save_point              // user message durable
  → run_started
  → message_assistant_started
  → message_assistant_text_delta*
  → tool_call_*                         // optional, see tool lifecycle
  → message_assistant_finished
  → usage_updated                       // optional, when provider usage exists
  → persistence_save_point              // assistant/tool results durable
  → run_finished { status: completed }
  → session_phase_changed { phase: idle }
  → session_settled
```

如果 immediate retry、pending manual compact、follow-up / steering / next-turn 导致继续工作，`run_finished` 后不应立即 `session_settled`；应先发对应 `queue_updated` / lifecycle event，再启动 compaction、下一次 `run_started` 或在安全点继续。

## Tool Lifecycle

```text
Driver receives CallTools
  → tool_call_proposed { call_id, name, args, risk, requires_approval }
  → tool_call_approval_requested?       // if policy asks user
  → tool_call_started
  → tool_call_output_delta*
  → tool_call_finished { result, is_error }
  → message_tool_result_appended
```

推荐理由：

- pi 把 `tool_execution_start/update/end` 和 tool result message 分开；这样 UI 能展示工具活动，同时 transcript 仍由 message 事件维护。
- Codex 对 exec、patch、MCP 都使用 begin/delta/end 或 item started/completed；这让长任务可被 UI 稳定折叠、恢复和计时。
- 未知工具、未启用工具、schema invalid、policy denied、approval rejected、executor failed 都应产生 error tool result，而不是让 run 崩溃。只有 abort/cancel 改变 run terminal status。

## Approval Lifecycle

```text
tool_call_proposed { requires_approval: true }
tool_call_approval_requested
UI dispatch(DecideToolApproval)
  ├─ approved  → tool_call_started → ... → tool_call_finished
  └─ rejected  → tool_call_finished { is_error: true } → message_tool_result_appended
```

审批请求是运行时事件，不是 UI callback。UI 只能通过 `DecideToolApproval` 回答，不能直接调用 tool executor，也不能替换工具参数。同一 host 生命周期内如果 UI adapter、subscriber 或 reducer 重建，当前仍未解决的审批必须从 `RuntimeSnapshot.active_session.current_run.pending_tool_approvals` 重新渲染。

## Abort Lifecycle

```text
UI dispatch(AbortRun)
  ← CommandAck
SessionRuntime / Driver cancellation
  → queued tool/model updates stop
  → message_assistant_finished?         // only if there is an abort/error message to close
  → run_finished { status: aborted }
  → session_phase_changed { phase: idle }
  → queue_updated                       // clears pending compact; may also return/clear message queues
  → session_settled
```

不要同时发 `run_aborted` 和 `run_finished`。推荐一个 terminal event：`run_finished { status: aborted }`，避免 UI reducer 处理双终态。

## Failure Lifecycle

Provider 或 protocol 失败：

```text
run_started
message_assistant_started?              // if failure is represented as assistant error message
message_assistant_finished?             // persisted as UI/model-visible or UI-only according to ContextVisibility
diagnostics_error { scope: run, recoverable }
run_finished { status: failed }
persistence_save_point?                 // only if failure message/diagnostic was persisted
session_phase_changed { phase: idle }
session_settled or retry_auto_started
```

原则：

- 同步 preflight 失败不启动 run，直接拒绝 command 或发 `diagnostics_error { scope: command }`。
- 可重试 provider 失败可以进入 `retry_auto_started`，但仍要关闭当前 run。
- context overflow recovery 不应把 transient overflow assistant error 放入下一次 retry prompt；如果持久化，必须是 `UiOnly` 或 diagnostic/custom entry。

## Compaction Lifecycle

```text
UI dispatch(Compact) while active work
  → queue_updated { pending_actions: [Compact] }
  → current work continues
  → run terminal facts + persistence_save_point
  → required immediate retry / overflow recovery chain, if any
  → queue_updated { pending_actions: [] }

UI dispatch(Compact) while idle, drain pending Compact, or auto threshold/overflow
  → session_phase_changed { phase: compaction }
  → compaction_started { reason }
  → future RuntimeHookRegistry.invoke(SessionBeforeCompact) // internal only
  → summary model call                         // not a normal assistant message
  → SessionHandle.append_compaction
  → compaction_finished { result, aborted: false, will_retry }
  → persistence_save_point
  → session_phase_changed { phase: idle }
  → session_settled or run_started             // overflow retry may continue immediately
```

压缩摘要是 session context projection，不是 system prompt。`compaction_finished` 可以给 UI 摘要预览，但完整摘要默认通过 session entry detail 读取。

## Resource Reload Lifecycle

资源 snapshot 的创建和替换发生在 `ResourceManager` 方法内部；事件只负责通知已经发生的事实。初始化路径也遵循这个规则：

```text
UI dispatch(OpenWorkspace)
  → WorkspaceServices::new(...)
  → ResourceManager.ensure_runtime_snapshot(ResourceInitReason::WorkspaceOpen)
      └─ missing: build RuntimeResourceSnapshot and replace_runtime(...)
  → workspace_opened / RuntimeSnapshot { active_session: None }

UI dispatch(OpenSession or NewSession)
  → SessionManager.open_handle(...) / create_handle(...)
  → read fixed { workspace_id, cwd } from session metadata
  → ResourceManager.ensure_cwd_snapshot(CwdResourceRequest { reason: SessionOpen, ... })
      ├─ missing: load cwd-local resources, overlay, replace_cwd(...)
      └─ stale runtime revision: recompose_cwd(...), replace_cwd(...)
  → create SessionRuntime { workspace_id, cwd, services }
  → session_opened / session_focus_changed
```

`ensure_*` 必须幂等。并发 open session、first turn 或 reload 只能通过 `ResourceSnapshotStore.replace_*` 发布完整的新 snapshot，不能让 UI 事件成为状态更新来源。

```text
UI dispatch(ReloadResources)
  → resources_reload_started { workspace_id, cwd }
  → ResourceManager.reload_cwd(CwdResourceRequest { workspace_id, cwd, ... })
      ├─ success: build CwdResourceSnapshot { runtime, local, resolved }
      │          and ResourceSnapshotStore.replace_cwd(workspace_id, cwd, snapshot)
      └─ failure: keep old CwdResourceSnapshot for cwd, collect diagnostics
  → resources_changed { workspace_id, cwd, runtime_revision, cwd_revision, summaries, diagnostics }
  → diagnostics_runtime_changed?               // only for runtime-level diagnostics
```

`resources_changed` 只传摘要，不传技能全文、context file 正文或完整 system prompt。UI 如果要详情，通过运行时命令读取。

`ResourceSnapshotStore.replace_cwd(...)` 是资源 reload 对后续 turn 生效的原子发布点。runtime 不把新 snapshot 推送到每个 idle `SessionRuntime`；下一次 `SubmitPrompt` / `InvokeSkill` / `InvokePromptTemplate` 真正启动 user turn 时，`SessionRuntime` 会调用 `ResourceManager.capture_turn(...)`，从 store 读取当前已发布的 `CwdResourceSnapshot` 并放入 `TurnState.resources`。

```text
ReloadResources 完成 replace_cwd(C2)
  → resources_changed { cwd_revision: C2.revision }

下一次 SubmitPrompt
  → SessionRuntime::start_user_turn(...)
  → ResourceManager.capture_turn(...)
  → ResourceSnapshotStore.current_cwd(...) == C2
  → TurnState.resources = TurnResourceSnapshot { cwd: Arc<C2> }
```

如果 submit 在 `replace_cwd` 之前已经开始 capture，它可以合法使用旧 snapshot；如果 submit 在 `replace_cwd` 之后才开始 capture，就必须看到新 snapshot。旧 snapshot 不会被原地修改，正在运行的 turn 继续使用自己已经捕获的 `TurnResourceSnapshot`。

## Session Open And Focus Lifecycle

```text
UI dispatch(OpenSession or FocusSession)
  → open existing runtime or SessionManager.open_and_load(...)
  → ensure ResourceManager has current CwdResourceSnapshot for session { workspace_id, cwd } if newly loaded
  → session_opened { session_id }?             // only if it was not already loaded
  → session_focus_changed { previous_session_id, focused_session_id: session_id }
  → session_phase_changed { phase: idle }
  → resources_changed?                         // if this cwd snapshot changed
  → queue_updated                               // active session queue projection
  → session_settled
```

多 session 同时 loaded 时，聚焦新 session 不应自动关闭旧 session，也不应改写旧 session 的 fixed cwd、phase、queue、current run 或已经捕获到 `TurnState` 的 `TurnResourceSnapshot`。只有显式 `CloseSession`、workspace teardown、idle unload 或 shutdown policy 才发 `session_closed`。

`session_opened` 或 `session_focus_changed` 之后 UI 应重新请求或接收 `agent_runtime_protocol::RuntimeSnapshot`。事件流负责增量变化，snapshot 负责权威恢复。`OpenWorkspace` 本身不自动恢复旧 session；此时 `RuntimeSnapshot.active_session = None`。TUI 的 `/resume` 和 GUI sidebar 通过 `ListSessions` 查询 `SessionManager` 的轻量会话目录。

## RuntimeSnapshot And Reconnect

推荐订阅流程：

```text
subscribe() -> agent_runtime_protocol::EventStream
snapshot() -> RuntimeSnapshot { last_event_sequence }
reduce events with sequence > snapshot.last_event_sequence
```

或者后续实现：

```rust
fn subscribe_after(sequence: u64) -> agent_runtime_protocol::EventStream;
```

MVP 可以只保留内存 ring buffer；session JSONL 是会话历史，不是 agent_runtime_protocol::Event log。同一 host 生命周期内的订阅中断较久时，UI 重新请求 RuntimeSnapshot，而不是要求 runtime replay 所有旧事件。这里不承诺 UI 进程失败后重连仍在后台运行的 runtime。

## AgentRuntimeProtocol Reference

`agent_runtime_protocol::Event`、`agent_runtime_protocol::EventMsg`、各事件族 enum、`RunTerminalStatus`、`EventScope` 和完整 wire event type 映射以 [AgentRuntimeProtocol](agent-runtime-protocol.md) 为权威定义。本文件只解释生命周期、顺序、所有权和场景约束，避免同一份协议类型在两个文档中漂移。

## Test Matrix

MVP event lifecycle tests 应覆盖：

- submit prompt text-only：事件顺序、`persistence_save_point`、`session_settled`。
- assistant streaming：delta 必须在 `message_assistant_started` / `message_assistant_finished` 之间。
- tool success：`tool_call_proposed` / `tool_call_started` / `tool_call_finished` + `message_tool_result_appended`。
- tool denied：产生 error tool result，不产生 run failure。
- abort run：只有一个 terminal `run_finished { status: aborted }`。
- compaction：phase、`compaction_started` / `compaction_finished`、save point、settled 顺序。
- resource reload failure：旧 revision 不变，diagnostics 更新。
- command text view：`/status`、`/usage` 产生 `command_output_appended`，不把结果放入 `CommandAck`。
- command interaction：`/model`、`/thinking` 产生 `command_interaction_requested`，选择后通过 `ExecuteCatalogCommand`、runtime-tracked `SubmitInteraction` 或明确结构化 `AgentCommand` 完成设置；不依赖 interaction request 携带 raw command text。
- command text semantic error：unknown command 或 phase 不允许时，`CommandAck` 可 accepted，并通过 error severity 的 `command_output_appended` 告知用户。
- resync：RuntimeSnapshot sequence 后的事件可正确 reduce。
