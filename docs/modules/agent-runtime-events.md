# AgentRuntimeEvents

`AgentRuntimeEvents` 描述 `agent_runtime_protocol::Event` 的事件生命周期。它不是 UI 状态管理方案，也不是 session 持久化格式；它是运行时向 UI adapter 发布事实的唯一通道。

设计参考：

- Rust 常见风格：内部类型和 enum variant 使用 `PascalCase`，序列化到 JSON 时常用 `snake_case`。
- pi-agent-core `AgentEvent`：`agent_start`、`turn_start`、`message_start/update/end`、`tool_execution_start/update/end`、`turn_end`、`agent_end`。
- pi coding-agent `AgentSessionEvent`：在核心事件外增加 `queue_update`、`compaction_start/end`、`auto_retry_start/end`、`thinking_level_changed`、`session_info_changed`，并在 session 层处理持久化和 extension events。
- Codex `Submission Queue / Event Queue`：用户提交进入 submission queue，agent 输出进入 event queue；`Event` 带 submission id；Rust enum variant 用 `PascalCase`，wire event type 用 `snake_case`，长任务使用 `Begin / Delta / End` 或 `ItemStarted / ItemCompleted` 配对。

以上项目只用于比较事件设计思路；MiniCore 不承诺兼容其事件名、payload、队列实现或生命周期顺序。后续章节定义的 MiniCore 事件契约才是权威。

## 推荐结论

1. `agent_runtime_protocol::Event` 是 UI 收到的一条完整事件记录，不是内部 hook，也不是 session log。
2. `agent_runtime_protocol::Event.msg` 使用分组 enum，例如 `agent_runtime_protocol::EventMsg::Run(RunEvent::Started)`。
3. UI/wire 事件类型使用 flat `snake_case`，例如 `run_started`、`tool_call_output_delta`。
4. 所有 UI 事件都携带顺序号、时间、workspace/session/run/command correlation；这些 metadata 属于 `agent_runtime_protocol::Event` 外层记录。
5. `SessionRuntime` 是把内部事件归约成 UI 事件的中心；`DriverEvent`、`Tools` update、后期 `RuntimeHooks` typed result 不能直接泄漏给 UI。
6. 长生命周期对象必须有明确 started/delta/finished 配对，例如 assistant message、tool call、run、compaction、resource reload。
7. 流式 delta、pending approval 和执行中的 tool round 不是持久化事实；需要恢复的领域事实只能在对应 `SessionWriter.commit(...)` 成功后发布。
8. `session_settled` 表示 UI 可认为该 session 当前没有立即继续的 run/compaction/retry、pending session action 或马上启动的 steering/follow-up continuation，并且 phase 已经回到 `idle`；`NextTurn` queue 可以保留。
9. 同步命令接收用 `CommandAck` 表达；只读 typed 查询由 `AgentRuntime.query()` 直接返回 `QueryResponse`；异步执行结果和用户可见命令结果用 `agent_runtime_protocol::Event` 表达。
10. core 不发布 session focus/selection event；adapter 选择哪个 session 属于客户端本地状态，session-scoped command 必须显式携带 `SessionId`。

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

`agent_runtime_protocol::EventStream` 传输完整的 `agent_runtime_protocol::Event`。外层事件记录负责“这条事实属于谁、排在第几、由哪个命令引起、重连时是否已经见过”；`msg` 只描述业务事实本身。

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

wire 形态固定为外层事件 + 内层消息：

```json
{
  "event_id": "evt_...",
  "sequence": 42,
  "session_id": "ses_...",
  "run_id": "run_...",
  "msg": {
    "type": "run_started"
  }
}
```

稳定 event type 位于 `msg.type`；外层 `agent_runtime_protocol::Event` 是 `workspace_id`、`session_id`、`run_id` 和 `command_id` 的唯一权威位置。`EventMsg` 不重复这些通用坐标，只保留局部对象 identity、transition operands 和业务数据。

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

`message_id`、`call_id`、`approval_id`、`interaction_id`、`output_id` 和 `resume_id` 是 payload 所描述对象的局部 identity，不属于 envelope route，继续放在 `EventMsg`。tree old/new leaf 等字段是 transition operands，也继续保留。禁止在 msg 中使用未限定角色的 `workspace_id` / `session_id` / `run_id` / `command_id` 复制外层坐标；若未来事实需要指向另一个实体，应使用 `source_session_id`、`origin_command_id` 等角色明确的字段。

UI reducer 必须消费完整 `Event`。组件若需要在 dispatch stack 外保存 payload，由 adapter 创建包含 event metadata 的本地 view；裸 `EventMsg` 不是可独立路由、排序或关联的完整事件。

### Routing Profiles

| 事件层级 | `workspace_id` | `session_id` | `run_id` | `command_id` |
| --- | --- | --- | --- | --- |
| diagnostics | 按最细可归属 owner；可空 | 按最细可归属 owner；可空 | 按最细可归属 owner；可空 | 相关命令 id 或 `None` |
| workspace resources | 必填 | `None` | `None` | reload 命令 id 或 `None` |
| session create/open/import/delete/close | 必填 | 目标 session 必填 | `None` | 触发命令 id 或 `None` |
| session phase/queue/config/settled | 必填 | 必填 | 通常 `None` | 触发命令 id 或 `None` |
| command catalog | 必填 | runtime catalog 为 `None`，session catalog 为必填 | `None` | 导致 catalog 变化的命令 id 或 `None` |
| user message appended | 必填 | 必填 | new-run admission 为 `None`，active Steer 为 current run | originating command id |
| skill/template invoked | 必填 | 必填 | idle/future-turn 为 `None`，active Steer 为 current run | originating command id |
| run lifecycle | 必填 | 必填 | 必填 | 启动 run 的 originating command id |
| assistant message stream | 必填 | 必填 | 必填 | 启动 run 的 originating command id |
| tool call | 必填 | 必填 | 必填 | 启动 run 的 originating command id；审批响应另有 command id |
| usage | 必填 | 必填 | 与具体 run 相关时填写，否则 `None` | 相关命令 id 或 `None` |
| compaction | 必填 | 必填 | 可空 | 手动 compact 命令 id；自动压缩可空 |
| retry | 必填 | 必填 | `None` | retry 源命令 id 或 `None` |
| command result | 可空或相关 workspace | 可空或目标 session | `None` | 触发输出或交互请求的命令 id 必填 |

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

内部 publisher 不应暴露 `publish(msg, workspace_id?, session_id?, run_id?, command_id?)` 这类可自由组合的入口。实现应提供按 routing profile 分类的 draft/constructor，例如 `runtime(...)`、`workspace(...)`、`session(...)` 和 `run(...)`；constructor 校验 required coordinates，再由 event bus 统一分配 event id、sequence 和 timestamp。公开 wire 暂时保持 flat optional coordinates，不为本项额外引入公开 `EventRoute` enum。

`Driver`、`Tools`、`ResourceManager`、`Compaction` 不直接生成 UI 可见的 `agent_runtime_protocol::Event`。后期 `RuntimeHooks` 只返回 typed result。它们的结果由 `SessionRuntime` / `AgentRuntime` 归约。

### RuntimeSnapshot 水位

`agent_runtime_protocol::RuntimeSnapshot.last_event_sequence` 是 adapter 在同一 host 生命周期内看到的权威状态水位。snapshot builder 必须通过 event-bus/projection barrier 让 loaded runtime membership、全部 `SessionSnapshot` 与该 sequence 对应同一逻辑时刻，不能拼接彼此跨水位的 session views：

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

