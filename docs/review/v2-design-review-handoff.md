# V2设计评审工作交接

日期：2026-07-31
状态：async loop / conversation-only best-effort recording refactor分支

架构事实以`docs/architecture.md`、`docs/modules/`、ADR 0126和ADR 0127为权威。本文记录恢复入口与剩余工作。

## 当前分支

```text
refactor/async-loop-eventual-session-log
base: 697c614 docs: archive agent loop execution research
remote: https://github.com/zqcli/minicore-runtime.git
```

分支名保留创建时的`eventual-session-log`历史命名；当前权威设计已经收窄为inline best-effort append，以ADR 0126和current modules为准。

换机恢复命令：

```bash
git clone https://github.com/zqcli/minicore-runtime.git
cd minicore-runtime
git fetch origin
git switch refactor/async-loop-eventual-session-log
git pull --ff-only
```

仓库已经存在时只执行`fetch / switch / pull --ff-only`。本handoff所在commit包含本轮全部文档变更，正常checkout后worktree应为clean。

该分支把原同步AgentLoop + SessionWriter commit barrier重构为：

```text
SessionExecutor control actor
└─ one ActiveTurnTask
   └─ async Model → Tool/Interaction → Model loop

LiveSessionState
→ current-process truth

SessionRecorder
→ ordered inline best-effort conversation JSONL append
→ no StoredTurnStart / Turn terminal
```

## 2026-07-30完成内容

- 接受ADR 0126：`SessionExecutor control actor + one ActiveTurnTask`；
- 删除同步AgentLoop、RunningOperation与SessionWriter durable commit barrier；
- Q1关闭：删除Recorder后台queue、容量、Flush和drain；改为`record(entry).await`顺序append；
- Q2关闭：公开`recording.state = healthy | degraded`，first degradation发布`session_recording_changed`并保留脱敏diagnostic；
- Q3/Q4关闭：无public Flush、无Recorder drain deadline；
- Q5关闭：Degraded在同一loaded instance内为终态；不retry、不创建segment、不backfill；Unload/Load只恢复recorded prefix；
- Q6关闭：loaded Fork使用同一LiveSnapshot解析anchor并复制unrecorded tail，unloaded Fork使用RecordedHistory；source进入outcome与durable provenance；
- Q7关闭：无recording policy、Disabled或ephemeral Session；Create严格stage SessionHeader，Load始终尝试初始化Recorder；
- Q8关闭：live apply → inline record attempt → final domain publication/protocol continuation；
- Q9关闭：LiveSessionState私有Session-scoped EntryIdGenerator在apply前分配并绑定parent；Recorder不得创建或改写ID；
- writable Load只截断final unterminated partial tail，不修改完整行或中段内容；
- 完成current Markdown链接、代码围栏、旧术语、9条INV owner和`git diff --check`验证。

## 2026-07-31完成内容

- checkpoint提交`aa32511 docs: define live EntryId ownership`；
- 接受ADR 0127：Session JSONL只保存稳定conversation facts与必要解释元数据；
- 删除`StoredTurnStart`、`StoredTurnTerminal`和三个recorded Turn terminal variants；
- TurnStatus与`TurnStarted/Completed/Interrupted/Failed`继续作为current-process live interface；
- Q10关闭为Not Applicable：Load不推断旧Turn outcome、不追加closure，`current_turn = None`；
- Fork原样复制selected conversation path，不追加fork-specific terminal；
- historical `ListTurns/GetTurn`按TurnId分组conversation且不返回execution status。

## 恢复顺序

1. [ADR 0126](../adr/0126-turn-execution-is-async-and-session-recording-is-best-effort.md)
2. [ADR 0127](../adr/0127-session-recording-omits-turn-lifecycle.md)
3. [架构总览](../architecture.md)
4. [Session Execution](../modules/session-execution.md)
5. [Conversation Recording与Replay](../modules/conversation-storage.md)
6. [Turn Execution Context](../modules/turn-execution-context.md)
7. [Turn / Item / Interaction](../modules/turn-item-interaction.md)
8. [Compaction](../modules/compaction.md)
9. [Async/Best-Effort Recording问题关闭记录](async-loop-best-effort-recording-open-questions.md)
10. [第三轮评审关闭记录](v2-design-review-3.md)
11. [AgentLoop跨项目研究](../research/agent-loop-execution-model-study.md)

## 已冻结决策

