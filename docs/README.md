# MiniCore V2 Documentation

本目录默认只把current V2合同暴露给开发者和AI。历史决策仍保存在`docs/archive/`与Git history中，但不参与当前实现判断。

## 开发入口

按以下顺序阅读：

1. [开发计划](development-plan.md)：阶段、依赖、测试与退出条件；
2. [架构总览](architecture.md)：领域模型、执行结构与跨模块不变量；
3. [模块总览](modules/README.md)：canonical owner与具体接口合同；
4. [ADR索引](adr/README.md)：current、refined与historical决策分类；
5. [Wire Schema](modules/wire-schema.md)、[Conversation JSONL V1](formats/conversation-jsonl-v1.md)、[Wire V1 fixtures](fixtures/wire-v1/README.md)、[Durable Store V1](formats/durable-store-v1.md)及其[Durable Store V1 fixtures](fixtures/durable-store-v1/README.md)：public/storage representation；
6. [第四轮设计评审](review/v2-design-review-4.md)：尚未关闭的production gates。

权威顺序：

```text
architecture.md + modules/
→ current/refined ADR（以adr/README.md分类为准）
→ formats/ + fixtures/（representation与conformance）
→ development-plan.md（实施顺序，不改变领域语义）
→ migration/ + research/
→ archive/（历史，不具权威性）
```

## 当前状态

