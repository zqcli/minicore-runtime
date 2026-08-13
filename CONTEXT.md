# MiniCore Agent Runtime

## 换机交接（2026-08-11）

当前开发分支是`dev`。M12 Production Provider Gate（V4-P1-3）与M13 Production Tool/Sandbox Gate（V4-C0-1）均已完成；M13 acceptance HEAD为`76a33c2 docs: record M13 sandbox acceptance`。Rig 0.40.0已因真实Rust 1.85冷编译失败被拒绝进入production baseline，协议证据保留在standalone `provider-gate/` package。当前进入M14：`OpenAiResponsesProviderAdapter`与`AnthropicMessagesProviderAdapter`两个direct slices、默认离线contract suites、host-only dynamic credential/catalog installation及两个显式ignored live smoke harness均已实现；stateless full-request wire policy已由ADR 0141冻结（每次`generate_model_turn`至多调用一次`ProviderAdapter::execute`——owner validation/pre-send cancellation/`AuthMissing`在调用adapter前以typed error terminal；独立地发送零或一个HTTP POST，adapter编码/build失败或adapter级pre-send cancellation为一次execute/零POST；若发送POST则携带完整full request，显式cache/continuation为有意omission），Anthropic `provider_total_tokens`恒为`None`（provider-reported only）；2026-08-12两个real-credential public Runtime release smoke均通过，固定产品User-Agent、Anthropic unsigned thinking与omitted start-stop字段由ADR 0145冻结；production Tool/Sandbox adapters中，`read_file`/`list_directory`/`write_file` filesystem slices已由ADR 0143/0144/0146完成，exact-origin/pinned-address `fetch_url` network slice已由ADR 0147完成；process、generic ToolService/schema/hooks/完整policy/approval及其他未实现adapter冻结为post-MVP。换机后执行`git switch dev && git pull --ff-only origin dev`，再用`git status --short --branch`确认工作树。与本任务无关的`stash@{0}: On refactor/async-loop-eventual-session-log: wip: paused bounded JSON implementation before dev merge`仍保留，不要自动apply/drop。

v0.1前端闭环审计已完成：此前唯一真实blocker是completed User/Assistant conversation在terminal、重连与cold load后没有public read seam。`c99ccf7 feat: expose paged session transcripts`与[ADR 0148](docs/adr/0148-v0-1-session-transcript-is-a-library-only-read-seam.md)已新增library-only `MiniCoreRuntime::session_transcript`：first page要求Session loaded；Session actor从canonical selected history捕获immutable entry Arcs；current Turn仍由Snapshot/Event展示；continuation绑定same capture且Unload后仍可读；restart后重新Load可恢复实际recorded prefix。Public Wire V1、capability manifest、Conversation JSONL V1与Store V1均未改变。v0.1 implementation、canonical docs与双工具链full gates已完成：stable/exact Rust 1.85 main library均`1035 passed / 3 ignored`，integration均合计`159 passed / 3 ignored`，stable provider-gate `25/25`，其余Clippy/format/docs/Wire/Store fixtures全绿；只剩按仓库流程完成远端同步。不再扩张process、generic Tool、Structured或Prompt/Skill authoring生态。

M12 checkpoint series：

- `30effd5 docs: align provider retry and concurrency rules`；
- `6190f16 test: probe Rig provider contracts`；
- `0bc549a test: probe Rig streaming contracts`；
- `47ec128 test: prove Rig streaming single attempts`；
- `cc1cd0b test: prove Rig terminal evidence seam`；
- `d38d8ed test: preserve Rig response metadata evidence`；
- `096cce1 test: freeze provider error delivery matrix`；
- `64228e8 test: probe Rig provider error envelopes`。
- `16fa1c8 docs: close M12 provider gate`；
- `476287d fix: isolate Rig from Rust 1.85 baseline`。

已冻结的production baseline：

- 首版`ProviderProtocol`只支持OpenAI Responses与Anthropic Messages；两个direct adapter、local contract suite及host-only dynamic credential/catalog installation均已完成。默认Runtime仍不内置或自动发布任何model，只有trusted host通过`MiniCoreRuntimeConfig::with_model_provider`显式安装的definition可解析；两个ignored live smoke harness已在stable/真实Rust 1.85编译且保持默认off；2026-08-12两个public Runtime-path real-credential release smoke均通过（ADR 0145）；OpenAI Chat Completions/Gemini等未经验证protocol不接受；
- exact `rig-core = 0.40.0`只存在于声明Rust 1.88、拥有独立lockfile且stable-only运行的standalone `provider-gate/` package；root `Cargo.toml`/`Cargo.lock`、`src/`、Prompt、Conversation与Runtime public DTO均不含Rig；
- 两协议真实`127.0.0.1:0` unary/stream tests覆盖system/instructions、ordered messages、Tool schema/identity/order、OpenAI reasoning/structured request、Anthropic thinking/signature/cache-control、usage、finish/terminal、body/header IDs、base URL、cancel、fragmented SSE、drop与early EOF；
- 一次Rig completion/stream invocation最多一个HTTP request；Gateway/SDK automatic retry为0，429/5xx/401不在Gateway内重发；
- Rig synthetic zero-usage `Final`不是terminal proof：OpenAI要求`response.completed`，Anthropic要求non-empty `message_delta.stop_reason`；公开`HttpClientExt` seam可在原样转发bytes时保存terminal、EOF/error/drop与allowlisted metadata；
- metadata allowlist为OpenAI `x-request-id/retry-after/openai-processing-ms`及Anthropic `request-id/retry-after`；canary/arbitrary headers、cookie、auth和raw body不越过private seam；
- `docs/fixtures/provider-gate-m12/error-mapping-v1.json`冻结26个case：只有`NotSent | RejectedBeforeExecution`可保留delivery-safe transient reason，HTTP 500/503/504 unknown outcome不盲重试，early EOF/partial stream分别归一化为`RequestOutcomeUnknown | StreamInterrupted`，Anthropic不得从human message猜context overflow；
- queued Steer在retry backoff只排队、不改变revision；safe point成功apply后才使旧basis失效；Gateway继续无local route/model/principal permits；
- ADR 0138/0139 Accepted：协议合同继续有效，M14不实现`RigProviderAdapter`，改为`OpenAiResponsesProviderAdapter`与`AnthropicMessagesProviderAdapter`直接拥有各自HTTP/SSE、terminal、metadata与typed error mapping；两个adapter共享exact `reqwest = 0.13.4`的`json + rustls + stream`最小transport/framing primitives并显式关闭retry/redirect/ambient proxy，但request、terminal与typed envelope parser保持独立；ADR 0140 Accepted：Tool Sandbox class-level admission、approval revalidation与pre-start fail-closed合同关闭V4-C0-1；第四轮全部finding Closed。

当前M14 `fetch_url` milestone已完成完整本地门禁：`./scripts/check.sh`运行主crate library 1031 passed、3 ignored，integration 159 passed、3 ignored，以及standalone provider-gate 25/25；`./scripts/check-msrv.sh`显式锁定真实Rust 1.85 compiler并在隔离target运行主crate全部targets，结果同为library 1031 passed、3 ignored与integration 159 passed、3 ignored。Clippy、format、current/archive docs、Wire V1 144 active/0 pending与Durable Store fixtures全部通过。production network tests默认离线；两个live smoke harness在两套toolchain中均被编译且默认ignored，不读取env或访问network；2026-08-12显式release run已通过两协议，nonsecret evidence见ADR 0145。`fetch_url` milestone包含ADR 0147、shared locked-down transport、exact-origin/pinned-address authority、bounded executor、32种Tool composition与Runtime wiring；provider/filesystem历史证据保持不变。最近remote CI仍是M13 closure SHA `0951b12a9584c5f26f4757bf84cefb21e824d1dd`的GitHub Actions run `31518732896`，已在Ubuntu stable、Rust 1.85、macOS与Windows四个job全部success。

