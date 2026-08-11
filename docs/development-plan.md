# MiniCore V2 开发计划

状态：Active；M0–M12已完成并统一review。M11 Fork与Full Runtime/Recovery Conformance已关闭：Session Fork command/storage、durable Agent/Session catalog/Fork provenance query、Runtime Session membership/lifecycle StateEvent、public Agent Create/Enable/Disable/Delete/UpdateDefinition/UpdateMetadata、public Session UpdateMetadata、ordinary Session definition CAS、Agent revision upgrade、Ready-state `ReloadWorkspace`、Workspace/Prompt Unavailable loaded readiness与ReloadWorkspace恢复、Agent readiness fan-out与ModelUnavailable及selected PromptUnavailable load/definition projection、shared-resource reload recovery/fanout与complete shared-root publication、active-Turn graceful Unload（default 30s/≤5min grace config、PrepareForUnload deadline signal与truthful settlement、shutdown broadcast）、public manifest无pending closure、host security Workspace authority invalidation（`MiniCoreRuntime::invalidate_session_workspace_authority` host-only非wire seam、Preparing+re-resolve recovery）及RuntimeDependencyUnavailable loaded readiness与exact read probe/Submit re-arm recovery均已完成；full recovery scenario/fixture closure覆盖loaded WorkspaceUnavailable recovery+real owner restart、Agent disable/enable active Turn/FU、RuntimeDependencyUnavailable真实historical storage fault+probe/rearm+retained FollowUp、host security Preparing/active Turn duplicate recovery、ReloadSharedResources public outcome/event、graceful Unload pre-Input SessionNotLoaded与PrepareForUnload fixture。最终`./scripts/check.sh`通过748个library tests（3 ignored）、主crate 160个integration tests及standalone provider-gate 25个tests；`./scripts/check-msrv.sh`用真实Rust 1.85运行主crate全部targets（748个library tests、3 ignored、160个integration tests）。两者均通过Clippy/format及其适用门禁，stable gate另通过current/archive docs、Wire V1 144 active/0 pending与Durable Store fixtures。crate-private `ToolOperationSlot`完整生命周期已实现（exact `ToolExecutionRequest` identity（ItemId + same `Arc<ToolCall>`）、per-slot first-wins gates、EmergencyControl owner mutex内exact unsignaled reserve + lock-free CAS、move-only `ToolStartPermit`→`ToolStartedExecution` proof、`run_started_execution`复验exact capture后调用move-only `ToolExecutionStart` factory、executor future只有proof后poll、signal/stale先赢→不调用factory且matching PreExecution Cancelled ToolResult apply+inline record后Turn Interrupted且不发起下一次Model、reservation/start先赢→Running持有`ToolCancellationHandle`、signal只触发cancellation observer且slot经Settling继续await same run后truthful settle（started run不因signal drop）、pre-start plans为typed `ToolExecutionPlan::{Execute, Approval, UserQuestion, PreExecution}`四路（旧generic Interaction plan删除）、serial/parallel保持call_index order、parent-owned join_all与per-boundary panic isolation→Abandoned）；crate-private scripted approval/UserQuestion控制正确性seam亦已完成：typed `ToolExecutionPlan::{Approval, UserQuestion}`拆分、Session-private concrete `ToolExecutionControl`复用既有Interaction actor/wire/storage owner（无public trait冻结）、Tools-owned move-only/redacted `UserQuestionAnswerBinding`（仅truthful PreExecution+Succeeded为answer、malformed/panic fail closed）、Emergency observation携带opaque owner identity且presentation/resolution/binding/unstarted-settlement move-only permit绑定owner+target/epoch+同一`ToolExecutionRequest`（ToolStartGate独立、Submit→Turn signal迁移原子、signal/close first-wins）、UserQuestion按typed plan shape hoisted到全部ordinary sibling之前（call_index串行、至多一个pending、不涉及ToolStartGate/mutation ticket、每个question outcome先apply+inline record再继续）、signal-first（Cancel/SecurityRevoked/Unload）跳过binding并settle全部unstarted calls为matching PreExecution Cancelled、abandoned question对remaining无副作用（known preflight保留、其余unstarted为PreExecution Failed）。M12/V4-P1-3已由ADR 0138/0139、OpenAI Responses/Anthropic Messages真实Rig standalone loopback evidence、terminal/metadata seam、26-case delivery/error fixture与真实Rust 1.85冷编译关闭；Rig被拒绝进入production baseline，M14改为两个direct provider adapters。Structured output foundation已实现，而public activation与provider-native schema mapping、具体Skill composition/source、Session-local mutation queue/mutation permit attachment to Settling、production ask-user builtin ToolName/schema与answer→model-visible ToolResult text/render格式、schema/hooks/policy/Sandbox与完整Tool policy/approval enforcement、production ToolService/executor/adapters（返回前须提供有界、可确认cleanup）、public Tool DTO、concrete source discovery、production OpenAI Responses/Anthropic Messages direct adapters仍pending；完整platform matrix acceptance已通过（全部七个`platform_m5_0`坐标均有对应的production行为与测试覆盖；GitHub Actions run 31433810296四个job全部通过：Ubuntu Rust stable、Ubuntu Rust 1.85.0、cargo test macos-latest、cargo test windows-latest），本地`./scripts/check.sh`与`./scripts/check-msrv.sh`亦在current HEAD通过：stable为748个library tests（3 ignored）+主crate 160个integration tests+provider-gate 25个tests，MSRV为真实Rust 1.85下主crate748个library tests（3 ignored）+160个integration tests，并通过Clippy、format、docs与全部fixtures。

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

production direct ProviderAdapter和OS/network/process Tool不属于上述internal milestone。V4-P1-3已由M12关闭，production Provider实现仍按M14交付；OS/network/process Tool继续受M13/V4-C0-1门禁约束。

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
- M11 catalog/query/event、manifest closure与full recovery conformance；
- production provider与sandbox adapter。

M4已完成Prompt-owned opaque `ModelMessage`、`ConversationRevision`/`EntryIdGenerator`、`LiveSessionState` User/Assistant/Tool/Interaction reducer、complete Tool exchange、coherent capture与Compaction stable units/source/replacement subset。Fast/MSRV运行的120项library tests、Clippy、docs/fixtures检查与3项heavy recipes均通过，最终four-way review无blocker。

## 实施原则

1. **保持单crate起步**：先用Rust module表达canonical owner，不在行为闭环前拆workspace或多个crate。只有编译边界、依赖隔离或独立发布出现真实需求时再拆分。
2. **fixture是规范资产**：Rust runner直接消费`docs/fixtures/wire-v1/`，不得复制一份测试schema或在测试中重新发明expected结果。
3. **bounded decode先于Serde便利性**：public/storage入口必须从bounded bytes/line scanner进入。不得把普通`serde_json::from_reader`、`Box<RawValue>`或raw `serde_json::Value`当作安全边界。
4. **canonical encoder自有数字语义**：dynamic JSON number必须按Wire算法直接编码，不能依赖第三方JSON number重新格式化。
5. **TDD与小提交**：每个下列task先建立失败测试，再实现最少代码；每个逻辑task单独commit，不把多个里程碑压成一个大提交。
6. **单owner并发**：每个loaded Session只有一个control actor和最多一个ActiveTurnTask；任何live-state guard不得跨`await`。
7. **先Scripted、后production**：先证明MiniCore自己的状态机、recording和协议，再接production provider或真实Sandbox，避免SDK/transport类型反向塑造domain seam。
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

当前进度：Protocol V1 bootstrap已通过exported byte-level router完成Hello decode、runtime capability intersection、Welcome/Reject生成与selected codec建立；public manifest已增加immutable owning slice和`active | pending`状态，Rust conformance runner只允许active target经exported production seam执行。`IncrementalRuntimeProtocolV1`为四个transport entry提供不含generic JSON envelope的typed root router；M7–M11 owning slices现已激活全部manifest target，包括Starting/active/approval snapshots、ResolveInteraction、SessionDefinitionUpdated、Progress与Closed，manifest无pending。Submit复用Prompt owner values并消费selected effective text/skill limits；Create复用Workspace/Prompt/Model/Runtime Interface owner values，保留host-neutral `CanonicalFileUri`并双向消费selected Workspace/text limits；Snapshot/Event使用无AST discriminator先选择variant-specific effective frame cap，再执行duplicate-aware bounded decode；CommandError强制canonical code/retry machine contract并对message/output执行selected limits。未由manifest列出的known future family仍返回独立`PendingPublicTarget`，不能伪报为unknown variant。

M9 当前补充：running/approval Session Snapshot fixture 已从 pending owning slice 激活，typed codec 与 production Runtime seam 均验证 current Turn、active Items、Pending Interaction。

任务顺序：

1. bootstrap、outer request/response/frame envelope、dispatch/query/snapshot/subscribe route skeleton；
2. 为M7实现Create/Load/Submit/Cancel/Unload、CommandCompletion/Error、minimal SessionSnapshot与StateEvent；
3. M8同步扩展Tool/Interaction Item和resolution DTO；
4. M9同步扩展actionable queues、Steer/FollowUp和retry projection；
5. M10同步扩展Compaction；Session usage、recording health与diagnostic projection已先随观察面接通；
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

