# AgentRuntime 不拥有当前会话

状态：部分被[ADR 0028](0028-runtime-protocol-uses-scoped-state-cursors.md)替代。Runtime没有current Session和所有session-scoped command显式携带SessionId的决策保留；runtime-global sequence与all-loaded atomic RuntimeSnapshot要求被scoped cursor/snapshot替代。

> 以下正文保留原始历史决定。涉及RunId、SessionPhase、runtime-global sequence或all-loaded RuntimeSnapshot的内容不再指导实现；当前协议以ADR 0028和`docs/refactor/runtime-interface.md`为准。

MiniCore 是不包含 UI selection 的 headless、多 session runtime。一个 runtime 可以同时加载多个 `SessionRuntime`，多个 session 也可以同时推进 run、compaction、retry 或 approval work。客户端当前显示、选中或准备向哪个 session 发送命令，是 adapter-local state，不是 runtime-global 领域事实。

我们决定删除 core 的 focused/current/active session selector：公开协议不提供 `FocusSession` 或 `session_focus_changed`，`SessionManager` / `LoadedSessionRuntimes` 不保存 focused session pointer。所有 session-scoped command 必须显式携带 `SessionId`；`ExecuteCommandText.session_id = None` 只允许 runtime/workspace-scoped command，解析到 session-scoped command 时返回 `SessionRequired`，不能从唯一 loaded、最近 opened 或客户端 selection 推断目标。`AbortRun { run_id }` 继续通过 host-global current-run lookup 路由，是已经公开 run identity 的特例，不构成默认 session。

这个决策避免把 per-client selection 错建模成共享 runtime state。若两个 adapter 分别查看不同 session，一个全局 focus pointer 会产生 last-writer-wins 路由竞态；要让 focus 正确就必须再引入 connection/client identity、连接生命周期和 per-client state，而这些都不属于 MiniCore 当前边界。Codex app-server 的 thread 操作显式携带 `thread_id`，OpenCode 的 session API 使用显式 `sessionID` 路径并提供 all-session status，ACP 也用每个 `sessionId` 的 running/requires-action/idle 更新；pi coding-agent 的 current session 来自单会话 host 绑定，不适合作为多 session backend 模型。

`active session` 也不作为独立状态保留。session 的真实状态分为 persistent catalog membership、loaded runtime residency、`SessionPhase::{Idle, Turn, Compaction, RetryBackoff}`、optional `CurrentRunState` 和派生的 `session_settled`。一个 boolean `active` 无法区分 run、compaction、retry、approval 或客户端 selection，并会和这些权威状态漂移。run failed/aborted 不会让 session 进入 terminal session 状态。

由于 `Event.sequence` 是 runtime-global，`RuntimeSnapshot` 必须在同一个 `last_event_sequence` 水位原子覆盖全部 loaded sessions。我们使用 `loaded_sessions: Vec<SessionSnapshot>`，删除单一 `active_session_id` / `active_session`；原先依赖当前 session 的 resources 和 command catalog 移入每个 `SessionSnapshot`。snapshot builder 必须通过 event-bus/projection barrier 保证 loaded membership、各 session view 和 sequence 对应同一逻辑水位。持久化但未 loaded 的 session 继续通过 `SessionQuery::List` 读取，不为了 snapshot 全部重建。

代价是 adapter 必须本地维护 selected session 并在发送命令时填写 `SessionId`；all-loaded snapshot 也比单 session projection 更大。MVP 接受这个代价，并可通过显式 close / idle unload 控制 loaded set。未来如果 BR-044 引入 session-scoped subscription 和 cursor，可以再设计与 cursor scope 对齐的 scoped snapshot；在此之前不能让全局水位配合只覆盖某个客户端视图的 snapshot。