未实现边界不要误判为回归：M14已实现OpenAI Responses与Anthropic Messages两个direct private adapters、provider-native Structured strict mapping、bounded SSE、default local contract suites、host-only dynamic credential/catalog installation与explicit ignored live smoke harness，以及production `ask_user` builtin（ADR 0142：closed/default-off/opt-in、零permission、仅UserQuestion/frozen PreExecution plans、deterministic compact JSON answer）与narrow OS-backed production `read_file` builtin（ADR 0143：closed/default-off、`MiniCoreRuntimeConfig::with_read_file_tool()` idempotent opt-in且与`with_ask_user_tool()`相互独立可组合、cwd-relative-only单一required `path`、per-admission对exact captured Workspace materialize ToolSet、ReadOnly authority ceiling（`ReadOnlyWorkspaceAuthority`，永不`ReadWrite`且Prompt/Skill source ceiling false）、capability-relative cap-std open（symlink escape在capability open拒绝，无ambient path/`canonicalize`）、Execute plan恰带`FilesystemRead`且sandbox contract available exactly for `FilesystemRead`、至多65,537字节bounded读取→恰一个≤65,536字节UTF-8 Text part、frozen PreExecution/Completed texts、owner-tracked blocking job绝不drop/detach；host `invalidate_session_workspace_authority`对read_file Runtime先经`WorkspaceReadAccessControl::revoke`永久撤销该Session read grant（idempotent、Runtime lifetime内无unrevoke，recovery re-resolve只授予filesystem `None`、绝不恢复`ReadOnly`）再采样timestamp signal/re-resolve，restriction在residency返回SessionNotLoaded/Closing/internal时仍保持current；bytes/application work有界、special files nonblocking拒绝、ordinary regular file的wall-clock完成依赖OS/filesystem且不宣称timeout）以及production `list_directory` builtin（ADR 0144：closed/default-off/`with_list_directory_tool()` opt-in、共享filesystem authority与per-Session permanent revocation、empty path表示cwd、opaque directory capability-relative open、direct nonrecursive enumeration、不读content/不跟随entry symlink、256-entry/8,192 retained-name-byte/65,536 compact JSON bounds、UTF-8 name bytes排序、owner-tracked no-detach settlement）。五个production builtin组成32种fixed selection，固定顺序`ask_user → read_file → list_directory → write_file → fetch_url`。默认Runtime仍保持empty catalog且从不读取env/home；只有trusted host显式安装的model definition可解析，credential availability在attempt时独立决定。显式cache annotation与continuation已按ADR 0141冻结为有意omission（stateless full-request wire policy：每次`generate_model_turn`至多调用一次`ProviderAdapter::execute`——owner validation/pre-send cancellation/`AuthMissing`在调用adapter前以typed error terminal（零execute/零POST），adapter编码/build失败或adapter级pre-send cancellation为一次execute/零POST——独立地发送零或一个HTTP POST，若发送POST则携带完整full request，无optimization-specific fallback POST、重试或continuation state；省略不声称provider零自动缓存，正确性不依赖cache）；未来激活需独立provider-specific evidence/ADR并满足稳定credential binding、tenant/session privacy scope、canonical full-wire successor proof、retention/billing policy与one-POST reconciliation。provider real-credential smoke项已由ADR 0145关闭；exact-origin/pinned-address `fetch_url` network authority与executor已由ADR 0147关闭；process及其他未实现adapter、generic ToolService/schema/hooks/完整policy/approval与public Tool DTO冻结为post-MVP；Session-local mutation queue/mutation permit已由ADR 0146随`write_file`实现。production `ask_user` builtin的ToolName、closed input schema和answer→model-visible ToolResult text/render格式已由[ADR 0142](docs/adr/0142-production-ask-user-is-a-closed-opt-in-builtin.md)冻结并实现（closed/default-off、`MiniCoreRuntimeConfig::with_ask_user_tool()` idempotent opt-in、`ToolSet::ask_user_builtin()`、零capability permission+available empty sandbox、仅UserQuestion/frozen PreExecution failure plans、answer binding先经exact `validate_answer`再产生恰一个deterministic compact JSON Text part，render/invariant失败fail closed为Abandoned RuntimeFailure；绝不创建ToolExecutionStart/executor future/cancellation pair/start-gate reservation/approval/OS资源）；production `read_file`/`list_directory`/`write_file` builtins及其filesystem authority/revocation、bounded result与same-Session mutation contracts已由[ADR 0143](docs/adr/0143-production-read-file-uses-workspace-capabilities.md)/[ADR 0144](docs/adr/0144-production-list-directory-uses-bounded-capability-enumeration.md)/[ADR 0146](docs/adr/0146-production-write-file-binds-capability-targets-to-session-fifo.md)冻结并实现；production `fetch_url`的exact HTTPS origin、host-pinned addresses、reject-all DNS、bounded safe-text result与owner-contained cancellation已由[ADR 0147](docs/adr/0147-production-fetch-url-pins-exact-https-origins-to-host-addresses.md)冻结并实现；其余production ToolService/executor/adapters、完整generic schema/hooks/policy/Sandbox/permission enforcement、public Tool DTO、具体Prompt/Skill source与Skill composition冻结为post-MVP。没有真实producer前，不要伪造`DurableStateCorrupt`/`DurableStateTooLarge` loaded readiness；Prompt/Skill authoring grammar与Runtime consumer未冻结前，不要实现filesystem source adapter。

M13 Production Tool/Sandbox Gate（V4-C0-1）已由ADR 0140关闭：closed四类capability set、final `ToolPermissionSet` restricted candidate、adapter available/enforceable contract、exact差集、Sandbox unavailable fail-closed与fixed `PreExecution + Denied` mapping；direct Execute在任何ToolStartGate reservation/start factory poll前admit；Execute/Approval携带唯一permissions，Restricted option exact映射AllowWith，并在host allow后复验ceiling与captured Sandbox；非空、已admit permission plan的adapter-independent Session round conformance覆盖SecurityRevoked-before-start、Sandbox unavailable与Running cooperative truthful settlement。当前M14 OpenAI与Anthropic direct adapter/local contract slices、dynamic credential/catalog安装及explicit ignored live smoke harness均已完成；2026-08-12两个real-credential public Runtime release smoke均通过并由ADR 0145记录；generic production permission producer仍pending；Tool/Sandbox adapters中`read_file`/`list_directory`/`write_file` slices已完成（ADR 0143/0144/0146：closed/default-off opt-ins、filesystem ceiling/requested-access intersection与joint revocation、cwd-relative capability enforcement、bounded outputs、same-Session alias FIFO与permit-through-Settling、owner-tracked jobs），`fetch_url` network slice也已完成（ADR 0147：exact HTTPS origin + pinned addresses、无ambient DNS/redirect/retry/proxy/compression、bounded safe text与owner-contained cancellation）；process及其他未实现adapter冻结为post-MVP。任何新Tool adapter必须保持：无raw `tokio::spawn`脱离owner、started run不因signal drop、返回前提供有界可确认cleanup、Emergency mutex内只允许固定非阻塞exact reservation/transition。