M0–M6 foundations、M7 ordinary AgentRun与M8最小Tool/Interaction/Cancel路径已完成；M9当前control/observation范围已接通Steer/FollowUp、queued cancellation、logical retry、EmergencyControl、runtime-wide CommandId dedup、Starting/Running/Finishing Snapshot及`session_execution_changed`/terminal StateEvent，并通过Wire V1 active fixtures。M10已完成Runtime-global validated Compaction settings、Turn-pinned Prompt/Model planning basis、pressure、deterministic large-ToolResult reduction、checked stable-unit prefix plan、CompactionSummary request/validation、ActiveTurnTask live Replace、inline best-effort marker recording、phase/Snapshot publication与下一次AgentRun。M11已接通Session Fork、durable Agent/Session catalog与Fork provenance query、snapshot-first Runtime subscription和Session created/loaded/unloaded/forked StateEvent，完成public Session Archive/Unarchive/Delete/UpdateMetadata及Agent Create/Enable/Disable/Delete/UpdateDefinition/UpdateMetadata、typed outcomes/NoChange与matching StateEvent；loaded Session metadata更新同时发布exact SessionSnapshot。ordinary Session definition CAS亦已关闭：`SessionCommand::UpdateDefinition`发布Workspace/Model/Prompt完整替换并生成新SessionDefinitionRevision，loaded Idle Workspace变更安装prebuilt Snapshot、future-only Model/Prompt变更在active Turn期间安全提交，二者均发布exact Runtime+Session `SessionDefinitionUpdated`事件。typed ResolveInteraction、SessionDefinitionUpdated、Progress/Closed codec与public manifest全部target亦已关闭，当前manifest为139项active。Agent revision upgrade亦已关闭：`SessionCommand::UpgradeAgentRevision`以`None`钉住Agent current或以exact target执行historical rollback，target current解析与Enabled/retained校验只在DurableState的Agent→Session gates内完成；unloaded直接发布durable definition，loaded经executor既有publication slot原子安装同WorkspaceSnapshot并发布exact Runtime+Session `SessionDefinitionUpdated`事件，active Turn继续使用已captured旧ref、future admission与跨terminal FollowUp使用新ref，no-op/error无事件且不触碰conversation。Ready-state `ReloadWorkspace`亦已关闭：`SessionCommand::ReloadWorkspace`只作用于loaded Session的current Workspace state，复用executor既有single active publication slot，仅Idle接受，worker重新resolve exact installed definition.workspace→capture Workspace Prompt source→required authority revalidation→finish exact WorkspaceSnapshot且绝不调用DurableState，install前验证exact definition与snapshot SessionId/revision，成功后原子替换WorkspaceSnapshot Arc并发布exact Session-scope `SessionWorkspaceReloaded`事件（Runtime scope不发事件），普通resolver/capture失败保留old snapshot且不发事件，错误映射`Unavailable`/`Unauthorized`/`ReloadValidationFailed`/`SessionNotLoaded`/`SessionBusy`；wire新增reload command/outcome/session-state fixture。Workspace/Prompt Unavailable loaded readiness与ReloadWorkspace恢复亦已关闭：Load在Workspace resolve/capture/revalidation普通失败时安装带Unavailable cause的loaded executor（继续replay conversation并初始化Recorder，随后执行final durable exact recheck），非Ready+Idle Session对所有Submit返回typed `SessionNotReady`；ReloadWorkspace在Unavailable+Idle可运行，成功恢复Ready并发布既有`SessionWorkspaceReloaded`事件，失败保持原cause/None；true Workspace definition update同样恢复Ready，future-only Model/Prompt与Agent upgrade保持Unavailable；Runtime/`SessionSnapshot`/wire readiness投影与legal matrix已接通。Agent readiness fan-out亦已关闭：`SessionExecutorSnapshot`改为内部`agent_available`+`resource_unavailable`两事实派生public readiness（AgentUnavailable优先、其次resource cause、否则Ready），`SetStatus/Delete` durable Updated后Runtime按同一owner timestamp经residency per-Session gate逐个fan-out（Idle立即应用，Starting/Running/Finishing保存最新pending并在回Idle后应用，Turn terminal在FollowUp决策前应用，non-Ready不pop/start且保留queue以便Enable后handoff，仅readiness真实变化发布`SessionReadinessChanged` Session StateEvent）；Agent Disabled/Deleted的Load仍返回Loaded并投影AgentUnavailable，Enable恢复底层Ready或原resource Unavailable，active Turn不变，future admission拒绝，future-only definition/Agent revision upgrade保留全部状态。ModelUnavailable load/definition projection亦已关闭：`SessionExecutorSnapshot`新增独立`model_available: bool`事实并收窄重命名`resource_unavailable`为`workspace_unavailable`，readiness优先级固定为AgentUnavailable→workspace cause→ModelUnavailable→Ready；Load用现有`resolve_for_turn`按captured definition.model同步分类model_available（普通model incompatibility→false且Load仍Loaded，catalog owner/source/definition internal→现有internal load路径），任何install新definition的publication（future-only/Workspace change/Agent upgrade）install前按当前catalog计算新definition的model_available并与definition一起安装，ReloadWorkspace保留当前事实，true Workspace publication只清workspace cause，DefinitionUpdated/WorkspaceReloaded event snapshot自然携带新readiness且不发额外ReadinessChanged event。selected PromptUnavailable load/definition projection亦已关闭：`SessionExecutorSnapshot`新增独立`prompt_available: bool`事实，readiness优先级固定为AgentUnavailable→workspace cause→selected prompt unavailable→PromptUnavailable→ModelUnavailable→Ready；Load独立await `prompt_available_for_definition`（`read_agent_definition`读exact retained Agent revision，复用`for_turn` selection阶段验证exact Agent+Session Prompt selection：missing/wrong role/duplicate resolved key→false且Load仍Loaded，Agent read的Closing→Load Closing、其余Agent read失败与owner/identity mismatch→internal load路径），任何install新definition的publication在durable commit后install前await同一helper（current installed Prompt resources），Closing/internal走既有active restore+`close_for_fatal(Internal)`，随后同步算model并原子安装definition+prompt/model事实，ReloadWorkspace保留当前两事实，DefinitionUpdated/WorkspaceReloaded event snapshot自然携带新readiness。shared-resource reload recovery亦已关闭：`RuntimeCommand::Runtime(ReloadSharedResources)`并行build Prompt/Model candidates（任一普通失败保留old roots并以`ReloadValidationFailed`+UserActionRequired拒绝、无事件），成功后经residency per-Session gate对全部loaded Sessions预计算并fan-out new PromptResourceView/ModelCatalogView（executor仅替换future TurnResources的Prompt/Model roots，active Turn保留已captured旧context，Unavailable总是Idle故恢复立即生效，非Idle合并为单一pending availability composite并在terminal/admission failure后、FollowUp决策前应用，仅readiness真实变化的Session发布`SessionReadinessChanged`），随后一次原子替换Runtime root pair并发布Runtime-scope `SharedResourcesReloaded`（detail null）；external Submit在reload publication期间持shared-resource read gate直到Turn context admission完成。active-Turn grace Unload亦已关闭：`MiniCoreRuntimeConfig::with_unload_grace`（default 30s、非zero且≤5min否则open返回`InvalidConfiguration`）在open时安装到residency actor；public Unload经runtime publication gate与residency per-Session gate执行`prepare_for_unload(grace)`→`executor.close()`→`remove_exact`，executor先同步关admission gate并经unbounded emergency lane接受`PrepareUnloadRequest`（不被bounded work lane阻塞），grace内active admission/Turn自然完成，deadline到期对exact current emergency target signal `PrepareForUnload`（sticky first-wins，更早Cancel/SecurityRevoked保留原reason）并cancel其cancellation token、以`SessionUnloaded` settle pending Interaction、不直接drop task；Starting Submit在Input未live apply时公开映射`SessionNotLoaded`而非`SubmitCancelled`，Input先赢仍`TurnStarted`随后同一Turn `Interrupted(PrepareForUnload)`；registry shutdown先stop residency admission（`request_closing`只cancel admission token、不触发executor force token——loaded executor的lifecycle token是独立force token，仅fatal/owner failure cancel），再对全部installed executors同步广播begin_prepare使grace并行计时再逐个await waiter再close（不累加N×grace）；已Idle的prepared executor在close时不伪造Finishing ExecutionChanged；不新增queue_updated event，manifest保持139项，final Unload fixtures/tests deferred。RuntimeDependencyUnavailable/Preparing、security invalidation event、full recovery scenarios与full scenario/recovery closure、具体Prompt/Skill source adapter、完整Tool policy/approval及完整cross-platform native matrix仍pending。

开发计划M0与M1已经完成，M2按行为slice增量推进，M8.1已完成；后续主要门禁：

- M5.0–M10 foundation/behavior已完成并统一review；当前推进M11 remaining full recovery conformance；
- V4-P1-3：production ProviderAdapter前关闭Rig reality与provider scope；
- V4-C0-1：production Tool/Sandbox adapter开始前关闭enforcement fail-closed合同。

## 目录角色

- `modules/`：当前semantic contract与canonical owner；`DurableState`是local entity-store physical operation owner；
- `adr/`：决策理由与successor关系，分类见[ADR索引](adr/README.md)；
- `formats/`、`fixtures/`：exact wire/storage format与测试资产（含Durable Store V1 golden/crash matrix）；
- `review/`：仍开放的评审finding；
- `research/`：非权威研究证据，见[Research索引](research/README.md)；
- `migration/`：V1到V2概念迁移说明；
- `archive/`：历史原文，不用于current实现。

## 搜索规则

仓库`.rgignore`默认排除`docs/archive/`：

```bash
rg 'SessionExecutor' docs
```

显式查询历史时使用：

```bash
rg -uu 'SessionWriter' docs/archive
```

不要从archive命中推导current Rust接口；发现current合同冲突时先修canonical module或新增ADR。
