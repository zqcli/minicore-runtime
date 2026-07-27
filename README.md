# MiniCore

MiniCore 是一个轻量级原生 Agent harness runtime core。

本仓库提供可复用的 Agent 运行时核心能力：`AgentRuntimeProtocol`、会话与运行编排、资源加载、技能、工具、`CommandSurface`、运行时事件、内部 `RuntimeHooks`、持久化、上下文压缩、`ModelGateway`、usage stats、自研 `AgentLoop` 状态机，以及 `ModelGateway` 内部的 Rig-backed provider adapter。Rig只负责具体provider的协议编码、单次request/stream调用和响应映射，底层SDK automatic retry固定为0；模型解析、请求校验、错误分类与provider-neutral terminal result由`ModelGateway`拥有，有限logical retry由`SessionExecutor`拥有，MVP不执行transparent transport/model fallback。CLI、TUI 和 GUI 产品会在独立仓库中开发，并通过运行时协议嵌入 MiniCore。

MiniCore 不是 CLI/GUI 产品仓库。下游宿主应通过下面的运行时接口接入：

```text
AgentRuntime.dispatch(agent_runtime_protocol::Command)
AgentRuntime.subscribe() -> agent_runtime_protocol::EventStream
AgentRuntime.snapshot() -> agent_runtime_protocol::RuntimeSnapshot
```

架构说明见 [docs/architecture.md](docs/architecture.md) 和 [docs/modules/README.md](docs/modules/README.md)。

## 文档权威顺序

当前架构文档（[docs/architecture.md](docs/architecture.md) 与 [docs/modules/](docs/modules/README.md)）
→ 当前 ADR（[docs/adr/](docs/adr/)，0100+）
→ [docs/research/](docs/research/)
→ [docs/archive/v1/](docs/archive/v1/README.md)（非权威，仅历史参考）

V1已归档，仅用于历史参考；V2是当前权威架构（目标设计已冻结，生产实现待启动）。下一里程碑是阶段6–8模型调用协同交付束，完整进度见[版本迁移记录](docs/migration/v1-to-v2.md)。正式文档不再链接V1归档；只有迁移说明和新ADR的历史依据部分可以引用它。

跨机器恢复当前设计评审工作时，从[设计评审工作交接](docs/review/v2-design-review-handoff.md)开始。
