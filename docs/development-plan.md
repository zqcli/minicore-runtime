# MiniCore V2 开发计划

状态：Active；M0、M1、M2 minimal Snapshot/Event、M3.1、M3.2、M4与M5.0 durable entity/async **design gate**已完成；M5.0 durable foundation与exact historical definition resolution已完成，remaining Fork anchors/LiveSnapshot与完整platform matrix pending；M5.1 DurableState-issued published conversation target、same-open writable proof、owner-tracked SessionRecorder physical append及全部七个 `slice = m5_1` Recorder fixture坐标已完成，M5.2 tolerant semantic replay/corruption sidecars与replay/Recorder-backed Ready+Idle Load hydration也已实现并通过独立全量验证；M6.1 Workspace resolver/Snapshot、crate-private loaded Ready+Idle publication owner及Runtime-owned residency foundation（single-flight Load、draining Unload、lifecycle exclusion、unified loaded/unloaded Workspace update）已完成，Prompt candidate/profile/Text composition、Runtime-owned PromptService/initial PromptResourceView、Workspace Prompt candidate capture在Load与loaded Workspace publication中的接入，以及owner-bound empty SkillView/ToolSet foundation已实现；M6.2 scripted text-only ModelGateway foundation已完成AgentRun Prompt assembly/proof、model resolution/request、single provider attempt、progress/final/error validation、cancel linearization和Runtime-owned initial empty catalog；M7 ordinary AgentRun vertical slice已完成public Create/Load/Submit/Snapshot/Subscribe/Unload、immutable Turn context capture、Input/final Assistant live apply与inline record、single scripted model attempt、terminal Event、Unload/Load replay，以及context overflow、Recorder failure和busy/Unload关键路径；M8.1最小Scripted Tool round-trip与M8.2最小crate-private Interaction approval seam已完成，M8.3已接通crate-private Cancel seam（active Submit/Turn、pending Interaction owner cancellation、cancel-before-input）；M9.1已建立crate-private bounded FollowUp FIFO并接入SessionExecutor terminal handoff，M9.2已接通crate-private Steer admission、expected Turn 校验、per-Turn bounded FIFO与tool-round safe-point消费，M9.3已接通crate-private queued-message cancellation（按 CommandId actor-local remove、重复取消返回 NotQueued、跨 lane CommandId 冲突拒绝）；M9.4文本候选与Steer/Final first-wins 仲裁、M9.5 AgentRun delivery-safe logical retry、M9.6 retry control-generation/ConversationRevision/lifecycle 重验最小 seam、M9.7 crate-private Snapshot 的 active Submit/FollowUp/Steer command-id lane projection、M9.8 crate-private sticky EmergencyControl first-wins/epoch/wakeup seam已完成；完整Emergency/SecurityRevoked control lanes、public FollowUp DTO、CancelQueuedMessage public projection、Skill composition仍后置；public SubmitCancelled/TurnInterrupted、ToolStartGate、complete shared-root publication、concrete source discovery、Skill source capture、完整Tool policy/approval、Structured/Compaction assembly、production Rig adapter与grace/cancel式active-Turn Unload仍后置

初始实现基线：`dev` at `144039a`

范围：从当前Wire基础到可验证的ScriptedProvider vertical slice，再到production Provider与Tool/Sandbox gate

本文只定义实施顺序、依赖、交付物和验收条件，不重新定义领域语义。设计权威仍是[`architecture.md`](architecture.md)、[`modules/`](modules/README.md)、架构总览引用的current/refined ADR、[Conversation JSONL Format V1](formats/conversation-jsonl-v1.md)、[Wire V1 Fixtures](fixtures/wire-v1/README.md)、[Durable Store V1](formats/durable-store-v1.md)与[Durable Store V1 Fixtures](fixtures/durable-store-v1/README.md)。发生冲突时必须先修canonical owner文档或新增ADR，不能在实现计划中暗改合同。M0完成后由`docs/adr/README.md`唯一索引current、refined与historical ADR。

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

M0与M1已经提交并通过Fast、MSRV 1.85与heavy boundary gates：

- exact V1.0 `ProtocolLimits`、checked counters、exact-version negotiation与sealed Runtime capability intersection；
- byte-oriented duplicate-aware `BoundedJsonValue/Object`与exact decimal canonicalization；
- bounded Draft 2020-12 `BoundedJsonSchema`、local-ref/regex/node safety；
- duplicate-aware typed bootstrap codec、bounded encoder与selected-version sender/receiver rules；
- Prompt/Skill/Workspace、Tools、ModelGateway、Turn/Item/Interaction和Compaction-owned semantic spine及M3 replay reconstruction seams；
- authoritative limits/JSON/Schema/public compatibility vectors由Rust tests直接消费；
- crate继续保持单package，M1没有实现Session、Recorder、provider attempt、Tool execution、Compaction planning或live reducer行为。

当前不能视为已完成：

- M2 remaining public protocol DTO families与完整manifest closure；
- M8–M10 Tool/Interaction/Cancel、queues/logical retry、Skill composition与Compaction；
- M11 remaining Fork anchors/LiveSnapshot与full recovery conformance；
- production provider与sandbox adapter。