- 删除同步`AgentLoop`、`next_action()`、`AgentLoopAction`和`accept_*()`；
- 每个loaded Session一个SessionExecutor control actor；
- 每个Running Session最多一个ActiveTurnTask；
- ActiveTurnTask直接await ModelGateway、ToolSet、Interaction和retry timer；
- LiveSessionState是current-process truth；
- LiveConversation reducer拥有complete Tool exchange和ConversationRevision；
- LiveSessionState私有持有唯一Session-scoped EntryIdGenerator；replay/Fork copied IDs seed collision guard，Degraded继续分配fresh ID；
- SessionRecorder通过inline `record().await`顺序append稳定conversation facts，不使用后台queue，也不提供durable commit receipt；
- recordable conversation mutation成功后完成inline record attempt，再publish final event或推进协议；
- JSONL不保存StoredTurnStart或Turn terminal；TurnId只用于conversation correlation；
- Turn creation在Input live apply时线性化，Agent lifecycle gate不跨Recorder await；Starting期间Submit CommandId持续作为Cancel target；
- recorder first encode/write failure后Degraded并停止后续suffix；
- public recording wire固定为`{ state: healthy | degraded }`；first degradation发布`session_recording_changed`并保留当前脱敏diagnostic；
- Create严格stage SessionHeader后发布Unloaded Session；Load始终尝试初始化Recorder；不提供recording policy、Disabled或ephemeral Session；
- Degraded在同一loaded instance内为终态，不retry、不创建segment、不backfill；Unload/Load只恢复recorded prefix并可建立new health；
- writable Load只截断final unterminated partial tail，不修改完整行或中段内容；
- recording failure不终止Turn、不产生SessionUnavailable；
- recording Degraded或process crash时StateEvent/Snapshot可以领先可恢复recorded prefix；Healthy状态无后台queue lag；
- cold replay只恢复recorded conversation prefix，未record tail和旧TurnStatus丢失；Load后current_turn为空；
- loaded Fork可以把snapshot capture前已apply的unrecorded tail写入独立child record stream；unloaded Fork只复制RecordedHistory；
- ForkSourceKind进入SessionForked outcome和child durable provenance；target staging失败不发布partial child；Fork不追加terminal；
- restart不恢复task、stream、Tool process、waiter、queue或retry timer；
- Interaction request/resolution使用live apply + record attempt + oneshot；
- logical retry使用same Arc<ModelCallRequest>、control generation和ConversationRevision；
- Compaction先Replace live conversation，再best-effort record marker；
- ToolStartGate、Cancel immediate ack、FollowUp settlement、Workspace immutable capture和ModelGateway single-attempt保持；
- Rig仍只实现ModelGateway private ProviderAdapter。

## 跨模块不变量

```text
INV-001 recordable conversation fact: allocate EntryId/parent → apply → inline record attempt → publish/continue；TurnStatus only live
INV-002 tolerant replay of recorded prefix
INV-003 complete Tool exchange before model visibility
INV-004 loaded Fork LiveSnapshot / unloaded Fork RecordedHistory
INV-101 one control actor + at most one ActiveTurnTask
INV-102 Steer safe-point FIFO
INV-201 immutable Turn capture
INV-301 live Interaction apply/inline-record-attempt ordering
INV-401 ToolStartGate vs EmergencyControl
```

## 被取代的旧机制

```text
SessionStorage durable truth
SessionWriter
append/apply physical commit barrier
CommittedConversationView
CommittedToolExchangeDelta
CommittedSteerDelta
ConversationCheckpoint as live proof
single current RunningOperation
WaitForModelRetry slot
sync sans-I/O AgentLoop
Transcript-First guarantee
writer-poisoned/read-only execution admission
StoredTurnStart / StoredTurnTerminal
cold recovery Turn terminalization
HistoricalFork terminal closure
```

旧ADR正文作为历史保留，顶部状态与修订说明指向ADR 0126/0127。

## 当前开放问题

Recorder review的Q1–Q10已经全部关闭，详见[独立review](async-loop-best-effort-recording-open-questions.md)。Q10由ADR 0127确定conversation-only schema和无closure Load。

## 下一步

1. 冻结剩余wire/schema：通用serde casing、EntryId算法/文本格式、其他public ID、Timestamp/Money、StoredCompaction；
2. 创建Rust crate，先实现LiveConversation reducer与inline SessionRecorder；
3. 使用ScriptedProviderAdapter闭环async ordinary AgentRun与complete Tool exchange；
4. 增加recorder slow-write/failure/crash/reload-prefix fixtures；
5. 实现Interaction、Cancel、logical retry和Compaction async paths；
6. 完成Rig 0.40.0 OpenAI/Anthropic provider spike；
7. production Tool/Sandbox adapter前关闭O1/R7。

## Review状态

```text
第一轮 O2–O18   原设计下已关闭；O1 Sandbox条件性开放
第二轮 R1–R6   原设计下已关闭；R7 = O1
第三轮 L1–L5   已关闭或被ADR 0126取代
```

R6 canonical owner/link纪律继续适用。新横切决策的回写顺序仍是：canonical owner → interface consumers → review/handoff → supersession notes → `rg` residue scan。

## 生产实现状态

仓库仍无`Cargo.toml`、`src/`或`tests/`。本分支当前完成的是目标架构与review收口，不包含Rust生产实现。下一台电脑应从ADR 0126/0127开始，不要按旧同步AgentLoop、后台Recorder queue、durable SessionWriter或Turn terminal ledger开始编码。
