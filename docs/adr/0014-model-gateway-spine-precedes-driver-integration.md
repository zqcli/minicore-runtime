# ModelGateway spine 先于真实 Driver 集成

Status: accepted

`Driver` 一进入 text-only 真实集成就需要模型调用 seam，因此 MiniCore 会在真实 `Driver` 集成前实现最小稳定 `ModelGateway` spine，而不是在阶段 5 写一条临时 provider 路径。提前稳定的范围包括 `ModelSelection`、`ModelCallRequest`、`ModelCallResult`、`ModelCallErrorKind`、`ModelCallUsage`、`ModelGateway.call_model(...)`、最小 `ProviderRegistry.resolve(...)` 和 `AuthStore.resolve(...)`；后续阶段只在同一 seam 上扩展 custom provider、完整 auth 来源、fallback、provider-specific usage normalization 和 context usage。

这个决定让 provider/auth/Rig provider 细节从第一条真实 driver 切片开始就留在 `ModelGateway` 内部，不进入 `Driver` 或 `SessionDriverHost`。代价是前置一点 spine 工作，但可以避免先写临时 gateway，等测试和会话行为长出来后再整体替换。
