# MiniCore V2 设计评审（第三轮）

状态：已关闭；AgentLoop方案被ADR 0126整体取代
日期：2026-07-30
范围：AgentLoop、Session execution、Model response validation、Tool exchange与Steer

## 总体结论

第三轮最初评审同步sans-I/O AgentLoop的五项问题。L1、L3、L4、L5曾在原设计内关闭；L2要求在pull one-shot与transition-returning reducer之间选择。

用户随后明确要求采用参考Codex/Pi的async loop，并同时放弃SessionWriter commit barrier。ADR 0126因此选择了研究中的方案C：

```text
SessionExecutor control actor
└─ one ActiveTurnTask
   └─ async Model → Tool/Interaction → Model loop
```

同步AgentLoop、`next_action()`、`accept_*()`、`AgentLoopAction`、`RunningOperation`和storage committed delta接口全部删除。L2不再通过方案A/B关闭，而是由新执行模型消除。

## L1 · Model response validation（保持关闭）

ModelGateway继续在返回`ModelCallResult`前校验：

- ModelFinishReason × ToolCall presence × OutputContract；
- UnexpectedToolCall；
- InvalidStructuredOutput；
- InvalidProviderResponse；
- IncompleteResponse。

ActiveTurnTask只接收validated response。含ToolCall进入Tool path；无ToolCall进入candidate arbitration。该规则由ADR 0120和ModelGateway module拥有。

## L2 · effect emission（由ADR 0126关闭）

原问题：`next_action()`可能重复发出NeedTools/Finished并重复启动副作用。

最终决议：删除pull/effect接口。ActiveTurnTask通过普通async局部控制流保存instruction position：

```text
await ModelGateway
→ apply validated response live
→ optional await ToolSet/Interaction
→ safe-point Steer
→ next iteration or terminal
```

不存在重复poll、issued marker、ActionAlreadyIssued或transition-returning AgentLoopEffect。

测试重点改为：

- 每Session最多一个ActiveTurnTask；
- task不detach Model/Tool future；
- terminal result只被live reducer接受一次；
- stale control_generation/ConversationRevision result拒绝；
- Cancel和ToolStartGate竞态；
- task panic/abort结构化收口。

## L3 · committed delta（接口被删除）

原`CommittedToolExchangeDelta`和`CommittedSteerDelta`由SessionStorage签发，用于证明physical append/apply后才可推进AgentLoop。

ADR 0126删除commit barrier后：

- complete Tool exchange由LiveConversation reducer拥有；
- Steer apply到live conversation后直接进入下一次assembly；
- SessionRecorder只best-effort记录，不产生执行permit；
- cold replay仍独立执行Tool exchange sanitizer。

## L4 · Steer与reseed（接口被删除）

ActiveTurnTask在safe point FIFO消费Steer，使用同一TurnExecutionContext compose并apply live UserMessage。Compaction Replace直接修改LiveConversation；不存在AgentLoop segment或reseed。

## L5 · Rig职责（保持关闭）

Rig继续只实现ModelGateway private ProviderAdapter中的单次provider attempt。Rig不拥有：

- ActiveTurnTask；
- Session logical retry；
- conversation；
- Tool治理；
- terminal arbitration；
- Session recording。

## 新风险

async设计需要覆盖的新风险：

1. SessionExecutor与ActiveTurnTask不得维护两份mutable conversation；
2. LiveSessionState lock guard不得跨await；
3. SessionRecorder slow-write/failure不得触发Model或Tool重放；
4. recording Degraded或process crash时StateEvent可以领先可恢复recorded prefix，public protocol必须明确；
5. recorder first failure后停止suffix，避免写出已知gap；
6. restart只恢复recorded prefix，未record tail按设计丢失；
7. candidate final与Steer admission仍需owner-local内存仲裁。

## 关闭依据

- [ADR 0126](../adr/0126-turn-execution-is-async-and-session-recording-is-best-effort.md)
- [Session Execution](../modules/session-execution.md)
- [Conversation Recording与Replay](../modules/conversation-storage.md)
- [Turn Execution Context](../modules/turn-execution-context.md)
- [AgentLoop执行模型跨项目研究](../research/agent-loop-execution-model-study.md)

## 结论表

| 项目 | 当前结论 |
| --- | --- |
| L1 | 保持关闭；ModelGateway validation |
| L2 | 已关闭；删除AgentLoop effect interface，采用async ActiveTurnTask |
| L3 | 原接口删除；live reducer拥有complete exchange |
| L4 | 原接口删除；async loop直接消费Steer/应用Compaction |
| L5 | 保持关闭；Rig provider-only |
| AgentLoop编码门禁 | 已消失 |
| 当前编码前门禁 | wire/schema与Recorder MVP参数已关闭；可创建Rust crate，Rig provider spike仅门禁production ProviderAdapter |