本上下文描述MiniCore V2当前架构。ADR 0126已经把Turn执行重构为async loop并把Session持久化降级为inline best-effort recording；ADR 0127进一步将JSONL收口为不含Turn lifecycle的conversation recording；ADR 0132冻结Compaction stable-unit/settings/provenance contract；ADR 0133冻结snapshot-recoverable Runtime public payload、安全Interaction和metadata/command completion闭环；ADR 0134/0135冻结bounded wire与host-neutral Workspace input；ADR 0136冻结DurableState/Store V1/root lease（new-entity Create/Fork complete-or-invisible、existing-head update old-or-new），ADR 0137冻结Tokio owner-tracked deterministic foundation。M5.0 durable foundation与exact historical definition resolution已实现，仍无standalone reservation API/token/receipt。M5.1 DurableState-issued conversation target/proof与owner-tracked SessionRecorder physical append slice已实现；M6.1 Workspace resolver/immutable Snapshot、loaded Ready+Idle `SessionExecutor` publication owner、Runtime-owned Ready+Idle residency registry/single-flight Load/draining Unload/lifecycle exclusion与loaded/unloaded Workspace update统一路由foundation、PromptSet foundation、Runtime-owned PromptService/initial PromptResourceView、Workspace Prompt candidate capture，以及replay/Recorder-backed Ready+Idle Load hydration已实现；M6.2 scripted text-only ModelGateway foundation已实现，包含AgentRun assembly/proof、immutable catalog/TurnModelSnapshot/ModelCallRequest、single provider attempt、progress/final response/error validation、cancel linearization，以及Runtime-owned empty ModelGateway/initial catalog。M10已完整实现validated settings/Turn capture、Prompt/Model planning basis、pressure、large ToolResult reduction、checked stable-unit prefix plan、CompactionSummary assembly/request/validation、ActiveTurnTask retry/control arbitration、live Replace、inline best-effort marker recording与Snapshot publication。M11已完成并关闭public Session Fork command/outcome、全部公开message anchors、与Load/Unload共用gate的LiveSnapshot/RecordedHistory线性化选择、degraded/unrecorded live tail capture、bounded-memory child re-encode/readback、complete-or-invisible publication与restart recovery，durable `ListAgents`/`ListSessions`分页、bounded cursor store与`GetSessionForkProvenance`查询，snapshot-first Runtime subscription与Session/Agent durable summary、Load/Unload membership、Agent Create/Status/Definition/Metadata StateEvent及Session Metadata Runtime/loaded-Session StateEvent，public Agent Create/Enable/Disable/Delete/UpdateDefinition/UpdateMetadata与Session UpdateMetadata typed outcomes，以及typed ResolveInteraction、SessionDefinitionUpdated、Progress/Closed selected-V1 closure；public manifest现有144项active且无pending target；ordinary Session definition CAS已公开关闭（`SessionCommand::UpdateDefinition`经per-Session gate发布Workspace/Model/Prompt完整替换，loaded Idle Workspace变更安装prebuilt Snapshot并发布exact Runtime+Session `SessionDefinitionUpdated`事件，future-only Model/Prompt变更在active Turn期间安全提交且不调用Workspace resolver）；Agent revision upgrade（`SessionCommand::UpgradeAgentRevision`）亦已公开关闭：`None`钉住Agent current、exact target支持historical rollback，target current解析与Enabled/retained校验只在DurableState既有Agent→Session gates内完成，unloaded直接发布durable definition，loaded经executor既有publication slot原子安装同WorkspaceSnapshot并发布exact Runtime+Session事件，active Turn继续使用已captured旧ref而future admission与跨terminal FollowUp使用新ref，no-op/error无事件且不触碰conversation）。Ready-state `ReloadWorkspace`亦已公开关闭：`SessionCommand::ReloadWorkspace`只作用于loaded Session的current Workspace state，经residency per-Session gate路由，复用executor既有single active publication slot/permit/completion owner（不创建第二套actor），仅Idle接受，worker重新resolve exact installed definition.workspace→capture Workspace Prompt source→required authority revalidation→finish exact WorkspaceSnapshot且绝不调用DurableState，install前验证active permit、admission-time exact definition与snapshot SessionId/revision，成功后原子替换WorkspaceSnapshot Arc（保留exact definition Arc、metadata、execution Idle、queues/observation/recording/usage/diagnostics）并发布exact Session-scope `SessionWorkspaceReloaded`事件（Runtime scope不发事件），普通resolver/capture失败保留exact old snapshot Arc且不发事件；wire新增reload command/`workspace_reloaded`/`session_workspace_reloaded` fixture，manifest扩展为139项全部active。Workspace/Prompt Unavailable loaded readiness与ReloadWorkspace恢复亦已实现：`SessionExecutorSnapshot`携带显式`SessionReadinessView`与optional WorkspaceSnapshot，Load在Workspace resolve/capture/revalidation普通失败时安装带Unavailable cause的loaded executor（conversation照常replay并初始化Recorder，随后执行final durable exact recheck），非Ready+Idle Session对所有Submit返回typed `SessionNotReady`；ReloadWorkspace在Unavailable+Idle可运行，成功安装exact WorkspaceSnapshot并恢复Ready且发布既有Session-scope `SessionWorkspaceReloaded`事件，失败保持原cause/None，true Workspace definition update同样恢复Ready而future-only Model/Prompt与Agent upgrade保持Unavailable；Runtime/SessionSnapshot/wire readiness投影与legal matrix已接通。Agent readiness fan-out亦已实现：`SessionExecutorSnapshot`改为内部`agent_available`+`resource_unavailable`两事实并由getter派生public readiness（AgentUnavailable优先、其次resource cause、否则Ready），所有clone/update方法保留两事实；`SetStatus/Delete` durable Updated后Runtime按同一owner timestamp枚举loaded Sessions逐个经residency per-Session gate调用executor `set_agent_availability`（Idle+无active admission/Turn立即应用且仅readiness真实变化发布`ReadinessChanged`，Starting/Running/Finishing保存最新pending并在回Idle后应用，Turn terminal在FollowUp决策前应用，non-Ready不pop/start且保留queue以便Enable后handoff）；Agent Disabled/Deleted的Load仍返回Loaded并投影AgentUnavailable（保留last-good WorkspaceSnapshot与resource cause），future admission拒绝，ReloadWorkspace/true Workspace update只清除resource cause而future-only definition/Agent revision upgrade保留全部状态，Enable恢复底层Ready或原resource Unavailable，active Turn不变且final Agent admission gate仍决定Starting race；wire `session_readiness_changed`已激活（Session route、detail null）。ModelUnavailable load/definition projection亦已实现：`SessionExecutorSnapshot`新增独立`model_available: bool`事实并收窄重命名`resource_unavailable`为`workspace_unavailable`，readiness只在Idle按固定优先级为AgentUnavailable→workspace cause→ModelUnavailable→Ready（非Idle执行始终投影Ready；facts是future-only，new Unavailable只在回Idle后显现）；Load用现有`resolve_for_turn`按captured definition.model同步分类model_available（普通model incompatibility→false且Load仍Loaded，catalog owner/source/definition internal→现有internal load路径），任何install新definition的publication（future-only/Workspace change/Agent upgrade）install前按当前catalog计算新definition的model_available并与definition一起安装，ReloadWorkspace保留当前事实，true Workspace publication只清workspace cause，DefinitionUpdated/WorkspaceReloaded event snapshot只在Idle publication携带derived readiness（Running等非Idle执行时保持Ready、new facts经terminal/回Idle显现）且不发额外ReadinessChanged event。selected PromptUnavailable load/definition projection亦已实现：`SessionExecutorSnapshot`新增独立`prompt_available: bool`事实，readiness只在Idle按固定优先级为AgentUnavailable→workspace cause→selected prompt unavailable→PromptUnavailable→ModelUnavailable→Ready（非Idle执行始终投影Ready；facts是future-only，new Unavailable只在回Idle后显现）；Load独立await `prompt_available_for_definition`（`read_agent_definition`读exact retained Agent revision，复用`for_turn` selection阶段验证exact Agent+Session Prompt selection：missing/wrong role/duplicate resolved key→false且Load仍Loaded，Agent read的Closing→Load Closing、其余Agent read失败与owner/identity mismatch→internal load路径），任何install新definition的publication在durable commit后install前await同一helper（current installed Prompt resources），Closing/internal走既有active restore+`close_for_fatal(Internal)`，随后同步算model并原子安装definition+prompt/model事实，ReloadWorkspace保留当前两事实，DefinitionUpdated/WorkspaceReloaded event snapshot只在Idle publication携带derived readiness（Running等非Idle执行时保持Ready、new facts经terminal/回Idle显现）。shared-resource reload recovery亦已实现：`RuntimeCommand::Runtime(ReloadSharedResources)`在runtime publication semaphore内并行build Prompt/Model candidates，任一普通失败保留old roots/executors并返回`ReloadValidationFailed`+UserActionRequired且无事件；成功后对全部loaded Sessions用exact installed definition预计算selected Prompt/model可用性，经residency per-Session gate fan-out new PromptResourceView/ModelCatalogView至每个loaded executor（executor仅替换future TurnResources的Prompt/Model roots，active Turn/admission保留已captured旧context不cancel，Unavailable总是Idle故恢复立即生效，非Idle事实合并为单一pending availability composite并在terminal/admission failure后、FollowUp决策前应用，仅readiness真实变化的Session发布`SessionReadinessChanged`），随后一次原子替换Runtime root pair并发布Runtime-scope `SharedResourcesReloaded`（detail null）；external Submit在reload publication期间持shared-resource read gate直到Turn context admission完成。active-Turn graceful Unload亦已实现：`MiniCoreRuntimeConfig`新增private `unload_grace`（default 30s、`with_unload_grace` builder、open验证非zero且≤5min否则`InvalidConfiguration`），open时安装到residency actor并保留于`RuntimeInner`；public Unload经runtime publication gate与residency per-Session gate执行`prepare_for_unload(grace)`→`executor.close()`→`remove_exact`（loaded entry与exact permit保持安装），executor先同步关admission gate并经unbounded emergency lane接受`PrepareUnloadRequest{deadline}`，actor清空Steer/FollowUp并re-project queue、新Submit/Steer/FollowUp拒绝（Submit公开映`SessionNotLoaded`、Steer/FollowUp按既有TurnNotRunning contract）、重复request共享state且effective deadline只取更早；grace内active admission/Turn自然完成，deadline到期对exact current emergency target signal `PrepareForUnload`（sticky first-wins，更早Cancel/SecurityRevoked保留原reason）并cancel其cancellation token、投影Finishing（仅首次发`session_execution_changed`）、以`SessionUnloaded` settle pending Interactions且不直接drop task；Starting Submit在Input未live apply时经internal `SessionSubmitError::PrepareForUnload`公开映射`SessionNotLoaded`而非`SubmitCancelled`，Input先赢仍`TurnStarted`随后同一Turn `Interrupted(PrepareForUnload)`；admission failure/Turn terminal/publication settlement后`settle_prepare_unload_if_idle`，Idle接受Prepare立即settle且gate保持closed直到close；已Idle的prepared executor在close时不伪造Finishing event，internal→poison/Internal、registry已closing时drain+remove后映射Closing；registry shutdown先广播begin_prepare使grace并行计时再逐个await waiter再close（不累加N×grace、不spawn untracked tasks）；不新增queue_updated event，manifest现为144项active且无pending target；Unload pre-Input SessionNotLoaded与PrepareForUnload fixture/tests已补齐（scenario/fixture closure已实现，统一质量门禁已通过）。production host security Workspace authority invalidation亦已实现：`MiniCoreRuntime`新增public host-only（非wire command）async seam `invalidate_session_workspace_authority(session_id)`，redacted `SessionWorkspaceInvalidationError{RuntimeClosing,SessionNotLoaded,InternalDispatchUnavailable}`，host已先发布current hard restriction fact，Runtime只驱动loaded executor的signal+Workspace re-resolve，不改durable definition/revision/metadata/conversation、无CommandId；route不取runtime_publication semaphore、不等待普通work lane，经residency loaded map直接clone executor调用out-of-band API（先同步close turn_admission_gate再经unbounded emergency lane发送），采样single SystemClock timestamp；missing loaded executor或executor普通Closing且registry未closing（per-Session Unload/old exact executor race）→SessionNotLoaded，仅registry/runtime closing→RuntimeClosing，actor fatal→Internal。`SessionExecutorSnapshot`新增`workspace_preparing: bool`（所有clone/update保留），readiness优先级workspace_preparing→`Preparing`高于Agent/workspace cause/Prompt/Model，Preparing必须Idle+workspace None+空public queues/accepting false；enter Preparing drop旧WorkspaceSnapshot并mask workspace cause，finish success安装Some snapshot+false+清cause、ordinary failure安装None+false+WorkspaceUnavailable/PromptUnavailable，保留agent/prompt/model/metadata/usage/recording/diagnostics。`TurnAdmissionGate`新增`open()`；recovery完成后仅在未closing且无PrepareUnload时reopen（Unavailable仍由readiness提前拒绝）。actor新增最小`SecurityInvalidationState{timestamp, waiters, worker_task?}`：重复invalidation join同一state不重复signal/recovery；PrepareUnload/closing→settle Closing；active admission/Turn→exact current emergency target sticky `SecurityRevoked` first-wins+cancel security_revocation/cancellation token（更早Cancel/PrepareUnload保留原reason，terminal/admission cleanup后仍recovery；仅形成Turn投影Finishing、pre-Input Starting legal，pending Interaction按SecurityRevoked truthful settlement；即使active publication在飞也立即signal——publication不屏蔽security signal），waiter直到recovery final state安装后settle；仅无admission/Turn而active publication→立即进入Preparing（drop旧WorkspaceSnapshot、mask workspace cause、发布唯一一次`ReadinessChanged(None)`）但recovery worker仍等待publication settlement（不阻塞、不取消已到durable barrier的publication），settle后以post-publication exact definition启动recovery（enter幂等、不重复Preparing start event、settled snapshot不重新安装）；Idle且无active admission/Turn即进入Preparing（active publication不阻塞Preparing entry、只等Turn/admission settle）并发布`ReadinessChanged(None)`，recovery worker单独等待publication settle后才spawn（全空Idle立即spawn）。recovery复用ReloadWorkspace resolve/capture/revalidate/finish（最小shared async helper返回exact WorkspaceSnapshot或neutral classification，普通ReloadWorkspace映射不变）：security分类所有非internal resolver失败（含AuthorityDenied）→WorkspaceUnavailable、Workspace Prompt SourceDiscovery/ContentLoad/DuplicateKey→PromptUnavailable、Closing→typed Closing、shape/channel/task mismatch与Skill roots非空→fatal Internal；install前验证Arc ptr_eq与snapshot SessionId/revision，不调用DurableState。启动点：Idle immediate、admission failure cleanup后、Turn terminal event后FollowUp handoff前（先TurnInterrupted再ReadinessChanged(Preparing)）、publication success/ordinary settlement后；recovery pending禁止FollowUp pop/start，新Submit因gate close失败（Preparing→公开`SessionNotReady`+RetryWithBackoff）。start/finish各发布一次`SessionReadinessChanged`（command_id None），不发WorkspaceReloaded event、No Runtime event。`SessionExecutorEvent::ReadinessChanged.command_id`改`Option<CommandId>`（Agent/shared reload传Some、security传None、Runtime EventStream复用Option）。close/fatal/reap settle security waiters exactly once（closing→Closing、fatal/task/channel/shape→Internal），worker owner-tracked并reap；既有`security_revoke(target)` seam不变。manifest现为144项；wire `Preparing`由并行worker激活，host security Preparing/active Turn duplicate recovery fixtures/tests已补齐（scenario/fixture closure已实现，统一质量门禁已通过）。RuntimeDependencyUnavailable loaded readiness与probe recovery亦已实现：`SessionExecutorSnapshot`新增独立`runtime_dependency_unavailable: bool`事实，唯一真实producer是loaded Turn admission读取pinned historical AgentRevisionRef时`DurableState::read_agent_definition`的transient `StorageUnavailable`（不是host global bool，也不是`ReloadSharedResources`——shared-resource reload不触碰本fact，Tokio `RuntimeDependencyUnavailable`仍只属于open error），readiness只在Idle按固定优先级为workspace_preparing→Preparing、agent_available=false→AgentUnavailable、workspace cause→cause、prompt_available=false→PromptUnavailable、model_available=false→ModelUnavailable、runtime_dependency_unavailable→RuntimeDependencyUnavailable、否则Ready（非Idle执行始终投影Ready；facts是future-only，new Unavailable只在回Idle后显现）；首次失败settle回Idle后安装该fact并发布`ReadinessChanged(command_id None)`，Submit返回`SessionNotReady(RuntimeDependencyUnavailable)`+RetryWithBackoff并立即启动owner-tracked无TurnId probe（复用同一exact read路径），probe仍Unavailable则保持fact并等待next Submit re-arm，probe Recovered清fact、发布`ReadinessChanged(command_id None)`并保留retained FollowUp handoff；admission直接观察到AgentNotFound/RevisionUnavailable时分类为AgentUnavailable而非本cause；fact安装后的probe若发现同一retained ref消失则是durable invariant并进入internal/fatal，model recapture failure同样是internal invariant（不伪装为本cause），fatal/closing/corrupt/too-large不进入本cause，普通Unavailable保留last-good Workspace且active Turn不受影响；恢复只由exact DurableState read probe与Submit re-arm拥有，无新public/wire command，manifest现为144项；RuntimeDependencyUnavailable真实historical storage fault+probe/rearm+retained FollowUp fixtures/tests已补齐（scenario/fixture closure已实现，统一质量门禁已通过）。full recovery scenario/fixture closure已实现；M14五个closed/default-off production builtins均已完成：`ask_user`/`read_file`/`list_directory`/`write_file`/`fetch_url`（ADR 0142/0143/0144/0146/0147；四个ask/filesystem bool + materialized fetch authority Option形成32种fixed selection；filesystem authority/requested-access intersection与joint revocation、capability-relative bounded read/list/write、same-Session mutation FIFO、permit-through-Settling，以及exact-origin/pinned-address network authority、bounded safe text与owner-contained cancellation）；current `fetch_url` milestone的stable/真实Rust 1.85 acceptance均为library 1031 passed、3 ignored与integration 159 passed、3 ignored，stable provider-gate 25/25，Clippy/format/docs/Wire/Store fixtures全绿；crate-private Structured output foundation已实现（`OutputContract::Structured` exact-model contract、schema v1 subset、terminal本地schema validation与ScriptedProviderAdapter conformance），crate-private `ToolOperationSlot`完整生命周期亦已实现（Prepared→Running→Settling→Terminal：exact `ToolExecutionRequest` identity（ItemId + same `Arc<ToolCall>`）、per-slot first-wins gates、EmergencyControl owner mutex内exact unsignaled reserve + lock-free CAS、move-only `ToolStartPermit`→`ToolStartedExecution` proof、`run_started_execution`复验exact capture后调用move-only `ToolExecutionStart` factory、executor future只有proof后poll、signal/stale先赢→不调用factory且matching PreExecution Cancelled ToolResult、reservation/start先赢→Running持有`ToolCancellationHandle`、signal只触发cancellation observer且slot经Settling继续await same run后truthful settle（started run不因signal drop）、parent-owned join_all与per-boundary panic isolation→Abandoned），public Wire/fixtures保持144 active/0 pending不变；public structured activation、generic schema/hooks/policy/Sandbox、其余production ToolService/executor/adapters（返回前须提供有界、可确认cleanup）、public Tool DTO与具体Skill composition/source仍pending（production `ask_user`/`read_file`/`list_directory`/`write_file`/`fetch_url` builtins已由ADR 0142/0143/0144/0146/0147冻结并实现）；OpenAI Responses与Anthropic Messages direct adapters、provider-native Structured strict mapping、host-only dynamic credential/catalog installation与explicit ignored live smoke harness已实现；2026-08-12两个real-credential public Runtime release smoke均通过，nonsecret evidence见ADR 0145；完整native cross-platform matrix acceptance已通过（全部七个`platform_m5_0`坐标——lock_contention、lock_reacquire、lock_holder_death、root_lease_identity_loss、cleanup_open_handle、case_alias_rejected、symlink_reparse_rejected——均有对应的production行为与测试覆盖；GitHub Actions run 31433810296四个job全部通过：Ubuntu Rust stable、Ubuntu Rust 1.85.0、cargo test macos-latest、cargo test windows-latest）；M5.2 tolerant semantic replay seam与corruption sidecars已实现并通过独立全量验证，M5.1完整fixture gate已完成、统一native matrix acceptance已通过。

