# 实现路线图

实现路线图按可验证闭环推进。MiniCore 不应先平铺所有模块空壳，也不应纯粹 Driver-first；更合理的路径是 **spine-first + early Driver/Rig seam validation**：先打通最小协议、事件、会话、存储和 fake driver 纵切，再尽早验证 Rig sans-IO seam，随后把 JSONL、资源、工具、usage、compaction、mutation 和 bash 逐层接到已经稳定的主脊柱上；hook system 作为后期扩展点另行接入。

完整 CLI/TUI/GUI 产品不在本仓库实现；本仓库只提供可嵌入 runtime core、协议、session、resources、tools、events、compaction 和 driver orchestration；hooks 是后期 runtime extension 能力。

## 路线原则

```text
Minimal protocol / event / session spine
  → InMemory storage vertical slice
  → fake driver SubmitPrompt loop
  → early Rig Driver seam spike
  → JSONL / resources / tools / usage / compaction / mutation / bash
  → future hooks / extension runtime
```

核心原则：

- 先验证能被下游 UI 观察和恢复的 runtime spine，再扩大功能面。
- `SessionRuntime` 尽早成为单会话事实 owner；`AgentRuntime` 保持薄门面；`Driver` 只驱动 Rig step。
- `InMemorySessionStorage` 先作为 storage contract 和测试底座；JSONL 在 entry、leaf、save point 语义稳定后实现。
- `Driver` 不能拖到最后才验证。先用 fake driver 跑通纵切，再用隔离 spike 验证 Rig `AgentRun / AgentRunStep` 是否满足设计假设。
- `ModelGateway` 的稳定 seam 必须早于真实 `Driver` 集成出现。阶段 5 不能写临时 provider/auth/usage 路径；应先有最小 `ModelGateway` spine，再让 `DriverHost::call_model(...)` 只走这条 seam。
- `Tools` 子系统在 read-only tool 切片出现；`ToolApprovalBroker`、`ToolApprovalGrantStore`、approval modes 和完整 sandbox/mutation 约束等到 mutation tool 前后再做完整闭环。
- `CommandSurface` 和 `Compaction` 是 runtime spine 上的扩展能力，不应早于它们依赖的 owner 流程；`CommandSurface` 的实现形态是共享无状态 `CommandManager` + session-scoped `Command`。`RuntimeHooks` 是后期扩展点系统，当前 MVP 不实现 hook registry / hook invocation，只在设计上固定 owner 分层。

## 先决设计点

正式实现前先定清楚这些容易返工的点：

1. `RuntimeSnapshot` scope：MVP 使用 workspace/runtime-scoped `snapshot() -> RuntimeSnapshot`，不接受 `session_id`，也不单独持久化。打开 workspace 后默认不聚焦旧 session；`RuntimeSnapshot.active_session` 可以为空。会话清单由 `SessionIndex` / `ListSessions` 提供，TUI `/resume` 默认按当前 workspace 筛选，GUI sidebar 复用同一 query。MVP 中 UI host 和 `AgentRuntime` 同进程、同生命周期，不支持 UI 断线但 runtime daemon 继续后台运行再被重连；`last_event_sequence` 只用于同一 host 生命周期内的初始化、late subscribe、reducer/subscriber 重建和 sequence gap recovery。未来如需多 tab、多 session detail、独立 runtime server、多窗口共享 runtime 或大规模分页，再拆 `WorkspaceSnapshot` / `SessionSnapshot` 或 scoped event cursor。
2. `RuntimeServices` scope：采用单 UI/runtime 进程内多 session 运行模式。`AgentRuntime` 持有 `WorkspaceServices`，其中包含共享 `ResourceManager`、user-global settings/provider/auth 和 runtime-global `ModelGateway`；不引入 `CwdScopedServices` 或 service generation registry。`ResourceManager` 管理级联不可变资源快照：`RuntimeResourceSnapshot -> CwdResourceSnapshot -> TurnResourceSnapshot -> StepResourceSnapshot`。每个 `SessionRuntime` 固定一个 workspace cwd；每次 run 启动时通过 `ResourceManager.capture_turn(...)` 捕获 `TurnResourceSnapshot` 并构建 `TurnState`。Focused session 只影响 UI 和默认命令路由，不能作为资源或服务 scope 锚点。
3. `ModelGateway` seam：它负责 provider 调用、凭据解析、fallback、usage 归一化和错误分类，是 `Driver`、`Compaction`、`UsageStats` 的共享依赖。后期 provider hook 的 owner 也在该边界内。正式实现前以 [ModelGateway](modules/model-gateway.md) 为 source of truth；实现顺序上先落稳定最小 spine，再扩展 custom provider、完整 auth、fallback 和 usage/context usage。
4. 失败事件顺序：失败 assistant message、diagnostic、`persistence_save_point` 和 `run_finished { status: failed }` 的顺序必须统一。`persistence_save_point` 是 durable barrier，不能让 UI 在 terminal event 后仍处于不可恢复状态。