M4已完成Prompt-owned opaque `ModelMessage`、`ConversationRevision`/`EntryIdGenerator`、`LiveSessionState` User/Assistant/Tool/Interaction reducer、complete Tool exchange、coherent capture与Compaction stable units/source/replacement subset。Fast/MSRV运行的120项library tests、Clippy、docs/fixtures检查与3项heavy recipes均通过，最终four-way review无blocker。

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
11. **Coding-agent主路径优先**：行为里程碑只实现coding-agent纵向闭环直接需要的产品能力。非coding-agent的独立主体、persona、编排或通用工作流不做专门实现；只有当共享Agent/Session语义、durable format或当前coding-agent流程明确依赖时，才实现对应的共同基础。
12. **只建立当前可消费的最小seam**：文档中的representative interface用于说明owner边界，不等于必须提前创建所有production API。过渡slice只实现当前行为或下一紧邻slice确实消费的最小concrete code；不得为了测试或未来灵活性固化standalone production API、receipt/token、generic source/adapter/transaction abstraction或成套dead-code路径。确定性注入优先保持`#[cfg(test)]`私有，并在真实production consumer出现时再提升为production seam。

## 依赖图

```text
M0 Documentation Convergence + Quality Baseline
└─ M1 Wire Limits + Bounded Codec + Owner Semantic Spine
   ├─ M2 Incremental Public Protocol DTOs
   ├─ M3 Conversation Line Codec + Physical Scanner
   ├─ M4 LiveConversation Reducer
   └─ M6 Minimal Turn Resources + Scripted Gateway

M3 + M4 + M5.0 Durable Entity/Async Foundations implementation
└─ M5.1 Recorder + M5.2 Semantic Replay

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

状态：Completed。

已建立current文档入口、ADR/review/research索引、V2 archive与默认搜索隔离；fully superseded ADR和closed reviews已归档并保留redirect stubs。Fast、MSRV 1.85、heavy boundary三套gate与双toolchain CI已经可执行。

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

状态：Completed。

M1.1–M1.5已按owner拆分提交；最终review确认无correctness/security/spec blocker。`./scripts/check.sh`、`./scripts/check-msrv.sh`和`./scripts/check-heavy.sh`全部通过。M1只建立Wire foundation与pure semantic values；`LiveCompactionSourceView`/`LiveCompactionUnit`因依赖尚未实现的`ConversationRevision`与canonical `ModelMessage`，按本计划保留到M4，不以placeholder或shadow DTO提前实现。

### M1.1 ProtocolLimits与bounded counters

先实现后续所有parser/codec共同消费的唯一limits：

- 完整`ProtocolLimits` tree及exact v1 constants；
- byte/depth/node/member/item/string/number的checked counter；
- bounded string/list/map helper，不把limit magic number复制到各wrapper；
- protocol negotiation exact pair与capability intersection；
- boundary recipe runner的small/heavy分类基础与validator-selector registry。M1.1只冻结每个leaf的exact value、selector和generic boundary floor；真实owner在对应milestone落地时必须消费该field并执行owner-level boundary/+1，完整`protocol-limit-cases.json`关闭属于M11。

退出条件：limits tree与selector registry完整匹配`protocol-limit-cases.json`，negotiation vectors通过；现有Workspace owner和后续BoundedJson/Schema只通过typed limit view取值，不保留重复magic number。

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

当前进度：Protocol V1 bootstrap已通过exported byte-level router完成Hello decode、runtime capability intersection、Welcome/Reject生成与selected codec建立；public manifest已增加immutable owning slice和`active | pending`状态，Rust conformance runner只允许active target经exported production seam执行。`IncrementalRuntimeProtocolV1`为四个transport entry提供不含generic JSON envelope的typed root router；当前已激活Runtime reload、Session Create/Load/Unload、Turn Submit/Cancel、TurnStarted/CommandOutput/typed Rejected completion、capabilities query/response、Runtime/Session SnapshotRequest、SubscriptionRequest、RuntimeDispatchError，以及M7 minimal loaded-ready-idle SessionSnapshot、Runtime command-catalog invalidation、Turn completed/failed StateEvent。Submit复用Prompt owner values并消费selected effective text/skill limits；Create复用Workspace/Prompt/Model/Runtime Interface owner values，保留host-neutral `CanonicalFileUri`并双向消费selected Workspace/text limits；Snapshot/Event使用无AST discriminator先选择variant-specific effective frame cap，再执行duplicate-aware bounded decode；CommandError强制canonical code/retry machine contract并对message/output执行selected limits。selected V1中尚属pending slice的known target返回独立`PendingPublicTarget`，不能伪报为unknown variant。Starting/active/approval snapshots、TurnInterrupted、Progress与Closed仍随owning slice保持pending。

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
- Create/Update Workspace public input使用Workspace-owned `WorkspaceDefinitionInput`与`WorkspaceRootInput { path: CanonicalFileUri }`；M2只关闭host-neutral codec，checked URI→`WorkspaceRootSpec { path: PathBuf }` lowering属于M7 command application（ADR 0135）；
- Command accepted failure不能混入outer dispatch error；
- Snapshot不得因size临时截断actionable state；超限按owner合同fail closed；
- Interaction request/resolution不得泄露private Tool args、secret或resolution key；
- Debug/diagnostic覆盖redaction测试。

M2初始退出条件：M7所需public路径有真实codec与route skeleton，不能只通过private API测试ordinary slice。完整manifest closure属于M11退出条件。

建议按行为family拆成多个提交；M11最后提交：`test: enforce public wire v1 manifest`。

## M3 · Conversation Format与Bounded Scanner

### M3.1 Exact Header/Entry codec

状态：Completed。已实现strict Header、六种flat body的exact per-line codec、bounded duplicate-aware preflight与raw ToolCall `arguments` cap，并在owner/writer invariants下完成全部conversation golden的byte-exact round-trip。

实现：

- strict `SessionHeader`；
- required `entryId/parentId/sessionId/turnId/timestamp/body`；
- User、Assistant、Tool、InteractionRequested、InteractionResolved、Compaction六种flat body；
- exact field/tag/null order；
- Tool outcome truth matrix与Compaction model-call projection；
- Header/session/catalog identity validation。
- bounded duplicate-aware preflight与raw ToolCall `arguments` cap；
- owner/writer invariants。

退出条件：全部conversation golden complete line已通过byte-exact round-trip；跨行selected path、sanitized messages和relation assertions及corruption sidecars仍留给M5 semantic replay。

### M3.2 Physical scanner

状态：Completed。已实现bounded streaming scanner：对known size和stat unavailable input均执行1 GiB cap，支持LF/CRLF、strict Header、line/count limits、complete-entry fault recovery，并仅在DurableState root-lease-derived writable proof下返回final partial-tail truncation action/offset；scanner本身不修改文件，heavy recipes已覆盖。

实现：

- whole-file 1 GiB cap优先；
- Header 64 KiB、entry line 1 MiB；
- complete entry count 1,000,000合法，第1,000,001个失败；
- LF/CRLF处理；
- malformed/unknown/oversized complete line可skip并继续；
- 仅final unterminated tail在DurableState root-lease-derived writable proof下可truncate；
- scanner在读取完整oversized line前保持bounded allocation。

退出条件：scanner层的UTF-8、newline、Header/line/file/count、partial tail和oversized-line recovery assertions通过；semantic corruption `.expected.json` sidecar留给M5。1 GiB测试使用streaming generator并归入heavy test tier。

建议提交：`feat: add conversation jsonl codec`、`feat: add bounded jsonl scanner`。

## M4 · LiveConversation Reducer

状态：Completed。

已完成纯内存conversation protocol与**INV-005 reducer-owned subset**，不包含async execution或full Compaction。Fast/MSRV运行的120项library tests、Clippy、docs/fixtures检查与3项heavy recipes均通过，最终four-way review无blocker。

任务：

- Session-scoped `EntryIdGenerator`与collision guard：`allocate()` typed fallible、16-byte CSPRNG candidate、success前reserve、最多32次collision retry；entropy/exhaustion redacted error不panic，且不改变state/head/revision；replay/Fork reserved IDs均seed guard；
- 在构造任何stable-unit view前，按Prompt canonical owner冻结crate-private opaque exact provider-neutral `ModelMessage`、`ModelAssistantContent`及borrowed refs；Prompt alone construct/destructure private transcript kinds。二者是immutable Arc-backed `Clone` values：clone保持semantic identity/order/provenance，可将同一message投影到stable unit和flattened LiveConversationView，绝不从borrowed message或raw suffix重建。它们和`as_ref()`不是Runtime external API；ProviderAdapter、Compaction estimator/reduction及Prompt assembly/tests等authorized consumers只能inspect `ModelMessageRef`/`ModelAssistantContentRef`。User projection精确为`ModelMessageRef::User { content: &[MessageContent] }`且stamp通过refs不可能访问，Assistant只含ordered Reasoning/Text/ToolCall，Tool只含ToolCallId + ToolResultContent；完整ReasoningContent及portable provider_item_id保留，response ID/index/order/metadata/usage等attempt facts禁止。public `PromptValueError`保持不变；transcript constructors返回crate-private redacted `ModelMessageError { EmptyText | UnsafeText | TextTooLong | EmptyAssistantContent | DuplicateToolCallId }`。rolling summary只可达前三个text reason（含任意CR/CRLF，绝不normalization）；assistant constructor独立覆盖后两个reason。accepted summary text verbatim且无label/envelope/stamp；live reducer只能调用Prompt crate constructors，不得引入Compaction/Storage/Wire shadow DTO；
- User/Assistant/Tool/Interaction typed apply与ItemId、ToolCallId/relation validation；`ConversationRevision::checked_next()`按exact matrix preflight：Input/Steer、每个accepted Assistant（含hidden ToolCalls）、complete-exchange promotion、Compaction Replace各`+1`；partial/abandoned/non-visible settlement、Interaction、progress/usage/recording、failed/idempotent apply均`+0`；overflow先于EntryId allocation/state mutation；
- first truthful ToolResult与duplicate/conflicting terminal处理；incomplete Tool exchange不进入`LiveConversationView`；complete exchange按assistant call order投影，不按completion order；
- stable-unit `LiveCompactionSourceView`与`LiveCompactionUnit`是private-field immutable `Clone` handles；clone共享Arc-backed units/messages并保持origin/kind/order，不重建unit。source fields之外只冻结`has_same_stable_identity(&self, other: &Self) -> bool`：它比较SessionId/revision/unit count/ordered `(first_entry_id, kind)`，绝不比较`ModelMessage`，且不存储或暴露identity DTO。Compaction-owned `PreparedLiveCompactionUnit::for_live_reducer(kind, messages) -> Result<_, CompactionSourceError>`完成all message/kind validation而不需要origin；`bind_origin(self, EntryId) -> LiveCompactionUnit` infallible。new User/ordinary Assistant/rolling summary只在new ID allocation后bind；complete Tool exchange用already-existing Assistant origin在current Tool allocation前bind。source factory仍返回redacted `CompactionSourceError { EmptyUnitMessages | DuplicateUnitOrigin | MisplacedRollingSummary }`，强制nonempty messages、unique origins和leading-only RollingSummary；`LiveSessionState`只在factory caller boundary映射该error到own typed live error，Compaction绝不依赖`LiveConversationError`；reducer负责完整Tool exchange grouping；
- M4 Compaction Replace只接受immutable source、nonzero/in-range cut、opaque `CompactionReplacement` (exact StoredCompaction + prebuilt Prompt rolling summary)和orchestration-supplied `TurnId + Timestamp`。M4 replacement interface只有`#[cfg(test)] for_m4_test(StoredCompaction) -> Result<_, CompactionReplacementError>`，其narrow/redacted唯一reason为`InvalidRollingSummary`；the consuming `into_parts(self) -> (StoredCompaction, ModelMessage)` supplies exact owned values. M4没有production constructor或`ValidatedCompactionSummary` dependency；M10才在those types exist时新增production construction。M4创建fresh current source，调用`source.has_same_stable_identity(&fresh_current_source)`，从source+cut派生marker，并consume replacement to prepare its rolling unit；之后可clone prebuilt immutable rolling summary into the leading unit and flattened LiveConversationView。拒绝pending exchange、cross-session、stale或mismatched source/marker，完成all fallible validation/projection/candidate preparation和`checked_next()`后才分配new rolling-summary origin。allocation后依序infallibly construct exact entry Arc、bind prepared summary origin、commit Replace、只clone fresh current source中的retained units作为exact suffix、append同一Arc到full selected path并install preflighted revision；不从borrowed message或caller replacement suffix重建，不接受raw replacement messages/suffix/marker/StoredCompaction，且不做I/O/provenance validation；
- `LiveSessionState` ordinary typed apply只接收existing valid-by-construction `StoredUserMessage`、`StoredAssistantMessage`或`StoredToolMessage` body加`TurnId + Timestamp`，不定义User/Assistant/Tool candidate types，也不接受prebuilt StoredSessionEntry或caller entry envelope identity；state绑定SessionId、EntryId与parent并返回exact `AppliedConversationFact` Arc。all fallible steps finish before allocation; after allocation it constructs the exact Arc before any state change, binds any prepared new-origin unit, commits state, appends that same Arc to the full selected path, and installs the preflighted revision. It verifies supplied TurnId current/start semantics；Timestamp是owning Session/Turn orchestration提供的typed fact，绝不读ambient clock，Input start之前也不从state导出TurnId/timestamp。Interaction是唯一exception：fields/raw `InteractionState`只属于LiveSessionState；its request/resolution apply methods alone construct/transition it，siblings只读safe facts而不能mutate/match raw state。private request candidate仅`RequestId + ItemId + InteractionRequest`，再传`TurnId + Timestamp`；resolution candidate仅`RequestId + optional host key + opaque ResolvedInteraction`，只再传`Timestamp`。`host(...) -> Result<_, InteractionCandidateError>`只接受ToolApproval/UserAnswer或Cancelled(HostCancelled)并seal Some key；`owner_cancellation(...) -> Result<_, InteractionCandidateError>`只接受Cancelled non-Host并seal None，wrong origin在apply/EntryId allocation前拒绝。reducer从exact stored pending request导出TurnId/Item/family、safe stored request/resolution并保留private live resolution。`capture_conversation_views()`从同一state/revision生成LiveConversationView、source、derived selected head、relations和safe pending facts的一份crate-private aggregate；M4只读head，state保留full path给future LiveSnapshot/Fork。不激活M8 public DTO。每Item最多一个Pending Interaction，terminal resolution后允许顺序later interaction，same-key/same-payload idempotent resolution不分配ID且保持`+0`。