session JSONL 不是 runtime event log，`RuntimeSnapshot` 也不是单独持久化的文件。不能要求 adapter 通过 session entries 重放 `message_assistant_text_delta`、`tool_call_output_delta` 或 approval waiting 状态；这些瞬时状态必须从 runtime 内存投影出的 `RuntimeSnapshot.loaded_sessions[*]` 或新事件恢复。关闭 host 后，下一次打开 workspace 时重新生成 `RuntimeSnapshot`，默认没有 loaded session。

MVP 不支持 adapter 失败/断线但 `AgentRuntime` 作为独立 daemon 继续运行再被重连的故障模型。adapter host 和 runtime 同生命周期；因此 reconnect contract 只覆盖同一程序上下文内的初始化、late subscribe、reducer/subscriber 重建和 sequence gap recovery。由于 event sequence 是 runtime-global，`RuntimeSnapshot` 在同一水位覆盖全部 loaded sessions；若未来使用 session-scoped subscription/cursor，再评估 scoped snapshot。

## Command Ack

`agent_runtime_protocol::CommandAck` 只说明命令是否被运行时接收，不返回助手文本，也不返回 command text 的查询或展示结果。`RuntimeQuery` 不经过 `CommandAck`：它直接返回 `QueryResponse`，也不发布 query-response event。事件文档只解释它和异步事件的关系，结构定义以 [AgentRuntimeProtocol](agent-runtime-protocol.md) 为准。

原则：

- runtime 无法接收或无法路由的命令，例如 runtime 已关闭、workspace 不存在、目标 session id 无效，返回 `CommandAck { accepted: false }` 或 `RuntimeError`。
- 对 `ExecuteCommandText` / `ExecuteCatalogCommand`，unknown command、参数非法、phase 不允许这类用户级错误，优先返回 `CommandAck { accepted: true }`，再发 `command_output_appended { severity: Error }`，让下游 CLI/TUI/GUI 的 message panel 或等价输出呈现一致。
- 已接受命令的后续成功、失败、审批、流式输出，都通过 `agent_runtime_protocol::Event`。
- UI 不应把 `CommandAck` 当成最终状态，只能当成“命令进入运行时”的确认。

## Runtime Query Lifecycle

```text
UI / SDK
  → AgentRuntime.query(RuntimeQuery)
  → AgentRuntime routes to owning module
  ← QueryResponse { as_of_sequence, revision?, data }
```

query 是只读 request/response，不分配 `CommandId`，不增加 event sequence，也不向其他 subscribers 广播。`as_of_sequence` 只是结果生成时 runtime 已发布到的水位，不表示 query 自己是事件。查询对象后续变化时，正常业务事件负责更新或失效 UI cache，例如 `session_name_changed` / `session_deleted` 影响 session list，`resources_changed` 影响资源详情，`command_catalog_changed` 影响 catalog，`usage_updated` 直接替换 usage view。

面向用户的 `/status`、`/usage`、`/help` 仍是 command text，结果通过 `command_output_appended` 显示；GUI/sidebar/设置页等程序化读取使用 `RuntimeQuery`。长时间 indexing、export、login、reload 等会产生副作用或进度的操作仍是 command/job + events，不得伪装成 query。

## Event Families

| Family | Wire event type 示例 | 生命周期 | 是否可由 RuntimeSnapshot 重建 |
| --- | --- | --- | --- |
| session lifecycle | `session_created`、`session_opened`、`session_closed`、`session_tree_changed` | 离散事实 | 是 |
| run lifecycle | `run_started`、`run_suspended`、`run_resumed`、`run_finished` | started -> suspended/resumed* -> terminal | 是，当前 run 只能部分重建 |
| message lifecycle | `message_user_appended`、`message_assistant_started`、`message_assistant_text_delta`、`message_assistant_finished`、`message_tool_result_appended` | appended 或 started -> delta* -> finished | committed message 可重建；active partial 只在当前 host/current run 中可见 |
| tool call lifecycle | `tool_call_proposed`、`tool_call_approval_requested`、`tool_call_started`、`tool_call_output_delta`、`tool_call_finished` | proposed -> approval? -> started -> delta* -> finished | active tool state 可从 current run 重建；重启后只保留 committed `ToolRound` facts |
| queue lifecycle | `queue_updated` | 每次队列变化发完整队列摘要 | 是 |
| usage lifecycle | `usage_updated` | 模型调用、run 结束、压缩或 prompt material 改变后替换 usage/context view | 是 |
| resource lifecycle | `resources_reload_started`、`resources_changed` | reload started -> committed revision | 是 |
| command catalog | `command_catalog_changed` | catalog revision changed | 是 |
| command result | `command_output_appended`、`command_interaction_requested`、`command_interaction_resolved` | 用户命令产生 UI-safe 输出、display-neutral interaction 请求或交互结果 | 部分；取决于是否持久化 UI-only panel item |
| compaction lifecycle | `compaction_started`、`compaction_finished` | started -> finished | 是 |
| retry lifecycle | `retry_auto_started`、`retry_auto_finished` | started -> finished | 是 |
| diagnostics | `diagnostics_runtime_changed`、`diagnostics_error`、`diagnostics_warning` | 离散或替换式 | 部分 |
| idle lifecycle | `session_settled` | idle state fact | 是 |

## Event Sources