## MVP 阶段

| 阶段 | 目标 | 主要文件 | 可验证产物 |
| --- | --- | --- | --- |
| 0. Crate skeleton 和基础类型 | 建立最小 crate、ID、error、message、协议子集 | `src/lib.rs`、`src/ids.rs`、`src/error.rs`、`src/messages.rs`、`src/agent_runtime_protocol.rs` | `AgentCommand` / `Event` / `RuntimeSnapshot` / `MessageRecord` / `SessionPhase` 可编译、可序列化。 |
| 1. EventBus 和最小 AgentRuntime | 建立 `dispatch`、`subscribe`、`snapshot()`、event sequence、水位、command ack、`WorkspaceServices` 和 `ResourceManager` / `ResourceSnapshotStore` 骨架 | `src/agent_runtime.rs`、`src/agent_runtime_events.rs`、`src/runtime_services.rs`、`src/resource_manager.rs` | event sequence 单调递增；`RuntimeSnapshot` 带 `last_event_sequence`；`OpenWorkspace -> ensure_runtime_snapshot` 后 `active_session = None`；无法路由命令能 rejected；`replace_runtime` / `replace_cwd` 原子替换 current pointer。 |
| 2. InMemory session spine | 实现 `SessionStorage` trait、`SessionHandle`、内存 storage、最小 `SessionManager`、轻量 `SessionIndex` 和 loaded runtime lifecycle | `src/session_storage.rs`、`src/session_storage/memory.rs`、`src/session_manager.rs` | create/open/list、workspace-scoped `ListSessions`、append message、leaf/path-to-root、context rebuild 通过同一组 storage contract tests；loaded runtime 记录 fixed workspace cwd，focus 切换不改写。 |
| 3. First vertical slice with fake driver | `NewSession` / `OpenSession` 后 `SubmitPrompt` 跑通：phase guard、user message、run-time `TurnResourceSnapshot` capture、fake assistant stream、save point、RuntimeSnapshot | `src/session_runtime.rs`、`src/driver.rs` | `OpenSession -> ensure_cwd_snapshot -> RuntimeSnapshot.active_session(idle)`；两个 loaded sessions 可各自持有 `SessionRuntime` 和 fixed cwd；focus B 不中止 running A；`SubmitPrompt -> capture_turn -> message_user_appended -> run_started -> assistant delta -> persistence_save_point -> run_finished -> session_settled` 顺序稳定；`capture_turn` 在缺失 cwd snapshot 时能兜底 ensure。 |
| 4. Rig Driver + ModelGateway seam spike | 隔离验证 Rig sans-IO step，并提前固定模型调用 seam；不掺 session persistence | `src/driver.rs`、`src/driver/rig.rs`、`src/model_gateway.rs`、`src/provider_registry.rs`、`src/auth_store.rs` | 证明 `CallModel -> Done` 只能经 `DriverHost::call_model -> ModelGateway.call_model(ModelCallRequest)`；`ModelSelection`、`ModelCallRequest`、`ModelCallResult`、`ModelCallErrorKind`、`ModelCallUsage` shape 稳定；最小 `ProviderRegistry.resolve` / `AuthStore.resolve` 可 fake；若 Rig API 不符，返工范围限制在 driver/model gateway seam。 |
| 5. Text-only Driver integration | 用真实 `Driver.drive_run()` 替换 fake driver，先只接模型文本流，并用 per-run `SessionDriverHost` wrapper 连接 `SessionRuntime` 与 `DriverHost`；复用阶段 4 的 `ModelGateway` spine | `src/driver.rs`、`src/driver/rig.rs`、`src/model_gateway.rs`、`src/model_gateway/rig.rs`、`src/session_runtime.rs` | `DriveRequest` 只携带 `DriverTurnInput`，不携带完整 `TurnState` / `TurnResourceSnapshot`；真 Driver 只传 `ModelSelection`、system prompt、tool schemas、thinking/stream options；`DriverHost::call_model` 不允许 match provider 或直读 env；`ModelGateway` fake/minimal provider adapter 产生 assistant started/delta/finished；普通 provider failure 归约为 `DriveResult::Failed` 和唯一 `run_finished`；`Driver` 不长期持有 session state。 |
| 6. JSONL storage | 在 storage contract 稳定后实现文件持久化 | `src/session_storage/jsonl.rs`、`src/session_manager.rs` | InMemory 和 JSONL 通过同一组 conformance tests；重开后 RuntimeSnapshot/context 一致。 |
| 7. Resources / Skills / Prompt | 接入 `ResourceManager` 级联 snapshot、cwd overlay policy、skill、prompt template、纯 prompt builder | `src/resource_manager.rs`、`src/skills.rs`、`src/prompt_templates.rs`、`src/prompt.rs` | `ReloadResources(cwd) -> build CwdResourceSnapshot -> replace_cwd -> resources_changed`；replace 后开始的下一轮 `capture_turn` 必须拿到新 snapshot；replace 前已经开始 capture 的 turn 合法使用旧 snapshot；`InvokeSkill` / prompt template 在真正进入 user turn 时用 captured `TurnResourceSnapshot` 展开为 user message；资源变化只影响 future turn。 |
| 8. CommandSurface skeleton | 建立跨 UI command catalog、nested JSON manifest、dynamic providers、parse/suggest/resolve 和 session-scoped execution facade，但不要求所有后端命令完整 | `src/command.rs`、`src/command/manager.rs`、`src/command/session.rs`、`src/command/handlers/` | `/help`、`/reload`、`/skill <name>`、兼容 `/skill:{name}`、`/{template}` 可用；`/compact` 在 compaction 后端完成前为 disabled；`/usage`、`/model`、`/thinking` 随后端能力逐步启用。 |
| 9. ModelGateway 扩展和 UsageStats | 在既有 `ModelGateway` spine 上补齐 custom provider、完整 auth 来源、fallback chain、usage 归一化和 context usage | `src/provider_registry.rs`、`src/model_gateway.rs`、`src/auth_store.rs`、`src/usage_stats.rs` | `SetModel -> ModelState -> DriverTurnInput -> ModelCallRequest -> ModelGateway` 可测；custom provider/base URL/auth_ref 走同一 seam；provider usage 变成 `usage_updated`；RuntimeSnapshot 包含 provider/model view，`active_session` 包含 `session_stats` 和 `context_usage`；后续 compaction 能复用 context usage。 |
| 10. Read-only Tools | 实现 session-scoped `Tools` 子系统、工具定义、registry、active set、prompt catalog、最小 policy、planner/coordinator/executor 和只读工具 | `src/tools.rs`、`src/tools/subsystem.rs`、`src/tools/definition.rs`、`src/tools/registry.rs`、`src/tools/active.rs`、`src/tools/prompt.rs`、`src/tools/policy.rs`、`src/tools/planner.rs`、`src/tools/coordinator.rs`、`src/tools/executor.rs`、`src/tools/builtin/{read,grep,find,ls}.rs` | Rig `CallTools -> DriverHost::invoke_tool_batch -> SessionRuntime -> Tools::invoke_batch -> tool results -> Rig continuation` 跑通；tool error 作为 error tool result 回填；结果按 `call_index` 稳定回填。 |
| 11. Tool approval | 实现 pending approval 状态机和协议决定 | `src/tools/approval.rs`、`src/agent_runtime_protocol.rs`、`src/session_runtime.rs` | `tool_call_approval_requested`、`DecideToolApproval`、approve/reject、active session pending approval RuntimeSnapshot/resync 行为稳定；approval 后 args 冻结。 |
| 12. Compaction | 实现手动压缩、summary message、context rebuild；再做 threshold 和 overflow recovery | `src/compaction.rs`、`src/session_runtime.rs`、`src/session_storage.rs` | `/compact` 从 disabled 变可执行；`SessionEntry::Compaction` 重建上下文；overflow recovery 不污染重试上下文。 |
| 13. Mutation tools | 在 approval、policy、event、storage 都稳定后接文件修改工具 | `src/tools/builtin/{write,edit,apply_patch}.rs`、`src/tools/policy.rs`、`src/tools/approval.rs` | approval preview、diff stats、mutation queue、reject-as-error-tool-result、rewrite 后重新 schema validate / sandbox / policy。 |
| 14. Bash | 最后接最高风险进程工具 | `src/tools/builtin/bash.rs` | timeout、cancel、cwd/sandbox、stdout/stderr streaming、输出截断和 approval 都可观察、可恢复。 |
| 15. Protocol harness / example adapter | 验证下游 CLI/TUI/GUI 能只靠协议接入 | `examples/`、`tests/` | fake adapter/reducer 测试 submit、tool、approval、reload、compaction、RuntimeSnapshot/resync 的事件生命周期。 |