M4明确不实现Compaction planner/token/budget/model call、`Arc<CompactionPlan>`、`Arc<ModelCallRequest>`、summary validation、orchestration/retry、Recorder ordering或publication；M5拥有tolerant recorded-marker ignore/diagnose，M10才完成完整INV-005。

测试：

- property tests覆盖任意Tool completion permutation与revision delta matrix；
- duplicate text但不同EntryId的marker identity；
- incomplete/orphan/abandoned exchange与hidden Assistant/complete promotion的两次basis变化；

- M4 **no-ID contract** uses a deterministic candidate source with one unreserved sentinel `EntryId` `S`. Each table-driven rejection case snapshots selected head, the full selected path (including exact Arc identities), revision, relations, interactions, pending/exchange state and stable units; it must return with that entire snapshot unchanged. After every returned `Err`, disable the scripted failure and execute a known-valid apply: its first allocation must receive `S`. Validation cases assert zero allocation calls; scripted `EntropyUnavailable` and `CollisionAttemptsExhausted` may enter `allocate()` but neither reserves nor advances the sentinel, so the same `S` is returned by the next successful allocation. This is an observable no-reservation contract, not merely a no-visible-entry assertion. The table is the complete M4 returned-apply-error matrix, not a sample of cases:

  | Table-driven returned apply class | Required cases |
  | --- | --- |
  | revision / allocation | `RevisionOverflow`; `EntropyUnavailable`; collision-attempt exhaustion |
  | body / relation / Turn | invalid body-relation combination; invalid current/start `TurnId`; duplicate ToolResult; cross-Turn result; mismatched Item/ToolCall identity; conflicting terminal result |
  | ordinary Prompt projection | every reducer-reachable `ModelMessageError` while projecting User/Assistant/Tool canonical facts; assistant constructor coverage remains separate from replacement construction |
  | Compaction source | prepared/source factory error; stale, cross-session and `has_same_stable_identity()` mismatch |
  | Compaction boundary | out-of-range nonzero cut; derived-marker mismatch; pending Tool exchange |
  | Interaction | second Pending conflict; terminal/same-key conflict; resolution key mismatch; request/resolution family mismatch |

  Zero-cut boundary construction is separately tested before `apply_compaction` is invoked; it proves zero reducer/allocation calls and the unchanged snapshot. Test-only `CompactionReplacement::for_m4_test` covers only rolling-summary's reachable `ModelMessageErrorReason::{EmptyText, UnsafeText, TextTooLong}` and proves its redacted `InvalidRollingSummary` construction failure occurs before `apply_compaction`, preserves the snapshot and leaves `S`; it is not misclassified as a reducer-returned Prompt projection error. Separate assistant constructor tests cover `EmptyAssistantContent` and `DuplicateToolCallId`; they are not impossible replacement-factory cases. `InteractionResolutionCandidate::host`/`owner_cancellation` wrong-origin cases likewise fail before reducer invocation or allocation. Same-key/same-payload Interaction resolution is additionally table-tested as `Idempotent`: it performs zero allocation calls, produces no entry/path append, preserves the whole snapshot and leaves `S` for the next successful recordable apply.
