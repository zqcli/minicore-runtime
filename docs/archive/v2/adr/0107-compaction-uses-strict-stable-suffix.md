# ADR 0107: Compaction 使用严格 stable suffix

状态：Superseded by [ADR 0112](../../../adr/0112-compaction-supports-active-turn-checkpoints.md)；identity/fingerprint条款 further superseded by [ADR 0123](../../../adr/0123-identity-uses-refs-and-explicit-reload.md)
日期：2026-07-24

> 历史说明：以下正文保留为被取代的ADR历史记录。当前Compaction rolling-summary/single-marker durable shape与tolerant replay以ADR 0124为准；immutable request与explicit reload规则以ADR 0123仍有效的条款为准。以下active-checkpoint、strict-suffix和fingerprint叙事均不再是当前架构决策。

## 背景

MiniCore 已确立 Transcript-First 模型输入、by-entry JSONL storage、单一权威 SessionExecutor、单一 PromptSet assembly seam 和单一 provider-neutral ModelGateway operation。Conversation compaction 必须在这些约束下缩减 model-visible history，但不能：制造第二份 conversation truth、拆散 ToolCall/ToolResult 等协议稳定单元、丢失 current user request，或安装崩溃后无法恢复的 in-memory replacement。

同类 runtime 取舍不同：pi 使用 rolling summary 并支持 split-turn 与 post-turn/manual compaction；Codex 支持 provider-native compaction、跨模型 fallback 和直接 replacement history；Rig 暴露 load-time memory policy；Claude Code 的 cut/storage 细节未公开。首版应优先 durable truth、protocol-safe cut、deterministic replay 和 bounded recovery，而不是先做 split-turn、provider-specific opaque artifact 或 manual maintenance state。

模块设计见 [`../modules/compaction.md`](../../../modules/compaction.md)。

## 决策

1. Compaction 是 crate-internal 的纯 planning/validation 模块，不是 Runtime Service 或领域 entity。它只提供：context budget 计算、stable-unit projection、strict cut/protection planning、protected EntryId、portable summary directive 构造、summary input reduction 和结果/commit candidate 校验。它不构造 `ModelCallRequest`，不拥有 SessionWriter、ModelGateway、events、CancellationToken 或 Turn terminal state。
2. SessionExecutor是唯一的orchestration owner：判断trigger、驱动`RunningOperation::CompactConversation`、调用PromptSet与ModelGateway、仲裁Steer/Cancel/SecurityRevoked、执行writer append/apply。
3. automatic compaction 只在 active Turn 的 `NeedModel` safe point 评估，trigger 为 soft context pressure、Prompt local context overflow 和 provider context overflow。final assistant 完成后不做 eager post-turn compaction，也不提供 standalone/manual compaction。
4. cut基于model-visible stable unit（一个UserMessage、一个无ToolCallAssistant Continue、一个完整ToolRound、一个final AssistantMessage或一个已有Compaction summary），只在unit boundary之间发生，不拆散任何协议稳定单元。
5. summarized range 是非空连续 prefix，retained range 是非空连续 suffix，两者在 stable boundary 处相邻；projector 不从 history 中间删除任意 message。
6. hard-protect active Turn 的 initiating UserMessage 及其后全部连续 model-visible history。由于 retained range 必须连续，active Turn 内部更早的 ToolRound 也原样保留。protected suffix 过大时报告 `ProtectedSuffixTooLarge`，不 split、summarize 或截断 current Turn。
7. 重复 compaction 是 portable rolling summary：previous effective summary 与新 evicted stable units 被合成为一个新 summary，后接 retained suffix；每次只产生一个 effective leading summary。
8. summary 生成使用 active Turn exact `TurnModelSnapshot`、`CompactionSummary` purpose 和 `NoToolCalls` output contract；PromptSet 是唯一 context 组装 seam，ModelGateway 是唯一 model-call seam，且不调用 provider-native compaction endpoint。summary source 只来自 `CommittedConversationPrefixView`；大 ToolResult 仅在 summary request representation 中带标记 reduction，durable Tool message 不被改写。
9. SummaryModel 成功不立即改变 conversation。只有 `StoredCompaction` entry append/apply 成功后，trusted projector 才确定性构造 `Replace([summary] + retained suffix)`；此前旧 conversation 仍是模型可见的权威历史。caller 不能提交任意 replacement message 向量。
10. `Compacting`是transient `TurnExecutionPhase`，保持`TurnStatus = Running`。append/apply是唯一linearization point：append前Cancel与SecurityRevoked可获胜并取消operation，Steer只排队；成功后重建ConversationSeed和private AgentLoop segment，并沿用同一TurnExecutionContext。
11. soft-pressure 失败只在原 ModelCallRequest 的 checkpoint、assembly fingerprint、execution version 和 control state 全部未变时才回退到未压缩调用；hard-overflow 失败 TurnFailed。同一 active Turn 最多一次 automatic overflow recovery，compact 后仍 overflow 则 TurnFailed，不做无界 compact-and-retry。
12. compacted conversation 可从 storage 重建：restart replay validate durable `StoredCompaction` 并确定性 apply Replace，但绝不恢复 summary call、retry timer、CompactionPlan、provider continuation 或旧 AgentLoop。原始 entries 保持 append-only，在 Compaction entry 之前的 history branch 上仍可得未压缩历史。
13. 首版不做 split-turn summary、hierarchical summary tree、provider-native compaction、cross-model fallback、standalone/manual compaction 或 deterministic conversation truncation；Runtime Interface 不公开 manual `CompactSession` 协议。

## 后果

- ToolCall/ToolResult 等协议 ordering 在每个 cut 处都保持完整。
- current user request 保持精确，但单个 active Turn 可能大到无法 recovery；该限制以显式 `ProtectedSuffixTooLarge` 暴露，而非隐藏在 lossy truncation 之后。
- durable result 是 provider-neutral text 加 typed provenance，因此 summary 跨 restart、fork 和未来 provider 变化都 portable。
- static Prompt/Workspace/Tool/Skill 内容不进入 summary，由后续 AgentRun assembly 重新注入，不会重复或过时，也不获得 summary authority。
- writer/projector 校验比直接 in-memory replacement 复杂，但 replay 确定性，SessionStorage 仍是唯一 durable truth。
- 不为可能不再继续的 Session 预付 post-turn latency；下一次 NeedModel 才可能承担 compaction latency。
- provider-native compaction 只能作为后续优化加入，且必须保留等价 portable durable representation 与 exact request 语义；manual compaction 需未来 Runtime maintenance 协议，而非隐式取消 active Turn。

## 历史

本 ADR 属 V2 决策集，取代 V1 的 ADR 0027（Compaction 使用严格 stable suffix）与 ADR 0002（compaction 由 session runtime 编排）。保留的核心原则是：session execution（而非 Driver 或 AgentLoop）拥有 compaction，cut 在 stable-unit boundary，Replace 由 trusted projection 派生。原文见 `docs/archive/v1/adr/`。
