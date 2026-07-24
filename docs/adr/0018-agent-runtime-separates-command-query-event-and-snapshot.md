# AgentRuntime 分离 Command、Query、Event 和 Snapshot

## 状态

Superseded by [ADR 0028](0028-runtime-protocol-uses-scoped-state-cursors.md)。Command、Query、Event和Snapshot职责分离保留；generic CommandAck、runtime-global sequence和all-loaded RuntimeSnapshot水位被替代。

> 以下Decision/Impact保留原始历史形状。涉及CommandAck、global sequence和RuntimeSnapshot水位的内容不再指导实现。

## 决策

`AgentRuntime` 对下游宿主提供四种稳定能力：`dispatch(AgentCommand) -> CommandAck` 用于 mutation 和异步工作接收；`query(RuntimeQuery) -> QueryResponse` 用于只读、typed、request/response 查询；`subscribe() -> EventStream` 用于流式进度和状态变化；`snapshot() -> RuntimeSnapshot` 用于 UI 初始化和事件水位恢复。

`RuntimeQuery` 按领域分组为 runtime、session、settings、resources、command surface、models、usage 和 diagnostics。等待未来状态不属于 Command 或 Query：公开协议不提供 `WaitForIdle`，core 也不新增稳定 `wait_until_settled()`；observer 使用 Event/Snapshot，adapter/test-support 可以在边缘基于事件提供本地 await helper。新增子查询时扩展对应领域 enum，不把所有查询平铺成一个巨型列表。query 必须只读、有大小边界、不消费 queue、不创建 turn、不发布业务事件，也不能为了读取目录或持久化详情而加载完整 `SessionRuntime`。

## 影响

`ListSessions`、资源详情、command catalog/suggestion、usage/context usage 和 settings/config read 从 `AgentCommand` 移到 `RuntimeQuery`；`WaitForIdle` 从 `AgentCommand` 删除且不迁入 Query。`CommandAck` 继续只表达命令是否被接收；query 结果不通过 event stream 广播，也不增加 event sequence。`RuntimeSnapshot` 保留为带 `last_event_sequence` 的粗粒度恢复读模型，不扩展成通用分页/详情容器。

配置读取使用 `SettingsQuery`；配置草稿属于 UI；配置 mutation 使用带 expected revision 的 `AgentCommand`，成功后的事实变化通过 event 发布。凭据 query 只能返回 redacted status，不能返回 secret material。

JSON-RPC、Tauri IPC、stdio 或 WebSocket adapter 可以把领域 query 映射成独立 typed 方法；transport request id 只负责单次 request/response correlation，不替代 `CommandId`、event sequence 或领域 revision。