- successful recordable applies reserve exactly one `S`, construct/return/append the same Arc, retain full selected path, and only then use the next candidate; each prepared-unit error is before allocation, while bind-origin and post-allocation commit are infallible. M4 does not invoke or test Recorder ordering;
- stale/cross-session `has_same_stable_identity()`、nonzero/in-range cut和derived-marker/replacement mismatch rejection（不测plan/request staleness）；
- 每Item一个Pending、terminal后顺序Interaction/same-key idempotence，以及同一revision `CapturedConversationViews` aggregate；
- reducer方法同步执行，不持锁或执行I/O。

退出条件：INV-003与仅限reducer-owned的INV-005在纯内存测试中成立；完整INV-005仍为M10。

建议提交：`feat: add live conversation reducer`、`feat: expose compaction stable units`。

## M5 · Durable Foundations、Recording与Replay

状态：M5.0 design gate与当前durable foundation Completed（无 standalone production reservation API/token/receipt）；crate-private loaded Workspace composite publication和Runtime residency lifecycle exclusion已消费该durable seam；三条new-entity路径的Unix process-abort tracer已在macOS本地验证，自动化Linux/macOS/Windows native matrix、remaining Fork anchors/LiveSnapshot及public Runtime command Pending；M5.1 target/proof、owner-tracked SessionRecorder physical append及全部七个 Recorder fixture坐标已完成，M5.2 tolerant semantic replay/corruption sidecars与replay/Recorder-backed Ready+Idle Load hydration也已实现；Load fault-and-replay conformance继续以确定性测试覆盖admitted Load caller cancellation、replay worker spawn rejection/panic/join failure、Recorder initialization degradation、stale Workspace candidate recheck与completed append后的cold replay。