状态：M5.0 design gate与当前durable foundation Completed（无 standalone production reservation API/token/receipt）；crate-private loaded Workspace composite publication和Runtime residency lifecycle exclusion已消费该durable seam；三条new-entity路径的process-abort tracer已解除Unix限制并在macOS本地验证，cross-process root lease测试已覆盖`lock_contention`、`lock_reacquire`与`lock_holder_death`，CI已新增macOS/Windows native jobs；`case_alias_rejected`与`symlink_reparse_rejected`现由跨平台public-open测试及Windows reparse attribute检查关闭；`root_lease_identity_loss`由RootLease用safe `same_file::Handle`直接拥有已加锁`File`并在actor统一request入口fail-close关闭；`cleanup_open_handle`亦已由Windows测试关闭（持有G1 `COMMITTED`、`FILE_SHARE_READ|FILE_SHARE_WRITE`且排除`FILE_SHARE_DELETE`，open返回`StorageUnavailable`，drop后cleanup/reopen成功），全部七个`platform_m5_0`坐标均有对应的production行为与测试覆盖；本地`./scripts/check.sh`与Rust 1.85门禁均已通过，统一native matrix acceptance已通过（GitHub Actions run 31433810296四个job全部通过：Ubuntu Rust stable、Ubuntu Rust 1.85.0、cargo test macos-latest、cargo test windows-latest）；M5.1 target/proof、owner-tracked SessionRecorder physical append及全部七个 Recorder fixture坐标已完成，M5.2 tolerant semantic replay/corruption sidecars与replay/Recorder-backed Ready+Idle Load hydration也已实现；Load fault-and-replay conformance继续以确定性测试覆盖admitted Load caller cancellation、replay worker spawn rejection/panic/join failure、Recorder initialization degradation、stale Workspace candidate recheck与completed append后的cold replay。

### M5.0 DurableState / async foundation implementation

设计已由[DurableState](modules/durable-state.md)、[Durable Store V1](formats/durable-store-v1.md)、fixtures、ADR 0136和ADR 0137关闭；implementation不得重新打开store shape。当前durable foundation、exact historical definition resolution、loaded Workspace composite publication、Runtime residency exclusion与Session Fork command/storage completion也不代表M11 remaining catalog/query/event/full recovery或cross-platform crash matrix已通过。以下列表是M5.0 implementation series的总退出范围，已完成项继续作为后续slice不可回归的门禁：

- private `DurableStateActor`、immutable catalog snapshots/capabilities、poison/closing state和all mutation/catalog-head serialization；
- permanent CSPRNG-ID reservation (`create_new`，32 definite collision cap)、root `.minicore.lock` fs4 exclusive lease、strict user-private local filesystem validation和no-follow link/reparse/case-alias handling；
- Store V1 create/open/scanner/cleanup, capped enumeration, canonical head/definition encoder/decoder, contiguous immutable generations, CAS recheck/no-op, markerless final-path staging, `DurableCommitBarrier` immediately before COMMITTED, and exact COMMITTED/PUBLISHED payload readback publication; no caller staging/path/generation/marker API;
- exact Agent/Session definition resolution：current revision复用installed Arc且零filesystem I/O，historical revision按immutable revision index只读取一个bounded `definition.json`；owner-tracked read在caller取消后仍完成，已索引definition的缺失、错owner/revision、corrupt bytes或worker panic触发DurableState closing，ordinary read I/O unavailable保持retryable；
- initial Agent/Session and streamed Fork semantic re-encode/publication, publication-time Agent Enabled/current-ref check, opaque conversation target/`RecordedForkConversationLease`/writable proof, and closed publication-certainty/Runtime-close behavior;
- host-only `MiniCoreRuntime::open(config, Handle)` / `shutdown(&self)` and closed redacted initialization errors; initialization owner-tracks/joins a timer probe rather than allowing a missing-driver panic; DurableState/ConversationStorage/Recorder receive only internal `RuntimeTaskContext`; `spawn_blocking_tracked` pre-registers every owner-retained JoinHandle/shared settlement, plus cancellation barriers, clocks and the two real filesystem adapters;
- future manifest dependency/lock update only in this implementation task: Tokio 1.53.1 caret with `default-features = false`, production features only `macros,rt,sync,time`, dev `rt-multi-thread,test-util`; tokio-util 0.7.19 `default-features = false` + `rt`; fs4 0.13.1 sync；`same-file = 1.0.6`让RootLease安全拥有并比较已加锁handle identity，`file-id = 0.2.3`为deferred cleanup提供handle-free跨平台identity值；no Tokio fs/io-util without consumer; clippy lock lint config/smoke test on Rust 1.85/current;
- consume `docs/fixtures/durable-store-v1/`, including native Linux/macOS/Windows process tests for lock contention/reacquire, create_new, aliases, links/reparse, holder death, cleanup/open-handle and deterministic crash matrix points.

退出条件：Store V1 opens only after strict cleanup; new-entity Create/Fork is actor-owned complete-or-invisible, while an existing-head update reopens as complete old or complete new generation; no detached job remains; **every** Durable Store fixture case with `slice = m5_0 | platform_m5_0` passes. Native macOS/Windows CI jobs and the cross-process lease/process-abort slice are implemented；case-alias/reparse rejection, same-file root identity loss and Windows cleanup open-handle coverage are all implemented (the cleanup test holds G1 `COMMITTED` with `FILE_SHARE_READ|FILE_SHARE_WRITE` and no `FILE_SHARE_DELETE`, so open returns `StorageUnavailable` until the handle drops). All seven `platform_m5_0` coordinates now have production behavior and test coverage；unified native matrix acceptance passed final CI (GitHub Actions run 31433810296: Ubuntu Rust stable, Ubuntu Rust 1.85.0, cargo test macos-latest, and cargo test windows-latest jobs all green). This task deliberately does **not** implement Recorder append semantics or tolerant semantic replay.

建议提交：`feat: implement DurableState foundations`，并在同一implementation series中以测试证明deterministic async seams。

### M5.1 SessionRecorder

状态：Completed。全部七个 Durable Store `slice = m5_1` Recorder坐标已由same-named deterministic tests消费；完整cross-platform native matrix仍属于M5.0/platform gate，不由本里程碑宣称完成。

实现：

- [x] DurableState-issued `PublishedConversationTarget`与paired writable proof：在root lease下owner-tracked打开已发布conversation，严格校验initial Header、bounded physical length、regular-file mode，以及append handle、truncation handle与current path的same-file identity；Recorder只消费opaque target/proof，不取得path；
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

当前进度：Completed（scripted foundation）。`PromptSet::assemble`把固定System sections、ordered User static context和sanitized `LiveConversationView`组装为唯一`AssembledModelContext`，使用pinned `TurnModelSnapshot` estimator/context limit执行final input preflight，proof绑定AgentRun、exact process-local `TurnModelRef`和ConversationRevision；Model catalog resolution、retained `TurnModelSnapshot`、唯一`ModelCallRequest` constructor、single `ProviderAdapter::execute` attempt、provider-neutral progress、typed delivery-aware errors、minimal text terminal validation及cancel/terminal first-wins均已实现。M8.1增加最小ToolCall，M10增加CompactionSummary purpose、budget proof、explicit max-output request validation与ActiveTurnTask orchestration。unsafe `AcceptedNoOutput | Unknown | OutputStarted`不会保留包括`RateLimited`在内的retryable reason。Runtime拥有empty `ModelGateway`与initial immutable empty `ModelCatalogView`。reload只影响future snapshot，旧snapshot继续调用旧adapter。crate-private Structured output foundation已实现（`OutputContract::Structured` exact-model contract、schema v1 subset、terminal本地schema validation与ScriptedProviderAdapter conformance），public structured activation与provider-native schema mapping、credential/auth/connection实现与OpenAI Responses/Anthropic Messages direct production adapters仍Pending。

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

当前进度：M8.1最小Scripted Tool exchange、M8.2最小crate-private scripted approval seam与M8.3最小crate-private Cancel seam已完成。`ToolSet`捕获immutable definitions与crate-private executor，按ToolExecutionMode选择并发/串行 round，结果按assistant call order回填；ModelGateway已允许非空ToolPromptView并校验ToolCall名称，ActiveTurnTask已接通ToolCall → ToolResult → 下一次Model → final Assistant，以及Interaction request → snapshot pending → host resolution → truthful ToolResult；Cancel现可路由active Submit/Turn、取消pending Interaction并覆盖cancel-before-input。M9已补齐public SubmitCancelled/TurnInterrupted；crate-private `ToolOperationSlot`完整生命周期已实现（M8.3的first-wins gate升级为slot-owned per-request gate + typed started proof + Running cancellation pair + Settling/Terminal truthful settle），crate-private scripted approval/UserQuestion控制正确性seam已完成（typed `ToolExecutionPlan::{Approval, UserQuestion}`拆分（旧generic Interaction plan删除）、Session-private concrete `ToolExecutionControl`复用既有Interaction actor/wire/storage owner（无public trait冻结）、Tools-owned move-only `UserQuestionAnswerBinding`（仅truthful PreExecution+Succeeded为answer、malformed/panic fail closed）、UserQuestion hoisted到全部ordinary sibling之前（call_index串行、至多一个pending、不涉及ToolStartGate/mutation ticket、每个question outcome先apply+inline record再继续）、signal-first跳过binding并settle全部unstarted calls为matching PreExecution Cancelled），Session-local mutation queue/mutation permit attachment to Settling、production ask-user builtin ToolName/schema与answer→model-visible ToolResult text/render格式、具体schema/hooks/policy/approval enforcement、public Tool DTO与其它完整M8 control行为仍后置。

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
- 当前start/preexec truth已闭合：pre-start plan（unknown/schema invalid/approval deny/cancel-before-start）产生matching PreExecution ToolResult且不reserve gate、不poll executor，reservation/start先赢后只有exact Executed/Abandoned truthful settle，panic经parent-owned join_all与per-boundary isolation恰好映射Abandoned；完整pre-execution/Executed truth matrix的full policy paths（schema/hooks/policy/Sandbox/mutation queue与production ToolService）仍pending。

