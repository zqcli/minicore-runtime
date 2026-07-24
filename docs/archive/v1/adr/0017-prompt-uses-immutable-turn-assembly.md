# Prompt 使用不可变 turn 组装而不是长期 Manager

> **归档（V1）**：本 ADR 属于 MiniCore V1 架构，仅作历史参考，不得作为当前实现或新开发的设计依据。当前权威决策见 `docs/adr/`（0100+）。原文保持历史原貌。

## 状态

Accepted

## 决策

MiniCore 将 `Prompt` 从单一 system prompt builder 提升为无状态深模块：`SessionRuntime` 作为 Pull Master，在目标 user-turn boundary 获取 captured `PromptResourceView` 和独立 `ToolPromptView`，并调用：

```rust
prompt::assemble_turn(PromptTurnSpec {
    resources: PromptResourceView,
    tools: ToolPromptView,
})
```

得到 immutable `PromptTurn`。`PromptTurn` 负责 pin captured resources、结构化 skill/template intent 展开并提供原子 `PromptCallProfile`。

`PromptResourceView` 是所有非工具稳定 Prompt 输入的唯一 seam，暴露 materials、behavior、model、environment、policy、skill/template catalog 和 fingerprint。`ToolPromptView` 保持 Tools 独立；session-scoped `Tools` 通过 `capture_profile_baseline()` 原子产出同 fingerprint 的 `ToolProfileBaseline { prompt, invoker, fingerprint }`，Prompt 只消费其中的 active tool schemas/snippets/guidelines view，执行路径使用同一 baseline 的 invoker。

每次模型调用前的协议安全 projection 由纯 `prompt::project_model_call(ModelCallProjectionInput { profile, call-time lanes })` 完成；它不以 `PromptTurn` 为 receiver，也不需要 `PromptResourceView`。system prompt 与 active tool schemas 继续绑定在同一个 `PromptCallProfile` 中，resource identity 继续复用 `ResourceManager` 的 canonical key/hash/source 类型。

MVP 在 active `Turn` 中拒绝 model、thinking、stream options、active tools 和 profile mutation。后续 full version 若允许 safe-point mutation，必须通过 `StepResourceSnapshot` 或明确 step override，并在同一 actor transaction 中原子替换 `PromptCallProfile` 与 future `ToolBatchInvoker`，保持 fingerprint 一致；不能分别 patch system prompt 与 tool schemas。

## 影响

不创建 workspace-global `PromptManager` 或长期 `ContextManager`：resources、history、queues、tools、model 和 provider 已有明确 owner，新增 manager 会复制状态与失效协议。

动态 RAG/memory/IDE context 由对应 owner 收集成显式 `ContextMaterialContribution::Available/Unavailable`，再交给 Prompt 最终排序和校验；required 获取失败不能以缺项表达。

只有未来出现多个异步 context provider、跨 call working set、动态 token budget 和后台 distillation 后，才考虑不拥有 durable history 的 session-scoped `ContextWorkspace`。

## Amendment 2026-07-14 (ADR 0023)

[ADR 0023](0023-driver-starts-from-one-committed-conversation-seed.md) 将本 ADR 中的历史命名修订为已接受的 public seam：`Prompt.prepare_message_turn(...) -> PreparedMessageTurn -> ModelContextProfile`，`compose_user_message(...) -> CanonicalUserMessage`，以及 `assemble_model_context(...) -> AssembledModelContext`。Prompt 仍是无状态深模块；变化是它现在也是 `AgentRun` 和 `CompactionSummary` 唯一的模型上下文组装 seam。旧文中的 `PromptTurn` / `PromptCallProfile` / `ModelInputProjection` 表述应按上述命名理解，不重写本 ADR 的历史决策正文。
