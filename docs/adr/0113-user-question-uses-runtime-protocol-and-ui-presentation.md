# ADR 0113: UserQuestion 使用 MiniCore 交互协议与 UI 展示 Adapter

状态：Partially Superseded by ADRs 0124, 0126 and 0127
日期：2026-07-25

> 2026-07-31：typed Interaction、MiniCore-owned live request/resolution与Presentation Adapter职责继续有效；ADR 0127规定restart不恢复Pending waiter/state，也不合成Cancelled resolution或Turn terminal。Recorded request/resolution只作为historical facts。

> 2026-07-30：typed Interaction、Presentation Adapter和oneshot等待保留；request/resolution改为live apply + inline best-effort append attempt，record failure不阻止notify/resume。

> 2026-07-29修订：MiniCore-owned durable Interaction、UI-owned presentation和pre-execution ask-user顺序保持有效。ADR 0124删除durable `ToolExecutionStarted`和`tool_round_completed`；ask-user仍发生在owner-local Tool start permit与file mutation ticket之前，answer的ToolResult在complete Tool exchange形成后进入conversation。

## 背景

`InteractionRequest` 已包含 `ToolApproval | UserQuestion`，但 Tool 到 SessionExecutor 只有 approval 与 execution-start seam，没有真正创建 `UserQuestion` 的 producer。若让 UI 自己决定何时提问、自己保存 pending state，MiniCore 收到的答案就无法可靠绑定到原来的 `TurnId`、`ItemId` 和 `RequestId`；UI 断线、重复提交、Cancel 和 crash recovery 也会各自形成第二套状态。

同时，等待用户回答不能预留file mutation ticket，也不能持有TurnControl reservation。否则一个用户不回答就会阻塞同一Session后续sibling ToolCall。

## 决策

### 1. 分离展示职责与交互协议职责

- **MiniCore-owned interaction protocol**：SessionExecutor/SessionStorage 创建并持有 typed Interaction 的 request/resolution、identity、durable Pending/Resolved state、append-before-notify、resolution-before-resume、Cancel、Unload、幂等和 recovery。
- **UI-owned presentation**：TUI、Web、GUI 或 RPC host 作为 Adapter，决定问题如何呈现（对话框、聊天消息、终端菜单、表单、文案和本地校验），收集答案后提交 `InteractionCommand::Resolve`。
- UI 不直接持有 Tool waiter、SessionWriter、SessionExecutor handle，也不直接 append Interaction entry。MiniCore 向 UI 发布 UI-safe 的 `InteractionView`，不发布 executor handle、凭据、准备好的 Tool 参数或 Sandbox 内部信息。

这不是两份状态：UI 只保存临时展示状态，MiniCore 的 durable Interaction 才是事实来源。UI 可以在断线后通过 Snapshot/StateEvent 使用同一个 `RequestId` 重新展示。

### 2. 增加窄的内部 producer seam

`ToolExecutionControl` 增加与 `request_approval` 对称的：

```rust
request_user_question(
    item_id: ItemId,
    request: UserQuestionRequest,
) -> Result<UserQuestionAnswer, ToolExecutionControlError>
```