### M8.2 Interaction

当前 slice 只闭合一个crate-private scripted approval vertical seam：Tool executor发出Interaction后由Session actor live apply并inline record，pending request进入Session snapshot；host按exact request resolution actor apply并inline record，随后恢复Tool waiter并继续下一次Model。UserQuestion控制seam已由M8.3后的typed plan slice闭合（move-only answer binding与hoisted exclusive scheduling）；public Interaction DTO、resolution key幂等路由与完整policy/approval enforcement仍待后续slice。

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

当前实现状态：M8.3的gate foundation已升级为完整`ToolOperationSlot`生命周期——assistant ToolCall live apply后、record await前为每个exact `ToolExecutionRequest`（ItemId + same `Arc<ToolCall>`）构造`Prepared` slot（绑定exact request + round EmergencyControl handle/observation），每个request对应slot自己的`ToolStartGate` lock-free atomic slot（Prepared → Reserved → Started | Cancelled），reservation在EmergencyControl owner mutex内对exact unsignaled target/epoch执行lock-free CAS（与`signal`在同一mutex线性化，first-wins），move-only `ToolStartPermit`经Reserved→Started产生typed `ToolStartedExecution` proof，`ToolSet::run_started_execution`复验exact capture后调用move-only `ToolExecutionStart` factory构造future，executor future只在proof后poll；signal/stale先赢→不调用factory、matching PreExecution Cancelled ToolResult apply+inline record后Turn Interrupted且不发起下一次Model；reservation/start先赢→slot进入Running（持有operation自己的`ToolCancellationHandle`）并继续await same run，signal只触发cancellation observer、slot经Settling等待executor cooperative cleanup/result后truthful settle（started run不因signal drop），exact Executed/Abandoned；pre-start plan为typed `ToolExecutionPlan::{Execute, Approval{request, allowed, denied}, UserQuestion{request, answer: UserQuestionAnswerBinding}, PreExecution}`四路（旧generic Interaction plan删除），Approval在gate前publish、allow后same gate revalidate、deny/cancel不reserve且approval tool name exact-bound，UserQuestion绝不reserve gate/构造factory并hoisted到全部ordinary sibling之前（call_index串行、至多一个pending、每个question outcome先apply+inline record再继续、等待期间`WaitingForUserInput`、ordinary sibling不factory/poll），signal-first（Cancel/SecurityRevoked/Unload）跳过binding并settle全部unstarted calls为matching PreExecution Cancelled，abandoned question对remaining无副作用（known preflight保留、其余unstarted为PreExecution Failed）；serial/parallel保持call_index order；parent-owned join_all与per-boundary panic isolation使panic恰好映射Abandoned。production ToolService/executor/adapter teardown与Session-local mutation queue/mutation permit attachment to Settling仍pending（production adapter返回前必须提供有界、可确认cleanup）。

退出条件：ordinary Tool round-trip、serial/parallel scheduling matrix、ask-user、approval deny、cancel-before-start、cancel-running-tool和recording-degraded场景全部端到端通过；Tool result recording failure不得重复Tool side effect。

建议拆分提交：Tool exchange、Interaction、Cancel各一个。

## M9 · Steer、FollowUp与Logical Retry

当前补充：M9.21 之后已将首次 Finishing transition 接通 `session_execution_changed` StateEvent；current Turn、active Item 与 Pending Interaction 以最小安全摘要接通 Session Snapshot 和 Wire V1，并激活 running/approval/Finishing fixtures；fair admission 与 Skill composition仍后置。

当前进度：M9.1 已完成最小 crate-private FollowUp FIFO seam：bounded admission、duplicate command rejection、按 CommandId remove 基础与 terminal handoff；M9.2 已接通 crate-private Steer admission、expected Turn 校验、per-Turn bounded FIFO，以及完整 tool round 后、下一次 Model 前的一条 FIFO safe-point 消费（复用 captured TurnExecutionContext 并按 `UserMessageSource::Steer` apply/record）；M9.3 已接通 actor-local queued-message cancellation，Steer/FollowUp 均按 CommandId remove，重复取消返回 `NotQueued`，且两 lane 拒绝重复 CommandId；M9.4 已闭合 text-only candidate 与迟到 Steer 的 first-wins 仲裁：Steer 胜出时先记录 Intermediate assistant，再记录 Steer 后进入下一次 Model；Final reservation 胜出时关闭该 Turn 的 Steer admission；M9.5 已接通 AgentRun delivery-safe logical retry（同一 request Arc、最多 3 次重试、2/4/8 秒取消感知 backoff、Steer backoff 排队）；M9.6 已补齐 retry 前的 process-local control-generation、current Turn/ConversationRevision 与 executor lifecycle 重验，并以确定性测试覆盖 stale basis 与 close 中断 backoff；M9.7 已补齐 Snapshot 的 active Submit/FollowUp/Steer command-id projection；M9.8 已建立 sticky EmergencyControl 的 target/epoch、Cancel 与 SecurityRevoked first-wins、stale-target rejection、retire 与 cancellation wakeup seam；M9.9 已将 Cancel 接入 active Submit、Starting Turn 与 Running Turn 的 EmergencyControl target，并覆盖取消后的 admission→Turn sticky 迁移；M9.10 已接通 crate-private SecurityRevoked target route，覆盖 active Submit、Starting Turn、Running Turn、sticky first-wins 与取消后不再启动后续 Model attempt；M9.11 已将 logical retry basis 显式绑定当前 EmergencyControl epoch，并覆盖 stale epoch 在 backoff 后阻止下一次 Model attempt；M9.12 已让 retry backoff 直接监听 EmergencyControl wakeup，并将已 signal 的 epoch 判为不可重试；M9.13 已在 Tool round 启动前加入 EmergencyControl unsignaled/current safe-point，signal 或 stale epoch 时丢弃未执行的 tool exchange；该safe-point现已升级为per-call round-local ToolStartGate matching settlement——signal/stale先赢对每个exact request生成matching PreExecution Cancelled ToolResult并apply+inline record，随后Turn Interrupted且不发起下一次Model；M9.14 已将 Steer 仲裁、resolve await、live apply 与下一次 Model 前的 basis 绑定同一 EmergencyControl target+epoch，signal/stale 时丢弃迟到 candidate 或 Steer；M9.15 已让 Running Tool Interaction 在 SecurityRevoked 下由 actor 以对应 owner reason settle pending resolution，记录 cancelled PreExecution ToolResult 并阻止后续 Model attempt；M9.16 已让 close/Unload 在 pending Interaction 时先取消 active Turn 的后续推进，以 `SessionUnloaded` settle pending resolution，完成 truthful Tool settlement 后不再发起下一次 Model；M9.17 已将 Steer、FollowUp 与 CancelQueuedMessage 接入 public Runtime command/wire route，返回 typed queued/cancelled outcome，并完整映射 lane full、target stale、conflict 与 not-queued error；M9.18 已将 internal queue projection 投影为 public `SessionQueueView`，在 Starting/Running/Finishing Snapshot 中暴露完整 lane-local CommandId/expected Turn target（不暴露 intent 正文），并激活 Wire V1 starting queue vector；M9.19 已将 Starting 阶段 user Cancel 映射为 `SubmitCancelled` command completion 并激活 Wire V1 response vector；M9.20 已将 Running Turn 的 user Cancel 与 SecurityRevoked terminal 映射为 public `TurnInterrupted` StateEvent 并激活 Wire V1 vector；M9.21 已将首次 Finishing transition 映射为 public `session_execution_changed` StateEvent 并激活 Wire V1 vector。current Turn/active Item/Pending Interaction public projection、fair admission 与 Skill composition仍后置。

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

当前进度：Completed。`MiniCoreRuntimeConfig`持有并在open时验证Runtime-global `CompactionSettings`，Turn admission捕获immutable snapshot；PromptSet投影同一Turn estimator下的AgentRun/CompactionSummary fixed assembly basis，TurnModelSnapshot投影exact model basis。`Compaction::pressure()`闭合proactive/hard overflow、unknown/disabled/empty/count-exhausted分类；`plan()`使用checked `u64`按stable-unit从旧到新选择first feasible prefix，求交summary/model/context budget并同时验证post-Replace headroom与minimum reclaim，只保存source+nonzero cut并派生summary prefix、exact suffix及marker。超过16 KiB的大ToolResult在summary source中按V1格式确定性保留最多4 KiB head与4 KiB tail，live/durable source不改写。PromptSet组装required-only System、source+directive、empty ToolSpec与`NoToolCalls`，proof和ModelCallRequest exact绑定budget/revision/model；`validate_summary()`接受portable single-Text结果、忽略optional Reasoning、保存完整automatic provenance并生成sealed production replacement。ActiveTurnTask已接通proactive/Prompt/provider overflow、至多一次logical retry、exact control/plan/request arbitration、live Replace、same-Arc inline record、post-Compaction Steer safe point、`Compacting` phase及Snapshot usage/recording refresh。

planning foundation已完成统一Standards/Spec review且无blocker；`./scripts/check.sh`全量通过，其中library tests为616 passed、3 ignored，集成测试、Clippy、format及current/archive/fixture检查均通过。

CompactionSummary request/validation与production replacement切片亦完成统一Standards/Spec review且无blocker；`./scripts/check.sh`全量通过，其中library tests为620 passed、3 ignored，集成测试、Clippy、format及current/archive/fixture检查均通过。