### M5.0 DurableState / async foundation implementation

设计已由[DurableState](modules/durable-state.md)、[Durable Store V1](formats/durable-store-v1.md)、fixtures、ADR 0136和ADR 0137关闭；implementation不得重新打开store shape。当前durable foundation、exact historical definition resolution、loaded Workspace composite publication与Runtime residency exclusion也不代表remaining Fork anchors/LiveSnapshot、public Runtime command、Recorder/replay或cross-platform crash matrix已通过。以下列表是M5.0 implementation series的总退出范围，已完成项继续作为后续slice不可回归的门禁：

- private `DurableStateActor`、immutable catalog snapshots/capabilities、poison/closing state和all mutation/catalog-head serialization；
- permanent CSPRNG-ID reservation (`create_new`，32 definite collision cap)、root `.minicore.lock` fs4 exclusive lease、strict user-private local filesystem validation和no-follow link/reparse/case-alias handling；
- Store V1 create/open/scanner/cleanup, capped enumeration, canonical head/definition encoder/decoder, contiguous immutable generations, CAS recheck/no-op, markerless final-path staging, `DurableCommitBarrier` immediately before COMMITTED, and exact COMMITTED/PUBLISHED payload readback publication; no caller staging/path/generation/marker API;
- exact Agent/Session definition resolution：current revision复用installed Arc且零filesystem I/O，historical revision按immutable revision index只读取一个bounded `definition.json`；owner-tracked read在caller取消后仍完成，已索引definition的缺失、错owner/revision、corrupt bytes或worker panic触发DurableState closing，ordinary read I/O unavailable保持retryable；
- initial Agent/Session and streamed Fork semantic re-encode/publication, publication-time Agent Enabled/current-ref check, opaque conversation target/`RecordedForkConversationLease`/writable proof, and closed publication-certainty/Runtime-close behavior;
- host-only `MiniCoreRuntime::open(config, Handle)` / `shutdown(&self)` and closed redacted initialization errors; initialization owner-tracks/joins a timer probe rather than allowing a missing-driver panic; DurableState/ConversationStorage/Recorder receive only internal `RuntimeTaskContext`; `spawn_blocking_tracked` pre-registers every owner-retained JoinHandle/shared settlement, plus cancellation barriers, clocks and the two real filesystem adapters;
- future manifest dependency/lock update only in this implementation task: Tokio 1.53.1 caret with `default-features = false`, production features only `macros,rt,sync,time`, dev `rt-multi-thread,test-util`; tokio-util 0.7.19 `default-features = false` + `rt`; fs4 0.13.1 sync; no Tokio fs/io-util without consumer; clippy lock lint config/smoke test on Rust 1.85/current;
- consume `docs/fixtures/durable-store-v1/`, including native Linux/macOS/Windows process tests for lock contention/reacquire, create_new, aliases, links/reparse, holder death, cleanup/open-handle and deterministic crash matrix points.

