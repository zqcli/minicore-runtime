# ADR 0028: Runtime Protocol Uses Scoped State Cursors

状态：Accepted
日期：2026-07-24

## 背景

MiniCore阶段0–8已经固定新的领域和执行模型：

```text
MiniCoreRuntime
└─ Agent
   └─ Session
      └─ Turn
         └─ Item
            └─ Interaction
```

一个Runtime允许多个SessionExecutor并行Running；每个loaded Session有一个独立执行owner。SessionStorage是conversation/execution ledger的唯一durable truth，streaming progress不持久化。Workspace是Session-owned definition，没有WorkspaceId；Turn是从committed initiating UserMessage开始的公开执行边界。

旧Runtime protocol在这些决策前形成，使用RunId、runtime-global event sequence、all-loaded RuntimeSnapshot、generic CommandAck和SessionRuntime-owned command facade。继续保留这些形状会要求跨Session全局snapshot barrier，并把已删除的ownership带入公开协议。

## 决策

1. `MiniCoreRuntime`公开四类能力：

```text
dispatch(CommandRequest) -> CommandResponse
query(RuntimeQuery) -> QueryResponse
snapshot(SnapshotRequest) -> SnapshotResponse
subscribe(SubscriptionRequest) -> EventStream
```

2. 公开领域identity使用`AgentId → SessionId → TurnId → ItemId → RequestId`。不定义公开RunId或WorkspaceId。CommandId只做协议correlation和幂等；SubmissionId只做Starting admission控制。

3. Command在明确线性化点返回typed CommandOutcome。Submit在initiating UserMessage append/apply后返回TurnId；Turn长期完成通过Event发布。Steer、FollowUp、Interaction resolution和Cancel都返回与其真实admission/commit点对应的typed outcome。

4. 可靠StateEvent与best-effort ProgressEvent分离。只有StateEvent推进恢复cursor；ProgressEvent可以合并或丢弃，final StateEvent携带完整view进行校正。

5. Runtime scope和每个Session scope使用独立cursor与snapshot：

```text
RuntimeCursor + RuntimeSnapshot
SessionCursor(SessionId) + SessionSnapshot
```

不建立runtime-global total order，不要求all-loaded Session stop-the-world snapshot。跨scope cursor不可比较。

6. CommandSurface保留为Runtime-owned用户命令领域面。CommandManager保持共享、无状态、每次按显式optional SessionId构造CommandContext。旧SessionRuntime-held长期`Command` facade不进入目标架构。

7. SessionStorage拥有message tree。Runtime通过Query提供history read model，通过受限message anchor提供Fork Session command。首版不公开同一Session原地checkout。

8. 所有改变Runtime事实的UI操作经过MiniCoreRuntime。selected Session、editor draft、scroll、layout和其他纯UI状态留在adapter。

9. 首版Runtime protocol不公开standalone/manual CompactSession。automatic compaction仍只在active Turn NeedModel安全点执行。

10. 首版实现in-process Rust interface和transport-neutral serde types。JSON-RPC、Tauri IPC或WebSocket adapter只映射transport，不拥有领域状态。

完整类型和ordering见[Runtime Interface与公开协议架构设计](../refactor/runtime-interface.md)。

## 后果

- CLI、TUI和GUI共享同一领域命令、查询、快照和事件语义。
- 每个Session可以独立snapshot/reconnect，一个慢Session不会阻塞全Runtime恢复。
- progress backpressure不会制造StateEvent cursor gap。
- Runtime无需维护RunId到SessionExecutor的全局执行索引。
- UI需要本地维护selected Session，并为需要观察的Session建立subscription。
- Runtime与Session stream之间没有total order；跨scope workflow必须使用领域revision、command correlation和显式SessionId，而不能比较cursor。
- SessionSnapshot只覆盖当前执行恢复状态；完整历史使用分页Query。
- 旧protocol adapter如需短期存在，只能转换payload，不能拥有新的状态或成为事实来源。

## 被替代和保留的旧决策

- [ADR 0018](0018-agent-runtime-separates-command-query-event-and-snapshot.md)：保留Command/Query/Event/Snapshot职责分离；替代generic CommandAck、runtime-global sequence和all-loaded RuntimeSnapshot水位。
- [ADR 0020](0020-agent-runtime-has-no-current-session.md)：保留Runtime没有current Session；替代由global sequence推导出的all-loaded atomic snapshot要求。
- [ADR 0012](0012-command-manager-is-stateless-session-command-facade.md)：保留stateless CommandManager和explicit SessionId；替代SessionRuntime-held长期Command facade。
- [ADR 0016](0016-separate-command-run-policy-from-prompt-delivery.md)：保留command执行策略与prompt delivery分离；首版不建立PendingSessionAction/manual compact，公开Turn control使用Submit、Steer和FollowUp typed command。

## 未选择方案

### Runtime-global sequence和all-loaded snapshot

它提供单一total order，但需要协调所有SessionExecutor并让filtered subscriber处理无关gap。多Session之间没有足够业务因果关系来支付该复杂度。

### 一个长请求等待完整Turn

它适合简单CLI，但把transport request lifetime与Turn、Interaction和后台Session execution耦合。

### Generic accepted acknowledgement

它无法表达revision、TurnId、Steer Applied/Queued或Interaction commit outcome，调用方必须从事件反推命令结果。

### UI各自实现slash command

它会让catalog、动态候选、权限和handler语义在不同宿主中漂移。
