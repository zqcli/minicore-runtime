# MiniCore V2 开发计划

状态：Proposed，等待review后恢复实现

基线：`dev` at `144039a`

范围：从当前Wire基础到可验证的ScriptedProvider vertical slice，再到production Provider与Tool/Sandbox gate

本文只定义实施顺序、依赖、交付物和验收条件，不重新定义领域语义。设计权威仍是[`architecture.md`](architecture.md)、[`modules/`](modules/README.md)、架构总览引用的current/refined ADR、[Conversation JSONL Format V1](formats/conversation-jsonl-v1.md)与[Wire V1 Fixtures](fixtures/wire-v1/README.md)。发生冲突时必须先修canonical owner文档或新增ADR，不能在实现计划中暗改合同。M0完成后由`docs/adr/README.md`唯一索引current、refined与historical ADR。

## 目标

首个MVP开发里程碑必须形成一个真实、可重复验证的纵向闭环：

```text
MiniCoreRuntime
→ SessionExecutor
→ one ActiveTurnTask
→ PromptSet
→ ScriptedProviderAdapter
→ Model / Tool / Interaction / Model
→ LiveConversation
→ inline best-effort SessionRecorder
→ unload/load tolerant replay
→ Snapshot / StateEvent
```

MVP完成时应支持：

- Wire V1 public/storage codec通过全部manifest、golden、corruption和boundary recipes；
- ordinary AgentRun；
- parallel Tool calls与complete Tool exchange；
- ToolApproval和non-secret UserQuestion；
- Starting/Running Cancel、Steer、FollowUp和logical retry；
- inline best-effort recording、Degraded、cold replay和Fork；
- pressure-triggered CompactionSummary、live Replace和recorded marker；
- snapshot-first观察和actionable queues；
- 全流程只依赖ScriptedProvider和无真实副作用的测试Tool。

production Rig ProviderAdapter和OS/network/process Tool不属于上述internal milestone，分别受V4-P1-3与V4-C0-1门禁约束。

## 当前基线

已经提交并通过单元测试、`rustfmt`与Clippy的Rust基础：

- `c06042a`：typed IDs/revisions、`CanonicalU64`、`PageCursor`、`InteractionResolutionKey`；
- `7071e77`：exact-millisecond `Timestamp`、bounded `Duration`、canonical `Money/Currency`；
- `144039a`：platform-independent `CanonicalFileUri`与`WorkspaceRelativePath`；
- authoritative file URI vectors由Rust测试直接消费；
- crate仍保持单package，尚无Runtime、Session、Conversation、storage或provider实现。

当前不能视为已完成：

- `BoundedJsonValue/Object/Schema`；
- public DTO与manifest runner；
- Conversation JSONL codec/scanner/replay；
- LiveConversation reducer；
- SessionRecorder、SessionExecutor和ActiveTurnTask；
- Prompt/Skill/Tool/ModelGateway实现；
- production provider与sandbox adapter。

## 实施原则

1. **保持单crate起步**：先用Rust module表达canonical owner，不在行为闭环前拆workspace或多个crate。只有编译边界、依赖隔离或独立发布出现真实需求时再拆分。
2. **fixture是规范资产**：Rust runner直接消费`docs/fixtures/wire-v1/`，不得复制一份测试schema或在测试中重新发明expected结果。
3. **bounded decode先于Serde便利性**：public/storage入口必须从bounded bytes/line scanner进入。不得把普通`serde_json::from_reader`、`Box<RawValue>`或raw `serde_json::Value`当作安全边界。
4. **canonical encoder自有数字语义**：dynamic JSON number必须按Wire算法直接编码，不能依赖第三方JSON number重新格式化。
5. **TDD与小提交**：每个下列task先建立失败测试，再实现最少代码；每个逻辑task单独commit，不把多个里程碑压成一个大提交。
6. **单owner并发**：每个loaded Session只有一个control actor和最多一个ActiveTurnTask；任何live-state guard不得跨`await`。
7. **先Scripted、后production**：先证明MiniCore自己的状态机、recording和协议，再接Rig或真实Sandbox，避免SDK类型反向塑造domain seam。
8. **错误留在owner**：不建立全局Error/Common registry；跨边界只做文档已冻结的确定性映射。
9. **不顺带扩展MVP**：不实现durable Turn lifecycle、same-Turn restart、recording retry/backfill、remote Workspace、secret UserQuestion、event replay或跨Session文件锁。
10. **合同变化先停工**：测试暴露文档矛盾时，先记录最小场景并回到canonical owner review；不能用implementation-only exception绕过。