ActiveTurnTask orchestration与deterministic ToolResult reduction切片已完成统一Standards/Spec review。review发现并关闭两项问题：Compaction logical call chain的公开phase原先仍显示`Sampling`，现由actor-owned phase投影在调用期间发布`Compacting`并在下一次AgentRun前恢复；UTF-8截断点落在多字节字符内时原实现可能跨part继续填充head/tail，现于当前part边界停止。最终`./scripts/check.sh`全量通过，其中library tests为631 passed、3 ignored，集成测试、Clippy、format及current/archive/fixture检查均通过；M10无剩余blocker。

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

当前进度：Session Fork command/storage纵向切片已完成。public `SessionCommand::Fork`与`SessionForked` outcome覆盖Genesis及Before/After User/Final Agent message anchors；source Session在与Load/Unload共用的FIFO gate内选择loaded `LiveSnapshot`或unloaded `RecordedHistory`。Live capture复制同一短guard内的immutable selected entry Arcs，包含Recorder degraded/unrecorded tail；RecordedHistory从tolerant replay selected path解析anchor。child只重绑定SessionId，保留历史Entry/parent/Turn/Item/body事实，以bounded-memory逐行canonical re-encode并逐条流式readback验证后才COMMITTED/PUBLISHED；staging失败complete-or-invisible，restart可恢复child且不继承source current Turn。Fork/Load与Fork/Unload两种排队顺序已有确定性竞态测试。统一Standards/Spec review已关闭RecordedHistory source-path invariant误报、whole-file allocation、async guard与fixture verifier遗漏；最终`./scripts/check.sh`全量通过，其中library tests为637 passed、3 ignored，集成测试、Clippy、format及current/archive/fixture检查均通过。

durable catalog/query纵向切片亦已完成：`ListAgents`按stable AgentId分页，`ListSessions`按`created_at desc + SessionId`分页并默认排除Archived/Deleted，`GetSessionForkProvenance`直接返回child持久化的source kind与semantic anchor，均不隐式Load Session。4096-entry/15-minute cursor store绑定exact family、filter、sort与captured immutable snapshot；跨family/filter、reuse/expiry/eviction/restart返回`StaleCursor`，page limit超限返回`InvalidArgument`。selected-V1请求、Agent/Session page响应、Fork provenance响应与`QueryError` fixtures已激活；测试覆盖filter、restart stale以及分页期间新Fork不改变旧continuation。统一Standards/Spec review确认无未解决finding，最终`./scripts/check.sh`通过library、integration、Clippy、format、docs与fixtures全部门禁。

Runtime Session membership StateEvent纵向切片已完成：Runtime subscription以原子snapshot-first baseline开始，Create/Fork在durable publication成功后携带matching safe `SessionSummary`，Load/Unload只在residency membership真正变化时发布且Snapshot反映变化后的loaded set；32-entry bounded publisher在背压或Runtime closing时终止stream。selected-V1 codec与active manifest fixtures已覆盖四种event kind/detail，semantic encoder拒绝route/kind/detail不一致的StateEvent。remaining Agent/Session mutation event和full scenario/recovery conformance仍pending。

Session lifecycle public closure纵向切片已完成：Archive/Unarchive/Delete复用Runtime-owned residency exclusion与sealed durable lifecycle attempt，分别返回typed `SessionArchived | SessionUnarchived | SessionDeleted`；Archive/Unarchive canonical retry返回`NoChange`，already Deleted的Delete幂等返回`SessionDeleted`，两者都不发布第二event。真实变化在同一Runtime publication gate内发布matching `session_archived | session_unarchived | session_deleted` StateEvent与safe `SessionSummary`；loaded Archive返回`SessionBusy`，Open→Deleted返回`InvalidArgument`。selected-V1 command/outcome/event fixtures已激活，统一Standards/Spec review关闭重复dispatch流程后无剩余finding；最终`./scripts/check.sh`全量通过，其中library tests为639 passed、3 ignored，全部integration targets、Clippy、format、docs与fixtures门禁通过。remaining Agent/Session mutation family和full scenario/recovery conformance仍pending。

public manifest closure纵向切片已完成：`RuntimeCommand::Interaction::Resolve`携带exact Session/Turn/Item/Request与Presentation-issued resolution key，same key/same semantic input幂等、same key/different input返回CommandConflict、different key after terminal返回InteractionAlreadyResolved；selected-V1 codec覆盖ToolApproval/UserAnswer/Cancelled input与typed InteractionResolved outcome。SessionDefinitionUpdated outcome以及Progress/Closed EventFrame全部materialize为semantic DTO并执行canonical round-trip、route/update consistency、safe Debug与variant-specific frame cap；manifest现无pending target。统一Standards/Spec review修正了Interaction内部EntryId分配失败被误降级为Closing、以及已实现Resolve但未声明InteractionResolution capability两项问题，并补充actor存活回归测试；最终`./scripts/check.sh`全量通过，其中library tests为642 passed、3 ignored，全部integration targets、Clippy、format、docs与fixtures门禁通过。remaining Agent/Session mutation family与full scenario/recovery conformance继续推进。

Agent lifecycle public closure纵向切片已完成：`AgentCommand::Create | SetStatus | Delete`复用sealed Agent owner values和DurableState complete-or-invisible/Create、expected-status CAS与Deleted terminal语义，分别返回typed `AgentCreated | AgentStatusChanged | AgentDeleted`；repeated Enable/Disable返回`NoChange`，already Deleted的Delete幂等返回`AgentDeleted`，两者均不写generation且不发布第二event。真实Create/status变化在Runtime publication gate内发布matching `agent_created | agent_status_changed` StateEvent与完整safe `AgentSummary`；`ListAgents`默认过滤Deleted、`include_deleted`及restart recovery反映durable terminal状态，missing/stale/deleted错误保持typed映射。selected-V1 command/outcome/event fixtures已激活，public manifest保持117项全部active；统一Standards/Spec review修正了already Deleted Delete被误映射为rejected `AgentDeleted`的幂等偏差，修正后无剩余finding。最终`./scripts/check.sh`全量通过，其中library tests为643 passed、3 ignored，全部integration targets、Clippy、format、docs与wire/durable fixtures门禁通过。remaining Agent/Session definition/metadata CAS、readiness与full scenario/recovery conformance继续推进。

Agent definition/metadata CAS纵向切片已完成：public `AgentCommand::UpdateDefinition | UpdateMetadata`直接复用Prompt与lifecycle owner values，definition与metadata分别CAS `AgentRevision`/`AgentMetadataRevision`；empty/equivalent patch在stale/Deleted检查后归约为`NoChange`，Enabled与Disabled可更新，Deleted terminal。真实变化只发布Agent durable generation，并返回typed `AgentDefinitionUpdated | AgentMetadataUpdated`，同时发布携带完整safe `AgentSummary`的matching Runtime StateEvent；definition不fan-out既有Session，metadata Keep/Set/Clear不写conversation JSONL。selected-V1 command/outcome/event fixtures将public manifest扩展为125项全部active；统一Standards/Spec review合并了重复的private/public description patch表示，并把metadata owner timestamp采样移入Runtime publication串行区，修正后无剩余finding。最终`./scripts/check.sh`全量通过，其中library tests为644 passed、3 ignored，全部integration targets、Clippy、format、docs与wire/durable fixtures门禁通过。remaining Session definition/metadata CAS、readiness与full scenario/recovery conformance继续推进。

Session metadata CAS纵向切片已实现：public `SessionCommand::UpdateMetadata`复用`OptionalTextPatch`表达name/description Keep/Set/Clear，并在Session owner重新执行name non-empty/display-name limit与description allow-empty/description limit校验；durable CAS保持expected metadata revision→Deleted→canonical no-op→checked revision+1顺序，Open与Archived可更新。Runtime publication gate内采样single owner timestamp，residency actor在与Load/Unload/Fork/Lifecycle共用的per-Session gate下覆盖durable update、loaded membership判断与required executor publication；loaded executor把exact `SessionMetadata`纳入immutable Snapshot并原子发布Session-scope `session_metadata_updated`，Runtime同时发布携带完整safe `SessionSummary`的matching event。metadata更新不要求Idle、不改变active Turn/execution/queues、不调用Recorder或写conversation JSONL；post-commit loaded publication失败按integrity-fatal关闭。selected-V1增加Set/Clear/Keep command、typed outcome及Runtime/Session两种StateEvent共6个fixture，public manifest扩展为131项全部active。

Ordinary Session definition CAS纵向切片已实现：public `SessionCommand::UpdateDefinition`以optional complete replacement patch原子修改Workspace、Model或Session Prompt selection，保持stale→Open lifecycle→canonical no-op→checked revision publication顺序。Runtime先完成host-neutral Workspace input的checked host lowering，再在runtime publication semaphore与residency per-Session gate下提交CAS；unloaded路径直接发布durable definition，loaded true-Workspace change只在Idle resolve/capture prebuilt Snapshot并于durable commit后原子安装，canonical-equivalent Workspace及Model/Prompt future-only更新可在active Turn期间提交且不调用resolver。active Turn继续使用已captured definition，FollowUp若与publication terminal handoff竞态则保留到publication settle后按new current definition admission；definition update不写conversation或调用Recorder。真实变化返回`SessionDefinitionUpdated`并发布exact Runtime+loaded Session事件，no-op/error无事件；selected-V1新增command及两类StateEvent fixture，将manifest扩展为134项全部active。

