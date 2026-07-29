# MiniCore V2 设计评审（第三轮）

状态：设计评审记录（发现待决议）
日期：2026-07-29
范围：ADR 0105、0115、0120，`session-execution.md`、`turn-execution-context.md`、`model-gateway.md`、`conversation-storage.md`
方式：在 ADR 0115 接受 AgentLoop 自研协议状态机后，对其状态转换、输入 interface、动作消费语义和既有文档同步情况进行专项复核。

## 总体判断

ADR 0115 的核心方向合理：AgentLoop 收敛为 MiniCore 自研、crate-private、同步 sans-I/O 协议状态机；SessionExecutor 继续拥有模型调用、Tool 副作用、storage、control、compaction 与 terminal arbitration；Rig 只作为 ModelGateway private `ProviderAdapter` 的实现选择，不拥有 AgentLoop 或 ModelGateway interface。

该设计符合 Transcript-First、单执行 owner 和深模块原则，也消除了 Rig monolithic run 造成的第二 owner 与双向翻译 adapter。原评审发现五项interface/文档缺口；L1已由ADR 0120关闭，L3已由ADR 0124关闭，L4已通过权威流程同步关闭，L5此前已关闭。L2仍是唯一AgentLoop编码前阻塞项。2026-07-30补充研究进一步发现，L2需要先决定是否保留pull式`next_action()`，不能只讨论issued marker和error reason。

## 一、编码前必须定案

### L1 · Model finish reason 与三态转换缺少完整决策表（已关闭）

原问题是：ADR 0115将模型响应归约为content含ToolCall时进入pending Tool状态，不含ToolCall时进入`EmittedCandidate`；当时`model-gateway.md`又把Length、ContentFiltered、Refused和Unknown的最终裁决留给Session execution，导致转换不唯一。当前状态名已由ADR 0124统一为`PendingToolExchange`。

- 决议：ModelGateway在构造`ModelCallResult`前统一校验`ModelFinishReason × ToolCall presence × OutputContract`。当前调用禁止Tool却返回call时使用`UnexpectedToolCall`；Structured JSON/schema错误使用`InvalidStructuredOutput`；finish/content、empty Refused或wire语义冲突使用`InvalidProviderResponse`；Length、ContentFiltered、empty Stop/Unknown和reasoning-only terminal使用`IncompleteResponse`。四者均non-retryable，不进入AgentLoop或ToolSet。
- AgentLoop precondition：`accept_model_response`只接收validated response；含ToolCall进入`PendingToolExchange`，不含ToolCall进入`EmittedCandidate`。non-empty Refused是合法candidate；pre-generation safety block仍是Model error。
- 测试：覆盖`ToolCalls + no calls`、`Stop/Length/Refused/Unknown + calls`、Length/ContentFiltered、empty与non-empty Refused、Structured syntax/schema，以及NoToolCalls/Structured下UnexpectedToolCall。
- 关闭依据：[ADR 0120](../adr/0120-failures-stay-with-owning-modules.md)、`model-gateway.md` Response Validation与ADR 0115更新。
- 出处：ADR 0115「内部状态机冻结为三态」；`model-gateway.md`「Finish Reason」。

### L2 · `next_action()` 的一次性发出与消费语义未冻结

AgentLoop interface只有`next_action(&mut self) -> AgentLoopAction`，ADR同时要求`EmittedCandidate`只能被消费一次。文档没有说明重复调用`next_action()`时返回相同动作、返回typed duplicate-action error，还是依赖内部issued marker。相同问题也存在于`NeedTools`，但当前测试要求只点名candidate。

- 影响：Executor 重入、错误恢复或未来重构可能重复启动 Tool operation、重复处理 candidate final；三态名称本身不能证明 action 已发出还是尚未发出。
- 建议：冻结`next_action()`为one-shot action emission；每个带副作用后续处理的action在当前状态中只能成功取出一次，重复调用返回typed duplicate-action error。具体reason名称随L2一起决定，不由ADR 0120提前冻结。实现可在三种顶层状态内部使用private `issued/response: Option<_>`子状态，无需增加public trait、第四个公开状态或额外方法。
- 测试：`NeedTools`、`Finished` 重复 poll 均拒绝；`NeedModel` 在 operation 已启动期间不会被二次发出；合法 accept 后 issued marker 被正确重置。
- 出处：`session-execution.md`「AgentLoop Interface」；ADR 0115「协议校验」「测试要求」。

#### 2026-07-30补充研究

[AgentLoop执行模型跨项目研究](../research/agent-loop-execution-model-study.md)核对了Pi 0.80.6、Codex `61a4488`、OpenCode `7565e03`、Gemini CLI `3818efb`和OpenHands SDK `68cd02e`：五者均自研loop，但主要使用async task/fiber或step内I/O，没有采用MiniCore式纯`NeedModel | NeedTools | Finished` pull reducer。Codex和OpenCode在Tool授权期间分别通过`oneshot`和`Deferred`暂停当前Tool future，同时由独立control路径处理approval reply，因此Cancel/approval响应并不要求loop外I/O。

