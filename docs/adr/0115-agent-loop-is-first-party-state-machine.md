# ADR 0115：AgentLoop 使用自研协议状态机（Rig 收窄为 provider adapter）

状态：Accepted
日期：2026-07-27

## 背景

Turn execution 内部的 AgentLoop 是 sans-I/O 推理协议状态机，只在 `NeedModel | NeedTools | Finished` 之间推进；Prompt assembly、Tool 执行、模型调用、持久化、Steer/FollowUp、Interaction、Compaction 和 terminal arbitration 全部由设计明确外置给 SessionExecutor 与各深模块（ADR 0105、`session-execution.md`、`turn-execution-context.md`）。

既有文档为 AgentLoop 保留了两条实现路径：「AgentLoop 可以使用 Rig 或其他 SDK 作为 private adapter」；`architecture.md` 曾表述「Rig 拥有 agent loop 的协议级状态机」。同时为 Rig 可能只支持 monolithic async run 的情况保留了 private adapter task 例外（"强制 two-owner execution" 否决条款的但书）。

本 ADR 关闭该开放分支：AgentLoop 自研，Rig 的使用范围收窄为 ModelGateway 的 private ProviderAdapter。

## 同类项目依据

| 项目 | agent loop 实现 | 观察 |
| --- | --- | --- |
| pi | 自研 `pi-agent-core` AgentLoop；provider 调用走独立 `pi-ai` | loop 本身很小；steering/followup/持久化在 loop 之外 |
| Codex | 自研 turn/task 事件循环；`ModelClient` 只负责 provider | loop 与 provider client 是两个 seam，从不共用一个 SDK |
| Claude Code | 自研 | 未公开内部，但可观察行为表明 loop 是产品自有编排 |
| Grok Build | 自研 actor 式 loop；sampler 独立 crate | loop 与 sampling 分离，与 MiniCore 的 Executor/Gateway 分界同构 |
| OpenHands | 自研 agent controller loop | SDK 层与 loop 层分离 |

跨项目共识：**认真做 harness 的产品无一例外自研 loop**。SDK 内置 agent loop（Rig `Agent`/AgentRun、LangChain agents 等）服务于轻量集成场景，其 loop 通常自持 conversation history、tool 注册与调用编排——这三点分别与 MiniCore 的 Transcript-First（模型可见事实只来自 committed conversation）、ToolService 统一治理、`tool_round_completed` 可见性规则直接冲突。

## 决定

1. **AgentLoop 是 MiniCore 自研的 crate-private 同步 sans-I/O 状态机**，不由 Rig 或其他 SDK 驱动。
2. **Rig 的使用范围收窄为 ModelGateway 的 private ProviderAdapter**（provider 协议编码、streaming、auth payload、finish reason/usage 提取）。Rig 0.40.0 spike 的门禁范围同步收窄为 provider mapping 验证，不再评估 Rig sans-I/O AgentRun。
3. **crate-private interface 维持 `session-execution.md` 既有形状不变**：`next_action()`、`accept_model_response()`、`accept_committed_tool_round()`、`accept_committed_steer()` 与 `AgentLoopAction { NeedModel | NeedTools | Finished }`。不新建 public trait、`AgentLoopFactory` 或 registry；「第二个真实实现出现才建立稳定 seam」的既有原则不变。
4. **内部状态机冻结为三态**：

```text
AwaitingModel { output_contract }
  ← ConversationSeed 构造 / accept_committed_tool_round / accept_committed_steer
  → accept_model_response(FinalizedAssistantResponse)
     ├─ content 含 ToolCall → PendingToolRound { expected: ordered ToolCallIds }
     └─ content 无 ToolCall → EmittedCandidate（next_action 返回 Finished candidate）

PendingToolRound
  → next_action 返回 NeedTools { response, calls }
  → accept_committed_tool_round(trusted CommittedConversationDelta)
     └─ 验证 delta 覆盖 exact expected calls → AwaitingModel

EmittedCandidate
  → SessionExecutor 仲裁：Steer FIFO 为空 → Assistant(Final)，Turn Completed
     Steer FIFO 非空 → Assistant(Continue) append/apply
     → accept_committed_steer 原地推进回 AwaitingModel（或等价重建 segment）
```

