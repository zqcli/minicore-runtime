# ADR 0106: ModelGateway 是单一深异步 operation

状态：Accepted
日期：2026-07-24

## 背景

- SessionExecutor 需要一个 provider-neutral 的模型调用 seam，把 provider catalog、credential、Rig adapter、stream、retry、usage、cache 等复杂性挡在 Session execution 之外。
- V1 曾把这些职责拆散：ADR 0009 让 ModelGateway 只是 Rig providers 的 wrapper，ADR 0014 把 Gateway spine 与 driver 集成分阶段，ADR 0013 让 Driver 接收 `DriverTurnInput`，ADR 0026 才收敛为一个深异步 operation。这些切分制造了多个 turn-scoped lifecycle 与 provider state 泄漏点。
- V2 需要明确：Model identity 在 Turn 内 exact 固定；模型调用只有一个真实 interface；provider variation 完全隐藏在 private adapter 内。
- 权威设计见 [ModelGateway 架构设计](../modules/model-gateway.md)。

## 决策

- ModelGateway 是 runtime-owned 深模块，对外只暴露两个 `pub(crate)` 操作：
  - `resolve_for_turn(...)` 在 Turn capture 期间返回 immutable `TurnModelSnapshot`，固定 exact model definition、capabilities、effective limits 与 generation policy；catalog revision 变化只影响 future Turn。
  - `generate_model_turn(ModelCallRequest, progress, cancel)` 是唯一真实模型调用 interface，返回一个 terminal `ModelCallResult` 或 typed `ModelCallError`。
- Gateway 隐藏 provider catalog、credential/auth、Rig adapter、stream、same-model retry、transport fallback、usage、cache 与 continuation；这些都是 private implementation detail，不进入 MiniCoreRuntime interface。
- ModelGateway 不重新组装 Prompt：PromptSet 产出的 `AssembledModelContext` 是模型上下文的唯一 producer；Gateway 不重新加载 message、不判断 message visibility、不截断或摘要 conversation。
- active Turn 内禁止 transparent cross-model fallback。同一 exact model identity 下允许 transport fallback（如 WebSocket → HTTP），跨 provider/model 替换必须由显式 Session definition update 或下一 Turn admission 完成。
- Rig provider 差异只存在于 private `ProviderAdapter`；Rig raw types、`additional_params`、SDK error 不越过 adapter seam。至少有 Rig adapter 与 deterministic fake adapter 两个实现，保证它是真实 seam。
- 错误分类为 closed taxonomy，足以驱动 retry、compaction recovery（如 `ContextOverflow`）与 terminal failure，caller 不解析 raw message；`RequestOutcomeUnknown`/`StreamInterrupted` 禁止 blind transparent replay。
- cache、connection reuse 与 continuation 必须保持 full-request equivalence：任何 optimization 都能退回完整 `AssembledModelContext` 请求，它们只是 wire optimization，不是第二 conversation truth。

## 后果

- caller 只理解完整 request、droppable progress 与一个 terminal result；provider attempt、connection、auth、retry、cache locality 全部集中在 Gateway 内。
- 删除 Gateway 会把 provider 复杂性重新散落到 caller，满足 deep module deletion test。
- 不引入 `ModelStep`、`ModelAttempt` 领域 entity、provider session public object 或第二 conversation state；transparent retry 与 logical retry 严格分离。
- Gateway 内部较深，需要 private planner/adapter/connection/retry seam 辅助测试；progress publisher 必须明确 non-authoritative、process-local 语义。
- exact model pin 与 full-request equivalence 优先于性能优化，cache/continuation failure 快速退回 full request。

## 历史

本 ADR 属 V2 决策集，取代 V1 的：

- ADR 0009（ModelGateway 包装 Rig providers）
- ADR 0026（一个深异步 operation）
- ADR 0014（ModelGateway spine 先于 driver）
- ADR 0013（Driver 接收 DriverTurnInput）

原文见 `docs/archive/v1/adr/`。