Git history同时确认，`next_action()`源自V1 Rig `AgentRun::next_step()`/Driver pull seam；ADR 0115切换为first-party实现时明确保留既有interface，没有重新比较transition-returning reducer。ADR 0124随后删除same-Turn recovery、durable Tool start/round marker和大部分execution proof chain，使AgentLoop当前只剩live protocol reduction。

L2现有两个候选关闭方式：

```text
方案A：保留next_action()
→ private issued/Option::take
→ repeated poll返回typed ActionAlreadyIssued

方案B：删除next_action()
→ from_seed/accept_*在每次合法状态转换时直接返回next AgentLoopEffect
→ NeedModel | NeedTools | CandidateReady
```

补充研究当前倾向方案B：它保留first-party、crate-private、sans-I/O、validated/committed typed input和Compaction reseed，同时从interface中删除poll discipline、issued marker和L2自造的重复poll状态。该倾向不是Accepted决策；正式关闭L2前必须二选一。若选择方案B，需要修订ADR 0115及Session Execution/Turn Context权威合同；若选择方案A，则按原建议冻结one-shot和typed error。

## 二、同轮文档修订应关闭

### L3 · `accept_committed_*` 使用过宽的通用 conversation delta（已关闭）

ADR 0124删除`ToolRoundCompleted` marker并收窄AgentLoop输入：

```text
accept_committed_tool_results(CommittedToolExchangeDelta)
accept_committed_steer(CommittedSteerDelta)
```

`CommittedToolExchangeDelta`只能由SessionStorage在同一assistant全部ToolCall形成有效matching result集合时生成，保留Turn、ordered ToolCall coverage和conversation checkpoint。live duplicate被strict writer拒绝；cold replay duplicate first valid wins，incomplete/orphan/identity-conflicting或abandoned-first exchange无法构造该类型。AgentLoop无需解析通用`AdvanceOnly | Append | Replace`。

- 关闭依据：[ADR 0124](../adr/0124-session-replay-is-tolerant-and-links-are-minimal.md)、`session-execution.md`「AgentLoop Interface」和`conversation-storage.md`「Tool Exchange Projection」。

### L4 · Steer 原地推进与 segment 重建的规范路径不一致（已关闭）

SessionExecutor的规范流程已固定为：Steer commit后调用`accept_committed_steer(CommittedSteerDelta)`；AgentLoop可在该方法内部增量推进或使用private helper等价重建。只有Compaction Replace由SessionExecutor从new `ConversationSeed`强制重建segment。

- 关闭依据：ADR 0115「Steer原地推进为默认路径」、`session-execution.md`「Steer流程」、`turn-execution-context.md`「AgentLoop lifecycle」和`conversation-storage.md`的typed delta private-construction contract。
- 测试：Tool exchange后Steer、Assistant Continue后Steer都调用同一公开crate-private method；Compaction Replace不调用Steer delta并强制new seed。

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
| 已关闭 | L1 finish-reason/OutputContract决策表、L3 typed protocol delta、L5 Rig职责旧叙事 |
| 编码前阻塞 | L2：保留pull one-shot或改用transition-returning effect reducer，必须二选一 |
| interface 收窄 | `CommittedToolExchangeDelta`与`CommittedSteerDelta`已冻结 |
| 文档一致性 | L4 Steer规范路径已统一 |
| Rig 边界 | 只实现 ModelGateway 内部 ProviderAdapter 的 provider 映射与调用，不拥有 ModelGateway |

## 评审决议

- **L1**：**已关闭**（2026-07-27）。ADR 0120将Provider response错误归ModelGateway，并冻结四个直接error reason与non-retry语义；AgentLoop只接收validated response。
- **L2**：待决议，必须在AgentLoop首个实现前冻结。2026-07-30补充研究将决策面扩展为“保留pull one-shot”与“transition-returning effect reducer”二选一；研究倾向后者，但尚未形成Accepted ADR。
- **L3**：**已关闭**（2026-07-29）。ADR 0124采用`CommittedToolExchangeDelta`与`CommittedSteerDelta`，删除通用conversation delta输入。
- **L4**：**已关闭**（2026-07-29）。SessionExecutor固定调用`accept_committed_steer`；只有Compaction Replace由Executor从new seed重建segment。
- **L5**：**已关闭**（2026-07-27）。ADR 0105旧结论已修订，Rig职责已统一为ModelGateway private ProviderAdapter中的provider attempt映射与调用；Rig不拥有AgentLoop或ModelGateway编排。
- 剩余完成门槛：L2先冻结AgentLoop effect emission interface。方案A必须测试duplicate poll/issued marker；方案B必须测试每次合法transition只返回一次effect且stale/wrong-state accept被拒绝。finish-reason、typed Tool/Steer delta、Steer规范路径与Rig职责均已完成。