权威顺序：`docs/architecture.md`与`docs/modules/` → current/refined ADR → formats + fixtures → development plan → migration + research → archive。

## 核心术语

**MiniCore**：
可嵌入CLI、TUI、GUI可信宿主的原生Agent harness runtime core。负责Session、async Turn执行、Prompt、Tool、Skill、ModelGateway、Runtime协议、观察事件和best-effort recording。

**MiniCoreRuntime**：
下游host唯一顶层门面。`dispatch / query / snapshot / subscribe`是Wire-compatible transport families；Rust embedding另可调用library-only `session_transcript`与host-only lifecycle/security seams。内部拥有PromptService、ToolService、SkillService、ModelGateway和LoadedSessionExecutors，不保存UI selected Session。

**Runtime Interface**：
由RuntimeCommand/CommandResponse、RuntimeQuery/QueryResponse、RuntimeSnapshot/SessionSnapshot和StateEvent/ProgressEvent组成的transport-neutral interface；ADR 0148 transcript是并列的library-only read seam，不属于Wire V1 RuntimeQuery。

**Wire Schema**：
public/storage representation唯一owner。v1固定camelCase fields、snake_case variants、adjacent `type/data`、typed IDs/revisions、Timestamp/Duration/Money/path/cursor、ProtocolLimits、canonical BoundedJson和bounded scanner；不拥有domain business semantics。