## 首个开发切片

第一条可交付切片应非常小，但必须完整经过 runtime spine：

```text
AgentRuntime.dispatch(SubmitPrompt)
  → SessionManager.get_or_create_runtime(session_id)
  → SessionRuntime phase guard
  → SessionHandle append user message backed by InMemorySessionStorage
  → fake Driver emits assistant text
  → message_user_appended
  → run_started
  → message_assistant_started
  → message_assistant_text_delta*
  → message_assistant_finished
  → persistence_save_point
  → run_finished
  → session_phase_changed(idle)
  → session_settled
  → agent_runtime_protocol::RuntimeSnapshot(last_event_sequence, active_session.messages, active_session.phase)
```

这条切片的验收重点：

- `run_started` 只能有一个终态 `run_finished`。
- assistant delta 必须位于 `message_assistant_started` / `message_assistant_finished` 之间。
- `persistence_save_point` 必须发生在可恢复写入之后。
- `session_settled` 只在 phase 回到 `idle` 且不会立即 retry/compaction/follow-up 时出现。
- `RuntimeSnapshot` 是同一 host 生命周期内订阅/状态重建后的权威当前状态，而不是事件流的替代品；它不落盘，关闭窗口后由 runtime 在下次启动时重新投影。

## Driver/Rig 验证策略