## 依赖图

```text
M0 Documentation Convergence + Quality Baseline
└─ M1 Wire Limits + Bounded Codec + Owner Semantic Spine
   ├─ M2 Incremental Public Protocol DTOs
   ├─ M3 Conversation Line Codec + Physical Scanner
   ├─ M4 LiveConversation Reducer
   └─ M6 Minimal Turn Resources + Scripted Gateway

M3 + M4 + M5.0 Durable Entity/Async Foundations
└─ M5 Recorder + Semantic Replay

M2(minimal) + M4 + M5 + M6
└─ M7 Create/Load/Submit/Snapshot/Unload Ordinary Slice
   └─ M8 Control Lanes + Tools + Interaction + Cancel
      └─ M9 Steer + FollowUp + Skill Composition + Logical Retry
         └─ M10 Compaction
            └─ M11 Fork + Full Runtime/Recovery Conformance

M6 provider-neutral seam ──> early private Rig spike ──> M12 Provider Gate
M11 ──> M13 Sandbox Contract Gate
M12 + M13 ──> M14 Production Adapters ──> M15 Hardening + Release
```

`M2`按行为slice增量扩展，不再先横向实现全部DTO；`M3`只关闭line codec与physical scanner，semantic corruption sidecar由`M5`关闭。`M7`必须同时等待minimal public DTO、reducer、record/replay和Turn resources。M13正式关闭前不得开始production Tool/Sandbox adapter。

## M0 · 文档收敛、Baseline与质量门禁

目标：让开发者和AI默认只读取当前权威合同，并让每次后续提交都在同一最小质量基线上验证。历史资料保留，但不得继续污染current搜索和实施判断。

### M0.1 当前权威文档面

任务：

- 新增`docs/README.md`，只列开发必读入口、权威顺序、当前开放门禁和历史资料入口；
- 新增ADR索引，将决议分为`Current`、`Current With Later Refinements`和`Historical/Superseded`；
- fully superseded ADR可在更新全部internal links后移入`docs/archive/v2/adr/`；partially superseded ADR默认保留稳定路径并由索引指向later refinements，只有剩余规则已被current module/ADR完整接管时才能归档；
- 把已关闭的旧review和过期handoff/progress移入`docs/archive/v2/`，保留原文和Git history，不做内容性重写；
- 第四轮review在V4-P1-3/V4-C0-1关闭前继续保留current；research继续保留但明确标记non-authoritative；
- 默认`rg`搜索排除`docs/archive/`，需要历史时显式使用`rg -uu`或指定archive路径；
- 更新所有受移动影响的relative links、ADR supersession links和导航；
- 清除current canonical/navigation文档中的过时实施状态，例如“尚无Cargo.toml”“下一步创建Rust crate”；历史review正文允许保留原始场景，但必须有明确Historical标记；
- archive文件不得重新成为current module的canonical owner。

退出条件：

- `docs/README.md`能够在一页内给出完整current reading path；
- ADR索引中每份文件只有一个明确分类和successor/refinement link；
- architecture、modules、README、CONTEXT与development plan不再把已删除机制写成prescriptive current design，也不包含错误实施状态；
- current文档的relative links、Markdown fences和ADR successor links全部有效；
- 默认文本搜索不会返回archive内容，显式历史搜索仍可用；review/research中的历史术语通过目录和状态清楚区分；
- Git history与archive中仍可追溯每个旧决策的原始理由。

建议分开提交：`docs: define current documentation surface`、`docs: archive superseded v2 design records`。

### M0.2 Rust质量门禁

任务：

- 验证既有MSRV、edition和dependency policy；无必要不新增异步runtime、SDK或schema engine；
- 增加`scripts/check.sh`统一fast gate，至少运行format、all-target tests、Clippy和fixture structural verifier；
- 增加独立`scripts/check-msrv.sh`与`scripts/check-heavy.sh`入口；heavy gate不得被默认`cargo test`隐式触发，避免普通PR生成1 GiB文件；
- 确认现有11个wire carrier/value/path测试继续通过；
- CI在declared Rust 1.85和current stable分别执行check/test，防止误用post-MSRV API；
- CI只缓存构建产物，不缓存测试semantic结果。