| Source | 可以发 UI `agent_runtime_protocol::Event` 吗 | 说明 |
| --- | --- | --- |
| `AgentRuntime` | 可以 | workspace、session open/close/delete、runtime diagnostics、resource reload 入口、command catalog 和 command result；query result 直接返回，不发布事件。 |
| `SessionRuntime` | 可以 | 单会话 phase、run、queue、usage/context usage、稳定 message batch commit、compaction 和 retry。 |
| `RunTask` | 不直接发 UI event | 只推进一次 Driver/Rig run，并经私有 `RunLink` 把 update、safe-point、commit candidate 和 terminal effect 交给 owner `SessionRuntime` actor。 |
| `Driver` | 不直接发 UI event | 只发 `DriverEvent` 给 `SessionRuntime` 归约。 |
| `ModelGateway` | 不直接发 UI event | 通过 `ModelStreamSink` / `DriverEvent` 上报模型流和 usage。 |
| `Tools` | 不直接绕过 `SessionRuntime` | 可以使用 `SessionRuntime` 传入的工具更新 sink，但所有 UI 事件仍由会话运行时归约并拥有 correlation 和 phase。 |
| `ResourceManager` | 不直接发 UI event | 管理 `ResourceSnapshotStore`、返回 reload/capture 结果和诊断；`AgentRuntime` 发布 `resources_changed`，`SessionRuntime` 在后续 user turn 捕获新 snapshot 并创建新 `PromptTurn`。 |
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
| `SessionRuntime` actor | `SessionPhase`、current run projection、queues、model state、session-scoped `Tools` lifecycle、fixed workspace cwd、run-captured `TurnResourceSnapshot`、compaction/retry state、pending stable batches、RuntimeSnapshot projection state | 发布/归约绝大多数 `run_*`、`message_*`、`tool_call_*`、`queue_updated`、`compaction_*`、`retry_*`、`session_settled` | 通过 mailbox 消费上层 command、`RunTask` control effect、`Tools` progress/update、`SessionHandle.commit(...)` 结果；后期消费自己拥有安全点的 hook result |
| `RunTask` | 单次 `Driver` / Rig `AgentRun`、run-local usage/limits、owned `SessionDriverHost`、cancellation | 不发布 UI event；经 `RunLink` 上报内部 effect | 消费 `DriverHost` model/tool/safe-point 结果，不拥有 session phase、queues、writer 或 terminal arbitration |
| `Driver` | Rig `AgentRun` 推进中的临时 protocol state、step handling、driver-local counters/limits | 不发布 UI event；发内部 `DriverEvent` | 消费 `DriverHost` 的 model/tool/safe-point 结果 |
| `ModelGateway` | provider/model selection execution context、credentials resolution、future provider payload hooks、fallback/retry metadata、usage/error normalization | 不发布 UI event；通过 stream sink 返回 model delta/usage/failure | 被 `SessionRuntime` / `DriverHost` 调用；完整边界见 [ModelGateway](model-gateway.md) |
| `Tools` | session-scoped 工具注册、active tools、prompt catalog、policy、approval、grants、sandbox、mutation locks、execution coordination、executor update forwarding | 通过 `SessionRuntime` event sink 归约为 `tool_call_*`；自身不拥有 UI event metadata | 消费 tool definitions/policy/approval/executor；后期消费工具治理 hook；返回 `ToolBatchResult` |
| `ToolExecutor` | 单个工具执行过程和底层副作用句柄 | 不发布 UI event；只返回 result 和 progress chunk | 被 `Tools` 调用 |
| `ResourceManager` | `ResourceSnapshotStore`、current `RuntimeResourceSnapshot`、current `CwdResourceSnapshot`s、overlay policy、resource diagnostics、reload/capture results | 不发布 UI event；返回 reload result / capture result | `AgentRuntime` 发布 `resources_reload_started` / `resources_changed`；`SessionRuntime` 在 future turn 捕获 `TurnResourceSnapshot` |
| `CommandSurface` / `CommandManager` | command catalog materialization、name conflict diagnostics、`CommandRunPolicy`、command text parse result、suggestion、execution resolve | 不发布 UI event；返回 catalog、suggestion、resolved invocation 或 UI-safe command result | `AgentRuntime` 发布 `command_catalog_changed` 和 command output events；`SessionRuntime.command` 执行 session-scoped command |
| `Skills` | 无生命周期状态；只提供 metadata/catalog parsing/format helpers | 不发布事件 | 被 `ResourceManager` / `SessionRuntime` 调用 |
| `Prompt` | 无运行生命周期状态；纯创建 `PromptTurn`、展开 intent，并由 `prompt::project_model_call(profile + call-time lanes)` 生成 `ModelInputProjection` | 不发布事件 | `SessionRuntime` 调用 turn/intent seam，Driver 调用纯 projection seam；事件由 SessionRuntime 归约 |
| `Compaction` | 无运行生命周期状态；只提供准备、cut point、summary prompt、result helper | 不发布事件 | `SessionRuntime` 持有 compaction lifecycle 并发 `compaction_*` |
| `SessionHandle` | 单会话领域操作 facade、统一 batch commit、上下文重建结果 | 不发布 UI event；返回 `CommittedSessionBatch` / context | `SessionRuntime` 根据 commit 结果发布对应 `message_*`、`session_*`、`compaction_*` 等领域事实 |
| `SessionStorage` / `SessionWriter` | 单会话 metadata、committed batches、leaf、path-to-root index | 不发布事件 | 被 `SessionHandle` 调用；隐藏 memory/JSONL adapter 写入细节 |
| `SessionManager` | persistent session catalog、`LoadedSessionRuntimes`、create/open/list/delete/fork/close、storage adapter | 通常不直接发事件；由 `AgentRuntime` 发 session lifecycle 事件 | 被 `AgentRuntime` 调用；创建/关闭/查找 `SessionRuntimeHandle`，但不拥有单会话运行状态机或客户端 selection |
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
pending stable batch drafts
Rig AgentRun
```

### SessionRuntime Ownership

`SessionRuntime` 是单会话产品状态机 owner。凡是与“这个会话正在发生什么”有关的事件，默认由它发布或归约。

持有状态：

```text
SessionPhase
CurrentRun
pending stable batch drafts
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
SessionHandle commit/build_context result
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
DriverEvent::ModelCallStarted
DriverEvent::ModelTextDelta
DriverEvent::ModelCallFinished { usage: Option<ModelCallUsage> }
DriverEvent::AssistantMessageStarted
DriverEvent::AssistantMessageDelta
DriverEvent::AssistantMessageFinished
DriverEvent::BeforeNextModelCall
DriverEvent::BeforeRunFinish
DriverEvent::RunFinished
```

`run_started` 也不包装成 `DriverEvent`；`SessionRuntime` 在建立 `CurrentRun`、调用 Driver 前直接发布。工具 lifecycle 不重复包装成 `DriverEvent`。`Tools` / `ToolUpdateSink` 只提供 proposed、approval requested、started、output delta 和 result-ready 等内部更新；`result-ready` 仍可能被 `ToolResultBeforeCommit` 改写，不能直接发布为 UI `tool_call_finished`。这些内部输入都不直接进入 UI。`SessionRuntime` 决定哪些转换成 `agent_runtime_protocol::Event`，何时组装稳定 batch、何时 commit，以及何时发 `run_finished`。

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
  ├─ loaded session ids
  ├─ open/close runtime lifecycle
  └─ shutdown_all / optional idle unload
```

相关 UI wire events 仍由 `AgentRuntime` 发布：

```text
session_created
session_opened
session_closed
session_deleted
session_tree_changed
```

`SessionManager` 不能把 `LoadedSessionRuntimes` 扩展成 Agent loop owner：phase、queue、current run、tool state、usage stats、compaction/retry 和稳定 batch 提交时机仍属于 `SessionRuntime`。

### SessionHandle / SessionStorage Ownership

`SessionHandle` 和 `SessionStorage` 拥有可恢复历史，不拥有实时 UI event。

`SessionHandle` 持有/提供：

```text
commit(SessionWriteBatch)
build_session_context
```

`SessionStorage` 持有：

```text
SessionMetadata
Committed SessionWriteBatch[] / SessionEntry[]
current leaf id + stable batch-boundary index
path-to-root and grouped get_committed_batches_to_leaf reconstruction support
```

相关 UI wire events 由 `SessionRuntime` 发：

```text
message_user_appended
message_assistant_finished // for committed ToolRound / AssistantFinal
message_tool_result_appended
session_name_changed / session_tree_changed
session_model_changed / session_thinking_level_changed / session_active_tools_changed
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
QueryResponse
```

## Global Ordering Rules

1. 同一个 `AgentRuntime` 的 `sequence` 必须严格单调递增。
2. 同一个 `run_id` 只能有一个 `run_started`，并且必须有且只有一个 terminal `run_finished`。
3. `run_suspended` / `run_resumed` 只能发生在 `run_started` 之后、terminal `run_finished` 之前；它们不是终态，不能替代 `run_finished`。
4. `message_assistant_text_delta` 必须发生在对应 `message_assistant_started` 和 `message_assistant_finished` 之间。
5. `tool_call_output_delta` 必须发生在对应 `tool_call_started` 和 `tool_call_finished` 之间。
6. `tool_call_approval_requested` 只能发生在 `tool_call_proposed` 之后、`tool_call_started` 之前。
7. `message_user_appended`、正常稳定 assistant message 的 `message_assistant_finished`、`message_tool_result_appended`、需要恢复的 session mutation event、成功 `compaction_finished { result: Some(...) }` 和正常 `run_finished { completed }` 只能在对应稳定 batch commit 成功后发出。
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
- 外层 `Event.session_id = None` / `run_id = None` 的 `diagnostics_error`：不可归属到具体 session/run 的错误。
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