`Driver` 是最高风险 seam 之一，但不应让它拖着所有模块一起验证。推荐分两步：

1. 先用 fake driver 跑通第一条纵切，让 `SessionRuntime`、event bus、storage、RuntimeSnapshot projection 的 owner 关系稳定。
2. 立刻做隔离 Rig spike：只验证 `AgentRun::next_step()`、`CallModel`、`CallTools`、`Done`、`model_response(...)`、`tool_results(...)`、usage 和 serialization/pause 方向。

如果 Rig sans-IO API 和设计假设不一致，应优先调整 [Driver](modules/driver.md) seam，而不是让 `SessionRuntime`、`Tools` 或 `AgentRuntimeProtocol` 直接吸收 Rig 细节。

## Storage 顺序

`SessionStorage` trait 和 `SessionHandle` 是必要 seam；`InMemorySessionStorage` 和 `JsonlSessionStorage` 是两个 adapter。

推荐顺序：

```text
SessionStorage trait
  → InMemorySessionStorage
  → first vertical slice and storage conformance tests
  → JsonlSessionStorage
```

不要同时开工 InMemory 和 JSONL。原因：

- InMemory 是最快的测试底座，可以先把 entry、leaf、path-to-root、context rebuild 和 save point 语义跑稳。
- JSONL 是产品持久化必需，但格式一旦落盘就会带来兼容性负担。
- JSONL 应复用 InMemory 已通过的 conformance tests，而不是长出另一套行为。