退出条件：

```bash
./scripts/check.sh
./scripts/check-msrv.sh
# 显式、非默认：./scripts/check-heavy.sh
```

建议提交：`build: establish rust quality gate`。

## M1 · Wire Foundations与Owner Semantic Spine

### M1.1 ProtocolLimits与bounded counters

先实现后续所有parser/codec共同消费的唯一limits：

- 完整`ProtocolLimits` tree及exact v1 constants；
- byte/depth/node/member/item/string/number的checked counter；
- bounded string/list/map helper，不把limit magic number复制到各wrapper；
- protocol negotiation exact pair与capability intersection；
- boundary recipe runner的small/heavy分类基础。

退出条件：`protocol-limit-cases.json`和negotiation vectors全部通过，BoundedJson/Schema只通过typed limit view取值。

建议提交：`feat: add wire protocol limits`。

### M1.2 BoundedJson parser与canonical encoder

实现：

- byte-oriented、duplicate-aware JSON parser；
- raw input cap与canonical output cap分别校验；
- root depth=1、direct member/item、decoded string bytes和number literal limits；
- exact decimal representation，不经过`f64`；
- decoded-key UTF-8 byte order；
- 唯一string escaping与number canonicalization；
- `BoundedJsonValue`与root-object-only `BoundedJsonObject`；
- public constructor从bounded byte slice进入，generic Serde API不得成为allocation boundary。

必须先覆盖的失败场景：

- nested duplicate decoded key；
- depth/member/item/string/number exact boundary与+1；
- raw input合法但canonical output超限；
- positive scientific exponent不得出现`+`；
- malformed escape、lone surrogate、invalid UTF-8、control character；
- 大量non-ASCII输入保持O(n)扫描，不重复验证remaining suffix；
- parser panic-free，错误不回显raw payload。

退出条件：ordinary embedded JSON全部recipes通过；canonical bytes可直接用于Eq/Hash/idempotency；不存在raw `serde_json::Value` public/storage carrier。

建议至少拆成：`feat: parse bounded dynamic json`、`feat: canonicalize dynamic json`、`test: enforce bounded json recipes`。

### M1.3 BoundedJsonSchema carrier

实现：

- Draft 2020-12 root object carrier；
- encoded bytes/depth/total nodes计数；
- `properties`、`required`、`enum`各自direct 256 cap；
- regex text 1,024-byte cap并使用bounded/non-backtracking实现；
- local fragment `$ref`；remote/network/file ref fail closed；
- Wire只保证bounded Draft 2020-12 object carrier、ref/regex safety和结构limits，不实现通用JSON Schema validator；Tool/Model owner继续做supported-keyword validation、instance validation和provider lowering。

退出条件：schema boundary recipes、local/remote ref、regex和node-count tests通过；decode期间无network/filesystem访问。

建议提交：`feat: add bounded json schema carrier`。

### M1.4 Typed codec primitives

实现：

- duplicate-key-aware typed object decode support；
- compact typed encoder，known fields按declaration order，`Option`显式value/null；
- client input strict unknown field/variant与Runtime output selected-version规则；
- request/response/progress frame preflight；
- generic Serde trait只能用于已bounded的内部值或输出便利层，不能替代Wire-owned input codec。

退出条件：bootstrap与representative mixed enum/nullable DTO可duplicate-aware decode并byte-exact encode。

建议提交：`feat: add canonical typed json codec`。

### M1.5 Canonical owner semantic type spine

在行为实现前，由各canonical owner建立M2–M4共同需要的纯数据类型与private constructors：

- Prompt：Text-only message/content parts、Prompt contribution stamp；
- Tools：ToolName/spec/call/arguments/result content、source/disposition/outcome；
- ModelGateway：model identity、reasoning/response/finish/usage/error semantic leaves；
- Turn/Interaction：Item relation、safe request/resolution/cancel reason；
- Compaction：stored model-call provenance和stable-unit semantic leaves。

这些类型仍位于所属module，Wire只实现representation；禁止建立`common`类型仓库或让storage format复制semantic declaration。