5. **Steer 原地推进为默认路径**：`accept_committed_steer` 消费 trusted delta 后直接回到 `AwaitingModel`；从 ConversationSeed 重建 segment 保留为等价实现自由。**Compaction Replace 后必须重建 segment**（committed conversation 被整体替换），该规则不变。原为"adapter 不支持原地注入 Steer"和"provider/SDK rollover"保留的重建理由随本 ADR 删除。
6. **协议校验归 loop**：状态错配（`AwaitingModel` 之外调用 `accept_model_response`、`PendingToolRound` 之外调用 `accept_committed_tool_round`）、tool round coverage 与 expected calls 不匹配、candidate 重复消费，均返回 typed `AgentLoopError`（ProtocolViolation 类），由 SessionExecutor 按 invariant violation 规则处理（停止执行、replay 或 Unavailable）。
7. **禁令清单不变**：不读写 SessionStorage、不调用 PromptService/ToolService/SkillService/ModelGateway、不读取 current Workspace/definition、不拼接 prompt、不在 `tool_round_completed` 前把 ToolResult 加入 conversation、不处理 approval、不发布事件、不决定 terminal。
8. **删除 monolithic adapter task 例外**：自研状态机是同步纯逻辑，由 SessionExecutor 主循环直接方法调用；`session-execution.md`"强制 two-owner execution"否决条款中为 Rig monolithic future 保留的 private adapter task 但书随本 ADR 删除。

## 理由

- **深度错配，adapter 不通过 deletion test**。Rig AgentRun 的价值（loop 编排、内建 tool 调用、history 管理）恰好是 MiniCore 明确外置或禁止的部分；采用 Rig 后仍需一个把 Rig 消息/历史类型翻译为 `FinalizedAssistantResponse` / `CommittedConversationDelta` 的双向 adapter，其体量与复杂度不低于自研 loop 本身——loop 的剩余职责只是三态推进加 coverage 校验（估计 200–400 行纯逻辑）。删除 Rig loop 不会让复杂性散落；删除翻译 adapter 反而消除一整层。
- **一致性净化**。自研消除三处特判：monolithic future 例外（two-owner 风险源）、Steer rollover 的 adapter 行为差异、Rig 版本升级对 loop 语义的耦合。sans-I/O 纯逻辑可直接 property test（任意合法 delta 序列驱动状态机，验证动作序列与协议不变量），无需 mock SDK。
- **风险有界**。放弃的是 Rig 未来 loop 侧能力（multi-step conveniences、structured extraction helper 等），但这些能力以 SDK 自持状态为前提，与 Transcript-First 冲突，在 MiniCore 中本就不可用——无净损失。Rig 在 provider 侧的真实价值（多 provider 协议、streaming、typed usage）完整保留。

## 后果

- `architecture.md`：设计定位与核心边界改写——Rig 从「原生 Agent SDK」降为「ModelGateway private provider adapter 的实现库」；AgentLoop 归 MiniCore 自研。
- `session-execution.md`：AgentLoop Interface 表述改为自研 concrete implementation；删除 monolithic adapter task 例外。
- `turn-execution-context.md`：AgentLoop Contract 删除 SDK adapter 措辞；segment 重建理由收窄为 Compaction Replace；后续问题 1（Rig sans-I/O adapter 形状）关闭。
- `CONTEXT.md`：「Driver（AgentLoop 适配器）」条目改写为自研状态机描述，Driver 旧称废弃。
- `README.md`、`docs/migration/v1-to-v2.md`：Rig 集成表述与 spike 验收范围同步收窄为 provider adapter。
- 不建立：public AgentLoop trait、`AgentLoopFactory`、loop 插件机制、推理策略框架（ReAct/Plan-and-Execute 等）、多 agent 编排——AgentLoop 是协议状态机，不是推理框架；这些能力若未来出现，属于独立设计，不进入本状态机。

## 测试要求

- 三态转换全覆盖：合法序列产生确定动作序列；每个非法转换返回 typed ProtocolViolation；
- tool round coverage：missing/duplicate/reordered/跨 Turn ToolCallId 均拒绝；
- candidate 幂等：EmittedCandidate 只能被消费一次；Continue 后状态机可继续；
- Compaction Replace 后旧 segment 不可继续使用，新 seed 重建后行为与全量 replay 等价；
- property test：任意合法 committed delta 序列驱动，loop 动作与 conversation 协议不变量（无孤立 ToolCall、无未覆盖 round 进入 NeedModel）恒成立；
- loop 不产生任何 I/O、不持有任何 `Arc<Service>`（编译期由字段类型保证）。