**Wire V1 Fixtures**：
`docs/fixtures/wire-v1/`中的public target manifest、byte-exact JSON/JSONL、corruption expectations、boundary recipes和structural verifier。首个Rust codec/storage crate必须消费这些assets。

**Runtime-owned共享module**：
`PromptService`、`ToolService`、`SkillService`和`ModelGateway`。四个current immutable resource roots只在initialize或显式reload成功后整体publication。

**Agent**：
可被多个Session引用的durable definition owner。AgentRevision immutable；Session pin exact AgentRevisionRef，不自动跟随Agent current。

**Session**：
长期工作上下文。SessionDefinition绑定AgentRevisionRef、Workspace、Model config和Prompt selection。Conversation recording失败或process crash时可以缺少loaded live tail。

**SessionExecutor**：
每个loaded Session一个的control actor。拥有SessionIngress、lifecycle、Submit/FollowUp admission、active-task handle、Snapshot publisher和SessionRecorder handle。它不再拥有同步AgentLoop或RunningOperation。

**SessionExecutionHandle**：
Runtime内部路由到SessionExecutor的cloneable handle。下游host不能取得该handle。

**ActiveTurnTask**：
每个Running Session最多一个的async task。直接await ModelGateway、ToolSet、Interaction resolution、retry timer和Compaction，并拥有current Turn的异步控制流。

**ActiveTurnControl**：
SessionExecutor向ActiveTurnTask发送Steer、Interaction resolution、Cancel、SecurityRevoked和Lifecycle signal的crate-private channels/tokens。

**LiveSessionState**：
loaded Session的current-process truth，保存live conversation、Turn、Item、Interaction、usage和read model。它保留完整`Vec<Arc<StoredSessionEntry>>` selected path、由末项导出selected head；EntryId-only不能materialize unrecorded live tail供future LiveSnapshot/Fork。all fallible preparation结束后才allocate；allocation后先construct exact entry Arc、再bind prepared new-origin stable unit、commit state、append returned `AppliedConversationFact`的**same Arc**到path并install preflighted revision，绝不在entry construction前apply state。它通过private typed methods修改；`Interaction` fields/raw state只由其request/resolution transition methods构造和改变，sibling只读safe projection；任何lock guard不得跨await。

**LiveConversation**：
模型协议安全的current-process conversation reducer。它拥有expected ToolCalls、first terminal result、complete exchange、Compaction Replace和ConversationRevision；`ModelMessage`构造/provider projection仍只调用Prompt-owned constructors。

**LiveConversationView**：
PromptSet可消费的sanitized只读view。private fields，只有crate-private revision/messages getter；LiveSessionState在`capture_conversation_views()`中构造。只包含provider-valid messages；incomplete、orphan或abandoned-first Tool exchange被排除。M4没有generic live/replay trait或public DTO。

**LiveCompactionSourceView**：
Live reducer额外提供的crate-private immutable Compaction projection。`LiveCompactionUnit`与source view是private-field Arc-backed `Clone` handles，clone保持origin/kind/order/message semantic identity，不重建unit。Compaction语义拥有`PreparedLiveCompactionUnit::for_live_reducer(kind, messages) -> Result<_, CompactionSourceError>`和infallible `bind_origin(self, EntryId) -> LiveCompactionUnit`，以及source factory/read getters和唯一deep `has_same_stable_identity(&self, other: &Self) -> bool` method；preparation在new ID allocation前完成all message/kind validation，source factory仍返回own redacted `CompactionSourceError { EmptyUnitMessages | DuplicateUnitOrigin | MisplacedRollingSummary }`。它绝不返回或依赖`LiveConversationError`。LiveSessionState仍是canonical producer，并在factory caller boundary映射该error到own typed live error。该method精确比较Session/revision、unit count与ordered`(first_entry_id, kind)` sequence，绝不比较message value；source不存储或暴露identity DTO。Tool exchange不可拆，rolling summary origin是对应StoredCompaction outer EntryId；retained suffix只clone fresh current source units。它不携带token estimate或settings。