`session_opened` 表示运行时已创建 `SessionRuntime` 并加入 `RuntimeSnapshot.loaded_sessions`；不表示当前正在执行 Agent run。

多 session 同时 loaded 时，打开新 session 不要求关闭旧 session。`session_closed` 只表示 runtime 从 `LoadedSessionRuntimes` 和后续 `RuntimeSnapshot.loaded_sessions` 卸载，不表示从 catalog 删除。客户端选择属于 adapter-local state，不产生 core event；若被选择的 session 收到 `session_closed`，adapter 自行清除或选择 fallback，runtime 不代选。

### 6. Session Phase Lifecycle

`SessionPhase` 是单个 `SessionRuntime` 的互斥工作状态。

| From | To | 触发条件 |
| --- | --- | --- |
| `Idle` | `Turn` | idle prompt/follow-up 开始产品级工作 |
| `Idle` | `Compaction` | manual/auto compaction 立即开始 |
| `Turn` | `Idle` | terminal handling（required stable commit if any）、terminal facts 和 post-run arbitration 完成，且没有立即 continuation |
| `Turn` | `RetryBackoff` | Agent run 失败且已安排自动重试；`delay_ms` 可以为 0 |
| `Turn` | `Compaction` | post-run manual/auto compaction 或 overflow recovery 开始 |
| `RetryBackoff` | `Turn` | backoff 结束并启动 retry run |
| `RetryBackoff` | `Idle` | retry 被取消、禁用或不再继续 |
| `RetryBackoff` | `Compaction` | retry 前必须先完成 overflow recovery |
| `Compaction` | `Idle` | compaction 完成/取消/失败，且没有立即工作 |
| `Compaction` | `Turn` | compaction 后立即启动 prompt continuation 或 overflow retry |
| `Compaction` | `RetryBackoff` | compaction 后仍需等待 Agent run retry delay |

连续 run、active Steer 或 immediate follow-up 可以保持 `Turn -> Turn`，这不是 phase 变化，不发布重复的 `session_phase_changed(turn)`。provider fallback/retry 不改变 SessionPhase；compaction summary 调用自身的 provider retry 也保持 `Compaction`。

对应事件：

- `session_phase_changed`
- `session_settled` 只能在 phase 回到 `idle` 且不会立即继续时发出。

`session_settled` 是状态事实，不是某个 wait command 的完成回执。公开协议没有 `WaitForIdle` / `wait_finished`；adapter reducer 直接消费该事件。CLI/RPC/test helper 只应覆盖“先订阅再 dispatch”或调用方已经维护该 session reducer 的场景。`RuntimeSnapshot.loaded_sessions` 可以恢复任一 loaded session 的当前 phase/queue/run state，但订阅起点、事件是否已经发生和 gap 语义仍留给 BR-044。

`run_started` 不替代 `session_phase_changed(turn)`：phase 是产品级互斥状态，run 是一次 Rig drive / agent 工作单元。反过来也一样：`CommandAck` 或 `session_phase_changed(turn)` 不表示公开 run 已开始，UI 不能据此要求一个尚不存在的 `RunId` 或显示 run-level Stop。`Turn + current_run = None` 仅表示有界 admission/finalization；所有可能长时间等待的 model/provider/tool/approval 工作都必须在 `run_started` 后发生。

`WaitingApproval` 和 `Suspended` 属于 `CurrentRunState`，发生时 SessionPhase 仍为 `Turn`。`BranchSummary` 是 session entry 和 future model-call purpose，不是 MVP SessionPhase。session close/unload 直接销毁 `SessionRuntime`，不增加 `Closed` phase。

### 7. Run Lifecycle

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

UI 只在收到 `run_started` 或 snapshot 中存在 `current_run.run_id` 后发送 `AbortRun { run_id }`。如果 adapter 希望支持用户在 run id 到达前按下 Ctrl-C，只能按 originating `command_id` 保存 UI-local pending abort intent；随后收到 matching `run_started` 时立即发送普通 abort，若先收到 command rejection、`session_phase_changed(idle)`、`session_settled` 或 session close 则清除 intent。该 local intent 不是 runtime command、queue 或 snapshot 状态。

`run_suspended` 是可恢复暂停，不是终态。它表示 `Driver` / `SessionRuntime` 已在协议安全 checkpoint 停住，并持有 resume state；恢复后继续同一个未完成 run 的 continuation。`run_finished` 仍是唯一 terminal run event，不存在 `run_finished { status: paused }`。

普通中间状态通过 message/tool/approval 事件体现，不额外暴露 `run_model_calling` 或 `run_waiting_tool`，避免状态重复。`WaitingApproval` 可以只通过 `tool_call_approval_requested` 和 `RunView.pending_tool_approvals` 表达；只有需要挂起并等待显式 resume 时，才进入 `CurrentRunState::Suspended`。

### 8. Model Call Lifecycle

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

### 9. User Message Lifecycle

User message 通常不是流式对象。

```text
AcceptedByCommand
  → Expanded(skill/template)
  → future hook expansion/patch if hook system is enabled
  → SessionWriter.commit(SessionWriteBatch::user_input(...))
  -- message_user_appended --> AppendedToSession
  → IncludedInRunContext
```

对应事件：

- `skill_invoked` / `prompt_template_invoked`：仅当目标 `PromptTurn` 已实际展开 intent 且对应 `UserInput` commit 成功后发布，并先于 `message_user_appended`。idle/future-turn admission 尚未公开 run，外层 `Event.run_id = None`；active Steer 的外层 `Event.run_id = Some(current_run_id)`。
- `message_user_appended`

FollowUp/NextTurn 入队时只发 `queue_updated`，不提前发 invoked；active Steer 在 active run safe point 展开后发 invoked。`message_user_appended` 只在 `UserInput` batch commit 成功后发布，因此它同时表示 UI 可以渲染消息、下次恢复也会包含该输入。

### 10. Assistant Message Lifecycle

Assistant message 是流式对象。

```text
NotStarted
  -- message_assistant_started --> Started
  -- message_assistant_text_delta* --> StreamingText
  ├─ final response: SessionWriter.commit(SessionWriteBatch::assistant_final(...))
  └─ tool-call response: collect all tool results, then SessionWriter.commit(SessionWriteBatch::tool_round(...))
  -- message_assistant_finished --> Finished
```

对应事件：

- `message_assistant_started`
- `message_assistant_text_delta*`
- `message_assistant_finished`

每条 assistant message 必须恰好有一个 `message_assistant_started` 和一个 `message_assistant_finished`；三类事件使用同一个 `message_id`，所有 delta 只能出现在 started/finished 之间，finished 后的最终文本必须等于有序 delta 拼接结果。正常 final assistant 在 `AssistantFinal` commit 成功后发布 finished；包含 tool calls 的 assistant message 在对应完整 `ToolRound` commit 成功后发布 finished，并先于该 batch 的 `message_tool_result_appended*`。abort/failure 为关闭 UI lifecycle 也必须发布 finished，但未完成 partial 不持久化，重启后不会恢复。

