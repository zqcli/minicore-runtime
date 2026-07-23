# Session Execution 研究进度

日期：2026-07-16

分支：`refactor/codex-style-message-cycle`

状态：阶段5 by-entry SessionStorage目标架构已收口；阶段6 Session execution已完成同类项目研究和方案推荐，尚未创建正式`session-execution.md`，关键待确认项保留在本文。

## 目的

本文保存阶段6开始前的研究依据和handoff状态，避免后续开发重新推导：

- pi、Codex、Grok Build、Claude Code和Cursor在Session execution场景中的真实做法；
- 哪些结论来自源码，哪些只能从产品行为观察；
- MiniCore当前已经固定的约束；
- 推荐的Session execution形状及原因；
- 进入正式设计前仍需确认的决策。

本文是progress，不替代正式目标文档。最终行为应写入未来的`docs/refactor/session-execution.md`和必要ADR。

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
- active Turn pin exact AgentRevisionRef、SessionDefinitionRevision、WorkspaceSnapshot、PromptSet、ToolSet、SkillCatalog和Model；
- Prompt是唯一模型上下文组装seam；
- SessionStorage是唯一durable truth；
- restart不恢复旧AgentLoop、provider stream、Tool task、approval waiter或Workspace lease。

## 研究可信度

| 项目 | 依据 | 可确认范围 |
| --- | --- | --- |
| pi | 本机安装包完整JavaScript实现、类型声明和session JSONL | AgentLoop、Steer/FollowUp、Tool执行、Abort、持久化时点 |
| Codex | 既有core源码研究、protocol类型、真实rollout和snapshot测试 | Session/Turn/task关系、Steer/Interrupt、TurnContext/StepContext、rollout持久化 |
| Grok Build | 既有session源码研究、ACP模块、persistence/tool crates和存储格式 | actor拆分、prompt queue/interjection、Tool/persistence barrier、fork |
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
- 没有MiniCore式durable Interaction、side-effect barrier或ToolRound completion gate。

## 同类流程对比

标记：`有`表示确认存在；`部分`表示类似但语义不等价；`无`表示确认没有等价步骤；`内部未知`表示证据不足。

