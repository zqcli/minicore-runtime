# MiniCore

MiniCore 是一个轻量级原生 Agent harness runtime core。

本仓库提供可复用的Agent运行时核心能力：Runtime协议、Session control actor、async Turn loop、资源加载、技能、工具、`CommandSurface`、live observer事件、best-effort Session recording、上下文压缩、`ModelGateway`和usage stats。每个loaded Session最多运行一个`ActiveTurnTask`，直接await Model、Tool与Interaction；`SessionRecorder`顺序inline append当前JSONL entry，失败不回滚live state或重放外部操作。Rig只负责具体provider的协议编码、单次request/stream调用和响应映射，SDK automatic retry固定为0；模型解析、请求校验、错误分类与provider-neutral terminal result由`ModelGateway`拥有，有限logical retry由`ActiveTurnTask`拥有。CLI、TUI和GUI产品在独立仓库中开发。

MiniCore 不是 CLI/GUI 产品仓库。下游宿主应通过下面的运行时接口接入：

```text
MiniCoreRuntime.dispatch(CommandRequest)
MiniCoreRuntime.query(RuntimeQuery)
MiniCoreRuntime.snapshot(SnapshotRequest)
MiniCoreRuntime.subscribe(SubscriptionRequest) -> EventStream
```

架构说明见 [docs/architecture.md](docs/architecture.md) 和 [docs/modules/README.md](docs/modules/README.md)。

## 文档权威顺序

当前架构文档（[docs/architecture.md](docs/architecture.md) 与 [docs/modules/](docs/modules/README.md)）
→ 当前 ADR（[docs/adr/](docs/adr/)，0100+）
→ [docs/research/](docs/research/)
→ [docs/archive/v1/](docs/archive/v1/README.md)（非权威，仅历史参考）

V1已归档，仅用于历史参考；V2是当前权威架构。第四轮评审的P0-1至P0-4与P1-4已关闭并推送，生产实现仍待启动；当前暂停点是[第四轮设计评审](docs/review/v2-design-review-4.md)的V4-P0-5 Compaction contract，之后再关闭P1/wire并启动阶段6–8 async vertical slice。完整换机进度见[设计评审工作交接](docs/review/v2-design-review-handoff.md)。

跨机器恢复当前设计评审工作时，从[设计评审工作交接](docs/review/v2-design-review-handoff.md)开始。
