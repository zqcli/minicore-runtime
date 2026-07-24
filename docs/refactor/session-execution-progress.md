# Session Execution 研究进度

> **已迁移 · 非权威 · 待删除**：本文内容已提炼迁移到正式架构文档（见 [docs/refactor/README.md](README.md) 的去向表）。当前权威架构以 `docs/architecture.md`、`docs/modules/`、`docs/adr/`（0100+）为准，本目录仅暂留供 review。

日期：2026-07-16

分支：`refactor/codex-style-message-cycle`

状态：研究已完成；正式目标设计已落入[Session Execution架构设计](session-execution.md)，本文保留研究依据和handoff历史。

## 目的

本文保存阶段6开始前的研究依据和handoff状态，避免后续开发重新推导：

- pi、Codex、Grok Build、Claude Code和Cursor在Session execution场景中的真实做法；
- 哪些结论来自源码，哪些只能从产品行为观察；
- MiniCore当前已经固定的约束；
- Session execution备选形状及推荐原因；
- 最终采用的SessionExecutor设计和已关闭决策。

本文是progress，不替代正式目标文档。最终行为以[Session Execution架构设计](session-execution.md)和[ADR 0025](../adr/0025-loaded-session-uses-one-session-executor.md)为准。

## 当前架构基线

阶段5已经从物理batch切换为真正的by-entry JSONL：

```text
line 1   SessionHeader
line 2+  StoredSessionEntry
```

唯一runtime写入seam：

```rust
SessionWriter::append(
    SessionEntryDraft,
) -> Result<CommittedSessionEntry, SessionWriteError>
```

Session execution必须遵守：

```text
append entry
→ resolve OutcomeUnknown if necessary
→ apply trusted projections
→ publish / wake / side effect / model call
```

已经固定：

- initiating UserMessage append开始Turn；
- final AssistantMessage、TurnInterrupted或TurnFailed append结束Turn；
- assistant tool-call response和tool messages在`tool_round_completed`前durable但不model-visible；
- `tool_execution_started`必须在外部副作用前append/apply；
- Interaction request先append/apply再notify，resolution先append/apply再wake；
- active Turn pin exact AgentRevisionRef、SessionDefinitionRevision、WorkspaceSnapshot、PromptSet、ToolSet、SkillCatalog和TurnModelSnapshot；
- Prompt是唯一模型上下文组装seam；
- SessionStorage是唯一durable truth；
- restart不恢复旧AgentLoop、provider stream、Tool task、approval waiter或Workspace lease。

## 研究可信度

| 项目 | 依据 | 可确认范围 |
| --- | --- | --- |
| pi | 本机安装包完整JavaScript实现、类型声明和session JSONL | AgentLoop、Steer/FollowUp、Tool执行、Abort、持久化时点 |
| Codex | 既有core源码研究、protocol类型、真实rollout和snapshot测试 | Session/Turn/task关系、Steer/Interrupt、TurnContext/StepContext、rollout持久化 |
| Grok Build | 既有session源码研究、ACP模块、persistence/tool crates和存储格式 | actor拆分、prompt queue/interjection、Tool持久化顺序、fork |
| Claude Code | 本机project session JSONL、transcript、tasks和settings | queue、interrupt、permission、tool result、resume/task行为；内部owner未知 |
| Cursor | 公开帮助和行为研究 | Steer/checkpoint/compaction等产品行为；内部owner未知 |

`内部未知`不能写成`无`，也不能用产品行为反推actor、锁或task形状。

## pi Session Execution 主流程

pi的真实主链：

```text
AgentSession.prompt
→ model/auth/extension preflight
→ UserMessage进入Agent内存状态
   └─ SessionManager收到entry，但首个assistant前JSONL可能尚未flush
→ Agent.runWithLifecycle
→ runLoop读取Steering queue
→ stream assistant response
→ finalized assistant触发message_end并持久化
→ 若有ToolCall：
     参数校验 / beforeToolCall hook
     → sequential或parallel执行
     → 每个ToolResult触发message_end并持久化
→ 全部Tool完成后读取Steering queue
→ 有ToolResult或Steer：下一次模型调用
→ Agent原本将结束时读取FollowUp queue
→ 有FollowUp：outer loop继续
→ 无FollowUp：agent_end
→ retry / auto-compaction / settled
```

pi的关键特征：

