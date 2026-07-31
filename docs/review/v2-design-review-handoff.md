# V2设计评审工作交接

日期：2026-07-31
状态：async loop / conversation-only best-effort recording refactor分支

架构事实以`docs/architecture.md`、`docs/modules/`、ADR 0126、ADR 0127、ADR 0128和ADR 0129为权威。本文记录恢复入口与剩余工作。

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
- 接受ADR 0128并关闭Prompt Q1：PromptContent在candidate build期间完全materialize，内部使用强`Arc<str>`共享；不定义可重新解析或durable的PromptContentRef；
- PromptSet `for_turn()`/`assemble()`不执行正文I/O或cache-key resolver lookup，cache eviction不影响已发布view；
- 接受ADR 0129并关闭Prompt Q4：PromptIntent使用`body + skills[]`，SkillIntent只保存SkillId；
- exact Skill/Workspace authorization在composition前完成，每个contribution形成独立顶层content part；
- live/JSONL共同使用`content_part_index + safe origin`，不保存字符offset、绝对路径或authorization；损坏stamp只产生diagnostic，不丢conversation正文。

## 2026-07-31第四轮完整复审

- 主审逐份通读全部55个current、非归档Markdown；subagent输出仅作查漏线索；
- 新增[第四轮设计评审](v2-design-review-4.md)，确认5个P0、4个P1和1个既有条件性Sandbox P0；
- P0集中在共享`ModelCallRequest`、Tool contract、async Skill composition、conversation JSONL event owner和Compaction source/settings；
- P1集中在Runtime public payload、wire/storage envelope、provider scope/Rig现实验证和Workspace跨Session测试残留；
- ADR 0126–0129主方向保持，不恢复同步AgentLoop、durable SessionWriter、Turn lifecycle JSONL或Prompt resolver。

## 恢复顺序

1. [ADR 0126](../adr/0126-turn-execution-is-async-and-session-recording-is-best-effort.md)
2. [ADR 0127](../adr/0127-session-recording-omits-turn-lifecycle.md)
3. [ADR 0128](../adr/0128-prompt-content-is-materialized-before-publication.md)
4. [ADR 0129](../adr/0129-user-message-contributions-use-part-level-safe-provenance.md)
5. [第四轮完整设计评审与实施门禁](v2-design-review-4.md)
6. [架构总览](../architecture.md)
7. [Prompt](../modules/prompt.md)
8. [Session Execution](../modules/session-execution.md)
9. [Conversation Recording与Replay](../modules/conversation-storage.md)
10. [Turn Execution Context](../modules/turn-execution-context.md)
11. [Turn / Item / Interaction](../modules/turn-item-interaction.md)
12. [Compaction](../modules/compaction.md)
13. [Async/Best-Effort Recording问题关闭记录](async-loop-best-effort-recording-open-questions.md)
14. [第三轮评审关闭记录](v2-design-review-3.md)
15. [AgentLoop跨项目研究](../research/agent-loop-execution-model-study.md)

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
- PromptContent是candidate build期间完全materialize的immutable text value；PromptResourceView/PromptSet强Arc持有，source locator与cache key不能解析Turn正文；
- PromptIntent使用body与ordered SkillIntent正交结构；SkillIntent只保存SkillId；
- UserMessage contribution按独立顶层content part规范化，stamp使用content_part_index与safe Skill/Workspace origin；
- exact source authorization不进入conversation JSONL；tolerant replay丢弃损坏stamp并保留正文；
- 用户显式Skill选择不创建Item，模型触发Skill Tool继续使用ToolInvocation Item；
- Rig仍只实现ModelGateway private ProviderAdapter。

## 跨模块不变量

```text
INV-001 recordable conversation fact: allocate EntryId/parent → apply → inline record attempt → publish/continue；TurnStatus only live
INV-002 tolerant replay of recorded prefix
INV-003 complete Tool exchange before model visibility
INV-004 loaded Fork LiveSnapshot / unloaded Fork RecordedHistory
INV-101 one control actor + at most one ActiveTurnTask
INV-102 Steer safe-point FIFO
INV-201 immutable Turn capture；PromptContent materialized before capture
INV-202 exact contribution authorization before composition；safe part-level provenance after composition
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
re-resolvable/durable PromptContentRef
PromptIntent::Skill / PromptIntent::Composite
character-offset contribution stamp
StoredPromptContributionStamp
exact source authorization in JSONL
```

旧ADR正文作为历史保留，顶部状态与修订说明指向ADR 0126/0127/0128/0129。

## 当前开放问题

Recorder review的Q1–Q10已经全部关闭，详见[独立review](async-loop-best-effort-recording-open-questions.md)。Q10由ADR 0127确定conversation-only schema和无closure Load。

Prompt Q1/Q4已分别由ADR 0128/0129关闭。剩余Prompt template/cache/hook等问题不阻塞当前conversation storage schema。

## 下一步

1. 按[第四轮评审](v2-design-review-4.md)顺序关闭P0：ModelCallRequest、Tool contract、async Skill composition、conversation JSONL event owner和Compaction source/settings；
2. 关闭第四轮P1并冻结通用serde casing、ID文本格式、Timestamp/Money、StoredCompaction与Runtime UI-safe payload；
3. 创建Rust crate，先实现LiveConversation reducer与inline SessionRecorder；
4. 使用ScriptedProviderAdapter闭环async ordinary AgentRun与complete Tool exchange；
5. 增加recorder slow-write/failure/crash/reload-prefix fixtures；
6. 实现Interaction、Cancel、logical retry和Compaction async paths；
7. 完成Rig 0.40.0 OpenAI/Anthropic provider spike；
8. production Tool/Sandbox adapter前关闭O1/R7。

## Review状态

```text
第一轮 O2–O18   原设计下已关闭；O1 Sandbox条件性开放
第二轮 R1–R6   原设计下已关闭；R7 = O1
第三轮 L1–L5   已关闭或被ADR 0126取代
第四轮 V4-*    P0/P1已识别，等待canonical owner决议与关闭
```

R6 canonical owner/link纪律继续适用。新横切决策的回写顺序仍是：canonical owner → interface consumers → review/handoff → supersession notes → `rg` residue scan。

## 生产实现状态

仓库仍无`Cargo.toml`、`src/`或`tests/`。本分支当前完成目标架构、Recorder/Prompt决策收口和第四轮完整review finding识别，不包含Rust生产实现。下一台电脑应先读取ADR 0126–0129与第四轮review并关闭P0/P1；不要按旧同步AgentLoop、后台Recorder queue、durable SessionWriter、Turn terminal ledger、PromptContent resolver、recursive Skill/Composite intent或字符offset provenance开始编码。