### 11. Tool Call Lifecycle

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
  → SessionWriter.commit(SessionWriteBatch::tool_round(...))
  -- message_assistant_finished --> AssistantToolCallMessageFinished
  -- message_tool_result_appended* --> ToolResultMessagesAppended
```

对应事件：

- `tool_call_proposed`
- `tool_call_approval_requested`
- `tool_call_started`
- `tool_call_output_delta*`
- `tool_call_finished`
- `message_tool_result_appended`

policy denied、approval rejected、schema invalid、unknown tool 都走 `tool_call_finished { is_error: true }`，并生成 error tool result message；不直接让 run failed。全部 calls 都产生 actual/error result 后，assistant tool-call message 与 results 作为一个 `ToolRound` commit；commit 成功后先发布该 assistant message 的 `message_assistant_finished`，再按 `call_index` 为每条 result 发布 `message_tool_result_appended`。

### 12. Approval Lifecycle

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

审批状态不能由 adapter 私有保存为权威状态；同一 host 生命周期内的订阅/状态重建后，从对应 `RuntimeSnapshot.loaded_sessions[*].current_run.pending_tool_approvals` 恢复。`tool_call_approval_requested` 是一次事件；pending approval 的 UI-safe current state 由 `SessionRuntime` actor 的 `CurrentRun` projection 持有，`ToolApprovalBroker` 只持冻结 execution record 与 waiter。snapshot 不暴露 prepared args。

### 13. Queue Lifecycle

消息队列和 pending session actions 都是 session runtime 状态，不是持久化 transcript。`PendingSessionAction::Compact` 是结构化 post-run action，不是模型可见消息。

```text
Empty
  -- queue_updated --> NonEmpty
  → DrainingAtSafePoint
  -- queue_updated --> Empty or NonEmpty
```

对应事件：

- `queue_updated { follow_up, steering, next_turn, pending_actions }`

`queue_updated` 每次发送完整队列摘要，而不是 delta，且只在 queue/pending state 实际变化时发布。新加载 session 的初始 queue 由 `RuntimeSnapshot.loaded_sessions` 投影，不为了客户端切换视图制造额外事件。这样 adapter reducer 不需要理解运行时 drain 细节。

普通 prompt、skill、prompt template 和 prompt-producing slash command 都通过 `PromptDelivery` 进入相同队列入口。`Steer` 在最早 `before_next_model_call` 安全点消费，`FollowUp` 在当前 work 和 pending session action 完成后消费，`NextTurn` 只在下一次显式用户 turn 合并。`CommandRunPolicy::Immediate` 的 `/status` 等命令不改队列，直接追加 command output；`QueueAfterRun` 的 `/compact` 只更新 `pending_actions`。

`AbortRun` 在进入 idle 前清除尚未消费的 steering/follow-up 和 pending actions，保留 `NextTurn`；只有状态实际变化时才发 `queue_updated`。`ClearQueue` 清除 steering、follow-up、next-turn 和 pending actions；若清理造成状态变化，则发布四者均为空的完整 `queue_updated`。两条路径都不携带 removed items 或 editor action。

### 14. Compaction Lifecycle

Compaction 是 session context projection，不是普通 run。

```text
Requested while idle
  -- session_phase_changed(compaction) --> PreparingCutPoint
  -- compaction_started --> PreparingSummary
  → future SessionBeforeCompact hook if hook system is enabled
  → Summarizing
  → SessionWriter.commit(SessionWriteBatch::compaction(...))
  → RebuildingSessionContext
  -- compaction_finished --> Finished(aborted? failed? will_retry?)
  ├─ immediate retry/continuation → session_phase_changed(turn) → run_started
  ├─ delayed retry → session_phase_changed(retry_backoff) → retry_auto_started
  └─ no immediate work → session_phase_changed(idle) → session_settled

Requested while active work
  -- queue_updated(pending compact) --> Deferred
  → current work terminal handling（required stable commit if any）+ terminal facts
  → required retry / overflow recovery chain completes
  -- queue_updated(remove pending compact) --> Dispatching
  -- session_phase_changed(compaction) --> PreparingCutPoint
```

对应事件：

- `session_phase_changed { phase: compaction }`
- `compaction_started`
- `compaction_finished`
- `session_phase_changed { phase: turn | retry_backoff | idle }`
- 只有进入 `idle` 时才发布 `session_settled`；进入 `turn` 时紧接 `run_started`

摘要模型调用不发 `message_assistant_started`，因为它不是用户会话里的 assistant 回复。

### 15. Auto Retry Lifecycle

Auto retry 是 run failure 后的恢复流程。每个 `retry_auto_started { attempt }` 表示一个确定的 retry attempt，必须恰好由同 attempt 的 `retry_auto_finished` 关闭；不能像 pi 的宽松事件流那样让多个 start 共用一个最终 end。`attempt` 从 1 开始，表示原始失败后的第几次自动重试，`max_attempts` 表示允许的最大自动重试次数。

```text
failed run terminal facts
  -- session_phase_changed(retry_backoff) --> RetryBackoff
  -- retry_auto_started(attempt=N) --> ScheduledBackoff
  → retry delay elapses
  -- session_phase_changed(turn) --> Turn
  -- run_started --> RunningRetryAttempt
  -- run_finished --> RetryRunTerminal
  -- retry_auto_finished(attempt=N, status) --> RetryAttemptFinished
```

对应事件：

- `retry_auto_started`
- `run_started`
- `run_finished`
- `retry_auto_finished`

`retry_auto_finished.status` 使用 `Succeeded | Failed | Aborted`，不能用 `success: bool` 把 failed 与 aborted 合并。若 attempt 失败且仍可继续，顺序是 `run_finished(failed)` → `retry_auto_finished(failed, attempt=N)` → `session_phase_changed(retry_backoff)` → `retry_auto_started(attempt=N+1)`；中间不进入 `Idle`。若 backoff 被 `AbortRetry` 取消，顺序是 `retry_auto_finished(aborted)` → 最终 post-run arbitration → `session_phase_changed(idle)` / `session_settled`，或直接进入另一个待执行 workflow。

即使 `delay_ms = 0`，也保留 `Turn → RetryBackoff → Turn` 的 phase 事实；两个 phase event 可以紧邻，但不能伪造 `Idle`。`retry_auto_started` 之后即使 retry admission 在 `run_started` 前失败，也必须发布 matching `retry_auto_finished(failed)`。如果 retry 会马上启动新 run，前一个 failed run 后不应发 `session_settled`，否则 UI 会短暂误判为空闲。

### 16. Usage And Context Usage Lifecycle

Usage lifecycle 描述模型调用消耗、run 汇总、会话累计 stats 和当前上下文占用 view 的更新。它不是持久化屏障，也不是成本账单。

```text
StatsStable
  -- model_call_finished --> RunUsageUpdated
  -- usage_updated --> StatsStable

RunCompleting
  -- final stable batch committed --> FinalUsageReady
  -- usage_updated --> FinalUsagePublished
  -- run_finished --> StatsStable

StatsStable
  -- compaction_finished/resources_changed/session_tools_changed --> ContextUsageRecomputed
  -- usage_updated --> StatsStable