| 流程/能力 | pi | Codex | Grok Build | Claude Code | Cursor | MiniCore状态 |
| --- | --- | --- | --- | --- | --- | --- |
| 单Session权威执行owner | AgentSession/Agent；无mailbox owner | Session/Thread owner + active Turn task | ACP Session + 多actor | 内部未知 | 内部未知 | 待确认：一个SessionExecutionTask |
| 运行期间接收控制 | 有，host loop并发 | 有，Steer/Interrupt/Approval | 有，ACP/prompt queue | 行为可见 | 行为可见 | 待确认：bounded mailbox |
| admission reservation | 只有preflight；无领域reservation | 有Turn start控制 | 部分 | 内部未知 | 内部未知 | 已定：Idle→Starting reservation |
| exact Turn context pin | 部分；prepareNextTurn可替换 | TurnContext，StepContext会refresh | tool config可冻结，interjection动态 | 内部未知 | 内部未知 | 已定：整个Turn不可变pin |
| UserMessage在模型前durable | 无保证；首assistant前可仅内存 | 有conversation item/rollout | 有persistence barrier | entry存在，写入时点未知 | 内部未知 | 已定：append/apply后才能调用模型 |
| 模型输入来源 | Agent内存messages | Session history + Turn/Step context | chat history + interjection | 内部未知 | 内部未知 | 已定：committed projection经PromptSet |
| Model调用脱离控制loop | 无；AgentLoop内await | 有active task | 有sampler actor | 内部未知 | 内部未知 | 待确认：cancellable external future |
| streaming/progress | 有，非session durable truth | 有item/event stream | 有update stream，部分持久化 | finalized JSONL可见 | 行为可见 | 已定：非authoritative、可合并 |
| Sampling期间Steer | 排队到safe point；不自动cancel | expectedTurnId Steer | prompt queue/interjection | queued prompt可见 | 行为可见 | 待确认：是否cancel旧model draft |
| assistant在Tool前保存 | 有，message_end | 有response item | 有chat/update entry | 顺序可见，副作用时点未知 | 内部未知 | 已定：先append/apply intermediate |
| durable ToolInvocation Started | 只有assistant ToolCall；无独立execution truth | 有call/item lifecycle | 有tool update lifecycle | tool_use可见 | 内部未知 | 已定：assistant tool_call投影Started |
| request-before-notify durable | 无；hook/内存 | 部分 | 有pending interaction，契约不完全等价 | permission行为可见，durability未知 | 行为可见，内部未知 | 已定 |
| resolution-before-resume durable | 无 | 部分 | 部分 | 内部未知 | 内部未知 | 已定 |
| ToolExecutionStarted barrier | 无 | 部分；无统一barrier | 部分；有durable update barrier | 内部未知 | 内部未知 | 已定 |
| Tool并发执行 | 有，parallel/sequential | 有，按工具策略 | 有tool runtime调度 | 行为可见 | 行为可见 | 已定：ToolSet内部调度 |
| 每个ToolResult独立持久化 | 有，tool result message | 有，function call output | 有chat/update entry | 有tool result entry | 内部未知 | 已定：每call一个role=tool entry |
| 显式durable ToolRound gate | 无；只在内存等齐 | 无MiniCore等价event | 无MiniCore等价event | 未观察到 | 内部未知 | 已定：tool_round_completed |
| 下一模型调用等待全部Tool | 有，内存规则 | 有 | 有 | 行为可见 | 行为可见 | 已定：等待completion append/apply |
| durable Turn terminal | 无领域Turn terminal | 有Completed/Interrupted/Failed | 有turn/update lifecycle | 有turn_duration等事实但不等价 | 行为可见 | 已定：final/Interrupted/Failed entry |
| FollowUp | 有；outer loop继续，同一agent run语义 | 通常新Turn，也有pending input | prompt queue继续 | queue-operation可见 | 行为可见 | 待确认：下一Turn bounded FIFO |
| Cancel/Interrupt | AbortController；无durable cleanup | explicit TurnInterrupt | task/tool cancellation | interrupted行为可见 | 行为可见 | 已定：cleanup entries + Interrupted |
| late completion fencing | 无generation fencing | Turn/task identity提供部分隔离 | actor/task cancellation，细节未完全公开 | 内部未知 | 内部未知 | 待确认：TurnId+generation+WorkKind |
| restart恢复active execution | 无；只重建messages | resume history，不恢复旧task | session/checkpoint恢复，不恢复旧future | resume/tasks可见 | checkpoint行为可见 | 已定：不恢复旧I/O，保守terminalize |
| 每次模型调用刷新上下文 | 有，prepareNextTurn可替换 | 有StepContext refresh | 有动态reminder/interjection | 内部未知 | 内部未知 | 明确无：active Turn保持exact pin |
| Compaction | 有，post-run/overflow/auto | 有，pre/mid/manual/model-switch | 有独立compaction engine | 行为可见，内部未知 | 行为可见 | 阶段8设计，Session execution协调 |

## 从pi保留什么

MiniCore应保留pi的两个简单机制：

1. 内外两层逻辑循环：

```text
inner：model → tools → model，stable barrier消费Steer
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
- 没有ToolExecutionStarted side-effect barrier；
- 没有explicit ToolRound conversation gate；
- 没有durable Interrupted/Failed cleanup；
- active inner turn可以替换model/tools/system prompt。

## 推荐MiniCore形状

```text
one loaded Session
→ one SessionExecutionTask
   ├─ one bounded mailbox
   ├─ one SessionWriter
   ├─ committed projections
   ├─ SessionExecutionState
   ├─ active TurnExecutionContext
   ├─ private AgentLoop
   ├─ FollowUp queue
   ├─ pending Interaction state
   └─ in-flight work registry

external work
├─ Context capture future
├─ Model future
└─ Tool future(s)
```

SessionExecutionTask是actor-like event loop，但不建立通用actor framework，也不作为公开Runtime interface。MiniCoreRuntime仍是唯一顶层facade。

推荐crate-private ingress：

```text
Submit
Steer
FollowUp
ResolveInteraction
Cancel
Quiesce
Snapshot
```

推荐completion fence：

```text
TurnId + generation + WorkKind
```

任何Steer preemption、Cancel、security revocation或terminal都会推进generation。旧generation的Context/Model和尚未越过side-effect barrier的Tool completion可以丢弃；Tool副作用可能已经开始时，迟到completion必须先确认并保存truthful outcome，不能因generation过期而丢失事实。

## 推荐执行流

```text
Submit
→ reserve candidate Turn
→ launch Context capture future
→ validate captured Context
→ Context.compose_message(PromptIntent)
→ acquire short Agent lifecycle gate
→ final AgentStatus = Enabled check
→ append/apply TurnContext
→ append/apply UserMessage(Input)
→ release Agent lifecycle gate
→ Running

