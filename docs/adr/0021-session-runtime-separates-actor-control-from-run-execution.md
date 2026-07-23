# ADR 0021: SessionRuntime 分离 actor 控制面与 run 执行

状态：部分被[ADR 0025](0025-loaded-session-uses-one-session-executor.md)替代
日期：2026-07-12

ADR 0025保留本文的单一权威owner、运行期间持续处理控制请求、禁止跨外部I/O长期借用mutable state以及progress独立处理原则；替代本文强制的`SessionRuntime actor + RunTask`形状、session-scoped Tools ownership和batch writer术语。本文保留为历史决策记录。

## 背景

`SessionRuntime` 必须在 Agent run 飞行期间继续处理同一 session 的 `DecideToolApproval`、`AbortRun`、`PromptDelivery` queue、`ClearQueue`、pending `Compact`、配置命令、snapshot 和 shutdown。原 `SessionDriverHost<'a>` 草图把 `&mut Tools`、`&mut QueueState`、`&mut Option<CurrentRun>` 和 mutable event sink 借给覆盖整个 `drive_run().await` 的 host；若 per-session actor 内联等待该 future，mailbox 无法消费，而工具执行又可能等待 mailbox 中的 approval decision，形成确定性死锁。把整个 `SessionRuntime` 放进 `Arc<Mutex<_>>` 只会把借用冲突转换成 lock-across-await 死锁。

## 决策

每个 loaded session 使用一个持续运行的 per-session actor 实现 `SessionRuntime`。`SessionRuntimeFactory::spawn(...)` 创建 actor mailbox、启动 actor loop，并返回显式、可克隆的 `SessionRuntimeHandle`；`LoadedSessionRuntimes` 保存 handle，而不暴露可直接借用或加锁的 `SessionRuntime`。Handle 只提供 command dispatch、snapshot/read projection 和 graceful shutdown 等联系能力，不保存权威 session 状态，也不等待一次 Agent run 完成。

`SessionRuntime` actor 是单 session 的唯一权威 mutable-state owner，持有 `SessionPhase`、`CurrentRun` projection、queues、pending session actions、model/options、session-scoped `Tools` 生命周期、stable batch admission、post-run arbitration 和公共事件归约。它可以在一次不可取消的 `SessionWriter.commit(...)` 临界区内等待确定结果，但不能在 mailbox loop 内等待 provider call、工具执行、approval decision 或完整 `Driver::drive_run()`。

每次已经公开启动的 Agent run 由一个短期 `RunTask` 推进。`RunTask` 拥有 `Driver`、run-local usage/limits、cancellation token 和 owned `SessionDriverHost`；Driver 通常推进一个 Rig `AgentRun`，active Steer 时可在同一 `RunId` 下顺序 rollover 多个 segment。同一 session 的 MVP 同时最多有一个 active `RunTask`。`RunTask` 不拥有 session phase、queues、pending actions、session writer、公共 event sequence 或 terminal arbitration。

生产实现的 `SessionDriverHost` 不携带指向 actor state 的长期 `&mut` 引用。它持有 immutable run identity/context、`ModelGateway` handle、绑定本次 immutable tool/profile baseline 的 run-only `ToolBatchInvoker`、cancellation token，以及一个私有窄 `RunLink`。`RunLink` 只允许 `RunTask` 向 owner actor 提交 lifecycle update、safe-point request、完整 tool-round commit candidate 和 terminal result，并通过 request/reply 得到 `NextModelCallPlan`、`FinishDecision` 或 actor 最终提交的 `ToolBatchResult`；它不是公开 `SessionRuntimeHandle`，不能提交任意 session command。

`Tools` 继续是 `SessionRuntime` actor 内部的 session-scoped 子系统。Actor 保留 active tools、approval mode、grants 和 decision/admin 能力；`RunTask` 只得到 `ToolBatchInvoker`，不能 set active tools、改变 approval mode 或直接提交 decision。`Tools::capture_profile_baseline()` 在一次无 await 的 owner 操作中，从当前 committed active-tool/policy/sandbox baseline 原子构造 `ToolProfileBaseline { prompt, invoker, fingerprint }`：Prompt 消费其中的 `ToolPromptView`，`RunTask` 使用其中绑定 immutable `ToolExecutionProfile`、共享 executor registry 与短锁 approval broker handle 的 invoker，三者 fingerprint 必须一致。MVP 在 `Turn` 中拒绝 model/thinking/stream/active-tool mutation；后续若支持 safe-point apply，必须原子替换完整 baseline 所对应的 `PromptCallProfile` 与 future `ToolBatchInvoker`，不能只 patch 一侧。