```

对应事件：

- `usage_updated`
- `run_finished { usage }`

`usage_updated` 可以在模型调用结束、run 结束、压缩结束、资源或工具变更后发出。它表示当前 host 的 UI usage/context view 已更新；需要跨进程恢复的 usage facts 必须包含在相关稳定 session batch 中，并在正常 `run_finished` 或 `compaction_finished` 前 commit。`SessionStatsView.total_usage` 是累计模型调用消耗，压缩不会让它下降；`ContextUsageView.current_tokens` 是下一次模型请求的上下文窗口占用，压缩后通常会下降。

### 17. Resource Reload Lifecycle

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

`resources_changed` 的外层 `Event.workspace_id` 必须是当前有效 workspace，payload 携带 `cwd` 和 revision。adapter reducer 用 `(workspace_id, cwd)` 更新所有 matching loaded `SessionSnapshot.resources`；同一 cwd 的多个 session 会看到同一 current effective summary。失败 reload 也可以发 `resources_changed`，但 revision 仍是旧值，diagnostics 描述失败。

如果 reload 改变 session-scoped catalog，`AgentRuntime` 为每个受影响的 loaded session 发布各自带 `Event.session_id` 的 `command_catalog_changed`；不能只更新某个客户端选中的 session。

`command_catalog_changed` 不是资源持久化屏障；它只是 command catalog projection 的替换式更新。外层 `Event.session_id = None` 时更新 `RuntimeSnapshot.runtime_command` 对应的 runtime/workspace catalog；`Some(session_id)` 时更新对应 loaded `SessionSnapshot.command`。adapter autocomplete / command palette 应按同一 scope reduce，不能把 runtime catalog 当成某个默认 session catalog。

### 18. Session Commit Semantics

Session commit 是内部写入 seam，不是公共事件 lifecycle。所有 mutation 通过 `SessionHandle.commit(SessionWriteBatch)`：

```text
Stable facts assembled in memory
  → SessionWriter.commit(batch)
      ├─ Ok  → publish corresponding domain events / continue work
      └─ Err → do not expose batch as committed; fail current mutation or run
```

正常 text-only run 至少提交两个稳定 batch：current user input 必须在 `run_started` 前 commit，最终 assistant message 必须在正常 `run_finished { completed }` 前 commit。包含工具时，每个将被下一模型调用消费的完整 tool round 都必须先把 assistant tool-call message 与全部 tool-result messages 作为一个 `ToolRound` commit；并行结果按 `call_index` 归约后共享该 batch。

streaming delta、tool output delta、queue update、partial assistant、pending approval 和执行中的 tool round 不是 session writes。abort/failure/shutdown 只保留此前成功 committed 的 batches，当前不完整单元直接丢弃。公共协议不发布通用 save-point event；后期内部 `AfterSessionCommit` observer 可以用于可重建 cache、backup、telemetry 和测试。

### 19. Session Tree Lifecycle

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

导航 session tree 不能删除历史，只移动当前 leaf。`session_tree_changed` 只能在 `TreeMutation` batch commit 成功后发布。

### 20. Diagnostics Lifecycle

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

`diagnostics_error` 是一次事实通知；`diagnostics_runtime_changed` 是当前诊断集合的权威替换。runtime/workspace/session/run/command 归属只读取外层 `Event`；payload 的 optional `DiagnosticSubject` 仅用于 tool call、resource path 或 model 等更细对象，不得形成第二套路由 scope。

### 21. Hook Lifecycle

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

### 22. Reconnect Lifecycle

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

这也是为什么 RuntimeSnapshot 必须在同一个 `last_event_sequence` 水位包含全部 loaded sessions 的权威运行态，而不是只包含某个客户端当前显示的 session。客户端 selected session 不影响 snapshot coverage。

## Submit Prompt Lifecycle

```text
UI
  → dispatch(SubmitPrompt)
  ← CommandAck { command_id, accepted: true }

SessionRuntime
  → session_phase_changed { phase: turn }
  → SessionWriter.commit(SessionWriteBatch::user_input(...))
  → message_user_appended
  → run_started
  → message_assistant_started
  → message_assistant_text_delta*
  → [for each tool-call response]
      → tool_call_*
      → SessionWriter.commit(SessionWriteBatch::tool_round(...))
      → message_assistant_finished      // finishes the tool-call assistant message
      → message_tool_result_appended*
      → message_assistant_started       // next model response
      → message_assistant_text_delta*
  → SessionWriter.commit(SessionWriteBatch::assistant_final(...))
  → message_assistant_finished          // finishes the final assistant message
  → usage_updated                       // optional, when provider usage exists
  → run_finished { status: completed }
  → session_phase_changed { phase: idle }
  → session_settled
```

如果 immediate retry、pending manual compact、follow-up 或 queued steering continuation 导致继续工作，`run_finished` 后不应立即 `session_settled`；应先发对应 `queue_updated` / lifecycle event，再启动 compaction、下一次 `run_started` 或在安全点继续。next-turn queue 本身不会阻止 `session_settled`。

## Tool Lifecycle

```text
Driver receives CallTools
  → tool_call_proposed { call_id, name, args, risk, requires_approval }
  → tool_call_approval_requested?       // if policy asks user
  → tool_call_started
  → tool_call_output_delta*
  → tool_call_finished { result, is_error }
  → SessionWriter.commit(SessionWriteBatch::tool_round(...)) // assistant tool calls + all results
  → message_assistant_finished          // assistant tool-call message; same committed batch
  → message_tool_result_appended*       // one per result, ordered by call_index
```

推荐理由：

- 工具活动事件与 tool result message 分离，这样 UI 能展示工具活动，同时 transcript 仍由 message 事件维护。
- Codex 对 exec、patch、MCP 都使用 begin/delta/end 或 item started/completed；这让长任务可被 UI 稳定折叠、恢复和计时。
- 未知工具、未启用工具、schema invalid、policy denied、approval rejected、executor failed 都应产生 error tool result，而不是让 run 崩溃。只有 abort/cancel 改变 run terminal status。

## Approval Lifecycle

```text
tool_call_proposed { requires_approval: true }
tool_call_approval_requested
UI dispatch(DecideToolApproval)
  ├─ approved  → tool_call_started → ... → tool_call_finished
  └─ rejected  → record error result → tool_call_finished { is_error: true }
all calls resolved
  → SessionWriter.commit(SessionWriteBatch::tool_round(...))
  → message_assistant_finished
  → message_tool_result_appended*
```

审批请求是运行时事件，不是 UI callback。adapter 只能通过 `DecideToolApproval` 回答，不能直接调用 tool executor，也不能替换工具参数。同一 host 生命周期内如果 adapter、subscriber 或 reducer 重建，当前仍未解决的审批必须从对应 `RuntimeSnapshot.loaded_sessions[*].current_run.pending_tool_approvals` 重新投影。

## Abort Lifecycle

本 lifecycle 的前置条件是目标 `run_id` 已通过 `run_started` 公开且仍为 current run；prompt admission/finalization 不进入 run abort lifecycle。

```text
UI dispatch(AbortRun)
  ← CommandAck
SessionRuntime / Driver cancellation
  → queued tool/model updates stop
  → if a stable commit already started: await its Ok/Err and apply commit-won facts
  → discard remaining uncommitted partial assistant / approval / incomplete tool round
  → message_assistant_finished          // iff still started; closes UI lifecycle, not persisted
  → run_finished { status: aborted }
  → queue_updated?                      // iff steering/follow-up/pending actions changed; next-turn preserved
  → session_phase_changed { phase: idle }
  → session_settled