退出条件：Conversation line codec与LiveConversation reducer所需共享类型都有唯一owner，`rg`不存在第二份同名不兼容struct/enum。

建议按owner拆成多个compile-green提交。

## M2 · Incremental Public Protocol Codec

目标：每个行为vertical slice同时获得对应public DTO，不在Runtime行为前横向实现全部协议，也不等到M11才第一次接facade。

任务顺序：

1. bootstrap、outer request/response/frame envelope、dispatch/query/snapshot/subscribe route skeleton；
2. 为M7实现Create/Load/Submit/Cancel/Unload、CommandCompletion/Error、minimal SessionSnapshot与StateEvent；
3. M8同步扩展Tool/Interaction Item和resolution DTO；
4. M9同步扩展actionable queues、Steer/FollowUp和retry projection；
5. M10同步扩展Compaction/usage/diagnostic projection；
6. 每个family落地时由manifest runner逐项decode、semantic assert和canonical re-encode；
7. M11要求manifest无pending target，并统一执行compat ignored-pointer比较。

实现约束：

- DTO语义由`runtime-interface.md`拥有，Wire只投影representation；
- Command accepted failure不能混入outer dispatch error；
- Snapshot不得因size临时截断actionable state；超限按owner合同fail closed；
- Interaction request/resolution不得泄露private Tool args、secret或resolution key；
- Debug/diagnostic覆盖redaction测试。

M2初始退出条件：M7所需public路径有真实codec与route skeleton，不能只通过private API测试ordinary slice。完整manifest closure属于M11退出条件。

建议按行为family拆成多个提交；M11最后提交：`test: enforce public wire v1 manifest`。

## M3 · Conversation Format与Bounded Scanner

### M3.1 Exact Header/Entry codec

实现：

- strict `SessionHeader`；
- required `entryId/parentId/sessionId/turnId/timestamp/body`；
- User、Assistant、Tool、InteractionRequested、InteractionResolved、Compaction六种flat body；
- exact field/tag/null order；
- Tool outcome truth matrix与Compaction model-call projection；
- Header/session/catalog identity validation。

退出条件：全部conversation golden complete line可byte-exact round-trip；跨行selected path、sanitized messages和relation assertions留给M5 semantic replay。

### M3.2 Physical scanner

实现：

- whole-file 1 GiB cap优先；
- Header 64 KiB、entry line 1 MiB；
- complete entry count 1,000,000合法，第1,000,001个失败；
- LF/CRLF处理；
- malformed/unknown/oversized complete line可skip并继续；
- 仅final unterminated tail在exclusive writable lease下可truncate；
- scanner在读取完整oversized line前保持bounded allocation。

退出条件：scanner层的UTF-8、newline、Header/line/file/count、partial tail和oversized-line recovery assertions通过；semantic corruption `.expected.json` sidecar留给M5。1 GiB测试使用streaming generator并归入heavy test tier。

建议提交：`feat: add conversation jsonl codec`、`feat: add bounded jsonl scanner`。

## M4 · LiveConversation Reducer

目标：先证明conversation协议正确性，再编排async执行。

任务：

- Session-scoped `EntryIdGenerator`与collision guard；
- User/Assistant/Tool/Interaction/Compaction typed apply methods；
- `ConversationRevision` checked increment；
- ItemId、ToolCallId和relation validation；
- first truthful ToolResult与duplicate/conflicting terminal处理；
- incomplete Tool exchange不进入`LiveConversationView`；
- complete exchange按assistant call order投影，不按completion order；
- stable-unit `LiveCompactionSourceView`；
- Compaction Replace exact revision/source/marker验证；
- Snapshot/read-model projection从同一live state派生。

测试：

- property tests覆盖任意Tool completion permutation；
- duplicate text但不同EntryId的marker identity；
- incomplete/orphan/abandoned exchange；
- stale revision/plan rejection；
- reducer方法同步执行，不持锁或执行I/O。

退出条件：INV-003与INV-005在纯内存测试中成立。

建议提交：`feat: add live conversation reducer`、`feat: expose compaction stable units`。

## M5 · Durable Foundations、Recording与Replay

### M5.0 Entity store与async test seam gate

`agent-session-lifecycle.md`仍未冻结durable entity-head/CAS store和跨entity/conversation staging协议。开始Session Create/Fork前必须先关闭该实现门禁：