- AgentLoop内部直接拥有working messages、Model调用和Tool执行；
- Steer在stable turn边界轮询，不自动取消当前sampling；
- FollowUp在Agent原本结束时由outer loop消费；
- Abort通过AbortController协作取消；
- host input loop在Agent运行期间仍可接收Steer、FollowUp和Abort；
- finalized assistant和ToolResult按message entry保存；
- 没有MiniCore式durable Interaction、ToolExecutionStarted前置记录或ToolRoundCompleted模型可见性规则。

## 同类流程对比

标记：`有`表示确认存在；`部分`表示类似但语义不等价；`无`表示确认没有等价步骤；`内部未知`表示证据不足。

| 流程/能力 | pi | Codex | Grok Build | Claude Code | Cursor | MiniCore状态 |
| --- | --- | --- | --- | --- | --- | --- |
| 单Session权威执行owner | AgentSession/Agent；无独立request queue owner | Session/Thread owner + active Turn task | ACP Session + 多actor | 内部未知 | 内部未知 | 已定：一个SessionExecutor |
| 运行期间接收控制 | 有，host loop并发 | 有，Steer/Interrupt/Approval | 有，ACP/prompt queue | 行为可见 | 行为可见 | 已定：bounded SessionRequestQueue |
| admission reservation | 只有preflight；无领域reservation | 有Turn start控制 | 部分 | 内部未知 | 内部未知 | 已定：Idle→Starting reservation |
| exact Turn context pin | 部分；prepareNextTurn可替换 | TurnContext，StepContext会refresh | tool config可冻结，interjection动态 | 内部未知 | 内部未知 | 已定：整个Turn不可变pin |
| UserMessage在模型前durable | 无保证；首assistant前可仅内存 | 有conversation item/rollout | 有持久化顺序控制 | entry存在，写入时点未知 | 内部未知 | 已定：append/apply后才能调用模型 |
| 模型输入来源 | Agent内存messages | Session history + Turn/Step context | chat history + interjection | 内部未知 | 内部未知 | 已定：committed projection经PromptSet |
| Model调用脱离控制loop | 无；AgentLoop内await | 有active task | 有sampler actor | 内部未知 | 内部未知 | 已定：cancellable RunningOperation |
| streaming/progress | 有，非session durable truth | 有item/event stream | 有update stream，部分持久化 | finalized JSONL可见 | 行为可见 | 已定：非authoritative、可合并 |
| Sampling期间Steer | 当前model/tool operation后消费；不自动cancel | expectedTurnId Steer | prompt queue/interjection | queued prompt可见 | 行为可见 | 已定：append Steer并cancel旧Model |
| assistant在Tool前保存 | 有，message_end | 有response item | 有chat/update entry | 顺序可见，副作用时点未知 | 内部未知 | 已定：先append/apply intermediate |
| durable ToolInvocation Started | 只有assistant ToolCall；无独立execution truth | 有call/item lifecycle | 有tool update lifecycle | tool_use可见 | 内部未知 | 已定：assistant tool_call投影Started |
| request-before-notify durable | 无；hook/内存 | 部分 | 有pending interaction，契约不完全等价 | permission行为可见，durability未知 | 行为可见，内部未知 | 已定 |
| resolution-before-resume durable | 无 | 部分 | 部分 | 内部未知 | 内部未知 | 已定 |
| ToolExecutionStarted前置记录 | 无 | 部分；无统一required record | 部分；有durable update ordering | 内部未知 | 内部未知 | 已定 |
| Tool并发执行 | 有，parallel/sequential | 有，按工具策略 | 有tool runtime调度 | 行为可见 | 行为可见 | 已定：ToolSet内部调度 |
| 每个ToolResult独立持久化 | 有，tool result message | 有，function call output | 有chat/update entry | 有tool result entry | 内部未知 | 已定：每call一个role=tool entry |
| 显式ToolRoundCompleted记录 | 无；只在内存等齐 | 无MiniCore等价event | 无MiniCore等价event | 未观察到 | 内部未知 | 已定：tool_round_completed |
| 下一模型调用等待全部Tool | 有，内存规则 | 有 | 有 | 行为可见 | 行为可见 | 已定：等待tool_round_completed append/apply |
| durable Turn terminal | 无领域Turn terminal | 有Completed/Interrupted/Failed | 有turn/update lifecycle | 有turn_duration等事实但不等价 | 行为可见 | 已定：final/Interrupted/Failed entry |
| FollowUp | 有；outer loop继续，同一agent run语义 | 通常新Turn，也有pending input | prompt queue继续 | queue-operation可见 | 行为可见 | 已定：下一Turn bounded process-local FIFO |
| Cancel/Interrupt | AbortController；无durable cleanup | explicit TurnInterrupt | task/tool cancellation | interrupted行为可见 | 行为可见 | 已定：cleanup entries + Interrupted |
| 迟到operation result校验 | 无execution version校验 | Turn/task identity提供部分隔离 | actor/task cancellation，细节未完全公开 | 内部未知 | 内部未知 | 已定：TurnId+execution_version+OperationType |
| restart恢复active execution | 无；只重建messages | resume history，不恢复旧task | session/checkpoint恢复，不恢复旧future | resume/tasks可见 | checkpoint行为可见 | 已定：不恢复旧I/O，保守terminalize |
| 每次模型调用刷新上下文 | 有，prepareNextTurn可替换 | 有StepContext refresh | 有动态reminder/interjection | 内部未知 | 内部未知 | 明确无：active Turn保持exact pin |
| Compaction | 有，post-run/overflow/auto | 有，pre/mid/manual/model-switch | 有独立compaction engine | 行为可见，内部未知 | 行为可见 | 阶段8已确定strict stable suffix方案，由SessionExecutor协调 |