## Tools 顺序

工具分三层推进：

```text
Read-only tools:
  Tools subsystem
  → ToolDefinition / ToolRegistry / ActiveToolSet / ToolPromptCatalog
  → minimal ToolPolicy / ToolInvocationPlanner / ToolExecutionCoordinator
  → read / grep / find / ls

Approval:
  ToolApprovalBroker
  → ApprovalRequestId / DecideToolApproval protocol
  → pending approval recovery

Grants and modes:
  ToolApprovalGrantStore
  → AskEveryTime / UseRememberedGrants / AutoAllow / AutoDeny

Mutation / process tools:
  sandbox + mutation lock
  → write / edit / apply-patch
  → bash
```

`Tools` 要在第一个工具切片就出现，因为它是真正连接 `DriverHost::invoke_tool_batch` 和产品级工具治理的 session-scoped 子系统。`ToolApprovalBroker` 不必在 read-only tools 阶段做满；它应在 mutation tools 前完成，因为 approval 后 args 冻结、reject-as-error-tool-result、preview、同一 host 生命周期内的 pending approval resync、duplicate/stale decision 和 grant scope 都是高风险行为。

## CommandSurface 顺序

`CommandSurface` 是 runtime-owned 用户命令面，但不是底层执行通道。它应在 resources/skills/prompt template 之后出现，因为 command catalog 需要 resource metadata projection。实现上先落共享无状态 `CommandManager`，再让每个 `SessionRuntime` 持有 session-scoped `Command` facade。

MVP 不要求所有 command 都完整可执行：

- 先实现 builtin nested JSON command pack、临时 materialized catalog 和 `ExecuteCommandText` / `ExecuteCatalogCommand` 的 parse/suggest/resolve 框架。
- 先实现 `/help`、`/reload`、`/skill <name>`、兼容 `/skill:{name}`、`/{template}` 和基础 UI-safe command output。
- 先把 skill / prompt template 作为 dynamic command nodes 注册进 catalog，但正文展开仍只在 `SessionRuntime` 的 captured turn snapshot 中发生。
- `/compact` 在 `Compaction` 后端完成前只进入 catalog，状态为 disabled。
- `/usage`、`/model`、`/thinking` 在 `UsageStats`、model state、provider registry 完成后再启用完整行为。
- extension executable commands、project-local command handlers、complex form、runtime-tracked `SubmitInteraction` 和 runtime-owned action ids 放到后续增强。

## RuntimeHooks 后期顺序

`RuntimeHooks` 不放入当前 MVP 阶段。Hook 的价值来自真实 owner 流程中的安全点；没有 owner 流程时，hook 只是过早抽象。当前阶段只固定 owner 分层和禁止边界，实际 `RuntimeHookRegistry`、hook invocation、capability gate 和 extension runtime 都作为后期需求。

后期第一批只接入已经存在且测试稳定的 owner 安全点：

- Resource discovery hook/callback 不属于后期第一批 hook；资源来源先由 `ResourceManager` 内置 resolver 管理。
- `BeforeAgentStart` / `PromptBuilt`：run 启动和 system prompt 构建后 patch。
- `ContextProjection`：模型可见上下文 projection 后 patch；必须保持协议安全。
- `ToolBeforePolicy` / `ToolAfterExecute`：工具治理链路内的 typed decision。
- `SessionBeforeCompact`：压缩前 gate / patch instructions / provide result。
- `AfterSavePoint`：保存点后 observer。
- `CommandOutputBuild`：UI-safe command output patch。