- 定义MVP single-process durable store、Agent/Session immutable definition head、metadata head和CAS shape；
- 定义SessionDefinition、initial SessionHeader、fork provenance与catalog visibility的staging/atomic publication协议；
- 列出crash point、cleanup/retry和exclusive lease语义；
- 通过canonical module更新或Accepted ADR冻结，不在storage implementation中即兴决定；
- 选择async runtime，并为clock/sleep、spawn/join、cancellation、controlled writer/filesystem fault和deterministic barrier建立crate-private test seams；
- 配置Clippy `await_holding_invalid_type`覆盖选定runtime的lock guards。

退出条件：Create/Fork能够写出确定性crash matrix；M5/M7不依赖未定义的跨文件事务。

建议提交：先提交design gate，再提交`build: add deterministic async test seams`。

### M5.1 SessionRecorder

实现：

- staged Header creation；
- single ordered `record(entry).await`；
- encode完成且size合法后才进行第一次write；
- `write_all`语义与partial/unknown write failure；
- first failure `Healthy → Degraded`，当前load停止后续记录；
- 不retry、不segment、不backfill、不回滚live mutation；
- diagnostic只保留allowlisted code与redacted bounded message。

### M5.2 Tolerant replay

实现：

- strict Header后逐行semantic decode；
- session mismatch在EntryId reservation前拒绝；
- duplicate/orphan/invalid relation隔离；
- first valid root + physical-last eligible leaf选择path；
- incomplete Tool exchange排除；
- bounded diagnostic detail、aggregate与truncation summary；
- cold state的`current_turn = None`。

退出条件：全部conversation corruption expected sidecar通过，LiveConversation与cold sanitizer对complete Tool exchange/Compaction规则一致。M5只证明Recorder failure不回滚已传入的live mutation；“不重复Model/Tool外部操作”分别由M7/M8 fault tests证明。

建议提交：`feat: add best effort session recorder`、`feat: replay tolerant conversation history`。

## M6 · Minimal Turn资源与Scripted ModelGateway

### M6.1 Workspace、Prompt与captured empty views

先实现ordinary Text turn所需最小但真实路径：

- Workspace definition resolve与immutable snapshot；
- Prompt source在publication前materialize；
- PromptService/PromptSet只做同步纯内存normalize/assemble；
- capture合法empty SkillView/ToolSet snapshot，保证TurnExecutionContext shape从第一条vertical slice开始稳定；
- Text Input normalize与safe provenance；
- 完整SkillIntent async load/composition延后到M9，完整Tool execution延后到M8。

### M6.2 ModelGateway与ScriptedProviderAdapter

实现：

- model resolution与immutable `TurnModelSnapshot`；
- 唯一`ModelCallRequest` constructor/proof；
- one provider attempt；
- response/finish/usage/error validation；
- scripted terminal and streaming sequences；
- Gateway无logical retry、无Session permit、无conversation owner。

退出条件：PromptSet产生唯一Model input；Scripted adapter不能绕过Gateway validation；reload只影响future Turn。

建议按owner各自提交，最后提交：`feat: add scripted model gateway`。

provider-neutral request/result seam稳定后立即并行运行private Rig reality spike，尽早发现缺失字段；该spike不发布production adapter、不关闭V4-P1-3，也不能把Rig类型引入domain。

## M7 · Ordinary AgentRun Vertical Slice

实现：

- minimal Agent/Session durable definitions与revision CAS；
- Session Create严格stage Header后publish `Open + Unloaded`；
- Load初始化replay、LiveSessionState和Recorder；
- 每loaded Session一个SessionExecutor control actor；
- 通过M2 minimal `MiniCoreRuntime.dispatch/snapshot/subscribe` facade驱动，不只测试private Session API；
- Submit admission捕获immutable TurnExecutionContext；
- Input apply线性化Turn creation；
- inline record attempt后发布`TurnStarted`；
- spawn one ActiveTurnTask；
- Prompt assemble → ModelGateway → final Assistant apply/record；
- Turn Completed live settlement与Snapshot/StateEvent；
- Unload不恢复task或旧TurnStatus。

端到端测试：

```text
Create → Load → Submit(Text) → Model(final text)
→ Snapshot Completed → Unload → Load
→ recorded User/Assistant restored, current_turn = None
```