退出条件：Store V1 opens only after strict cleanup; new-entity Create/Fork is actor-owned complete-or-invisible, while an existing-head update reopens as complete old or complete new generation; no detached job remains; **every** Durable Store fixture case with `slice = m5_0 | platform_m5_0` passes. Native macOS/Windows CI remains an implementation exit condition and is not changed by this design pass. This task deliberately does **not** implement Recorder append semantics or tolerant semantic replay.

建议提交：`feat: implement DurableState foundations`，并在同一implementation series中以测试证明deterministic async seams。

### M5.1 SessionRecorder

状态：Completed。全部七个 Durable Store `slice = m5_1` Recorder坐标已由same-named deterministic tests消费；完整cross-platform native matrix仍属于M5.0/platform gate，不由本里程碑宣称完成。

实现：

- [x] DurableState-issued `PublishedConversationTarget`与paired writable proof：在root lease下owner-tracked打开已发布conversation，严格校验initial Header、bounded physical length、regular-file mode与same-open path/handle identity；Recorder只消费opaque target/proof，不取得path；
- [x] open/use M5.0已经发布的valid Header与writable conversation proof；initial Header creation属于M5.0；
- [x] single ordered `record(entry).await`；
- [x] encode完成且size合法后才进行第一次write；
- [x] `write_all`语义与partial/unknown write failure；
- [x] first failure `Healthy → Degraded`，当前load停止后续记录；
- [x] 不retry、不segment、不backfill、不回滚live mutation；
- [x] diagnostic只保留allowlisted code与redacted bounded message；

当前实现已覆盖 canonical append、single in-flight、spawn/panic、caller drop、close drain、owner reap与redaction；确定性 physical-write 前后 barrier证明caller drop-before/after-write、partial tail、complete line after side effect、shutdown settlement与Registry drain，Runtime级测试证明 `MiniCoreRuntime::shutdown()` 先排空 loaded Recorder、再释放 DurableState root lease。`recorder.job.join_panic` 使用真实raw JoinError覆盖operation已发布的provisional success，并由并发 `record`/`close` shared waiters证明exact attempt在raw registration reap前不会提前返回；cold replay随后决定保留完整line或忽略/truncate partial tail。M5.2 tolerant semantic replay与corruption sidecars已独立实现并验证；同一published target现在经owner-tracked replay/tail-truncate/cold-seed/Recorder initialization进入Ready+Idle executor；Load fault-and-replay conformance还覆盖了admitted Load cancellation、replay worker spawn rejection/panic/join failure、degraded Recorder initialization、stale Workspace recheck和append后的cold replay。

退出条件：every Durable Store fixture case with `slice = m5_1` passes，并额外证明tracked-job pre-registration、spawn failure/panic、join panic、caller drop在RecorderWriteBarrier前后、同一时刻至多一个physical job、panic/close reaper复用exact attempt、shutdown join、raw guard不跨await，以及root lease只在所有Recorder jobs后释放。M5.1 does not consume any M5.0 durable case.

### M5.2 Tolerant semantic replay

状态：Semantic replay seam与全部conversation corruption sidecars已实现并通过独立 replay、全 suite、MSRV、heavy、docs与fixture gates；replay/Recorder-backed Ready+Idle Load hydration与M5.1全部Recorder fixture坐标已实现。

实现：

- strict Header后逐行semantic decode；
- session mismatch在EntryId reservation前拒绝；
- duplicate/orphan/invalid relation隔离；
- first valid root + physical-last eligible leaf选择path；
- incomplete Tool exchange排除；
- recorded Compaction marker无法应用时由M5 tolerant ignore并产生bounded diagnostic；
- bounded diagnostic detail、aggregate与truncation summary；
- cold state的`current_turn = None`。

退出条件：全部conversation corruption expected sidecar通过，LiveConversation与cold sanitizer对complete Tool exchange/Compaction规则一致。M5只证明Recorder failure不回滚已传入的live mutation；“不重复Model/Tool外部操作”分别由M7/M8 fault tests证明。

建议提交：`feat: add best effort session recorder`、`feat: replay tolerant conversation history`。

## M6 · Minimal Turn资源与Scripted ModelGateway

### M6.1 Workspace、Prompt与captured empty views

当前进度：Workspace definition resolve与immutable Snapshot foundation Completed，包括owner-tracked local canonicalization、canonical duplicate/overlap/cwd validation、fail-closed restricted authority、exact authority-request binding、Prompt/Skill capture contexts与cross-candidate fail-closed finish。已完成 crate-private Prompt candidate materialization、immutable PromptResourceView/PromptSet profile、固定层级与稳定排序、candidate-only Workspace source capture、Text-only atomic composition、Runtime-owned PromptService与initial PromptResourceView，以及Workspace Prompt candidate capture在Session Load和loaded Idle Workspace原子publication中的接入；capture监听owner/operation closing，capture后重新验证canonical path与authority facts，且Load的final revalidation位于replay/Recorder准备之后、executor安装之前；source unavailable/content rejection、revalidation mismatch或closing均不发布candidate且旧Snapshot不变。由 parent owner 构造的合法 empty SkillView/ToolPromptView也已完成；ordinary text-only AgentRun final assembly已在M6.2完成。具体 filesystem adapter、四模块complete shared-root publication、Skill source capture/contribution async resolve和non-empty Tool/Skill metadata projection仍 Pending。