Agent revision upgrade纵向切片已实现：public `SessionCommand::UpgradeAgentRevision { session_id, expected_revision, target }`在runtime publication semaphore内采样单一owner timestamp，经residency per-Session gate路由；`target: None`钉住该Agent current revision，`Some(exact)`钉住指定retained revision（含historical rollback），两者都在DurableState既有`Agent → Session` gates内解析/校验same-Agent、Enabled、retained membership，`latest`本身不进入durable definition。unloaded路径直接发布durable definition并验证exact head/definition shape；loaded路径复用executor既有single active definition publication slot：executor只precheck installed expected revision、publication-busy与closing/cancellation，worker只调用`DurableState::upgrade_session_agent(SealedSessionAgentUpgradeAttempt)`（不调用Workspace resolver、不capture Prompt/Skill），durable Updated后原子安装exact checked-successor definition并保留同WorkspaceSnapshot，发布exact Runtime+Session `SessionDefinitionUpdated`事件，NoChange不安装不发布。active Turn继续使用已captured旧Agent ref/definition，future admission与跨terminal的FollowUp handoff使用新ref；同expected revision并发只有一个winner，其余StaleRevision；no-op/error无事件且不写conversation；post-commit install失败保持integrity-fatal poison。wire新增current/exact两类command fixture，manifest扩展为136项全部active。remaining readiness与full scenario/recovery conformance、`ReloadWorkspace`继续推进。

Ready-state `ReloadWorkspace`纵向切片已实现：public `SessionCommand::ReloadWorkspace { session_id }`在runtime publication semaphore内采样单一owner timestamp，经residency per-Session gate路由（loaded-only操作，不读取/更新DurableState），executor缺失直接映射`SessionNotLoaded`。executor复用既有single active publication slot/permit/completion owner：仅Idle接受（Starting/Running/Finishing或已有active publication返回`SessionBusy`，不排队不cancel），worker只执行resolve exact currently installed definition.workspace→Workspace Prompt source capture→required authority revalidation→finish exact WorkspaceSnapshot（Skill source仍只能为空，skill capture roots非空视为internal invariant；绝不调用DurableState），completion install前验证active permit匹配、installed definition仍是admission时exact definition（全字段+revision）、returned snapshot SessionId与workspace revision匹配，成功后原子替换WorkspaceSnapshot Arc并保留exact definition Arc、metadata、execution Idle与queues/observation/recording/usage/diagnostics，成功必视为real reload（不返回NoChange），发布exact `SessionExecutorEvent::WorkspaceReloaded`并映射为Session-scope `SessionWorkspaceReloaded`（detail None，Runtime scope不发事件）；普通resolver/capture validation失败保留exact old snapshot Arc且不发事件，impossible shape/channel/task failure保持既有poison规则。错误映射：not loaded→`SessionNotLoaded`+UserActionRequired，busy→`SessionBusy`+RefreshAndRetry，resolver RootUnavailable/CanonicalizationFailed/AuthorityUnavailable或Prompt SourceDiscovery→`Unavailable`+RetryWithBackoff，AuthorityDenied→`Unauthorized`+UserActionRequired，RootNotDirectory/DuplicateRoot/OverlappingRoots/CwdOutsideRoots/CwdRootMismatch或Prompt ContentLoad/DuplicateKey→`ReloadValidationFailed`+UserActionRequired，closing→`RuntimeClosing`+RetryWithBackoff，impossible→outer `InternalDispatchUnavailable`并poison。wire新增reload command/`workspace_reloaded` outcome/`session_workspace_reloaded` session-state fixture，manifest扩展为139项全部active。remaining readiness（Unavailable状态与恢复）与full scenario/recovery conformance继续推进。

Workspace/Prompt Unavailable loaded readiness与ReloadWorkspace恢复纵向切片已实现：`SessionExecutorSnapshot`增加显式`SessionReadinessView`并将WorkspaceSnapshot改为optional，Ready必须有workspace、Unavailable必须Idle且无active admission/Turn/queues；所有snapshot clone/update方法保留readiness与optional workspace，`workspace()`保持Ready-only而production分支使用内部optional getter，`workspace_revision()`继续返回durable definition revision。`with_definition`拆成两种最小语义：安装Some(WorkspaceSnapshot)时恢复Ready（true Workspace definition publication与ReloadWorkspace recovery），future-only Model/Prompt与Agent upgrade保留当前optional workspace与readiness，且不以重建snapshot清空usage/recording/diagnostics。executor内部start seam接受`Option<Arc<WorkspaceSnapshot>>` + readiness，新增production `start_loaded_unavailable_idle_with_turn_resources_and_lifecycle`，start仍同步校验durable current lifecycle/definition/metadata而只有Some workspace才校验session id/revision；`SessionSubmitError`新增typed `SessionNotReady(SessionUnavailableView)`，`start_admission`在任何TurnId/execution/workspace读取前检查readiness，非Ready直接settle且不降级为DependencyUnavailable。`run_load`在durable current/lifecycle检查后，resolver RootUnavailable/RootNotDirectory/CanonicalizationFailed/DuplicateRoot/OverlappingRoots/CwdOutsideRoots/CwdRootMismatch/AuthorityUnavailable/AuthorityDenied与Prompt SourceDiscovery→WorkspaceUnavailable、Prompt ContentLoad/DuplicateKey→PromptUnavailable，replay之后authority revalidation普通失败也转WorkspaceUnavailable；上述普通失败仍继续打开/replay conversation并初始化Recorder，conversation target/replay corrupt/too-large/storage/internal仍按现有Load error失败且不安装partial owner；随后无论Ready或Unavailable都执行final durable current/lifecycle/definition exact recheck，stale/lifecycle仍关闭新Recorder并返回原typed Load error；最终按readiness启动Ready executor（Some snapshot）或Unavailable executor（None + cause）并安装到同一residency loaded map，Load返回既有`Loaded`。ReloadWorkspace在Unavailable+Idle可运行：成功安装exact WorkspaceSnapshot并恢复Ready且发布既有`WorkspaceReloaded` Session event，普通失败保持原Unavailable cause/None且不安装不发事件；true Workspace definition update同样恢复Ready，future-only Model/Prompt与Agent upgrade保持Unavailable。public `SessionSnapshot`增加真实readiness字段（既有`new_loaded_ready_*`保持Ready wrapper，新增crate-private general constructor并校验legal matrix：non-Ready必须Idle、currentTurn None、activeItems/pendingInteractions/queues空、acceptingInput false）；Runtime `public_runtime_snapshot`使用executor snapshot.readiness，`public_session_snapshot`使用真实readiness且Unavailable时queues.accepting_input=false；submit错误映射WorkspaceUnavailable/PromptUnavailable→`SessionNotReady`+UserActionRequired+Session subject（Agent/Model unavailable同样UserActionRequired，RuntimeDependencyUnavailable backoff，DurableStateCorrupt/TooLarge保留专用codes）。wire SessionSnapshot input/output停止硬编码Ready：`from_input`接受Unavailable+Idle合法shape并构造semantic owner，selected-V1 `validate_session_snapshot_shape`补legal matrix；后续切片已激活`session_readiness_changed` event kind，并接受Preparing+Idle empty semantic shape，manifest保持139项。Agent/Model readiness、Preparing、host security invalidation与RuntimeDependency readiness及full recovery scenario/fixture closure已由后续切片闭合（统一质量门禁已通过）。

Agent readiness fan-out纵向切片已实现：`SessionExecutorSnapshot`移除显式readiness字段，改为内部`agent_available: bool`与`resource_unavailable: Option<SessionUnavailableView>`两事实并由getter派生public readiness（agent_available=false→`Unavailable(AgentUnavailable)`，否则resource cause，否则Ready），所有clone/update方法保留两事实；`with_definition_and_workspace`只清除resource cause（ReloadWorkspace/true Workspace definition update在Agent仍disabled时保持AgentUnavailable），future-only definition与Agent revision upgrade保留全部状态，`workspace()` Ready-invariant保持且AgentUnavailable不丢last-good WorkspaceSnapshot。executor内部start seam改为接受`agent_available`+`resource_unavailable`（现有Ready wrapper默认true/None，resource Unavailable wrapper默认agent_available=true），新增generic production seam组合disabled Agent+Some workspace或resource cause。`run_load`在captured current definition后同步读`durable_state.agent_head(definition.agent().agent_id())`（缺失/identity mismatch为internal），status Enabled→agent_available=true、Disabled/Deleted→false，仍继续Workspace resolve/capture与conversation replay以便保存底层resource readiness，final durable exact recheck不变；disabled/deleted Agent的Load仍返回`Loaded`，Runtime SessionLoaded snapshot投影AgentUnavailable，Submit提前返回`SessionNotReady(AgentUnavailable)`。新增crate-private `SessionExecutor::set_agent_availability_with_cancellation(agent_id, available, timestamp, command_id, cancellation)`（不调用DurableState）与`SetAgentAvailability` request/error：actor验证installed definition.agent.agent_id匹配（mismatch为internal invariant poison），Idle且无active admission/Turn立即更新两事实并仅在public readiness真实变化时发送`SessionExecutorEvent::ReadinessChanged`（同状态重复不发event），Starting/Running/Finishing或active admission/Turn不取消、不改变legal非Idle snapshot，只在actor保存最新pending availability+timestamp+command_id（Disable→Enable在terminal前相互覆盖）；admission失败回Idle与Turn terminal回Idle后应用pending（terminal路径在决定是否pop/start FollowUp之前应用，non-Ready不pop/start且保留queue以便Enable后handoff，Enable immediate/terminal application在Idle后若Ready经`start_queued_follow_up_after_publication`启动一条，消息不丢；queue_projection在non-Ready时隐藏follow-ups以保持legal matrix但actor queue保留）；active Turn继续，不发送SecurityRevoked/Cancel，final Agent admission gate仍决定Starting race（Disable先赢则admission返回AgentUnavailable然后Idle应用pending）。residency新增per-Session `AgentAvailability` operation/request/error（复用actor child与per-Session gate，不建立Agent-level第二actor）：Runtime先同步枚举loaded snapshots中definition.agent.agent_id匹配的SessionId，逐个调用registry operation，每个operation在gate内recheck executor仍存在且仍匹配AgentId（Unload先赢NoChange，Load在status mutation后开始由agent_head直接装正确状态，不读/写DurableState），result为`()`，executor Closing映Closing而Internal/mismatch poison；registry新增`loaded_session_ids_for_agent`短锁projection与`set_session_agent_availability` route。Runtime `dispatch_agent_status`在`DurableAgentStatusOutcome::Updated`后（Agent gate已释放）采样单一owner timestamp，若residency存在枚举该Agent loaded Session IDs逐个await fan-out，fan-out完成后用同一timestamp发布AgentStatusChanged Runtime事件（Create/definition/metadata事件不fanout，NoChange/alreadyDeleted不fanout）；fan-out普通Closing按既有RuntimeClosing contract处理，post-durable internal publication failure返回outer `InternalDispatchUnavailable`。wire激活`session_readiness_changed`：`SessionStateEventKind`新增`SessionReadinessChanged`与`StateEvent::session_readiness_changed` constructor（Session route、detail null），selected-V1 input/output enum激活该variant并纳入与session_execution/definition/metadata/workspace_reload相同的Session route+null detail+legal SessionSnapshot active arm；Runtime/Session snapshots已投影真实readiness无额外shape，EventStream把`ReadinessChanged`映射为`session_readiness_changed`。manifest保持139项。Model readiness、Preparing、host security invalidation与RuntimeDependency readiness及full recovery scenario/fixture closure已由后续切片闭合（统一质量门禁已通过）。