退出条件：INV-001、INV-002、INV-101和INV-201在真实async integration test成立；任何guard不跨await；Recorder failure不导致同一Model request被重复调用。

建议提交：`feat: run ordinary scripted agent turn`。

## M8 · Tools、Interaction与Cancel

M8首先建立ActiveTurnControl、EmergencyControl、Interaction resolution和Tool settlement所需control lanes；不能等到M9才让Cancel/approval message进入actor。

### M8.1 Complete Tool exchange

- Assistant含A/B/C calls；
- 只有policy compatible、不同canonical file keys且非`Serial`/multi-file/open-world/ask-user的calls允许并行；其余按call order串行；
- ToolService/ToolSet、safe policy/approval options、无真实副作用的ScriptedToolExecutor与Session-local FileMutationQueue在本slice实现；
- ToolSet不写LiveSessionState或Recorder；
- 结果按first truthful settlement apply；
- 每个result完成inline record attempt；
- 全部expected results完成后下一次Model才允许；
- model view按call order输出A/B/C；
- pre-execution Succeeded/Failed/Denied/Cancelled与Executed truth matrix严格验证。

### M8.2 Interaction

- request live apply → record attempt → notify；
- approval只选择request-scoped option或Deny；
- UserQuestion只允许non-secret Text/SingleChoice；
- resolution key same-key/same-payload幂等，same-key/different-payload冲突；
- resolution apply → record attempt → resume waiter；
- owner-driven closure使用null key。

### M8.3 Cancel与ToolStartGate

- Cancel sticky epoch立即返回`CancelAccepted`；
- Starting Input apply前完成`SubmitCancelled`且不创建Turn；
- Input apply后原Submit先完成`TurnStarted`，再绑定同一Turn、阻止spawn并通过后续StateEvent进入Interrupted；
- ToolStartGate与Cancel/SecurityRevoked first-wins；
- Running Tool只能truthful settle，不能伪装pre-execution取消；
- subscriber disconnect与elapsed time不自动resolve Interaction。

退出条件：ordinary Tool round-trip、serial/parallel scheduling matrix、ask-user、approval deny、cancel-before-start、cancel-running-tool和recording-degraded场景全部端到端通过；Tool result recording failure不得重复Tool side effect。

建议拆分提交：Tool exchange、Interaction、Cancel各一个。

## M9 · Steer、FollowUp与Logical Retry

实现：

- control/work ingress lanes与bounded queues；
- Snapshot完整枚举可取消Submit/Steer/FollowUp CommandId；
- Steer只在完整assistant/tool step后、下一次Model前FIFO消费；
- FollowUp等待旧task terminal后重新capture新Turn context；
- same in-flight CommandId/same payload加入shared completion；
- logical retry复用exact same `Arc<ModelCallRequest>`；
- retry basis绑定Turn、control generation和ConversationRevision；
- backoff cancellation-aware；
- queued Steer不改变revision，不错误废弃retry。
- SkillService绑定captured SkillViewContext；Input与Steer共享async `resolve_user_message()`，reload-during-Steer继续使用old captured bytes；
- 每个Skill/Workspace contribution形成独立content part与safe part-level stamp。

退出条件：Starting竞态、retry/Steer、Cancel queued command、FollowUp settlement均有deterministic integration test；Snapshot alone足以恢复host可执行action。

建议提交：`feat: add session message queues`、`feat: add logical model retries`。

## M10 · Compaction

实现：

- Runtime-global validated settings与Turn snapshot；
- pressure input使用same Turn model estimator/limits；
- source按stable units，Tool exchange不可拆；
- plan只保存source+cut并派生summary prefix、retained suffix和marker；
- Summary Prompt仍经过PromptSet和ModelGateway；
- automatic model-call provenance始终`Some`；
- stale plan/result按exact Session/control/request/revision拒绝；
-先分配Compaction EntryId并Replace live，再inline record marker；
- marker record失败时restart恢复旧recorded conversation。

退出条件：overflow → summary → Replace → next AgentRun，以及Degraded/crash/replay marker矩阵全部通过；每Turn最多4次和minimum reclaim生效。

建议提交：`feat: compact live conversation by stable units`。

## M11 · Fork与Full Runtime/Recovery Conformance