等待 approval 的 batch future 不得持有会阻止 actor `Tools::decide_approval(...)` 的锁。`ToolApprovalBroker` 只用短临界区登记/移除 `approval_id -> oneshot sender`，随后释放锁再等待；内部 worker/task 不能因内联等待完整 batch 而停止 decision path。pending approval 的 UI-safe projection由 `ToolUpdateSink` 先交给 actor 更新 `CurrentRun`，actor 再发布 request event；resolved/abort/terminal 同样先更新 projection，再发布后续事实。`Tools` 仍不发布公共 UI event，也不写 session。

所有改变 session 事实的外部 command 与 `RunTask` control request 在 actor 处线性化。高频 streaming delta 使用独立 bounded/coalesced `RunProgressSink`，不能与 approval/abort/safe-point 共用一个可能被 delta 填满的 FIFO。每个 lifecycle/commit/terminal control request 携带已提交 progress watermark；actor 在发布依赖该 progress 的 finished/terminal fact 前归约到该 watermark。runtime snapshot barrier 同样要求每个 actor flush 已接受 progress，再投影状态；不能产生 finished-before-delta 或 snapshot 水位撕裂。

abort 与 commit 的胜负也在 actor 处决定：actor 在 commit admission 前已经观察到 abort/close/shutdown 时，可以拒绝 candidate 并取消 run；actor 一旦调用 `SessionWriter.commit(...)`，commit 获胜且不接收 run cancellation，terminal handling 等待其 `Ok` / `Err`。成功的 `ToolRound` 可以保留后再以 aborted 收尾；成功的 `AssistantFinal` 已跨过 completed terminal boundary，随后 abort 观察为 stale/no-active-run。

`RunTask` 只要求与 actor loop 并发推进，不把 `Send + tokio::spawn` 固化成稳定 interface。Rig/driver future 若非 `Send`，实现可以使用 local task 或由 runtime executor 分离轮询；无论采用哪种 executor，`RunTask` 都不能借用 actor-owned mutable state跨越 I/O await。

## 后果

- approval、abort、steer/follow-up/next-turn admission、pending compact、command output、snapshot 和 shutdown 在 run 飞行期间保持可响应。
- `SessionRuntimeHandle` 成为上层联系 loaded runtime 的显式深 interface；`RunLink` 成为 run task 回到 owner actor 的更窄内部 seam。
- `SessionRuntime` 仍是唯一 session 编排 owner；actor 与 `RunTask` 不是新增的平级架构模块。
- ADR 0011 的 Tools session ownership 保留，但其中长期 mutable-borrow 的 `SessionDriverHost` 实现草图由本 ADR 修订。
- ADR 0013 的窄 `DriverTurnInput`、ADR 0019 的单可信 writer、ADR 0020 的多 loaded session/no-current-session 决策保持不变。

## 未选择方案

- 未选择 actor 内联 `await drive_run()`：会阻塞 mailbox，并使 approval/abort/queue 命令无法到达。
- 未选择 `Arc<Mutex<SessionRuntime>>`：锁顺序和 lock-across-await 会把 BR-047 转换为运行时死锁，并让 snapshot/commit ordering 分散到多个调用者。
- 未选择只给 `ToolApprovalBroker` 增加 channel：它只能修 approval，不能解决 steer、abort、clear queue、pending compact、snapshot 和 shutdown。
- 未选择让 `SessionManager` 或 `Driver` 成为单 session 状态机 owner：前者只管理 runtime lifecycle，后者只适配 Rig。

## Amendment 2026-07-14 (ADR 0023)

[ADR 0023](0023-driver-starts-from-one-committed-conversation-seed.md) 将工具 capture 命名从 `Tools::capture_profile_baseline()` / `ToolProfileBaseline` 修订为 `Tools.capture_turn_tools(...) -> TurnToolProfile { prompt_view, executor, fingerprint }`。本 ADR 的并发与 owner 决策不变：actor 保留 tool admin/approval 能力，RunTask 只得到 run-only `ToolBatchInvoker` executor，并且 `prompt_view` 与 executor fingerprint 必须相同。该 amendment 只改 public seam 命名和 Transcript-First 输入连接，不重写历史决策。