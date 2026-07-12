# Prompt 使用不可变 turn 组装而不是长期 Manager

## 状态

Accepted

## 决策

MiniCore 将 `Prompt` 从单一 system prompt builder 提升为无状态深模块：`SessionRuntime` 作为 Pull Master，把 captured `PromptResourceView`、tool/model/agent/environment/policy views 交给 `prompt::begin_turn(...)`，得到 immutable `PromptTurn`；`PromptTurn` 负责 pin captured resources、结构化 skill/template intent 展开并提供原子 `PromptCallProfile`。每次模型调用前的协议安全 projection 由纯 `prompt::project_model_call(ModelCallProjectionInput { profile, call-time lanes })` 完成；它不以 `PromptTurn` 为 receiver，也不需要 `PromptResourceView`。system prompt 与 active tool schemas 继续绑定在同一个 profile 中，resource identity 继续复用 `ResourceManager` 的 canonical key/hash/source 类型。本 receiver 勘误关闭 BR-048，不改变 Prompt 的无状态定位。

## 影响

不创建 workspace-global `PromptManager` 或长期 `ContextManager`：resources、history、queues、tools、model 和 provider 已有明确 owner，新增 manager 会复制状态与失效协议。动态 RAG/memory/IDE context 由对应 owner 收集成显式 `ContextMaterialContribution::Available/Unavailable`，再交给 Prompt 最终排序和校验；required 获取失败不能以缺项表达。只有未来出现多个异步 context provider、跨 call working set、动态 token budget 和后台 distillation 后，才考虑不拥有 durable history 的 session-scoped `ContextWorkspace`。