## 从pi保留什么

MiniCore应保留pi的两个简单机制：

1. 内外两层逻辑循环：

```text
inner：model → tools → model，在当前model/tool operation完成后消费Steer
outer：当前工作原本结束后决定是否消费FollowUp
```

2. Steer与FollowUp语义分离：

- Steer作用于current Turn；
- FollowUp只在current Turn terminal后启动next Turn。

MiniCore不能照搬pi的部分：

- AgentLoop直接拥有working transcript；
- Tool执行和模型调用都内联在monolithic loop；
- UserMessage未durable就开始模型调用；
- approval只走内存hook；
- 没有ToolExecutionStarted前置持久化记录；
- 没有explicit ToolRoundCompleted模型可见性规则；
- 没有durable Interrupted/Failed cleanup；
- active inner turn可以替换model/tools/system prompt。

## MiniCore正式形状

```text
one loaded Session
→ one SessionExecutor
   ├─ bounded SessionRequestQueue
   ├─ one SessionWriter
   ├─ committed projections
   ├─ SessionExecutionState
   ├─ current TurnExecutionContext
   ├─ private AgentLoop
   ├─ FollowUp queue
   ├─ pending Interaction state
   └─ RunningOperation

asynchronous operations
├─ BuildTurnContext
├─ ComposeUserMessage
├─ GenerateModelResponse
└─ ExecuteTools
```

SessionExecutor是Runtime private执行对象，不建立通用actor framework，也不作为公开Runtime interface。MiniCoreRuntime仍是唯一顶层facade。

crate-private requests：

```text
Submit
Steer
FollowUp
ResolveInteraction
Cancel
PrepareForUnload
GetSnapshot
```

operation result identity：

```text
SessionId + TurnId + execution_version + OperationType
```

Steer、Cancel、security revocation或terminal会推进execution version。旧version的Context/Model和尚未记录ToolExecutionStarted的Tool result可以忽略；Tool副作用可能已经开始时，迟到result必须先确认并保存truthful outcome，不能因version变化而丢失事实。

## 推荐执行流

```text
Submit
→ reserve candidate Turn
→ launch Context capture future
→ validate captured Context
→ Context.compose_message(PromptIntent)
→ 与Agent status update串行化
→ final AgentStatus = Enabled check
→ append/apply TurnContext
→ append/apply UserMessage(Input)
→ release Agent status synchronization
→ Running

AgentLoop NeedModel
→ PromptSet assemble committed conversation
→ launch Model future
→ validate SessionId / TurnId / execution_version / OperationType
→ AgentLoop.ingest_model_output(finalized response)
→ AgentLoop.next_action

AgentLoop NeedTools
→ process queued Steer / Cancel / WorkspaceAuthorizationRevoked
→ validate current authorization
→ Steer wins：discard unpersisted model output并compose Steer
→ Cancel/revocation wins：进入Interrupted cleanup
→ Turn仍Running：取得WorkspaceCommitAuthorization
→ append/apply Assistant(Intermediate)
→ launch ToolSet future
→ ToolExecutionControl request:
     append/apply InteractionRequested → notify
     append/apply InteractionResolved → wake
     append/apply ToolExecutionStarted → side effect
→ each truthful result append/apply role=tool
→ append/apply tool_round_completed
→ feed committed round to AgentLoop
→ NeedModel

AgentLoop Finished
→ arbitrate Steer / Cancel / revocation / final candidate
→ validate lease并取得WorkspaceCommitAuthorization
→ append/apply Assistant(Final)
→ release TurnExecutionContext
→ Finishing → Idle
→ dequeue FollowUp并重新进入普通admission（Idle → Starting），或保持Idle
```