```

不要同时发 `run_aborted` 和 `run_finished`。推荐一个 terminal event：`run_finished { status: aborted }`，避免 UI reducer 处理双终态。若 abort 到达前 `AssistantFinal` commit 已进入 writer 并成功，则 commit admission ordering 判定 completed 已获胜，随后 abort 应得到 no-active-run / too-late 结果，不能把同一 run 改发 aborted；若获胜的是 `ToolRound` commit，则保留该完整 round、阻止下一模型调用，再以 aborted 收尾。

abort 只清除仍未消费的 steering/follow-up 和 pending actions；`NextTurn` 不属于当前自动 work chain，因此保留。已经完成 `UserInput` commit 的 active steer 是 durable message，不得删除。`queue_updated` 仍只发布清理后的完整 snapshot，不携带 `returned_to_editor` 或 removed delta；editor restore 是具体 adapter 基于 UI-local submission history 实现的可选体验，不是 runtime event 或重连保证。显式 `ClearQueue` 才清除包括 `NextTurn` 在内的全部 queue state。

## Failure Lifecycle

Provider 或 protocol 失败：

```text
run_started
message_assistant_started?              // if provider streaming had started
discard uncommitted partial assistant / incomplete tool round
message_assistant_finished?             // required iff started; not persisted
diagnostics_error { recoverable }        // run coordinate comes from outer Event
run_finished { status: failed }
post-run arbitration
  ├─ retry scheduled    → session_phase_changed(retry_backoff) → retry_auto_started
  ├─ overflow recovery  → session_phase_changed(compaction) → compaction_started
  ├─ pending compaction → session_phase_changed(compaction) → compaction_started
  ├─ immediate continuation → remain Turn → run_started
  └─ no immediate work  → session_phase_changed(idle) → session_settled
```

原则：

- 同步 preflight 失败不启动 run，直接拒绝 command 或发外层携带 `command_id` 的 `diagnostics_error`。
- 可重试 Agent run 失败必须先关闭当前 run，再直接从 `Turn` 进入 `RetryBackoff`；不能为了发布 retry event 先经过 `Idle`。
- context overflow recovery 不把 transient overflow assistant error 写入 session；它只通过 diagnostics 呈现，并从最后 committed context 启动 recovery。该分支使用 `Turn → Compaction → Turn` 和 `compaction_finished { will_retry: true }`，不发布 session-level `retry_auto_started`。
- provider fallback/retry 属于同一个 model call/run 的内部 attempt，不改变 `SessionPhase`，也不发布 session-level retry event。

## Compaction Lifecycle

```text
UI dispatch(Compact) while active work
  → queue_updated { pending_actions: [Compact] }
  → current work continues
  → run terminal handling（required stable commit if any）+ terminal facts
  → required immediate retry / overflow recovery chain, if any
  → queue_updated { pending_actions: [] }

UI dispatch(Compact) while idle, drain pending Compact, or auto threshold/overflow
  → session_phase_changed { phase: compaction }
  → compaction_started { reason }
  → future RuntimeHookRegistry.invoke(SessionBeforeCompact) // internal only
  → summary model call                         // not a normal assistant message
  → SessionHandle.commit(SessionWriteBatch::compaction(...))
  → compaction_finished { result, aborted: false, will_retry }
  ├─ overflow retry / immediate continuation
  │    → session_phase_changed { phase: turn }
  │    → run_started
  ├─ scheduled auto retry
  │    → session_phase_changed { phase: retry_backoff }
  │    → retry_auto_started
  └─ no immediate work
       → session_phase_changed { phase: idle }
       → session_settled
```

压缩摘要是 session context projection，不是 system prompt。`compaction_finished` 可以给 UI 摘要预览，但完整摘要默认通过 session entry detail 读取。

## Resource Reload Lifecycle

资源 snapshot 的创建和替换发生在 `ResourceManager` 方法内部；事件只负责通知已经发生的事实。初始化路径也遵循这个规则：

```text
UI dispatch(OpenWorkspace)
  → WorkspaceServices::new(...)
  → ResourceManager.ensure_runtime_snapshot(ResourceInitReason::WorkspaceOpen)
      └─ missing: build RuntimeResourceSnapshot and replace_runtime(...)
  → RuntimeSnapshot { workspace: Some(...), loaded_sessions: [] }

UI dispatch(OpenSession or NewSession)
  → SessionManager.open_handle(...) / create_handle(...)
  → read fixed { workspace_id, cwd } from session metadata
  → ResourceManager.ensure_cwd_snapshot(CwdResourceRequest { reason: SessionOpen, ... })
      ├─ missing: load cwd-local resources, overlay, replace_cwd(...)
      └─ stale runtime revision: recompose_cwd(...), replace_cwd(...)
  → create SessionRuntime { workspace_id, cwd, services }
  → session_opened
```

MVP 没有公开 `workspace_opened` 事件；`OpenWorkspace` 的接收由 `CommandAck` 表达，打开后的 workspace state 由 snapshot 恢复。后期 hook point `WorkspaceOpened` 也不等于公开 wire event。

`ensure_*` 必须幂等。并发 open session、first turn 或 reload 只能通过 `ResourceSnapshotStore.replace_*` 发布完整的新 snapshot，不能让 UI 事件成为状态更新来源。

```text
UI dispatch(ReloadResources)
  → resources_reload_started { cwd }           // workspace coordinate is on outer Event
  → ResourceManager.reload_cwd(CwdResourceRequest { workspace_id, cwd, ... })
      ├─ success: build CwdResourceSnapshot { runtime, local, resolved }
      │          and ResourceSnapshotStore.replace_cwd(workspace_id, cwd, snapshot)
      └─ failure: keep old CwdResourceSnapshot for cwd, collect diagnostics
  → resources_changed { cwd, revision, skills, prompt_templates, context_files,
                        system_prompt, append_system_prompts, diagnostics }
  → diagnostics_runtime_changed?               // only for runtime-level diagnostics
```

`resources_changed` 只传摘要，不传技能全文、context file 正文或完整 system prompt。UI 如果要详情，通过运行时命令读取。

`ResourceSnapshotStore.replace_cwd(...)` 是资源 reload 对后续 turn 生效的原子发布点。runtime 不把新 snapshot 推送到每个 idle `SessionRuntime`；下一次 `SubmitPrompt` / `InvokeSkill` / `InvokePromptTemplate` 真正启动 user turn 时，`SessionRuntime` 调用 `ResourceManager.capture_turn(...)`，把 current `CwdResourceSnapshot` 放入 `TurnState.resources`，并从同一 snapshot 创建新的 `PromptTurn`。active run 的 Steer 继续使用 `CurrentRun.prompt_turn`。

```text
ReloadResources 完成 replace_cwd(C2)
  → resources_changed { revision: C2.revision }

下一次 SubmitPrompt
  → SessionRuntime::start_user_turn(...)
  → ResourceManager.capture_turn(...)
  → ResourceSnapshotStore.current_cwd(...) == C2
  → TurnState.resources = TurnResourceSnapshot { cwd: Arc<C2> }
```

如果 submit 在 `replace_cwd` 之前已经开始 capture，它可以合法使用旧 snapshot；如果 submit 在 `replace_cwd` 之后才开始 capture，就必须看到新 snapshot。旧 snapshot 不会被原地修改，正在运行的 turn 继续使用自己已经捕获的 `TurnResourceSnapshot`。

## Session Open Lifecycle

```text
adapter dispatch(OpenSession)
  → open existing runtime or SessionManager.open_and_load(...)
  → ensure ResourceManager has current CwdResourceSnapshot for session { workspace_id, cwd } if newly loaded
  → session_opened?                            // outer Event.session_id identifies it; only if newly loaded
  → session_phase_changed { phase: idle }?     // only for newly loaded runtime projection
  → resources_changed?                         // if this cwd snapshot changed
  → session_settled?                           // if newly loaded runtime is settled