AgentLoop NeedModel
→ PromptSet assemble committed conversation
→ launch Model future
→ completion fence validation
→ AgentLoop.ingest_model_output(finalized response)
→ AgentLoop.next_action

AgentLoop NeedTools
→ append/apply Assistant(Intermediate)
→ control / revocation barrier
→ launch ToolSet future
→ ToolTurnPort request:
     append/apply InteractionRequested → notify
     append/apply InteractionResolved → wake
     append/apply ToolExecutionStarted → side effect
→ each truthful result append/apply role=tool
→ append/apply tool_round_completed
→ feed committed round to AgentLoop
→ NeedModel

AgentLoop Finished
→ arbitrate Steer / Cancel / final candidate
→ append/apply Assistant(Final)
→ release TurnExecutionContext
→ Finishing → Idle
→ dequeue FollowUp并重新进入普通admission（Idle → Starting），或保持Idle
```

## Control建议

### Steer

如果待确认决策3被接受，推荐MVP：

- Sampling阶段：append/apply Steer，推进generation，best-effort取消旧model draft；迟到Model completion丢弃；
- WaitingApproval或Tool执行阶段：先排队到stable barrier，不把Steer隐式解释为approval或Tool cancellation；
- Steer不结束Turn，也不重新capture TurnExecutionContext。

### FollowUp

推荐MVP使用process-local bounded FIFO：

- 不属于current Turn control；
- current Turn terminal后重新进入普通admission；
- 不承诺crash-safe acknowledgement；需要该能力时再扩展storage schema。

### Cancel

```text
advance generation
→ best-effort cancel Context/Model和可取消Tool futures
→ 对已越过side-effect barrier的Tool等待/确认outcome
→ resolve/cancel Pending Interaction
→ preserve exact Tool messages
→ abandon only outcome-unknown Started ToolInvocation
→ append TurnInterrupted
```

不回滚已发生副作用，不生成synthetic ToolResult。

## ADR 0021处理建议

保留ADR 0021的核心不变量：

- 一个authoritative Session owner；
- active work期间control ingress保持响应；
- 禁止`Arc<Mutex<SessionState>>`跨I/O await；
- progress不能堵塞approval/abort/control；
- external command与terminal winner在owner处线性化。

需要修订的旧限定：

- 不再强制`actor + RunTask`形状；
- Tool改为Runtime-owned ToolService和Turn-pinned ToolSet；
- writer改为by-entry append；
- `RunTask`只有在Rig只能提供monolithic async run时才作为private adapter存在；
- adapter不能拥有writer、projection、Session state、FollowUp queue或terminal arbitration。

## 待确认决策

进入正式`session-execution.md`前需要依次确认：

1. 是否接受一个长期`SessionExecutionTask + mailbox`作为loaded Session唯一owner；
2. 是否接受Context、Model、Tool全部作为external futures；
3. Sampling阶段Steer是否默认preempt旧model draft；
4. WaitingApproval/Tool执行阶段Steer是否MVP只排队；
5. FollowUp是否采用process-local bounded FIFO；
6. completion fence是否固定为`TurnId + generation + WorkKind`；
7. progress是否使用独立bounded/coalesced lane；
8. SessionWriter短append由owner直接await，还是由private I/O adapter offload blocking syscall；两者都不能产生第二semantic owner；
9. Rig 0.40.0是否需要monolithic RunTask adapter。

## 下一步开发顺序

```text
1. review本文和上述9项备选
2. 起草docs/refactor/session-execution.md并完整写出状态机/ownership
3. review并冻结最终决策
4. 按冻结结果修订ADR 0021
5. 冻结SessionExecutionState、mailbox request和completion类型
6. 定义private AgentLoop interface
7. 定义ToolTurnPort回到owner的barrier request/reply
8. 写race/recovery/performance测试矩阵
9. 执行Rig 0.40.0 integration spike
10. 再进入ModelGateway正式设计
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

- [Conversation与SessionStorage架构设计](conversation-storage.md)
- [Turn执行模块与执行上下文架构设计](turn-execution-context.md)
- [Turn、Item与Interaction架构设计](turn-item-interaction.md)
- [Tool子系统架构设计](tool-subsystem.md)
- [Agent与Session生命周期架构设计](agent-session-lifecycle.md)
- [Refactoring Roadmap](refactoring-roadmap.md)
- [ADR 0021](../adr/0021-session-runtime-separates-actor-control-from-run-execution.md)
- [ADR 0024](../adr/0024-session-storage-uses-by-entry-jsonl.md)