## Control建议

### Steer

正式MVP：

- Sampling阶段：append/apply Steer，推进execution version，best-effort取消旧Model；旧version Model result不再使用；
- WaitingApproval或Tool执行阶段：先排队，当前Tool operation得到truthful结果后再append，不把Steer隐式解释为approval或Tool cancellation；
- Steer不结束Turn，也不重新capture TurnExecutionContext。

### FollowUp

推荐MVP使用process-local bounded FIFO：

- 不属于current Turn control；
- current Turn terminal后重新进入普通admission；
- 不承诺crash-safe acknowledgement；需要该能力时再扩展storage schema。

### Cancel

```text
advance execution_version
→ best-effort cancel Context/Model和可取消Tool operations
→ 对已记录ToolExecutionStarted的Tool等待/确认outcome
→ resolve/cancel Pending Interaction
→ preserve exact Tool messages
→ abandon only outcome-unknown Started ToolInvocation
→ append TurnInterrupted
```

不回滚已发生副作用，不生成synthetic ToolResult。

## 正式决策结果

[ADR 0025](../adr/0025-loaded-session-uses-one-session-executor.md)保留ADR 0021的单一权威owner、运行期间持续处理控制请求、禁止跨外部I/O长期借用mutable state以及progress独立处理原则，并替代旧的强制two-owner execution形状。

已经冻结：

1. 一个长期`SessionExecutor + SessionRequestQueue`是loaded Session唯一owner；
2. Context构造、UserMessage composition、Model和Tool全部使用cancellable asynchronous operation；
3. Sampling阶段Steer默认取消旧Model并推进execution version；
4. WaitingApproval/Tool执行阶段Steer只排队；
5. FollowUp使用process-local bounded FIFO；
6. operation result使用`SessionId + TurnId + execution_version + OperationType`校验；
7. progress使用独立bounded `ProgressEventPublisher`；
8. SessionWriter短append由owner await；blocking syscall由storage implementation内部处理；
9. Rig只有在必须使用monolithic async run时才增加private adapter task，该task不拥有Session state。

## 下一步

ModelGateway目标设计已经完成，当前下一份设计文档：

```text
docs/refactor/compaction.md
```

实现前验证可以并行执行：

```text
Rig 0.40.0 adapter spikes
→ 验证private AgentLoop的NeedModel/NeedTools/Finished映射
→ 验证ModelGateway的role/tool/reasoning/finish/usage/cancel映射
→ 不反向改变SessionExecutor或ModelGateway ownership
```

实现顺序：

```text
1. 实现SessionExecutor value types和request queue
2. 实现private AgentLoop adapter
3. 实现ToolExecutionControl request/reply
4. 实现race/recovery/performance测试
5. 已在阶段9冻结公开Runtime protocol，见[runtime-interface.md](runtime-interface.md)
```

## 明确不建立

```text
TurnManager
ItemManager
InteractionService
ModelStep entity
ToolRound entity
通用AgentLoop public trait
第二Session writer/projection
每个子职责一个actor
恢复旧provider stream或Tool task
```

## 关键参考

- [Session Execution架构设计](session-execution.md)
- [ADR 0025](../adr/0025-loaded-session-uses-one-session-executor.md)
- [Conversation与SessionStorage架构设计](conversation-storage.md)
- [Turn执行模块与执行上下文架构设计](turn-execution-context.md)
- [Turn、Item与Interaction架构设计](turn-item-interaction.md)
- [Tool子系统架构设计](tool-subsystem.md)
- [Agent与Session生命周期架构设计](agent-session-lifecycle.md)
- [Refactoring Roadmap](refactoring-roadmap.md)
- [ADR 0021](../adr/0021-session-runtime-separates-actor-control-from-run-execution.md)
- [ADR 0024](../adr/0024-session-storage-uses-by-entry-jsonl.md)
