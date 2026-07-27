# ADR 0106: ModelGateway 是单一深异步 operation

状态：Accepted
日期：2026-07-24

## 背景

- SessionExecutor 需要一个 provider-neutral 的模型调用 seam，把 provider catalog、credential、private ProviderAdapter、stream、retry、usage、cache 等复杂性挡在 Session execution 之外；首个production ProviderAdapter使用Rig。
- V1 曾把这些职责拆散：ADR 0009 让 ModelGateway 只是 Rig providers 的 wrapper，ADR 0014 把 Gateway spine 与 driver 集成分阶段，ADR 0013 让 Driver 接收 `DriverTurnInput`，ADR 0026 才收敛为一个深异步 operation。这些切分制造了多个 turn-scoped lifecycle 与 provider state 泄漏点。
- V2 需要明确：Model identity 在 Turn 内 exact 固定；模型调用只有一个真实 interface；provider variation 完全隐藏在 private adapter 内。
- 权威设计见 [ModelGateway 架构设计](../modules/model-gateway.md)。

## 决策

- ModelGateway 是 runtime-owned 深模块，对外只暴露两个 `pub(crate)` 操作：
  - `resolve_for_turn(...)` 在 Turn capture 期间返回 immutable `TurnModelSnapshot`，固定 exact model definition、capabilities、effective limits 与 generation policy；catalog revision 变化只影响 future Turn。
  - `generate_model_turn(ModelCallRequest, progress, cancel)` 是唯一真实模型调用 interface，返回一个 terminal `ModelCallResult` 或 typed `ModelCallError`。
- Gateway拥有并隐藏provider catalog、credential/auth policy、single-attempt planning、stream lifecycle、usage、cache与continuation；这些都是private implementation detail，不进入MiniCoreRuntime interface。MVP retry policy由ADR 0119收窄为Gateway single attempt加Session logical retry。
- ModelGateway 不重新组装 Prompt：PromptSet 产出的 `AssembledModelContext` 是模型上下文的唯一 producer；Gateway 不重新加载 message、不判断 message visibility、不截断或摘要 conversation。
- active Turn内禁止transparent transport或cross-model fallback。跨transport/provider/model替换必须由显式Session definition update或下一Turn admission完成。
- Rig provider差异只存在于private `ProviderAdapter`；`RigProviderAdapter`只编码并执行一个由Gateway规划好的provider attempt，并把stream/terminal/error映射回MiniCore attempt类型。它不选择provider/model，不决定Session logical retry或cache/continuation policy，也不构造最终`ModelCallResult`；SDK automatic retry固定为0。Rig raw types、`additional_params`、SDK error不越过adapter seam。首批实现为RigProviderAdapter与ScriptedProviderAdapter，保证它是真实seam并支持阶段6–8共享vertical-slice tests。
- 错误原因使用closed typed taxonomy，足以驱动retry、compaction recovery（如`ContextOverflow`）与terminal failure，caller不解析raw message；`RequestOutcomeUnknown`/`StreamInterrupted`禁止blind replay。ADR 0120进一步规定Gateway在`ModelCallResult`前验证finish/content与OutputContract。
- cache、connection reuse 与 continuation 必须保持 full-request equivalence：任何 optimization 都能退回完整 `AssembledModelContext` 请求，它们只是 wire optimization，不是第二 conversation truth。

## 后果

- caller只理解完整request、droppable progress与一个terminal result；provider attempt、connection、auth和cache locality集中在Gateway内，SessionExecutor只理解typed terminal error与logical retry policy。
- 删除 Gateway 会把 provider 复杂性重新散落到 caller，满足 deep module deletion test。
- 不引入`ModelStep`、`ModelAttempt`领域entity、provider session public object、共享`ModelCallBudget`或第二conversation state。
- Gateway内部较深，需要private planner/adapter/connection seam辅助测试；progress publisher必须明确non-authoritative、process-local语义。
- exact model pin 与 full-request equivalence 优先于性能优化，cache/continuation failure 快速退回 full request。

## 历史

本 ADR 属 V2 决策集，取代 V1 的：

- ADR 0009（ModelGateway 包装 Rig providers）
- ADR 0026（一个深异步 operation）
- ADR 0014（ModelGateway spine 先于 driver）
- ADR 0013（Driver 接收 DriverTurnInput）

原文见 `docs/archive/v1/adr/`。

Model retry与transport fallback部分由[ADR 0119](0119-model-calls-use-session-logical-retries.md)进一步收窄；response error ownership与命名由[ADR 0120](0120-failures-stay-with-owning-modules.md)补充。本ADR的single deep operation与provider-neutral seam决策保持有效。