目标：扩展从M7开始使用的唯一public facade，完成Fork与全部public/storage conformance并关闭internal MVP。

任务：

- 补齐`MiniCoreRuntime.dispatch/query/snapshot/subscribe`全部family；
- 补齐Agent/Session lifecycle、metadata CAS、readiness与route errors；
- snapshot-first atomic baseline + ordered StateEvent + lossy ProgressEvent；
- command completion、idempotency和deterministic error mapping；
- recording health/diagnostic projection；
- history query与loaded live Snapshot明确分离；
- loaded Fork从同一immutable LiveSnapshot复制selected path；unloaded Fork使用RecordedHistory；
- child SessionDefinitionRevision(1)、metadata、copied definition、Header/history、fork provenance和catalog visibility按M5.0协议完整staging后原子publish；
- historical IDs原样复制，future EntryId generator从collision guard产生fresh IDs；staging失败不留下partial child；
- slow recorder、write failure、provider cancellation、Tool cancellation和subscriber reconnect fault injection；
- full scenario replay：ordinary、Tools、Interaction、Cancel、retry、Compaction、Unload/Load、Fork。

退出条件：

- public manifest全部通过；
- conversation golden/corruption全部通过；
- internal vertical scenarios全部通过；
- `cargo test --all-targets`不依赖network、真实credential或ambient home config；
- repeated/stress test没有同Session双ActiveTurnTask、waiter double-resume或guard-across-await；
- best-effort recording的数据丢失边界与文档一致。

建议最终internal milestone提交：`test: close scripted runtime vertical slice`。

## M12 · Production Provider Gate（V4-P1-3）

M6后允许private、不可发布的Rig reality spike；开始production `RigProviderAdapter`实现前必须关闭本门禁：

1. 首版只发布已验证的ProviderProtocol；建议OpenAI Responses和Anthropic Messages进入available catalog，其他variant删除或明确Unsupported；
2. 清除Gateway-local concurrency wait/per-principal permit旧措辞；
3. 固定queued Steer不使logical retry失效；
4. Rig 0.40.0 spike验证system/instructions、messages、Tool schema与ToolCall identity/order、Anthropic thinking/signature/cache-control、reasoning artifact round-trip、stream cancellation/EOF、finish、usage、request/response ID、base URL和allowlisted metadata；
5. 实证SDK automatic retry=0，并为每类error证明delivery state；只有可证明`NotSent`或`RejectedBeforeExecution`的失败才能进入允许logical retry的分类；
6. 验证context overflow、rate limit、auth、transport、malformed response和early EOF的provider-neutral映射；
7. OpenAI Responses与Anthropic Messages各自使用local mock server contract tests；
8. Rig类型只存在于private adapter，不进入Prompt、Conversation或Runtime public DTO。

退出条件：V4-P1-3标记Closed并有独立ADR/fixture证据；随后才实现production adapters。

## M13 · Production Tool/Sandbox Gate（V4-C0-1）

在开始任何production Tool/Sandbox adapter实现前，先用contract types与fake capability backend关闭本门禁：

1. adapter contract声明可强制capability classes；
2. 计算final PermissionSet与enforceable capability差集；
3. 差集非空时在ToolStartGate前生成PreExecution Denied ToolResult；
4. Sandbox initialization/enforcement失败不得fallback裸执行；
5. approval不能提升adapter无法强制的能力；
6. 同Session file aliases进入同一FIFO；跨Session明确不协调并测试可能并发；
7. fake backend完成SecurityRevoked、Sandbox unavailable与Running Tool truthful settlement contract tests。

退出条件：O1/R7/V4-C0-1通过Accepted ADR和adapter-independent conformance tests正式关闭；随后才允许实现production adapter。

## M14 · Production Adapters

门禁关闭后按独立adapter交付：

- OpenAI Responses RigProviderAdapter与mock/live opt-in smoke tests；
- Anthropic Messages RigProviderAdapter与mock/live opt-in smoke tests；
- production Tool/Sandbox adapters，每个声明并证明effective enforcement capability；
- production tests默认只使用local mock/fake backend，真实credential/network测试必须显式opt-in；
- adapter失败不能改变MiniCore domain truth、retry owner或ToolStartGate语义。