```

多 session 同时 loaded 时，打开新 session 不应自动关闭或改写其他 session 的 fixed cwd、phase、queue、current run 或已经捕获到 `TurnState` 的 `TurnResourceSnapshot`。目标已 loaded 时 `OpenSession` 是幂等 no-op；只有显式 `CloseSession`、workspace teardown、idle unload 或 shutdown policy 才发 `session_closed`。

`session_opened` 之后 adapter 可以重新请求 `agent_runtime_protocol::RuntimeSnapshot` 取得完整 `SessionSnapshot`。事件流负责增量变化，snapshot 负责权威恢复。`OpenWorkspace` 本身不自动恢复旧 session，因此 `RuntimeSnapshot.loaded_sessions` 为空；持久化 session list 使用 `RuntimeQuery::Session(SessionQuery::List { ... })`。adapter 选择显示哪个 session 是本地状态。

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

`agent_runtime_protocol::Event`、`agent_runtime_protocol::EventMsg`、各事件族 enum、`RunTerminalStatus`、`DiagnosticSubject` 和完整 wire event type 映射以 [AgentRuntimeProtocol](agent-runtime-protocol.md) 为权威定义。本文件只解释生命周期、顺序、所有权和场景约束，避免同一份协议类型在两个文档中漂移。

## Test Matrix

MVP event lifecycle tests 应覆盖：

- submit prompt text-only：`UserInput` / `AssistantFinal` commit 成功后才发布对应 message/run 事件，并验证 `session_settled` 顺序。
- skill/template invocation：idle/future-turn 在 `UserInput` commit 后发布外层 `Event.run_id = None` 的 invoked，再发 `message_user_appended`；active Steer 使用外层 `Event.run_id = Some(current_run_id)`；invoked payload 不含 `run_id`，展开或 commit 失败不发 invoked。
- user input commit failure：不发布 `message_user_appended` / `run_started`，不调用模型，phase 回到 idle 并产生 command/runtime diagnostic。
- assistant streaming：delta 必须在 `message_assistant_started` / `message_assistant_finished` 之间。
- tool success：`tool_call_proposed` / `tool_call_started` / `tool_call_finished`；`ToolRound` commit 后先发布 tool-call assistant 的 `message_assistant_finished`，再按 `call_index` 发布 `message_tool_result_appended*`。
- tool round commit failure：以非持久化 `message_assistant_finished` 关闭已 started lifecycle，不发布 `message_tool_result_appended`，不进入下一模型调用，当前 run 以 failed terminal 收尾。
- tool denied / approval rejected：产生 error tool result，与同 batch 其他 results 一起 commit 完整 `ToolRound` 后才发布 appended events，不产生 run failure。
- settled observation：没有 `WaitForIdle` command 或 wait-completed event；UI 通过 snapshot + `session_settled`，测试 event probe 必须先订阅再触发动作。任意 session 的 late-subscribe/gap 行为在 BR-044 关闭后再定义。
- pre-run admission：`CommandAck` / `session_phase_changed(turn)` 不产生 run-level abort target；在任何 provider/model/tool/approval wait 前必须先发布 `run_started`。
- actor responsiveness：active `RunTask` 正在等待 model/tool/approval 时，目标 `SessionRuntimeHandle` 仍能处理 `DecideToolApproval`、`AbortRun`、Steer/FollowUp/NextTurn admission、`ClearQueue`、pending `Compact`、snapshot 和 shutdown；禁止 actor 内联等待完整 `drive_run()`。
- approval wakeup：tool batch 登记 pending approval 并发布 request 后，actor 处理 matching decision，Tools waiter 被唤醒且 executor 恰好执行一次；duplicate/stale/abort-after-request 不重复执行。
- safe-point transaction：active Steer 经 actor drain/expand/`UserInput` commit/领域事件后才回复 `NextModelCallPlan`；commit failure 时 RunTask/Rig 不得到该 message。
- run identity fencing：旧 RunTask 在新 run 启动后发送的 late delta、safe-point、tool-round candidate 或 terminal effect 被识别为 stale，不能污染新 `CurrentRun`。
- control/progress ordering：高频 progress lane 不饿死 approval/abort；terminal event 和 snapshot barrier 前已接受的 delta/update 必须完成归约，不能 finished-before-delta 或产生水位撕裂。
- abort run：只有一个 terminal `run_finished { status: aborted }`；started assistant 必须 finished，uncommitted partial/approval/tool round 不进入 session；若清理造成 queue state 变化，清除 steering/follow-up/pending actions、保留 `NextTurn` 的 `queue_updated` 必须先于 idle/settled。
- clear queue：若 accepted command 实际清除了 queue/pending state，则发布 steering/follow-up/next-turn/pending actions 全为空的完整 `queue_updated`；空队列 no-op 不发冗余更新，也不发布 removed delta 或 editor action。
- abort/commit race：in-flight `ToolRound` commit 先完成并保留 round，但不进入下一模型；in-flight `AssistantFinal` commit 成功时 completed 获胜，同一 run 不得再发 aborted。
- final assistant commit failure：关闭已 started 的 assistant lifecycle，发 diagnostics + `run_finished { failed }`，不得发 completed。
- session mutation commit failure：model/thinking/tools/name 等 runtime-visible state 不替换，也不发布 corresponding changed event。
- compaction：`Compaction` batch commit、phase、`compaction_started` / `compaction_finished` 和 settled 顺序。
- resource reload failure：旧 revision 不变，diagnostics 更新。
- command text view：`/status`、`/usage` 产生 `command_output_appended`，不把结果放入 `CommandAck`。
- event envelope：所有 reducer 只从外层读取 workspace/session/run/command coordinates；msg schema 不重复通用坐标，tree old/new leaf 等 transition operands 保留；按 routing profile 分类的内部 constructor 拒绝缺失 required coordinate。
- command interaction：`/model`、`/thinking` 产生 `command_interaction_requested`，选择后通过 `ExecuteCatalogCommand`、runtime-tracked `SubmitInteraction` 或明确结构化 `AgentCommand` 完成设置；不依赖 interaction request 携带 raw command text。
- command text semantic error：unknown command 或 phase 不允许时，`CommandAck` 可 accepted，并通过 error severity 的 `command_output_appended` 告知用户。
- explicit session routing：core 没有 `FocusSession` / focus event；session-scoped command 缺少 `SessionId` 时返回 `SessionRequired`，不能回退到唯一 loaded 或最近 opened session。
- multi-session snapshot：两个或更多 loaded sessions 同时推进 work 时，projection barrier 产生同一水位的 `loaded_sessions`；客户端 selection 不改变 coverage，close/open 与 sequence 不得形成跨水位拼接。
- handle snapshot barrier：snapshot builder 冻结 loaded handle membership，flush 每个 actor 的 accepted progress/control effects，再读取 all-loaded projection 和 `last_event_sequence`；禁止逐 handle 无保护拼接。
- abort run routing：`RunOwnerIndex` 在 `run_started` 前登记并在 terminal boundary 清除；stale index 命中时目标 actor 二次校验拒绝，不能 abort 后来的 run。
- resync：RuntimeSnapshot sequence 后的事件可正确 reduce。