ModelUnavailable load/definition projection纵向切片已实现：`SessionExecutorSnapshot`把`resource_unavailable`收窄重命名为`workspace_unavailable: Option<SessionUnavailableView>`（当前只允许Workspace/Prompt cause，不做通用集合）并新增独立`model_available: bool`事实，public readiness只在Idle按固定优先级为agent_available=false→`Unavailable(AgentUnavailable)`、workspace cause→cause、model_available=false→`Unavailable(ModelUnavailable)`、否则Ready（非Idle执行始终投影Ready；facts是future-only，new Unavailable只在回Idle后显现），所有clone/update方法保留两事实。`with_definition_and_workspace`改为接收新definition对应model_available并只清除workspace cause（ReloadWorkspace `new_definition=None`时保留当前model_available），`with_definition`同样接收model_available以安装future-only Model/Prompt与Agent upgrade的新定义事实；新增crate-private `model_available_for_definition`共享helper（`model_gateway.resolve_for_turn(Arc::clone(catalog), ResolveTurnModelRequest::new(selection, reasoning, max_output_tokens))`：Ok→true，ModelUnavailable/UnsupportedReasoning/InvalidOutputLimit→false，CatalogUnavailable/SourceUnavailable/InvalidDefinition→internal invariant），actor新增同步`definition_model_available`：任何install新definition的publication在install前按当前installed catalog计算model_available并与definition一起原子安装（internal resolution failure走既有ActorFatality::Internal路径，不伪装ModelUnavailable），ReloadWorkspace保持当前事实，DefinitionUpdated/WorkspaceReloaded event snapshot只在Idle publication携带derived readiness（Running等非Idle执行时保持Ready、new facts经terminal/回Idle显现）且不发额外ReadinessChanged event；true Workspace publication只清workspace cause，model仍不可用时public readiness保持ModelUnavailable，ReloadWorkspace event snapshot可为ModelUnavailable仍表示Workspace snapshot reload成功；Agent disable/enable仍只改agent_available，Enable恢复到workspace cause或ModelUnavailable；Submit仍按derived readiness在Workspace clone前拒绝，ModelUnavailable自然映射既有`SessionNotReady`+UserActionRequired。`run_load`在turn_resources存在时、start executor前用module-local `model_available_for_load`检查captured definition.model：普通model incompatibility→model_available=false且Load仍Loaded，catalog owner mismatch/invalid/source impossible→`context.internal_load`，与Workspace/Prompt Unavailable cause独立保存两个事实；test-only无turn_resources路径默认model_available=true，production None仍按现有internal。executor内部start seam（`start_loaded_idle_inner`/production Load seam）新增`model_available`参数，现有Ready/test wrapper与resource Unavailable wrapper默认true，Load传实际值。shared-resource reload recovery/fanout纵向切片见下节；Preparing、host security invalidation与`RuntimeDependencyUnavailable`及full recovery scenario/fixture closure已由后续切片闭合（统一质量门禁已通过）。

selected PromptUnavailable load/definition projection纵向切片已实现：`SessionExecutorSnapshot`新增独立`prompt_available: bool`事实，public readiness只在Idle按固定优先级为agent_available=false→`Unavailable(AgentUnavailable)`、workspace cause→cause、selected prompt_available=false→`Unavailable(PromptUnavailable)`、model_available=false→`Unavailable(ModelUnavailable)`、否则Ready（非Idle执行始终投影Ready；facts是future-only，new Unavailable只在回Idle后显现），所有clone/update方法保留四事实。`with_definition_and_workspace`与`with_definition`均接收新definition对应的prompt_available+model_available并原子安装（ReloadWorkspace `new_definition=None`保留当前两事实且只清workspace cause）。PromptService新增crate-private同步`selection_available`：先验证resource view owner与service exact ptr（owner mismatch返回internal invariant的Err而非ordinary unavailable），复用`resolve_selected_definitions`（Agent selection要求System+runtime provenance、Session selection要求User且不要求runtime provenance），missing Prompt/wrong role/duplicate resolved key→`Ok(false)`、成功→`Ok(true)`、其他kind→Err，不复制解析逻辑也不暴露definitions。session_execution新增crate-private async `prompt_available_for_definition`与closed `SessionPromptAvailabilityError::{Closing, InternalDispatchUnavailable}`：`durable_state.read_agent_definition(definition.agent())`读exact retained revision而非current，Closing单独分类，AgentNotFound/RevisionUnavailable/StorageUnavailable/InternalDispatchUnavailable与returned Agent identity/revision mismatch均为internal；`Ok(bool)`直通、selection Err→internal。`run_load`在captured definition+Agent status后、Workspace preparation前独立await prompt helper（与model同步check相邻，test-only无turn_resources默认true）：Closing→`SessionResidencyLoadError::Closing`、internal→`context.internal_load`，即使Workspace/Model也Unavailable仍保存独立prompt事实；executor内部start seam与production Load seam新增`prompt_available`参数，现有test/Ready/resource-Unavailable wrapper默认true。任何install新definition的publication（ordinary future-only/Workspace change/Agent upgrade）在durable publication成功且`validate_completion`给出exact definition后、install前await同一helper（current installed Prompt resources），Closing或internal在postcommit均走existing active restore+`close_for_fatal(ActorFatality::Internal)`而非ordinary rejection，随后同步计算model并原子安装definition+prompt/model事实；`new_definition=None`的ReloadWorkspace保留current prompt/model事实；不发额外ReadinessChanged event，DefinitionUpdated snapshot只在Idle publication携带derived readiness（Running等非Idle执行时保持Ready、new facts经terminal/回Idle显现）；Agent disable/enable仍只改agent fact，Enable恢复到workspace/prompt/model底层cause；Submit仍在Workspace clone前按derived readiness拒绝，selected PromptUnavailable自然映射既有`SessionNotReady`+UserActionRequired。manifest保持139项。shared-resource reload recovery/fanout纵向切片见下节；Preparing、host security invalidation与`RuntimeDependencyUnavailable`及full recovery scenario/fixture closure已由后续切片闭合（统一质量门禁已通过）。
shared-resource reload recovery/fanout纵向切片已实现：`RuntimeCommand::Runtime(ReloadSharedResources)`在runtime publication semaphore内并行`PromptService::build_reload_candidate`与`ModelGateway::build_reload_candidate`，任一普通Prompt/Model失败保留old roots/executors且无事件，返回`ReloadValidationFailed`+UserActionRequired+Runtime subject；成功后先获取Runtime专用shared-resource write gate（`Arc<tokio::sync::RwLock<()>>`，外部`TurnCommand::Submit`持read gate直到`residency.submit`返回、Turn context admission完成，杜绝半切换capture，多reader不串行跨Session Submit），经residency新增Runtime-scope `SharedResources` operation（`ActiveOperation.session_id`改`Option<SessionId>`，remove_active仅Some清per-Session gate，不建dummy ID）两阶段安装：phase (a) 对全部loaded snapshots用exact installed definition Arc预计算model（现有`model_available_for_definition`+candidate catalog，普通incompatibility→false，其余→poison/internal）与selected Prompt（现有async `prompt_available_for_definition`+candidate Prompt resources，Closing→Closing、其余→poison/internal），全部完成前不更新任何executor；phase (b) 构造new ResidencyTurnResources（同model gateway/toolset/compaction、new Prompt/Model roots），按sorted SessionId逐Session取per-Session gate重取executor/snapshot并要求exact definition Arc ptr_eq（missing/mismatch在Runtime global publication下为closing/internal，不得silent NoChange），调用executor新增`update_shared_resources_with_cancellation`（actor验证`Arc::ptr_eq` current definition，仅替换`TurnResources`的Prompt/Model roots保留gateway/toolset/compaction，Idle立即应用+queue projection+仅readiness变化发布`ReadinessChanged`，非Idle把agent/prompt/model三事实合并为单一`pending_availability` composite并保留最后收到command的timestamp/command_id作为最终attribution，terminal/admission failure在FollowUp决策前应用一次并重新project queues，active Turn保留已captured旧context、terminal后FollowUp用new roots、request前已admitted的FollowUp线性化在reload前可用old capture），任一post-prepare错误poison/internal；全部成功后`OperationCompletion::SharedResources`把new ResidencyTurnResources交回residency actor在settle caller前替换自身`turn_resources`（future Load用new roots），Runtime再一次性Mutex替换root pair、发布Runtime-scope `SharedResourcesReloaded` StateEvent（detail null，snapshot为fan-out后current projection）并返回typed `CommandOutcome::SharedResourcesReloaded`；residency在candidate后的Closing/Internal映射outer `RuntimeDispatchError::InternalDispatchUnavailable`而非普通rejection。wire激活`shared_resources_reloaded`（Runtime event kind input/output+validators+selected schema、detail null，移除原pending-target识别；CommandOutcome新增`shared_resources_reloaded` unit并移除旧pending `runtime_reloaded`识别），`SessionReadinessChanged`既有constructor/codec复用，manifest现为144项active且无pending target，`shared_resources_reloaded` response与runtime-state fixtures已激活（ReloadSharedResources public outcome/event closure已实现，统一质量门禁已通过）。

