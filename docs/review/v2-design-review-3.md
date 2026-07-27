# MiniCore V2 设计评审（第三轮）

状态：设计评审记录（发现待决议）
日期：2026-07-27
范围：ADR 0105、0115、0120，`session-execution.md`、`turn-execution-context.md`、`model-gateway.md`、`conversation-storage.md`
方式：在 ADR 0115 接受 AgentLoop 自研协议状态机后，对其状态转换、输入 interface、动作消费语义和既有文档同步情况进行专项复核。

## 总体判断

ADR 0115 的核心方向合理：AgentLoop 收敛为 MiniCore 自研、crate-private、同步 sans-I/O 协议状态机；SessionExecutor 继续拥有模型调用、Tool 副作用、storage、control、compaction 与 terminal arbitration；Rig 只作为 ModelGateway private `ProviderAdapter` 的实现选择，不拥有 AgentLoop 或 ModelGateway interface。

该设计符合 Transcript-First、单执行 owner 和深模块原则，也消除了 Rig monolithic run 造成的第二 owner 与双向翻译 adapter。原评审发现五项interface/文档缺口；L1已由ADR 0120关闭，L5此前已关闭。L2仍是AgentLoop编码前阻塞项，L3–L4保持开放。

## 一、编码前必须定案

### L1 · Model finish reason 与三态转换缺少完整决策表（已关闭）

原问题是：ADR 0115将模型响应归约为content含ToolCall时进入`PendingToolRound`，不含ToolCall时进入`EmittedCandidate`；当时`model-gateway.md`又把Length、ContentFiltered、Refused和Unknown的最终裁决留给Session execution，导致转换不唯一。

- 决议：ModelGateway在构造`ModelCallResult`前统一校验`ModelFinishReason × ToolCall presence × OutputContract`。当前调用禁止Tool却返回call时使用`UnexpectedToolCall`；Structured JSON/schema错误使用`InvalidStructuredOutput`；finish/content、empty Refused或wire语义冲突使用`InvalidProviderResponse`；Length、ContentFiltered、empty Stop/Unknown和reasoning-only terminal使用`IncompleteResponse`。四者均non-retryable，不进入AgentLoop或ToolSet。
- AgentLoop precondition：`accept_model_response`只接收validated response；含ToolCall进入`PendingToolRound`，不含ToolCall进入`EmittedCandidate`。non-empty Refused是合法candidate；pre-generation safety block仍是Model error。
- 测试：覆盖`ToolCalls + no calls`、`Stop/Length/Refused/Unknown + calls`、Length/ContentFiltered、empty与non-empty Refused、Structured syntax/schema，以及NoToolCalls/Structured下UnexpectedToolCall。
- 关闭依据：[ADR 0120](../adr/0120-failures-stay-with-owning-modules.md)、`model-gateway.md` Response Validation与ADR 0115更新。
- 出处：ADR 0115「内部状态机冻结为三态」；`model-gateway.md`「Finish Reason」。

### L2 · `next_action()` 的一次性发出与消费语义未冻结

AgentLoop interface只有`next_action(&mut self) -> AgentLoopAction`，ADR同时要求`EmittedCandidate`只能被消费一次。文档没有说明重复调用`next_action()`时返回相同动作、返回typed duplicate-action error，还是依赖内部issued marker。相同问题也存在于`NeedTools`，但当前测试要求只点名candidate。

- 影响：Executor 重入、错误恢复或未来重构可能重复启动 Tool operation、重复处理 candidate final；三态名称本身不能证明 action 已发出还是尚未发出。
- 建议：冻结`next_action()`为one-shot action emission；每个带副作用后续处理的action在当前状态中只能成功取出一次，重复调用返回typed duplicate-action error。具体reason名称随L2一起决定，不由ADR 0120提前冻结。实现可在三种顶层状态内部使用private `issued/response: Option<_>`子状态，无需增加public trait、第四个公开状态或额外方法。
- 测试：`NeedTools`、`Finished` 重复 poll 均拒绝；`NeedModel` 在 operation 已启动期间不会被二次发出；合法 accept 后 issued marker 被正确重置。
- 出处：`session-execution.md`「AgentLoop Interface」；ADR 0115「协议校验」「测试要求」。

## 二、同轮文档修订应关闭

### L3 · `accept_committed_*` 使用过宽的通用 conversation delta