该方法是 crate-internal seam，不是 UI 接口。调用方只等待 typed answer；SessionExecutor 在内部完成 InteractionRequested append、事件发布、Resolve 校验、InteractionResolved append 和 waiter 唤醒。权威 trait/interface 见 [Tool 子系统](../modules/tools.md#tool-execution-control)。

### 3. 首版 producer 是独占的 pre-execution ask-user route

- ToolSet 提供一个内建 ask-user route；具体公开 ToolName 不在本 ADR 冻结。
- route在owner-local Tool start reservation、file mutation ticket reservation和任何外部副作用之前调用`request_user_question`。
- 等待期间不预留mutation ticket，也不持有TurnControl reservation或`ToolStartPermit`。
- ask-user route返回`UserAnswer`后生成`PreExecution` truthful ToolResult并append `role=tool`；当同一assistant的全部matching results存在时，ConversationStorage自动形成complete exchange。
- 首版ask-user Interaction在一个assistant Tool exchange中独占等待；同一assistant step的其他ToolCall不得并行启动。这样当前Turn的“等待用户”语义不会与未决副作用混在一起。
- 普通Tool在start reservation已获胜或开始file mutation后不得调用该seam；未来若确有需要，必须另行定义可证明不持有mutation permit的producer protocol。完整start规则见[INV-401](../architecture.md#跨模块不变量索引)。

### 4. 等待语义

Pending UserQuestion 时：

```text
TurnStatus = Running
SessionExecutionState = Running
TurnExecutionPhase = WaitingForUserInput
InteractionState = Pending
```

当前 Turn 的逻辑执行停在该 Interaction：不开始下一次 Model 调用，也不开始新的副作用 Tool operation。SessionExecutor 本身不阻塞，继续 poll 当前 Tool future、InteractionControl、EmergencyControl、LifecycleControl 和 Snapshot mailbox。Steer 继续进入该 Turn 的 FIFO，Cancel 或 Unload 可以获胜；elapsed time 不会自动结束等待。

`UserAnswer` 不是新的 UserMessage，也不创建新 Turn；它恢复原 ToolInvocation，随后由同一 Turn 继续。UI transport disconnect 默认不等于 Cancel；MiniCore process restart 仍按既定 conservative recovery 关闭 Pending Interaction 并中断 Turn，不恢复旧 waiter。

### 5. Session 隔离

每个loaded Session拥有独立的SessionExecutor、InteractionControl lane、pending waiter、file mutation queue和`WaitingForUserInput` phase。Session A等待答案时，Session B/C可以继续Sampling、ExecutingTools或等待各自的Interaction。provider/backend或共享宿主I/O可能产生正常外部竞争；UserQuestion等待本身不占用这些资源。

### 6. 公开协议不携带 UI 组件

Runtime 公开 `InteractionView`、`StateEvent::InteractionRequested` 和 `InteractionCommand::Resolve`，但不冻结 modal、button、页面、终端 UI 或具体 transport schema。不同 Presentation Adapter 共享同一个 request/resolution protocol。

## 时序

```text
Model ToolCall (ask-user)
→ SessionExecutor 验证 Turn/Item
→ InteractionRequested append/apply
→ StateEvent::InteractionRequested
→ Presentation Adapter 展示并收集答案
→ ResolveInteraction(SessionId, TurnId, ItemId, RequestId, resolution_key)
→ InteractionResolved append/apply
→ 唤醒 Tool future
→ PreExecution ToolResult append
→ matching result set complete时形成CommittedToolExchangeDelta
→ 同一 Turn 的下一次 Model 调用
```

## 同类产品依据

公开行为通常采用相同的职责分离：Codex 类 server request 由运行时发起、客户端展示并返回；Claude Code 的 AskUserQuestion/permission 由 Tool/agent 流程发起、CLI 展示；MCP elicitation 由 server 请求、client 展示并返回。各项目的内部 durable/recovery 细节不完全公开，MiniCore 只借鉴可观察的 request/response 与 client-presentation 形状，不声称复制其内部实现。

## 后果

- 新增一个小而稳定的 producer interface，Presentation Adapter数量可以增加而不改变Session execution语义。
- Pending、重复回答、断线和 terminal cleanup 仍集中在 MiniCore，避免 UI 与 Runtime 双重事实。
- 首版ask-user route的独占和pre-execution限制牺牲了一些Tool内部自由度，但明确避免mutation-permit-across-await和同批sibling ToolCall饥饿。
- `WaitingForUserInput` 是 transient phase，不写入 SessionStorage；Interaction request/resolution 仍是 durable facts。

## 被否决的方案

### UI 自己提问，MiniCore 只接收裸答案

这会把答案变成新的 Submit/UserMessage，无法继续原 ToolInvocation/Turn，也无法可靠处理重复回答、断线和 Cancel。

### UI 直接持有 Tool waiter 或写 SessionStorage

这会产生第二个 mutable owner，破坏 append/apply 线性化、Session 隔离和 crash recovery。

### 在已开始file mutation的普通Tool内等待用户

用户不响应时会长期占用该Session的file mutation permit并阻塞后续sibling ToolCall；首版明确禁止。

## 修订关系

本 ADR 补充并细化 [ADR 0103](0103-turn-item-interaction-model.md) 的 Interaction producer 与等待语义、[ADR 0105](0105-session-executor-owns-loaded-session.md) 的 transient phase 与单 Session owner、[ADR 0108](0108-runtime-public-protocol.md) 的 UI/Runtime facade 关系；不改变 durable Interaction、per-session Executor 或 append/apply ownership 决策。

2026-07-27：[ADR 0116](0116-file-mutations-use-session-local-queues.md)将本ADR的resource-lock表述修订为Session-local file mutation ticket/permit；pre-execution等待顺序保持不变。
