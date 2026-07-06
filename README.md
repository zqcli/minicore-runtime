# MiniCore

MiniCore 是一个轻量级原生 Agent harness runtime core。

本仓库提供可复用的 Agent 运行时核心能力：`AgentRuntimeProtocol`、会话与运行编排、资源加载、技能、工具、`CommandSurface`、运行时事件、内部 `RuntimeHooks`、持久化、上下文压缩、`ModelGateway`、usage stats，以及 Rig `Driver` 集成。CLI、TUI 和 GUI 产品会在独立仓库中开发，并通过运行时协议嵌入 MiniCore。

MiniCore 不是 CLI/GUI 产品仓库。下游宿主应通过下面的运行时接口接入：

```text
AgentRuntime.dispatch(agent_runtime_protocol::Command)
AgentRuntime.subscribe() -> agent_runtime_protocol::EventStream
AgentRuntime.snapshot(session_id) -> agent_runtime_protocol::Snapshot
```

架构说明见 [docs/architecture.md](docs/architecture.md) 和 [docs/modules/README.md](docs/modules/README.md)。