先实现ordinary Text turn所需最小但真实路径：

- Workspace definition resolve与immutable snapshot；
- Prompt source在publication前materialize；
- PromptService/PromptSet只做同步纯内存normalize/assemble；
- capture合法empty SkillView/ToolSet snapshot，保证TurnExecutionContext shape从第一条vertical slice开始稳定；
- Text Input normalize与safe provenance；
- 完整SkillIntent async load/composition延后到M9，完整Tool execution延后到M8；
- 本切片不宣称 public Runtime Load、TurnExecutionContext capture、ModelGateway request、Snapshot/Event 或 ActiveTurn 行为已经可用。

### M6.2 ModelGateway与ScriptedProviderAdapter

当前进度：Completed（scripted text-only foundation）。`PromptSet::assemble`把固定System sections、ordered User static context和sanitized `LiveConversationView`组装为唯一`AssembledModelContext`，使用pinned `TurnModelSnapshot` estimator/context limit执行final input preflight，proof绑定AgentRun、exact process-local `TurnModelRef`和ConversationRevision；Model catalog resolution、retained `TurnModelSnapshot`、唯一`ModelCallRequest` constructor、single `ProviderAdapter::execute` attempt、provider-neutral progress、typed delivery-aware errors、minimal text terminal validation及cancel/terminal first-wins均已实现。unsafe `AcceptedNoOutput | Unknown | OutputStarted`不会保留包括`RateLimited`在内的retryable reason。Runtime拥有empty `ModelGateway`与initial immutable empty `ModelCatalogView`。reload只影响future snapshot，旧snapshot继续调用旧adapter。Structured output、允许ToolCall的non-empty ToolSpec、CompactionSummary assembly、credential/auth/connection实现、Rig adapter与public Runtime/ActiveTurnTask消费仍 Pending。

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

当前进度：Completed。public `MiniCoreRuntime` facade已驱动Create/Load/Submit/Snapshot/Subscribe/Unload；Submit admission捕获exact Session/Agent/Workspace/Prompt/Model bindings，Input live apply与inline record后返回`TurnStarted`，one ActiveTurnTask完成一次text-only ModelGateway request、final Assistant apply/record和Completed/Failed settlement；subscription保持snapshot-first并携带matching terminal detail，Unload等待已经进入的ordinary Turn完成，cold Load只恢复recorded prefix且`current_turn = None`。端到端与fault tests覆盖successful User/Assistant replay、context overflow时provider零调用、Recorder failure不重复Model request、concurrent Submit Busy及Unload drain。

实现：

- minimal Agent/Session durable definitions与revision CAS；
- Session Create先把host-neutral `WorkspaceDefinitionInput` checked-lower为durable `WorkspaceRootSpec { path: PathBuf }`；unsupported host family返回accepted command的`InvalidArgument + DoNotRetry`且不开始staging；
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
- Unload不恢复task或旧TurnStatus；
- 激活loaded Workspace definition publication时使用owner-registered `SessionDefinitionPublicationTask`，并消费Durable fixture `slice = m7`的post-commit install failure坐标；dispatch waiter drop不得取消task。

端到端测试：

```text
Create → Load → Submit(Text) → Model(final text)
→ Snapshot Completed → Unload → Load
→ recorded User/Assistant restored, current_turn = None
```

退出条件：INV-001、INV-002、INV-101和INV-201在真实async integration test成立；任何guard不跨await；Recorder failure不导致同一Model request被重复调用；全部Durable fixture `slice = m7` case通过。

建议提交：`feat: run ordinary scripted agent turn`。

## M8 · Tools、Interaction与Cancel

当前进度：M8.1最小Scripted Tool exchange、M8.2最小crate-private scripted approval seam与M8.3最小crate-private Cancel seam已完成。`ToolSet`捕获immutable definitions与crate-private executor，按ToolExecutionMode选择并发/串行 round，结果按assistant call order回填；ModelGateway已允许非空ToolPromptView并校验ToolCall名称，ActiveTurnTask已接通ToolCall → ToolResult → 下一次Model → final Assistant，以及Interaction request → snapshot pending → host resolution → truthful ToolResult；Cancel现可路由active Submit/Turn、取消pending Interaction并覆盖cancel-before-input。public SubmitCancelled/TurnInterrupted、ToolStartGate、完整Interaction/control lanes、具体schema/policy/approval、public Tool DTO与其它完整M8 control行为仍后置。

M8首先建立ActiveTurnControl、EmergencyControl、Interaction resolution和Tool settlement所需control lanes；不能等到M9才让Cancel/approval message进入actor。

### M8.1 Complete Tool exchange

