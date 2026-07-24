# ModelGateway spine 先于真实 Driver 集成

> **归档（V1）**：本 ADR 属于 MiniCore V1 架构，仅作历史参考，不得作为当前实现或新开发的设计依据。当前权威决策见 `docs/adr/`（0100+）。原文保持历史原貌。

Status: accepted；exact interface已由[ADR 0026](0026-model-gateway-uses-one-deep-async-operation.md)确定

ADR 0026把spine收敛为`resolve_for_turn(...)`和一个深`generate_model_turn(...)`operation；旧`call_model`、Driver/SessionRuntime ownership和transparent cross-model fallback表述不再约束目标架构。

private AgentLoop/Rig真实集成需要模型调用seam，因此MiniCore在集成前先稳定ModelGateway spine，而不是写临时provider路径。按ADR 0026，spine包括`ModelSelection`、`TurnModelSnapshot`、`ModelCallPurpose`、`ModelCallRequest`、`ModelCallResult`、`ModelCallErrorKind`、`ModelUsage`、`ModelProgressEvent`、`resolve_for_turn(...)`和`generate_model_turn(...)`；ProviderCatalog、AuthStore、retry、cache和ProviderAdapter保持private implementation。

这个决定让 provider/auth/Rig provider 细节从第一条真实 driver 切片开始就留在 `ModelGateway` 内部，不进入 `Driver` 或 `SessionDriverHost`。代价是前置一点 spine 工作，但可以避免先写临时 gateway，等测试和会话行为长出来后再整体替换。

BR-049 的后续核对没有发现采用 Rig 的架构级障碍，因此版本 pin、`ModelTurn`/usage/tool-name 映射和 Steer segment rollover 延后到真实 Driver integration spike 验证。延期不改变本 ADR 的顺序：spine 类型继续保持 MiniCore-owned、provider-neutral，Rig 字段形状和 segment 实现不得泄漏进 `ModelCallRequest` 或 `ModelGateway` interface。