**CapturedConversationViews**：
`LiveSessionState::capture_conversation_views()`一次短capture返回`Result`的crate-private aggregate：同一revision的LiveConversationView、`Arc<LiveCompactionSourceView>`、从full state path末项导出的selected head、`Arc<[ItemRelation]>`和RequestId/TurnId/ItemId + safe request view的`Arc<[PendingInteractionFact]>`。M4 read scope只暴露head；state保留full applied facts给future LiveSnapshot/Fork。private fields/getters only；不是M8 public Snapshot，也不是Fork LiveSnapshot。

**CompactionSettingsSnapshot**：
Turn admission从Runtime-global validated CompactionSettings捕获的immutable policy。MVP无hot reload或per-Session override；默认pressure reserve 4096、summary output 512–2048、minimum reclaim 2048、每Turn最多4次Compaction、summary safety reserve 512。

**ConversationRevision**：
process-local单调live-conversation operation basis，不是当前可见消息的hash/version。Input/Steer与每个accepted Assistant（含hidden ToolCalls）各`+1`；complete exchange在所有expected calls first truthful Completed时再`+1`；partial/abandoned/non-visible settlement、Interaction、progress/usage/recording与failed/idempotent apply均`+0`；Compaction Replace `+1`。checked overflow先于EntryId allocation/state mutation失败。ModelCallRequest、logical retry和Compaction source/plan使用它验证stale result；它不持久化，不跨restart比较。

**EntryIdGenerator**：
`LiveSessionState`私有持有的Session-scoped identity generator。`allocate()`是typed fallible operation：16 CSPRNG-byte candidate最多32次、unique candidate在return前reserve；entropy或collision exhaustion是owner-local redacted error，不panic且不改变state/head/revision。domain validation和revision overflow preflight后、live apply前分配并绑定parent_id；replay/Fork全部reserved copied IDs seed collision guard。Degraded不影响分配，Recorder不能创建或改写ID，也不得从revision/ordinal/time派生ID。

**SessionRecorder**：
每个loaded Session一个的有序inline best-effort记录器。`record(entry).await`顺序encode并append稳定conversation fact，不使用background queue或durable commit receipt。其filesystem blocking job由owner追踪/join；started append不因Cancel/Unload/drop而detach，terminal/unload等待settlement。TurnStatus与terminal reason不进入Recorder。

**RecordingHealth**：
Recorder内部状态`Healthy | Degraded { reason, failed_entry_id }`。Create严格stage initial SessionHeader；每次Load都尝试初始化Recorder。Recorder第一次initialize/encode/write失败后Degraded并停止后续记录，replay最多恢复此前有效完整行前缀。Degraded在同一loaded instance内为终态，不retry、不创建segment、不backfill；recording failure不终止Turn、不使Session execution Unavailable。

**SessionRecordingView**：
公开`SessionSnapshot.recording`使用`{ state: healthy | degraded }`。first `Healthy → Degraded`发布一次`session_recording_changed`，同一Snapshot保留至少一条当前脱敏recording diagnostic。raw I/O error、路径和entry内容不公开。

**DurableState**：
private deep module，拥有local Store V1、permanent ID reservation、root lease、single actor、generation/CAS/marker publication/readback、catalog recovery/cleanup、poison和filesystem fault seam。它不暴露staging/path/generation/marker；`CommandId`不进入它。

**ConversationStorage**：
拥有SessionHeader/JSONL、tolerant replay、history tree/query和Fork semantic seed。它通过DurableState-issued opaque published target、RecordedHistory lease和root-lease-derived writable proof工作，不拥有entity path/publication，也不向async loop签发committed delta。

**StoredSessionEntry**：
SessionRecorder可能写入的一条immutable Format V1 conversation entry。exact wire fields依次为`entryId`、`parentId`、`sessionId`、`turnId`、`timestamp`和`body`；body是User/Assistant/Tool/InteractionRequested/InteractionResolved/Compaction六种snake_case flat variants。EntryId由live owner在apply前分配，Recorder不能创建或改写。

**Recorded prefix**：
process crash或recording degradation后实际留在JSONL中的完整行前缀。restart只能恢复该prefix，未record live tail永久丢失。

**Tolerant replay**：
顺序bounded读取recorded完整行，strict Header；session match先于EntryId collision reservation，随后skip duplicate并隔离orphan/invalid relation。first valid root建立canonical component，component内physical-last accepted leaf决定selected path；排除incomplete Tool exchange并返回typed bounded diagnostics。不恢复ActiveTurnTask、provider stream、Tool task、waiter、queue、retry timer或旧TurnStatus；Load后的current Turn为空。

**ForkSourceKind**：
Fork在source linearization point选择的事实来源：loaded Session固定为`LiveSnapshot`，unloaded Session固定为`RecordedHistory`。该值进入child durable fork provenance和`SessionForked`结果。

## Turn与执行

**Turn**：
一次current-process用户意图执行，从live Input UserMessage开始，到Completed/Interrupted/Failed terminal结束。一个Session同时最多一个Running Turn。JSONL只用TurnId分组conversation facts，不保存Turn lifecycle。

**TurnExecutionContext**：
Turn admission时捕获的immutable execution binding，固定AgentRevisionRef、SessionDefinitionRevision、WorkspaceSnapshot、PromptSet、ToolSet、SkillView、TurnModelSnapshot与Runtime-global validated CompactionSettingsSnapshot。M10 planning façade只使用同一次capture生成的AgentRun/CompactionSummary Prompt basis及exact model basis，active Turn不读取future Runtime config。

**TurnExecutionPhase**：
`Sampling | ExecutingTools | WaitingApproval | WaitingForUserInput | RetryBackoff | Compacting`。只属于live observer state，不记录为Turn lifecycle。

**Async run loop**：
ActiveTurnTask中的普通async Model→Tool→Model流程。first-party MiniCore implementation，不由Rig或其他SDK runner驱动。

**Steer**：
针对exact Running Turn的process-local FIFO输入。只在完整assistant/tool step后、下一次Model前消费一条，并apply为live UserMessage后best-effort record。

**FollowUp**：
等待当前Turn结束后创建新Turn的process-local FIFO输入。新Turn重新capture TurnExecutionContext。

**CancelAccepted**：
确认sticky cancel epoch已发布。Starting阶段保持`Submit CommandId` target：Input live apply前取消candidate，apply后绑定同一Turn并阻止ActiveTurnTask spawn；response publication后使用TurnId。它不等待Tool settlement、Turn terminal或Session recording。

**SecurityRevoked**：
WorkspaceAuthority/host发布的process-local emergency signal。阻止新Model/Tool/source operation；Running Tooltruthful settle，Turn结束后重新resolve Workspace。

**ToolStartGate**：
Tool side-effect start与Cancel/SecurityRevoked的owner-local first-wins gate。它不持久化，不依赖SessionRecorder。当前已实现crate-private correctness slice：per-round concrete gate以exact `ToolExecutionRequest` identity（ItemId + same `Arc<ToolCall>`）绑定，reservation在EmergencyControl owner mutex内对exact unsignaled target/epoch执行lock-free atomic CAS，move-only `ToolStartPermit`经Reserved→Started transition产生typed `ToolStartedExecution` proof，executor future只在proof存在后poll；signal/stale先赢时产生matching PreExecution Cancelled ToolResult，reservation/start先赢后只能truthful settle（exact Executed/Abandoned）。

**Logical model retry**：
ActiveTurnTask对同一个`Arc<ModelCallRequest>`执行的有限retry。使用control_generation与ConversationRevision验证，backoff可被Cancel/SecurityRevoked打断。

## Prompt、Tool、Skill与Model

**PromptService / PromptSet**：
PromptService拥有definitions/materialized content/source/cache；每Turn构造immutable PromptSet。PromptSet是`PromptIntent → CanonicalUserMessage`和`LiveConversationView → AssembledModelContext`的唯一seam。