- Assistant含A/B/C calls；
- 只有policy compatible、不同canonical file keys且非`Serial`/multi-file/open-world/ask-user的calls允许并行；其余按call order串行；
- 本slice只实现crate-private immutable `ToolSet`、ToolSpec投影、无真实副作用的test-only ScriptedToolExecutor与Session执行接缝；production ToolService、schema/policy/approval、Session-local FileMutationQueue和control lanes留待后续M8 slices；
- ToolSet不写LiveSessionState或Recorder；
- 结果按first truthful settlement apply；
- 每个result完成inline record attempt；
- 全部expected results完成后下一次Model才允许；
- model view按call order输出A/B/C；
- 已验证当前可达的unknown/pre-execution失败、Executed成功和Abandoned结算；完整pre-execution/Executed truth matrix随production ToolService与M8.2/M8.3 control lanes收口。

### M8.2 Interaction

当前 slice 只闭合一个crate-private scripted approval vertical seam：Tool executor发出Interaction后由Session actor live apply并inline record，pending request进入Session snapshot；host按exact request resolution actor apply并inline record，随后恢复Tool waiter并继续下一次Model。UserQuestion、public Interaction DTO、resolution key幂等路由、完整policy/approval与owner control lanes仍待后续slice。

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

当前进度：M9.1 已完成最小 crate-private FollowUp FIFO seam：bounded admission、duplicate command rejection、按 CommandId remove 基础与 terminal handoff；M9.2 已接通 crate-private Steer admission、expected Turn 校验、per-Turn bounded FIFO，以及完整 tool round 后、下一次 Model 前的一条 FIFO safe-point 消费（复用 captured TurnExecutionContext 并按 `UserMessageSource::Steer` apply/record）；M9.3 已接通 actor-local queued-message cancellation，Steer/FollowUp 均按 CommandId remove，重复取消返回 `NotQueued`，且两 lane 拒绝重复 CommandId；M9.4 已闭合 text-only candidate 与迟到 Steer 的 first-wins 仲裁：Steer 胜出时先记录 Intermediate assistant，再记录 Steer 后进入下一次 Model；Final reservation 胜出时关闭该 Turn 的 Steer admission；M9.5 已接通 AgentRun delivery-safe logical retry（同一 request Arc、最多 3 次重试、2/4/8 秒取消感知 backoff、Steer backoff 排队）；M9.6 已补齐 retry 前的 process-local control-generation、current Turn/ConversationRevision 与 executor lifecycle 重验，并以确定性测试覆盖 stale basis 与 close 中断 backoff；M9.7 已补齐 Snapshot 的 active Submit/FollowUp/Steer command-id projection；M9.8 已建立 sticky EmergencyControl 的 target/epoch、Cancel 与 SecurityRevoked first-wins、stale-target rejection、retire 与 cancellation wakeup seam；M9.9 已将 Cancel 接入 active Submit、Starting Turn 与 Running Turn 的 EmergencyControl target，并覆盖取消后的 admission→Turn sticky 迁移；M9.10 已接通 crate-private SecurityRevoked target route，覆盖 active Submit、Starting Turn、Running Turn、sticky first-wins 与取消后不再启动后续 Model attempt；M9.11 已将 logical retry basis 显式绑定当前 EmergencyControl epoch，并覆盖 stale epoch 在 backoff 后阻止下一次 Model attempt；M9.12 已让 retry backoff 直接监听 EmergencyControl wakeup，并将已 signal 的 epoch 判为不可重试。完整 public SecurityRevoked terminal route、public queue DTO/projection、fair admission 与 Skill composition仍后置。

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

M10完成完整INV-005；M4已经提供source/cut/marker/no-I/O reducer subset，M5已经提供recorded-marker tolerant replay。

实现：

- Runtime-global validated settings与Turn snapshot；
- pressure input使用same Turn model estimator/limits；
- source按stable units，Tool exchange不可拆；
- plan只保存source+cut并派生summary prefix、retained suffix和marker；
- Summary Prompt仍经过PromptSet和ModelGateway；
- automatic model-call provenance始终`Some`；
- exact Turn/control、`Arc<CompactionPlan>`与`Arc<ModelCallRequest>` identity、source revision/session的stale plan/result拒绝；
- validated summary、orchestration、at-most-one logical retry、inline recorder ordering与publication；
- M10在`ValidatedCompactionSummary`及its validation-error contract存在时新增production `CompactionReplacement` construction；all fallible source/candidate/projection/prepared-unit checks和`checked_next()`先于Compaction EntryId allocation，随后依序infallibly construct exact entry Arc、bind prepared summary origin、commit Replace、append同一Arc到full selected path并install preflighted revision，再inline record the same Arc marker；
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

## 后续实施顺序

M0与M1已完成；M2 minimal Snapshot/Event已落地，M3.1 exact Conversation Header/Entry per-line codec、M3.2 bounded physical JSONL scanner与M4 LiveConversation reducer已完成。继续按下列顺序执行，不提前进入Session执行：

1. `M5.0` DurableState/async foundations implementation（foundation已完成，完整platform matrix仍pending）；
2. `M5.1` SessionRecorder与全部七个fixture坐标（已完成）；
3. `M5.2` tolerant semantic replay、corruption sidecars及replay/Recorder-backed Ready+Idle Load hydration（已完成）；
4. M6–M10随owning behavior补齐resources、behavioral Runtime slice、non-empty Item/Interaction/queue、Degraded recording、usage/diagnostics、Progress/Closed EventFrame，并逐项激活remaining manifest vectors。

在M2–M6 prerequisites关闭前不得进入M7 ordinary behavior slice；production Provider与Tool/Sandbox继续分别受V4-P1-3和V4-C0-1门禁约束。