active-Turn graceful Unload纵向切片已实现：`MiniCoreRuntimeConfig`新增private `unload_grace: std::time::Duration`（default 30s，公开builder `with_unload_grace(Duration)->Self`；open验证非zero且≤5min否则`RuntimeInitializationError::InvalidConfiguration`，Std Duration本身finite），`RuntimeInner`保留同一grace并在open时经新production start seam安装到residency actor（既有test/start wrappers静态补齐default 30s，不改任何test constructor shape）；public `SessionCommand::Unload` route仍持runtime publication gate，经residency per-Session gate调用`run_unload`：loaded map entry与exact permit保持安装直至drain完，先`executor.prepare_for_unload(unload_grace)`（crate-private，调用方一开始同步关闭`turn_admission_gate`，然后经既有unbounded emergency lane发送`PrepareUnloadRequest { deadline: now+grace, waiter }`，绝不被bounded work lane阻塞），成功后`executor.close()`最后`remove_exact`；prepare/close任何internal→poison/Internal，registry已closing而prepare未完成时先close/drain再remove exact owner（不留partial owner）再映射Closing。executor actor新增最小`PrepareUnloadState { deadline, waiters }`（timer由main select每轮从copied deadline重建，不新建child actor；重复request共享same state，effective deadline只取更早不能延长）：接受Prepare立即关闭admission（handle已关、actor保持）、清空actor Steer+FollowUp并re-project queue，新Submit/Steer/FollowUp拒绝（Submit settle `Closing`并由residency在registry未closing时映射`SessionNotLoaded`——避免误映射`RuntimeClosing`——Steer/FollowUp按既有TurnNotRunning contract），ResolveInteraction/Cancel/SecurityRevoked/Snapshot及accepted publication completion仍可处理；`EmergencyControlSignal`新增`PrepareForUnload`（sticky first-wins：更早Cancel/SecurityRevoked保留原signal/reason，Prepare先赢后后续Cancel/Security按既有AlreadySignaled返回），`SessionTurnInterruption`新增`PrepareForUnload`并映射到wire既有`TurnInterruptionView::PrepareForUnload`（wire shape不变，仅补semantic exhaustive mapping）。grace期间active admission/Turn自然完成不cancel；actor main select加入deadline wakeup，deadline到期仍有active admission/Turn时对exact current emergency target signal PrepareForUnload并cancel其cancellation token、把execution投影到Finishing（仅首次发`session_execution_changed`）、以`InteractionCancelReason::SessionUnloaded`取消该Turn pending interactions、绝不直接drop active task。Starting deadline语义：Input尚未live apply时原Submit不得映射SubmitCancelled（那只属于user Cancel）——新增internal `SessionSubmitError::PrepareForUnload`，`handle_admission_completion`在retire前observe current emergency signal并把generic cancellation（Cancelled/Closing）重分类，residency映射`SessionNotLoaded`，Runtime public返回`CommandErrorCode::SessionNotLoaded`+UserActionRequired（既有SessionNotLoaded contract）；Input先赢则原Submit仍`TurnStarted`，随后同一Turn以`Interrupted(PrepareForUnload)`终止。每次active admission failure、Turn terminal、publication settlement后调用最小`settle_prepare_unload_if_idle`：仅active_publication/admission/turn全None才drain waiters Ok，Idle接受Prepare立即settle；状态可清除但admission gate保持closed直到executor close。terminal FollowUp handoff与`start_queued_follow_up_after_publication`在prepare/unloading期间不pop/start（queued lanes已清空，双保险guard）。`close_and_drain`不再对已Idle的prepared executor伪造Finishing event（仅确有active admission/Turn时投影Finishing），fatal/direct closing保持既有integrity行为，Recorder仍在actor settlement/close后await close；prepare waiter channel closure按fatal/closing区分（fatal→Internal、正常close→Closing），request reject/drop/exhaustive matches完整。registry shutdown `close_installed_executors`改为两段API：先对全部installed executors同步`begin_prepare_for_unload(grace)->PrepareUnloadWaiter`广播（使grace并行计时，不顺序累加N*grace、不spawn untracked tasks），再逐个await shared waiter、最后逐个`executor.close()`。显式shutdown的`request_closing`只cancel residency admission token、绝不触发executor force token（loaded executor的lifecycle token是独立`executor_force_closing`，仅fatal/owner failure路径cancel），grace完全由该广播授予并并行计时。unload不改durable lifecycle/definition/metadata/conversation内容；自然完成/forced terminal按既有recorder semantics。wire不新增`queue_updated` event（当前public enum未闭合，queue只经subsequent snapshots/terminal event体现，不扩大slice），manifest现为144项；Unload pre-Input SessionNotLoaded与`turn_interrupted_prepare_for_unload` fixture/tests已补齐（scenario/fixture closure已实现，统一质量门禁已通过）。

host security Workspace authority invalidation纵向切片已实现：`MiniCoreRuntime`新增public host-only（非wire command）async seam `invalidate_session_workspace_authority(session_id)->Result<(), SessionWorkspaceInvalidationError{RuntimeClosing,SessionNotLoaded,InternalDispatchUnavailable}>`，表示WorkspaceAuthority/host已先发布current hard restriction fact，Runtime只负责当前loaded executor的signal+recovery，不是RuntimeCommand、不改durable definition/revision/metadata/conversation；route不获取runtime_publication semaphore、不等待普通work lane，经residency loaded map直接clone executor并调用其out-of-band security invalidation API（先同步close `turn_admission_gate`再经现有unbounded emergency lane发送，绝不被bounded work阻塞），missing loaded executor或executor普通Closing且registry未closing（per-Session Unload/old exact executor race）→`SessionNotLoaded`，仅registry/runtime closing→`RuntimeClosing`，actor fatal→`Internal`，采样single SystemClock timestamp、无CommandId。`SessionExecutorSnapshot`新增`workspace_preparing: bool`（所有clone/update方法保留）：readiness优先级workspace_preparing→`Preparing`高于Agent/workspace cause/Prompt/Model，Preparing必须Idle、workspace None、public queues empty/accepting false；新增最小snapshot methods：enter Preparing drop旧WorkspaceSnapshot并mask workspace cause（final必须明确安装新cause/snapshot），finish success安装Some(WorkspaceSnapshot)+workspace_preparing=false+清workspace cause，finish ordinary failure安装None+false+`WorkspaceUnavailable`/`PromptUnavailable`，保留agent/prompt/model/metadata/usage/recording/diagnostics等全部既有事实。`TurnAdmissionGate`新增最小`open()`：Security invalidation先同步close gate再发request，recovery完成后仅在executor未closing且无PrepareUnload时reopen（Unavailable仍由readiness提前拒绝、gate可open），close/fatal不reopen。actor新增最小`SecurityInvalidationState{timestamp, waiters, worker_task?}`（不新建第二actor）：重复invalidation join同一state、不重复signal/recovery、不生成CommandId；request accepted后——PrepareUnload已开始或closing→settle Closing；active admission/Turn→立即对exact current emergency target发sticky `SecurityRevoked` first-wins并cancel security_revocation/cancellation token（更早Cancel/PrepareUnload已赢保留原reason但仍在terminal/admission cleanup后进行recovery；只有形成Turn才投影Finishing、pre-Input admission保持Starting legal；Security获胜时pending Interaction按既有SecurityRevoked truthful settlement；即使active publication在飞也立即signal——publication不屏蔽security signal），waiter直到recovery final state安装后settle；仅无admission/Turn而active definition/Agent/reload publication在飞（Idle）也立即进入Preparing（drop旧WorkspaceSnapshot、mask workspace cause、发布唯一一次`ReadinessChanged(command_id None)`——host restriction已current，不允许继续公开Ready+acceptingInput）但recovery worker仍等待publication settlement、settle后以publication后的exact current definition启动recovery（enter幂等不重复start event、settled snapshot不重新安装、worker启动时workspace仍None）、不取消已到durable barrier的publication；Idle且无active admission/Turn即进入Preparing并发布`ReadinessChanged(command_id None)`（active publication不阻塞Preparing entry、只等Turn/admission settle），recovery worker单独等待publication settle后才spawn（全空Idle立即spawn）。security invalidation为out-of-band：residency仅做loaded executor lookup+await executor API，request一旦送入actor由actor own（host waiter drop仍继续），Unload/close竞态由actor Closing settle（residency在registry未closing时把该Closing映射`SessionNotLoaded`而非`RuntimeClosing`）、old handle不得把signal转给future replacement。recovery启动点：Idle immediate、admission failure cleanup后、Turn terminal event发布后（先`TurnInterrupted(SecurityRevoked/earlier winner)`再`ReadinessChanged(Preparing)`）FollowUp handoff前、publication success/ordinary settlement后；recovery pending时禁止FollowUp pop/start、新Submit因gate close失败。recovery worker复用现有ReloadWorkspace resolve/capture/revalidate/finish代码（抽最小shared async helper返回exact WorkspaceSnapshot或neutral classification，普通ReloadWorkspace保持原AuthorityDenied→Unauthorized/shape→WorkspaceRejected映射不变）：security分类所有非internal resolver失败（Root/authority/canonical/validation，含AuthorityDenied——hard restriction仍在current policy）→`WorkspaceUnavailable`、Workspace Prompt SourceDiscovery/ContentLoad/DuplicateKey→`PromptUnavailable`、Closing→typed Closing、shape/channel/task mismatch与Skill roots非空→fatal Internal；completion install前验证Arc ptr_eq与snapshot SessionId/revision，不调用DurableState。start recovery时publish Preparing immutable snapshot（`SessionReadinessChanged` event with command_id None），finish success/failure再发布one `SessionReadinessChanged`（command_id None，即使final derived readiness因AgentUnavailable仍与Preparing不同也发布），不发WorkspaceReloaded event（security invalidation不是user reload）、No Runtime event。`SessionExecutorEvent::ReadinessChanged.command_id`改为`Option<CommandId>`：Agent/shared reload sources传Some、security传None、Runtime EventStream直接复用Option，所有accessors/matches补全。close/fatal/reap必须settle security waiters exactly once（ordinary closing→Closing、fatal/task/channel/shape→Internal），worker owner-tracked并reap、Preparing期间Snapshot可观察、close不留下worker/task；既有crate-private targeted `security_revoke(target)` tests/seam行为保持、不强制触发recovery。public SessionSnapshot语义constructor已允许Preparing+Idle empty shape，runtime projection确保workspace_preparing不生成非法currentTurn/items/interactions/queues；Submit error map Preparing使用独立internal carrier（`workspace_preparing`）、不再借用RuntimeDependency cause，公开映射仍为`SessionNotReady`+RetryWithBackoff。manifest现为144项（`session_readiness_preparing` state fixture已激活）；host security Preparing/active Turn duplicate recovery fixtures/tests已补齐（scenario/fixture closure已实现，统一质量门禁已通过）。