**PromptContent**：
Prompt candidate build期间已经读取、解析和规范化的immutable text value。多个definition/Turn可以通过进程内强`Arc`共享正文；path、URL、source ID、hash或cache key不承担正文resolver或durable identity。

**PromptIntent**：
用户body与ordered SkillIntent selections组成的结构化输入。MVP body只有Empty或non-empty Text；不定义Template、Skill或Composite顶层variant。队列保存intent，不提前展开Skill正文。

**SkillIntent**：
显式请求本次用户消息使用某个Skill的稳定选择，只保存SkillId；name、path与source authorization不属于intent。

**CanonicalUserMessage**：
PromptSet规范化产生、可apply到LiveConversation并best-effort record的标准UserMessage。

**PromptContribution**：
Skill/Workspace等module产生的typed User内容。exact source authorization在composition前验证；每个contribution形成独立顶层content part并进入CanonicalUserMessage和LiveConversation，不能作为current-call assembly旁路。

**PromptContributionStamp**：
通过`content_part_index`关联一个顶层content part的安全解释元数据。只保存SkillId或WorkspaceRootKey加relative location；不保存字符offset、绝对路径、authorization或正文引用。

**ModelMessage**：
Prompt拥有的crate-private opaque唯一provider-neutral transcript；Prompt alone construct/destructure private kinds。`ModelMessage`与`ModelAssistantContent`是immutable Arc-backed `Clone` values，clone保持semantic identity/order/provenance，是将同一message投影到stable unit和flattened LiveConversationView的唯一方式。read-ref enums和`as_ref()`都不是external API；ProviderAdapter、Compaction estimator/reduction及Prompt assembly/tests等authorized consumers只能inspect `ModelMessageRef`/`ModelAssistantContentRef`：`ModelMessageRef::User { content: &[MessageContent] }`不含stamp，且stamp通过refs不可能访问；Assistant是ordered opaque content；Tool是`ToolCallId + ToolResultContent`。`ModelAssistantContentRef`只读Reasoning、Text或`{ tool_call_id, name, arguments }`。完整ReasoningContent含portable `provider_item_id`这一明确允许的fixture/storage exception；response ID、stream/final index/order bookkeeping、metadata、usage等provider-attempt facts禁止进入。public `PromptValueError`保持不变；transcript constructors返回private redacted `ModelMessageError { EmptyText | UnsafeText | TextTooLong | EmptyAssistantContent | DuplicateToolCallId }`。`rolling_summary()`只可达text reasons（含任意CR或CRLF、无normalization），accepted summary恰为一条unstamped verbatim User/Text，无label/envelope/stamp；assistant constructor独立覆盖empty/duplicate reasons。`unstamped_user_text()`保持独立静态context规则。Storage/Wire/Compaction不得定义shadow transcript。

**AssembledModelContext**：
PromptSet产生的唯一provider-neutral模型输入，包含ordered System sections、User context、sanitized messages、ToolSpec、OutputContract和assembly proof；没有flat contribution_stamps。stamp只留在各User ModelMessage，既非provider payload/cache-control input，也非source locator/authorization。

**ToolService / ToolSet**：
ToolService拥有definition/registry/policy/sandbox/executor；每Turn构造immutable ToolSet。ToolSet只返回ToolExecutionOutcome，不修改LiveSessionState、不写SessionRecorder、不推进async loop。

**ToolCallId**：
由ModelGateway adapter归一化的response-local opaque correlation ID。同一assistant response内唯一；durable/live correlation使用TurnId + ItemId + ToolCallId。

**Complete Tool exchange**：
Assistant的全部expected calls都有first truthful ToolResult后，LiveConversation才把ordered Assistant + ToolResults暴露给下一次Model。recording failure或crash可以留下不完整exchange，cold sanitizer再次执行该规则。

**ToolExecutionControl**：
ActiveTurnTask注入ToolSet的crate-private interface，用于approval、UserQuestion和ToolStartGate。它不暴露Session state。完整trait（`request_approval`/`request_user_question`/`reserve_execution_start`）仍是future target；其start/cancel部分已由Session Execution的`ToolOperationSlot`（Prepared→Running→Settling→Terminal：Prepared绑定exact request+EmergencyControl handle/observation+per-slot `ToolStartGate`，Running持有`ToolCancellationHandle`与same started run；signal先赢→不调用factory且PreExecution Cancelled；start先赢→signal只触发cancellation observer、slot经Settling继续await same run后truthful settle）+ Tools的`ToolStartPermit`/`ToolStartedExecution`/move-only `ToolExecutionStart` factory组合实现，approval/UserQuestion lane仍pending。

**SkillService / SkillView**：
SkillService负责discovery、metadata、captured content和cache；Turn-pinned SkillView只使用capture时的immutable source。

**ModelGateway**：
共享深module，通过`resolve_for_turn`固定TurnModelSnapshot，通过`generate_model_turn`执行最多一个provider attempt。当前scripted text-only implementation由Prompt使用pinned estimator/context limit先验证final input，再验证exact assembly proof、输出上限、progress、terminal content/finish、delivery-aware typed error和cancel/terminal first-wins；包括`RateLimited`在内的retryable reason只有safe delivery proof时保留。它不拥有Session、conversation、Tool或logical retry。

**ProviderAdapter / Direct Provider Adapters**：
ModelGateway private seam，只执行具体provider request/stream/cancellation/response mapping。M12只批准OpenAI Responses与Anthropic Messages baseline；adapter/private HTTP client automatic retry固定为0，protocol terminal与allowlisted metadata由`OpenAiResponsesProviderAdapter`或`AnthropicMessagesProviderAdapter`在wire owner内保存。M14已实现两个direct adapter/local contract slices、host-only dynamic credential/catalog installation与explicit ignored live smoke harness；2026-08-12两个real-credential public Runtime release smoke均通过，nonsecret evidence见ADR 0145。Rig只存在于standalone evidence harness，provider/transport type不得越过private seam。

**ModelCallRequest**：
ActiveTurnTask创建的immutable provider-neutral request，包含TurnModelSnapshot、purpose、`Arc<AssembledModelContext>`、source ConversationRevision和effective output limit。

**ModelCallResult / ModelCallError**：
Gateway的一次terminal success或typed failure。ActiveTurnTask验证live basis后apply response；recording outcome不影响provider result真实性。

**StreamingItem**：
Model stream的process-local AgentMessage/Reasoning累积buffer。ProgressEvent使用stable ItemId；provider final成功后apply为live Item并完成inline record attempt。

**CompactionPlan**：
M10已从exact `LiveCompactionSourceView`、Turn-captured settings、Prompt assembly bases和TurnModelSnapshot basis构建immutable checked plan。planner按stable-unit从旧到新选择first feasible nonzero cut，求交summary configured/model/context output budget并验证post-Replace headroom与minimum reclaim；plan只保存source + cut，summary prefix、retained suffix和`first_kept_entry_id`均由cut派生。超过16 KiB的大ToolResult只在summary source中按format version 1保留最多4 KiB head与4 KiB tail，并记录original/omitted bytes；live/durable ToolResult不改写。PromptSet组装required-only System、source+directive、empty ToolSpec与`NoToolCalls`，proof和ModelCallRequest exact绑定plan budget/revision/model；`validate_summary()`只接受matching model、portable finish、exact one Text和0–1 retry，保留完整automatic provenance并生成production sealed replacement。ActiveTurnTask验证exact Turn/control/plan/request/session/revision，发布`Compacting` phase，完成至多一次logical retry、live Replace、same-Arc inline record与post-compaction Steer safe point；recording failure保留live summary并在下一次AgentRun前刷新Snapshot usage/recording。reducer consume后可clone the prebuilt immutable rolling summary into leading unit/flattened view，retained units只clone from fresh current source，均不重建borrowed message或caller suffix。all fallible preparation/checked-next preflight先于allocation；之后construct exact entry Arc → bind prepared rolling-summary origin → commit Replace → append same Arc → install revision → best-effort record。M4只关闭无await/no-I/O reducer subset；M5拥有bad recorded marker ignore/diagnose；automatic M10 model-call provenance始终为Some。

## Turn、Item与Interaction