退出条件：每个adapter独立通过contract suite，未配置credential时整个默认test suite仍可运行。

## M15 · Hardening与发布准备

在internal MVP与production adapters之后执行：

- parser/replay/public decoder fuzzing与corpus regression；
- reducer property tests与并发stress tests；
- generated 1 MiB/1 GiB/1,000,001-entry heavy suite；
- disk-full、partial write、permission denied和forced cancellation fault injection；
- memory/CPU benchmark，确认limits内近似线性；
- public API docs、examples和host integration guide；
- semver/export surface review；
- license/dependency/security audit；
- release checklist与versioned conformance report。

不在此阶段补做核心correctness；任何主路径协议缺陷必须退回所属早期milestone修复。

## 测试分层

### Fast PR Gate

每次提交运行：unit、focused integration、public small fixtures、conversation small fixtures、format、Clippy和structural verifier。不得访问network。

### Generated Boundary Gate

定期或显式运行Header/entry/frame/canonical JSON exact limits、1,000,000/1,000,001 entries和1 GiB file recipes。generator必须stream输出到临时文件并在测试后清理。

### Fault/Concurrency Gate

使用controlled writer、scripted clock/provider/tool和deterministic barriers复现：slow write、partial write、Cancel/ToolStart first-wins、Starting cancel、late model result、waiter closure和Unload。

### Fuzz/Property Gate

覆盖bounded JSON parser、typed public decoder、JSONL scanner、replay relation reducer和Tool completion permutations。每个发现先固化为最小regression fixture，再修实现。

## 每个Task的Definition Of Done

一个逻辑task只有同时满足以下条件才可提交：

- 失败测试先证明目标行为或bug；
- 实现只触及canonical owner及必要consumer；
- authoritative fixture被消费，不在测试中硬编码第二份合同；
- success、boundary、+1、wrong-shape和redaction场景与风险相匹配；
- 无raw secret/path/provider body进入Debug、event或diagnostic；
- `cargo fmt/test/clippy`与`git diff --check`通过；
- semantic contract变化已先更新canonical docs/ADR；
- commit只包含一个可描述、可回滚的逻辑任务；
- worktree中既有无关改动不被整理或撤销。

## 风险与停止条件

| 风险 | 触发信号 | 处理 |
| --- | --- | --- |
| best-effort recording不满足产品预期 | 要求crash后不丢任何已展示内容或恢复旧Turn | 立即停止实现并重新决策durability；不能在Recorder里偷偷增加commit语义 |
| bounded JSON无法在分配前限流 | 实现依赖完整`RawValue`/Value materialization | 停止该task，建立专用byte parser/codec API |
| 第三方SDK侵入domain | Rig message/agent/retry类型出现在public或conversation module | 回退adapter边界，先关闭V4-P1-3 |
| 双owner并发状态 | SessionExecutor与ActiveTurnTask各自维护conversation/Turn副本 | 停止并回到INV-101 owner设计 |
| recording成为执行permit | Model/Tool继续依赖append成功 | 删除依赖，恢复live apply → record attempt → continue |
| Sandbox不可强制 | adapter只能声明、不能实际限制capability | production Tool fail closed，不允许裸执行 |
| 跨Session同文件冲突 | 产品要求共享worktree强一致 | 由host隔离或新增独立跨Session协调决策；不能暗改Session-local queue |
| full protocol拖慢主闭环 | DTO批量实现长期没有运行场景 | 保持manifest family小提交，但不得跳过最终conformance gate |
| superseded文档污染实施判断 | current搜索命中旧AgentLoop、durable writer或错误实施状态 | 先完成M0归档和导航，不靠口头权威顺序长期兜底 |

## 首轮恢复顺序

计划review通过后，只恢复下列task，不提前进入Session执行：

1. `M0.1`：收敛current文档面并归档fully superseded V2资料；
2. `M0.2`：统一Rust质量检查入口；
3. `M1.1`：实现ProtocolLimits与bounded counters；
4. `M1.2`：从测试重新实现bounded dynamic JSON专用parser/encoder；
5. `M1.3`：实现BoundedJsonSchema carrier。

`M1.2`必须重新审视任何本地未提交草稿，不得因为已有代码存在而跳过positive exponent、pre-allocation limit和Unicode linearity测试。完成每个task后立即单独commit并汇报验证结果。