RuntimeDependencyUnavailable readiness与probe recovery纵向切片已实现：`SessionExecutorSnapshot`新增独立`runtime_dependency_unavailable: bool`事实（所有clone/update方法保留），唯一真实producer是loaded Turn admission读取pinned historical AgentRevisionRef时`DurableState::read_agent_definition`的transient `StorageUnavailable`——这不是host global bool，也不是`ReloadSharedResources`（shared-resource reload只替换Prompt/Model roots、不触碰本fact），Tokio `RuntimeDependencyUnavailable`仍只属于`RuntimeInitializationError` open error。readiness只在Idle按固定优先级为workspace_preparing→`Preparing`、agent_available=false→`AgentUnavailable`、workspace cause→cause、prompt_available=false→`PromptUnavailable`、model_available=false→`ModelUnavailable`、runtime_dependency_unavailable→`Unavailable(RuntimeDependencyUnavailable)`、否则Ready（非Idle执行始终投影Ready；facts是future-only，new Unavailable只在回Idle后显现）；该fact与其他事实独立保存，普通Unavailable保留last-good WorkspaceSnapshot，active Turn继续使用已captured context且不受影响。admission首次遇到transient StorageUnavailable时settle回Idle后安装该fact并发布`ReadinessChanged(command_id None)`，Submit公开返回`SessionNotReady(RuntimeDependencyUnavailable)`+`RetryWithBackoff`，同时立即启动owner-tracked无TurnId probe（复用同一exact `read_agent_definition` read路径，不建立第二actor、不调用Workspace resolver）；probe仍Unavailable则保持fact与投影、不重复发布event，等待next Submit re-arm再启动一次probe；probe Recovered时清fact、发布`ReadinessChanged(command_id None)`并恢复既有retained FollowUp handoff（recovery期间FollowUp保留在queue，recovered后按既有非Ready/Ready路径handoff）。admission直接观察到AgentNotFound/RevisionUnavailable时分类为`AgentUnavailable`而非本cause；fact安装后的probe若发现同一retained ref消失则是durable invariant并进入internal/fatal，model recapture failure同样是internal invariant（经既有fatal/internal路径，不伪装为本cause），fatal/closing/corrupt/too-large不进入本cause（DurableStateCorrupt/TooLarge保留既有专用映射）。Preparing保持独立internal carrier（`workspace_preparing`）、不借用RuntimeDependency cause；恢复只由exact DurableState read probe与Submit re-arm拥有，无新public/wire command，manifest现为144项（`session_readiness_unavailable_runtime_dependency` state fixture已激活）；RuntimeDependencyUnavailable真实historical storage fault+probe/rearm+retained FollowUp fixtures/tests已补齐（scenario/fixture closure已实现，统一质量门禁已通过）。

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

状态：Completed。V4-P1-3由[ADR 0138](adr/0138-production-provider-baseline-uses-verified-rig-contracts.md)、[ADR 0139](adr/0139-rig-is-evidence-only-under-rust-1-85.md)、[M12 provider fixture](fixtures/provider-gate-m12/README.md)、真实Rig standalone loopback evidence与真实Rust 1.85冷编译关闭；本里程碑没有实现production adapter。

关闭结果：

1. 首个production `ProviderProtocol` baseline只保留OpenAI Responses与Anthropic Messages；M14 adapter通过前仍不显示为available，OpenAI Chat Completions/Gemini等未验证protocol不接受；
2. current Gateway文档已清除local concurrency wait/per-principal permit冲突，继续遵守ADR 0125；
3. queued Steer只排队、不改变`ConversationRevision`，不会使in-flight result或retry backoff失效；safe point成功apply后才形成新revision；
4. standalone `provider-gate/` package中的exact `rig-core = 0.40.0`通过两协议真实`127.0.0.1:0` unary/stream tests，覆盖system/instructions、ordered messages、Tool schema与identity/order、OpenAI reasoning/structured request、Anthropic thinking/signature/cache-control、usage、finish/terminal、body/header IDs、base URL、cancel、fragmented SSE、drop与early EOF；
5. unary与streaming 429/500/529/error probes均证明每次Rig invocation最多一个HTTP request；Rig synthetic zero-usage `Final`不作为protocol terminal，公开`HttpClientExt` seam可在原样转发bytes时保存terminal与allowlisted metadata；
6. 26-case closed fixture冻结context overflow、rate limit、auth、quota、transport、provider unavailable、malformed response、stream error和early EOF的reason/delivery/normalization/policy；只有`NotSent | RejectedBeforeExecution`可保留delivery-safe transient reason，HTTP 500/503/504 unknown outcome不盲重试；
7. OpenAI 400 context code、Anthropic 400无overflow subtype、两协议401、429/529、malformed 200与5xx均有real loopback envelope/fail-closed证据；分类不匹配human message；
8. 真实Rust 1.85冷编译证明`rig-core` 0.36.0–0.40.0不能作为主crate dependency；Rig只存在于声明Rust 1.88、独立lockfile且stable-only运行的`provider-gate/` evidence package，root `Cargo.toml`/`Cargo.lock`、`src/`与public DTO均不含Rig；
9. `scripts/check-msrv.sh`通过`rustup which`锁定exact Rust 1.85 compiler并使用隔离target，防止PATH/Homebrew stable `rustc`或共享artifact造成假绿；
10. M14不实现`RigProviderAdapter`，改为`OpenAiResponsesProviderAdapter`与`AnthropicMessagesProviderAdapter`直接拥有各自HTTP/SSE、terminal、metadata和typed error mapping。

退出条件已满足：V4-P1-3为Closed并有独立ADR、fixture、real mock-server、明确SDK rejection与真实主crate MSRV evidence。production adapters继续属于M14。

## M13 · Production Tool/Sandbox Gate（V4-C0-1）

状态：In Progress。M13.1已实现closed `FilesystemRead | FilesystemWrite | Network | Process` class set、final `ToolPermissionSet` narrowing、adapter `Available(enforceable) | Unavailable` contract、exact capability gap与fixed `PreExecution + Denied` mapping；M13.2已让per-request direct Execute plan在任何ToolStartGate reservation/start factory poll前admit；M13.3已让Execute/Approval plan各自携带唯一permissions，并在host AllowOnce/Restricted AllowWith后复验ceiling与captured Sandbox，失败不进入start gate；M13.4已用非空、已admit permission plan完成SecurityRevoked-before-start、Sandbox unavailable与Running cooperative truthful settlement的adapter-independent Session round conformance。production permission producer/adapter仍pending。

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

- OpenAI Responses direct ProviderAdapter与mock/live opt-in smoke tests；
- Anthropic Messages direct ProviderAdapter与mock/live opt-in smoke tests；
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

1. `M5.0` DurableState/async foundations implementation（foundation已完成，完整platform matrix实现已交付、统一native acceptance已通过（GitHub Actions run 31433810296四job全绿））；
2. `M5.1` SessionRecorder与全部七个fixture坐标（已完成）；
3. `M5.2` tolerant semantic replay、corruption sidecars及replay/Recorder-backed Ready+Idle Load hydration（已完成）；
4. M6–M10随owning behavior补齐resources、behavioral Runtime slice、non-empty Item/Interaction、Degraded recording、usage/diagnostics、Progress/Closed EventFrame，并逐项激活remaining manifest vectors。

在M2–M6 prerequisites关闭前不得进入M7 ordinary behavior slice；production Provider与Tool/Sandbox继续分别受V4-P1-3和V4-C0-1门禁约束。