`accept_committed_tool_round` 与 `accept_committed_steer` 都接收通用 `CommittedConversationDelta`；该类型可表示 `AdvanceOnly | Append | Replace`。ToolRoundCompleted 产生的 `Append(messages)` 确实包含 assistant/tool messages，AgentLoop可以据此检查 expected ToolCallIds，但它必须重新解析 storage projection 的通用消息形状，才能确认调用来源和协议用途。

- 影响：AgentLoop 与 conversation projector 的内部表示耦合；方法接受的输入集合大于其合法输入集合，错误调用只能在运行时解析后发现；“trusted delta”与“适用于当前 AgentLoop transition 的 typed delta”没有被区分。
- 建议：优先增加 crate-private 窄类型，如 `CommittedToolRoundDelta` 与 `CommittedSteerDelta`，由 storage-owned apply receipt 生成并保留 checkpoint/Turn/ordered coverage proof。若暂不增加类型，至少在 interface 下冻结两种合法 `ConversationChange::Append` 的精确 shape、provenance 和拒绝规则。
- 出处：`session-execution.md`「AgentLoop Interface」；`conversation-storage.md`「Conversation Projection」「ToolRoundCompleted」。

### L4 · Steer 原地推进与 segment 重建的规范路径不一致

ADR 0115 将 `accept_committed_steer` 原地推进定为默认路径，ConversationSeed 重建只作为等价实现自由；`session-execution.md` 的 AgentLoop interface 说明和 Completed Turn 主流程仍将“重建 AgentLoop segment”写成外部主路径，其他位置又写成二选一。

- 影响：`accept_committed_steer` 可能成为未使用 interface；不同实现会把 candidate invalidation、checkpoint 验证和 segment 生命周期放在不同 owner 中，测试路径随实现分叉。
- 建议：SessionExecutor 的规范流程固定调用 `accept_committed_steer(trusted delta)`；AgentLoop implementation 可以在该方法内部增量推进或通过 private helper 等价重建。只有 Compaction Replace 由 SessionExecutor 从新 ConversationSeed 强制重建 segment。
- 出处：ADR 0115「Steer 原地推进为默认路径」；`session-execution.md`「AgentLoop Interface」「Steer流程」「Completed Turn流程」。

### L5 · ADR 0105 残留“AgentLoop 可替换（含 Rig adapter）”旧结论（已关闭）

ADR 0105 的后果仍写“AgentLoop 可替换（含 Rig adapter）”，与 ADR 0115 已关闭 Rig/SDK loop 实现分支、且第二个真实实现出现前不建立稳定 seam 的决定冲突。

- 影响：正式 ADR 同时给出两种相反结论，可能诱导实现者增加 `AgentLoop` public trait、factory、registry 或 Rig loop adapter，重新引入已删除的间接层。
- 处理：已修订 ADR 0105，明确 AgentLoop 保持 crate-private concrete state machine、第二个真实实现出现前不建立稳定替换 seam，并在历史段登记 ADR 0115；同时在架构入口、ModelGateway权威设计、ADR 0106/0115、Runtime调用链和迁移记录中统一Rig职责：RigProviderAdapter只处理provider attempt，ModelGateway拥有其余模型调用编排与terminal语义。
- 出处：ADR 0105「后果」；ADR 0115「决定」「后果」。

## 三、结论摘要

| 项目 | 结论 |
| --- | --- |
| 自研 AgentLoop 决策 | 合理，保持 Accepted |
| SessionExecutor / AgentLoop ownership | 总体清晰，无需合并 |
| 已关闭 | L1 finish-reason/OutputContract决策表、L5 Rig职责旧叙事 |
| 编码前阻塞 | L2 one-shot action emission |
| interface 收窄 | L3 建议使用 trusted typed protocol delta |
| 文档一致性 | L4 Steer路径待同步 |
| Rig 边界 | 只实现 ModelGateway 内部 ProviderAdapter 的 provider 映射与调用，不拥有 ModelGateway |

## 评审决议

- **L1**：**已关闭**（2026-07-27）。ADR 0120将Provider response错误归ModelGateway，并冻结四个直接error reason与non-retry语义；AgentLoop只接收validated response。
- **L2–L4**：待决议。建议在AgentLoop首个实现提交前回写ADR 0115、`session-execution.md`与必要的storage interface说明。
- **L5**：**已关闭**（2026-07-27）。ADR 0105旧结论已修订，Rig职责已统一为ModelGateway private ProviderAdapter中的provider attempt映射与调用；Rig不拥有AgentLoop或ModelGateway编排。
- 修订完成门槛：action one-shot语义可测试；ToolRound/Steer输入contract收窄；Steer规范路径唯一。finish-reason决策表与Rig loop adapter旧叙事已完成。
