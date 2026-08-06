# MiniCore

MiniCore 是一个轻量级原生 Agent harness runtime core。

本仓库定义并逐步实现可复用的Agent运行时核心能力：Runtime协议、Session control actor、async Turn loop、资源加载、技能、工具、`CommandSurface`、live observer事件、best-effort Session recording、上下文压缩、`ModelGateway`和usage stats。M4已实现Prompt-owned opaque `ModelMessage`与纯内存LiveConversation reducer；M5.0 recovery/root lease/owner-tracked actor、private permanent reservation foundation、crate-private Agent Create与ordinary Session Create exact G1 publication，以及unloaded RecordedHistory + Genesis Session Fork tracer已实现，仍无standalone reservation API/token/receipt；public Runtime command接入、non-Genesis/LiveSnapshot Fork、update/CAS、完整cross-platform native matrix、SessionRecorder/replay与后续behavioral slices仍待交付。CLI、TUI和GUI产品在独立仓库中开发。

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

V1已归档，仅用于历史参考；V2是当前权威架构。第四轮评审的全部P0、V4-P1-1、P1-2与P1-4已关闭，ADR 0134/0135、exact Conversation JSONL v1和wire conformance vectors已冻结；ADR 0136/0137现冻结DurableState/Store V1/root lease（new-entity Create/Fork complete-or-invisible、existing-head update old-or-new）与Tokio deterministic foundations。M0、M1、M2 minimal Snapshot/Event、M3.1、M3.2和M4已经完成；M1 Wire foundation/owner semantic spine已经完成并通过Fast、MSRV与heavy gates；M2 incremental public codec已完成bootstrap、typed roots、M7 Create/Load/Unload/Submit/Cancel、command completion，以及minimal idle SessionSnapshot/Runtime与Turn terminal StateEvent，remaining DTO随M8–M10增量激活。M3.1已完成strict Header、六种flat body的exact Conversation Header/Entry per-line codec、bounded duplicate-aware preflight与raw ToolCall `arguments` cap、owner/writer invariants和全部conversation golden的byte-exact round-trip；M3.2仅完成bounded streaming scanner（known size/stat unavailable 1 GiB cap、LF/CRLF、strict Header、line/count limits、recovery，以及要求opaque `ExclusiveWritableConversationLease`才返回final partial-tail truncation action/offset）；M5.0 recovery/root lease/owner-tracked actor、private permanent reservation foundation、crate-private Agent Create与ordinary Session Create exact G1 publication，以及unloaded RecordedHistory + Genesis Session Fork tracer已实现；public Runtime command接入、remaining Fork anchors/LiveSnapshot、update/CAS、完整cross-platform native matrix、SessionRecorder/replay pending。M4已完成Prompt-owned opaque `ModelMessage`、`ConversationRevision`/`EntryIdGenerator`、`LiveSessionState`的User/Assistant/Tool/Interaction reducer、reducer内complete Tool exchange、coherent capture与Compaction stable units/source/replacement subset；Fast/MSRV library与integration tests、Clippy、docs/fixtures检查与3项heavy recipes均通过，最终review无blocker。`SessionExecutor`/`ActiveTurnTask`、实际Recorder/replay、M8 public DTO、M10 planner/model compaction以及provider/Tool adapter行为仍待后续里程碑。production ProviderAdapter前关闭V4-P1-3，production Tool/Sandbox adapter前关闭V4-C0-1。

当前开发从[MiniCore V2开发计划](docs/development-plan.md)恢复；[设计评审工作交接](docs/review/v2-design-review-handoff.md)仅保留为合并前历史checkpoint。
