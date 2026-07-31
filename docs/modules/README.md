# 模块总览（V2 当前架构）

本目录是MiniCore V2当前权威module设计。ADR 0126已将执行模型更新为async Turn loop与inline best-effort Session recording；ADR 0129已冻结用户消息contribution与safe part-level provenance；仓库仍无Rust生产实现。

权威顺序：[`docs/architecture.md`](../architecture.md)与本目录 → Accepted ADR → `docs/research/` → `docs/archive/v1/`。

## 领域关系

```text
MiniCoreRuntime
└─ Agent*
   └─ Session*
      └─ Turn*
         └─ Item*
            └─ Interaction*
```

Runtime持有`PromptService`、`ToolService`、`SkillService`和`ModelGateway`四个共享深module。每个loaded Session由`SessionExecutor` control actor管理，并最多运行一个`ActiveTurnTask`。当前进程的`LiveSessionState`驱动Model、Tool和UI；`SessionRecorder`异步记录可恢复前缀。

## 模块索引

- [Runtime公开协议](runtime-interface.md)：`dispatch / query / snapshot / subscribe`、公开identity和live observer语义。
- [Agent与Session生命周期](agent-session-lifecycle.md)：definition/revision、create/load/unload/archive/fork与readiness。
- [Workspace](workspace.md)：Session-owned Workspace、trust、authorization和immutable snapshot。
- [Prompt](prompt.md)：PromptIntent、CanonicalUserMessage、safe part-level contribution provenance、`LiveConversationView`和AssembledModelContext。
- [Skills](skills.md)：SkillService、shared SkillResourceView、Turn-pinned SkillView和reload。
- [Tools](tools.md)：ToolSet、policy、approval、sandbox、executor和Session-local file mutation queue。
- [Turn执行上下文](turn-execution-context.md)：immutable capture、ConversationRevision和ModelCallRequest basis。
- [Turn / Item / Interaction](turn-item-interaction.md)：live lifecycle、complete Tool exchange、Interaction和Tool start gate。
- [Conversation Recording与Replay](conversation-storage.md)：SessionRecorder、best-effort JSONL prefix、RecordingHealth、tolerant replay和fork。
- [Session执行](session-execution.md)：SessionExecutor actor、ActiveTurnTask、async run loop、Steer/FollowUp/Cancel。
- [ModelGateway](model-gateway.md)：TurnModelSnapshot、single provider attempt、stream、usage和typed errors。
- [Compaction](compaction.md)：live rolling summary、single marker和best-effort recording。

## 权威归属

| 概念 | Canonical Owner |
| --- | --- |
| 公开command/query/event/snapshot | [Runtime公开协议](runtime-interface.md) |
| Agent/Session lifecycle与revision | [Agent与Session生命周期](agent-session-lifecycle.md) |
| Workspace与authority | [Workspace](workspace.md) |
| PromptIntent、CanonicalUserMessage、contribution provenance与model context assembly | [Prompt](prompt.md) |
| Skill discovery/load/reload | [Skills](skills.md) |
| Tool policy/approval/sandbox/execution | [Tools](tools.md) |
| immutable Turn capture与live execution basis | [Turn执行上下文](turn-execution-context.md) |
| Turn/Item/Interaction与Tool exchange | [Turn / Item / Interaction](turn-item-interaction.md) |
| JSONL recording、RecordingHealth与cold replay | [Conversation Recording与Replay](conversation-storage.md) |
| control actor、ActiveTurnTask与async loop | [Session执行](session-execution.md) |
| provider attempt与response validation | [ModelGateway](model-gateway.md) |
| compaction planning与summary validation | [Compaction](compaction.md) |

## 当前实现顺序

1. 冻结wire/schema和[开放问题](../review/async-loop-best-effort-recording-open-questions.md)中的必要MVP默认值；
2. 建立Rust crate和`LiveConversation`/`SessionRecorder`基础类型；
3. 通过`ScriptedProviderAdapter`实现async ordinary AgentRun与complete Tool exchange；
4. 加入recording slow-write/failure与cold replay fixtures；
5. 实现overflow → CompactionSummary → live Replace → inline best-effort record；
6. 完成Rig 0.40.0 provider spike和mock-server tests；
7. production Tool/Sandbox adapter前关闭O1/R7。

跨模块高风险规则见[架构总览的不变量索引](../architecture.md#跨模块不变量索引)。