后续 privileged hook，例如 raw provider payload patch、context replacement、tool args rewrite、system prompt replacement 和 before-session-write 类能力，应等对应 owner 流程和测试稳定后再开放。

## Compaction 顺序

`Compaction` 应晚于 storage context rebuild、ModelGateway 和 UsageStats，但早于 mutation/bash。它不依赖文件修改工具；反而能提前验证 session projection、summary message 和 context usage。

推荐推进：

1. 手动 `Compact { instructions }`。
2. summary model call 通过 `ModelGateway`，不是 `Driver.drive_run()`。
3. 后期再接 `SessionBeforeCompact` hook。
4. 追加 `SessionEntry::Compaction`。
5. `SessionHandle.build_session_context()` 重建为 summary message + kept messages。
6. `compaction_finished`、`persistence_save_point`、后续 `session_settled` 或 retry continuation。
7. threshold 自动压缩。
8. context overflow recovery。

## 后续增强

- 会话树导航、会话命名、标签、导出和 import 兼容性。
- 更完整的自动压缩策略、chunked summary、overflow retry 策略和摘要质量评估。
- 更完整的 `RuntimeHookRegistry`、trusted package hooks、privileged hooks 和 extension runtime。
- 多 workspace、多窗口、多 session 并行运行所需的 service scope 和 event routing 能力。
- 下游 CLI/TUI/GUI SDK packaging、example adapter 和兼容性测试。

## 设计约束

- 下游 UI/CLI 代码不能导入 Rig 类型。
- 下游 UI/CLI 代码不能直接扫描、解析或拼接技能内容。
- 下游 UI/CLI 代码不能直接读写会话文件。
- 下游 UI/CLI 代码不能注册能绕过工具策略、工作区沙箱或凭据边界的运行时 Hook。
- 工具执行只能发生在 Agent 运行时内。
- 下游渲染器不能拥有凭据或工作区访问权。
- 运行时方法应返回确认或快照，而不是助手文本。
- 助手输出、技能调用、提示模板调用、会话保存、队列变化、工具活动以及后期运行时 Hook 影响必须始终能通过运行时事件观察到。

## 必测项

- protocol serde / compatibility：命令、事件、快照、工具审批 preview。
- event lifecycle：run 单终态、assistant/tool started-delta-finished 配对、terminal event 和 save point 顺序。
- RuntimeSnapshot/resync：sequence gap、active session pending approval、active session running、no active session；多 session focus/background run 的运行时正确性通过 `SessionRuntime` 独立 phase、fixed cwd 和 run-captured `TurnResourceSnapshot` 测试，UI reconnect contract 仍只覆盖同一 host 生命周期内当前视图。
- RuntimeServices scope：打开两个不同 cwd 的 session，`ResourceManager` 为每个 cwd 保存 current `CwdResourceSnapshot`；后台 running session 继续使用 run 启动时捕获的旧 `TurnResourceSnapshot`；focus 切换、resource reload、close/unload 不污染其他 loaded session。
- storage conformance：InMemory 和 JSONL 对 entry append、leaf、path-to-root、compaction projection 行为一致。
- Driver / ModelGateway seam：Rig `CallModel`、`CallTools`、`Done`、tool result threading、provider/tool error-as-result、`ModelSelection` 传递、auth redaction、custom provider base URL、usage/error normalization 和 cancellation。
- tool governance：active tool membership、schema validation、rewrite 后重新 schema validate / sandbox / policy、approval 后 args 冻结。
- resource reload：atomic reload/recompose、diagnostics、runtime/cwd resource revision、future-turn-only prompt rebuild。
- future hooks：后期启用 hook system 时，hook 不直接发布 `agent_runtime_protocol::Event`，不直接读写 storage，不直接执行 tool，不接触 credentials。
- compaction：protocol-safe cut point、orphan tool call/result 检查、summary message projection、overflow recovery 不把 transient error 放进 retry context。