**Item**：
Turn内稳定可观察对象：UserMessage、AgentMessage、Reasoning或ToolInvocation。final live mutation产生authoritative Item，随后完成inline record attempt。

**Interaction**：
Item执行期间MiniCore发起的ToolApproval或UserQuestion。ordinary message apply只收valid-by-construction Stored User/Assistant/Tool body，连同orchestration-supplied `TurnId + Timestamp`，不定义message candidate；Interaction是唯一private-candidate exception：request为RequestId + ItemId + owner `InteractionRequest`，连同`TurnId + Timestamp`，resolution为RequestId + optional host key + opaque owner `ResolvedInteraction`，只另收`Timestamp`。`Interaction` fields/raw state只属于LiveSessionState；its transition methods alone construct/resolve it，siblings只读safe projection。`host(...) -> Result<_, InteractionCandidateError>`只接受ToolApproval/UserAnswer或Cancelled(HostCancelled)并seal Some key；`owner_cancellation(...) -> Result<_, InteractionCandidateError>`只接受Cancelled non-Host并seal None，wrong origin在apply/EntryId allocation前拒绝。reducer从exact stored pending request导出resolution的TurnId/Item/family、safe stored body并保留private resolution。reducer绑定SessionId/EntryId/parent并验证supplied TurnId的current/start semantics；Timestamp是Session/Turn orchestration提供的typed fact，绝不读ambient clock，Input start之前也不宣称TurnId/timestamp可从state导出。每Item最多一个Pending Interaction；terminal resolution后允许顺序later interaction。request/resolution先apply live、完成inline record attempt再notify/resume，但它们model-invisible且ConversationRevision `+0`。same-key/same-payload resolution是no-ID/no-entry/no-record/no-event的idempotent outcome。record failure不阻止notify或resume。

**InteractionView**：
公开UI-safe request/resolution view。Presentation Adapter不能创建虚假Pending Interaction或持有Tool waiter。

**StateEvent**：
当前subscription内按序交付的live observer record。final domain event从live mutation派生，可以领先recorded history；Turn terminal StateEvent不进入JSONL，restart后不重放。

**ProgressEvent**：
可合并/丢弃的streaming、Tool output或retry update。它不进入LiveConversation或SessionRecorder。

**SessionSnapshot**：
一个loaded Session的live observer baseline，包含execution、current Turn、live Items、Pending Interaction、queues、usage、recording health和diagnostics。它不是durable checkpoint。

**Snapshot-first subscription**：
订阅第一帧返回完整Snapshot，随后交付实时event。断线或背压后重新subscribe；restart后的新Snapshot只基于recorded conversation prefix和new live state，`current_turn`为空。

## 生命周期与Workspace

**Workspace**：
`SessionDefinition.workspace`中的Session-owned definition。没有WorkspaceId或Runtime-global registry。

**WorkspaceRootInput**：
public Create/Update命令中的host-neutral Workspace root intent，具体字段为`path: CanonicalFileUri`；它不是durable native path。typed command进入Runtime后由Workspace按current host checked-lower为`WorkspaceRootSpec { path: PathBuf }`；unsupported family是accepted command的InvalidArgument，不是wire decode failure。

**WorkspaceRootSpec**：
durable `Workspace`中的current-host native root definition。只能由Workspace checked lowering或trusted native constructor形成，不越过public input seam。

**WorkspaceSnapshot**：
Turn admission时resolve的immutable Workspace结果。active Turn不读取future Workspace definition。

**SessionReadiness**：
loaded Session是否可admit future Turn。Workspace/Agent revision不可用可导致Unavailable；SessionRecorder Degraded不会。

**SessionExecutionState**：
`Idle | Starting | Running | Finishing`。Running表示ActiveTurnTask存在；Finishing表示停止新逻辑推进并settle。

**Unload**：
停止admission，等待/取消ActiveTurnTask，然后删除loaded handle。Recorder没有后台queue；task结束后不存在待drain record tail。Degraded health与unrecorded live tail随loaded instance销毁。forced process exit可以中断当前append。

**Fork**：
创建新SessionId。loaded source从同一immutable LiveSnapshot解析anchor并复制selected path，因此可以包含unrecorded live tail；unloaded source复制tolerant replay得到的RecordedHistory。Fork不复制task、waiter、queue、Tool process、Recorder object或in-flight append。

## Runtime命令与观察

**RuntimeCommand**：
可信host提交的typed mutation/work request，包括Agent/Session lifecycle、Submit/Steer/FollowUp/Cancel、Resolve Interaction和CommandSurface action。

**CommandId**：
当前process内command correlation和in-flight去重ID。Submit在TurnId创建前也使用CommandId作为Cancel target。不跨restart恢复。SessionSnapshot完整列出当前可取消Submit/Steer/FollowUp CommandId，不公开queued prompt正文。

**InteractionResolutionKey**：
Presentation Adapter为一次logical Resolve生成的不可预测random 128-bit key。exact request内same key/same canonical payload幂等；same key/different payload冲突；不同key不能覆盖terminal resolution。它不是approval capability。

**Metadata revision**：
AgentMetadataRevision与SessionMetadataRevision分别为metadata CAS token；与AgentRevision/SessionDefinitionRevision正交。Create/read/outcome/event闭合下一次UpdateMetadata所需token。

**CommandSurface**：
Runtime内部无状态命令解释module。slash text和GUI catalog selection最终解析为同一typed RuntimeCommand或PromptIntent。

**RuntimeQuery**：
只读typed request。recorded history query与loaded live Snapshot是不同read path。

**Recording degradation**：
Host从`SessionSnapshot.recording.state = degraded`知道当前Session已停止后续记录；`session_recording_changed`提供实时transition，Snapshot中保留当前脱敏diagnostic供重连恢复。

## 已删除术语

以下名称或语义已经被ADR 0126/0127删除，不得用于新实现：

```text
AgentLoop
AgentLoopAction
next_action
accept_committed_tool_results
accept_committed_steer
RunningOperation
OperationResult
SessionWriter
CommittedSessionEntry as execution permit
CommittedConversationView
CommittedToolExchangeDelta
CommittedSteerDelta
ConversationCheckpoint as live proof
Transcript-First
append/apply commit barrier
writer-poisoned Session Unavailable
StoredTurnStart
StoredTurnTerminal
HistoricalFork terminal closure
cold recovery Turn terminalization
PromptIntent::Skill / PromptIntent::Composite / PromptBodyIntent::Template
```

## 当前开放问题

- 第四轮评审：全部普通V4-P0/P1与conditional V4-C0-1已关闭；V4-P1-3由M12/ADR 0138/0139关闭，V4-C0-1由M13/ADR 0140关闭；
- M0–M13已完成并通过本地stable/MSRV统一门禁；M13 closure SHA `0951b12`的GitHub Actions run `31518732896`四平台全部success；
- M12冻结OpenAI Responses/Anthropic Messages production baseline、terminal/metadata与delivery/error contract，并明确拒绝Rig进入Rust 1.85 production baseline；M14已实现两个direct private adapters、provider-native Structured strict mapping、默认离线production contract suites、host-only dynamic credential/catalog installation与explicit ignored live smoke harness，stateless full-request wire policy已由ADR 0141冻结（含OpenAI store=false/无previous_response_id与prompt cache字段、Anthropic递归无cache_control/无anthropic-beta、credential逐attempt解析、Anthropic provider_total_tokens恒为None）；2026-08-12两个real-credential public Runtime release smoke均通过，固定产品User-Agent与Anthropic wire refinements由ADR 0145记录；
- v0.1 MVP：provider real-credential smoke由ADR 0145关闭，首个file-mutation adapter与Session-local queue/permit由ADR 0146关闭，首个resource-level network adapter由ADR 0147关闭；五个production builtins `ask_user`/`read_file`/`list_directory`/`write_file`/`fetch_url`已冻结并实现。process、generic ToolService/policy/schema/hooks、Structured public activation、具体Prompt/Skill source与public Tool DTO冻结为post-MVP；Public Wire V1与Durable Store V1均为closed exact schemas，Structured activation若进入SessionDefinition必须走独立protocol minor与storage migration。

Recorder特有问题见`docs/review/async-loop-best-effort-recording-open-questions.md`。
