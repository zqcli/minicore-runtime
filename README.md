# MiniCore

MiniCore 是一个轻量级原生 Agent harness runtime core。

本仓库定义并逐步实现可复用的Agent运行时核心能力：Runtime协议、Session control actor、async Turn loop、资源加载、技能、工具、`CommandSurface`、live observer事件、best-effort Session recording、上下文压缩、`ModelGateway`和usage stats。M4已实现Prompt-owned opaque `ModelMessage`与纯内存LiveConversation reducer；M5已实现DurableState/Store V1、conversation target/Recorder及tolerant replay；M6已实现Workspace/Prompt text foundation与single-attempt scripted ModelGateway；M7已接通public Create/Load/Submit/Snapshot/Subscribe/Unload与ordinary AgentRun；M8已接通最小Scripted Tool round-trip、Interaction和Cancel；M9当前control/observation范围已接通Steer/FollowUp、queued cancellation、logical retry、EmergencyControl、runtime-wide CommandId dedup、Starting/Running/Finishing Snapshot及`session_execution_changed`/terminal StateEvent；M10已接通pressure-triggered CompactionSummary、live Replace、inline best-effort marker recording与下一次AgentRun；M11当前已接通public Session Fork command/outcome、全部公开message anchors、loaded LiveSnapshot/unloaded RecordedHistory source selection、bounded-memory child re-encode/readback和restart recovery，durable `ListAgents`/`ListSessions`分页与`GetSessionForkProvenance`查询，snapshot-first Runtime subscription与Session membership StateEvent，public Session Archive/Unarchive/Delete及Agent Create/Enable/Disable/Delete/UpdateDefinition/UpdateMetadata、typed outcomes/NoChange与matching lifecycle/definition/metadata StateEvent，以及typed ResolveInteraction、SessionDefinitionUpdated、Progress/Closed wire closure；public manifest现已无pending target。具体Prompt/Skill source adapter、完整Tool policy/approval、M11 remaining Session definition/metadata CAS、readiness与full recovery closure、production Rig adapter、grace/cancel式active-Turn Unload及完整cross-platform native matrix仍待交付。CLI、TUI和GUI产品在独立仓库中开发。

MiniCore 不是 CLI/GUI 产品仓库。下游宿主应通过下面的运行时接口接入：

```text
MiniCoreRuntime.dispatch(CommandRequest)
MiniCoreRuntime.query(RuntimeQuery)
MiniCoreRuntime.snapshot(SnapshotRequest)
MiniCoreRuntime.subscribe(SubscriptionRequest) -> EventStream
```

文档入口见 [docs/README.md](docs/README.md)；架构说明见 [docs/architecture.md](docs/architecture.md) 和 [docs/modules/README.md](docs/modules/README.md)，分阶段交付、依赖与验收条件见 [MiniCore V2开发计划](docs/development-plan.md)。

## 文档权威顺序

当前架构文档（[docs/architecture.md](docs/architecture.md) 与 [docs/modules/](docs/modules/README.md)）
→ current/refined ADR（[docs/adr/](docs/adr/)，0100+）
→ formats + fixtures
→ [开发计划](docs/development-plan.md)
→ migration + [docs/research/](docs/research/)
→ [docs/archive/v1/](docs/archive/v1/README.md)（非权威，仅历史参考）

V1已归档，仅用于历史参考；V2是当前权威架构。第四轮评审的全部P0、V4-P1-1、P1-2与P1-4已关闭，ADR 0134–0137、exact Conversation JSONL v1、Store V1与wire conformance vectors已冻结。M0–M10已经完成，M11正在推进；M2 public manifest已完成无pending closure。当前public scripted闭环覆盖Agent Create/Enable/Disable/Delete/UpdateDefinition/UpdateMetadata、Session Create/Load/Submit/Fork/Archive/Unarchive/Delete、durable Agent/Session catalog分页与Fork provenance查询、snapshot-first Runtime/Session subscription、Runtime Agent/Session membership/lifecycle/definition/metadata StateEvent、Steer/FollowUp/Cancel/ResolveInteraction、Starting/Running/Finishing Snapshot、execution/terminal StateEvent、Tool/Interaction、logical retry、Compaction、Unload/Load replay与关键Recorder/context/busy故障路径；Progress/Closed及SessionDefinitionUpdated已具备typed selected-V1 codec。后续包括具体Skill composition、M11 remaining Session definition/metadata CAS、readiness与full recovery closure以及production provider/Tool adapters；production ProviderAdapter前关闭V4-P1-3，production Tool/Sandbox adapter前关闭V4-C0-1。

当前开发从[MiniCore V2开发计划](docs/development-plan.md)恢复；[设计评审工作交接](docs/review/v2-design-review-handoff.md)仅保留为合并前历史checkpoint。
