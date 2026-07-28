# ADR 0119: 模型调用使用Session逻辑重试

状态：Accepted
日期：2026-07-27

## 背景

ModelGateway原设计允许同一个`generate_model_turn` operation内部执行provider transparent retry、401 refresh后重发和同model transport fallback；SessionExecutor在Gateway terminal error后还可以启动logical retry。两层各自有局部上限时，provider attempt、backoff和总耗时会相乘。为两层建立Turn-scoped共享`ModelCallBudget`可以解决上限问题，但会增加跨operation可变状态、消费回执和更多测试面。

pi当前默认关闭provider层retry，由AgentSession对失败模型调用执行最多3次auto-retry。MiniCore首个production vertical slice更需要行为确定、实现简单和错误语义可验证，而不是最大化透明恢复率。

权威设计见[ModelGateway](../modules/model-gateway.md)、[Session Execution](../modules/session-execution.md)和[Compaction](../modules/compaction.md)。

## 决策

1. MVP中一次`ModelGateway::generate_model_turn`最多执行一个provider attempt。`ProviderAdapter::execute`最多调用一次；Rig和底层provider SDK的automatic retry必须配置为0。ModelGateway不执行普通transparent retry，不在401后refresh-and-resend，也不自动执行WebSocket到HTTP或其他transport fallback。
2. Credential可以在provider request开始前由AuthStore进行一次singleflight resolve/refresh；provider返回401后本次operation直接terminal为`AuthRejected`，不在Gateway内部重发。
3. Provider delivery state仍必须准确分类：未得到完整terminal response时，根据实际阶段返回`Timeout`、`TransportUnavailable`、`ProviderUnavailable`、`RequestOutcomeUnknown`或`StreamInterrupted`。没有内部retry不等于可以弱化delivery proof。
4. SessionExecutor拥有logical model retry。它只对同一个immutable `Arc<ModelCallRequest>`重启新的唯一`RunningOperation`，并且必须在旧operation terminal/remove或安全drop并关闭结果路径后执行；retry不重新assemble Prompt，不重新resolve Model，也不比较request/context fingerprint。
5. `AgentRun`默认最多3次logical retry，即一次初始调用加三次重试，backoff固定为2秒、4秒、8秒。一次成功finalized response或新的`ModelCallRequest`开始后计数重置；不是整个agentic Turn共享一个计数器。
6. `CompactionSummary`默认最多1次logical retry，使用2秒backoff。它必须复用同一个`Arc<CompactionPlan>`派生出的同一个`Arc<ModelCallRequest>`，以及相同source checkpoint、summary directive、assembled context和request proof。
7. 默认可自动logical retry的错误必须由Gateway证明delivery为`NotSent`或`RejectedBeforeExecution`，且reason是`Timeout`、`TransportUnavailable`、`ProviderUnavailable`，或typed `Retry-After`不超过60秒的`RateLimited`。仅“尚无delta”或`AcceptedNoOutput`不足以重放；缺少provider明确的pre-execution rejection proof时必须映射`RequestOutcomeUnknown`。实际delay取当前指数backoff与provider hint的较大值；超过60秒时直接返回terminal error。
8. `RequestOutcomeUnknown`、`StreamInterrupted`、`AuthMissing`、`AuthRejected`、`QuotaExceeded`、`ModelUnavailable`、`UnsupportedCapability`、`InvalidRequest`、`SafetyBlocked`、`ProviderRejected`、`UnexpectedToolCall`、`InvalidStructuredOutput`、`InvalidProviderResponse`和`IncompleteResponse`默认不自动retry。`ContextOverflow`进入bounded Compaction recovery，不算普通retry。模型响应错误的ownership与命名由[ADR 0120](0120-failures-stay-with-owning-modules.md)确定。
9. logical retry前必须重新确认Turn仍Running、`execution_version`、exact `ConversationCheckpoint.entry_id`、`current_operation`仍为持有同一request的对应retry slot、control basis、purpose、output contract和effective max output均未变化。Steer、Cancel、revocation、Compaction Replace或任何model-visible conversation变化都会推进checkpoint或control basis，使scheduled retry失效。由于retry复用同一个`Arc<ModelCallRequest>`，不重新assemble，也不比较`AssembledModelContextFingerprint`。
10. `StoredAssistantMessage.retry_count`和`StoredCompactionModelCall.logical_retry_count`只记录Session logical retry次数。Gateway不再返回`transparent_retry_count`，不发布provider retry lifecycle progress，也不引入`ModelCallBudget`、`ModelAttempt`或retry registry。

## 后果

- 每个AgentRun logical call最多4次Gateway invocation、因此最多4次provider request；每个CompactionSummary最多2次。preflight/cooldown可以使实际provider request更少，次数不再由两层上限相乘。
- retry backoff由SessionExecutor timer调度，不持有ModelGateway并发permit，也不阻塞SessionIngress control loop。
- Host可通过`model_retry_scheduled`观察logical retry；provider attempt本身仍只发布content delta，失败attempt不进入SessionStorage。
- MVP面对瞬时网络故障时会经过完整operation terminal/revalidation路径，代码路径略长，但符合单Session严格串行和Transcript-First边界。
- 禁用transparent transport fallback和401 resend会降低部分瞬时故障恢复率；有真实生产数据后可以另立ADR增加窄Gateway retry，但必须先证明不会重新引入隐藏SDK retry和次数相乘。
- 失败provider attempt可能已经产生work或计费；因此不确定delivery默认不自动重放，MiniCore仍不宣称model exactly-once。

## 后续修订

2026-07-28：[ADR 0123](0123-identity-uses-refs-and-explicit-reload.md)删除logical retry中的request/context fingerprint比较，明确以同一个immutable `Arc<ModelCallRequest>`、exact checkpoint entry和SessionExecutor单current operation验证执行一致性。本ADR的Gateway single attempt、retry次数、backoff和错误分类保持不变。

## 被否决方案

### Gateway与SessionExecutor共享Turn级ModelCallBudget

否决原因：可以给attempt、elapsed和backoff建立严格总上限，但当前阶段需要在两个operation owner之间传递可变余额和消费回执；一个Turn又可能包含多个健康ToolRound模型调用，Turn级共享attempt池容易过度收紧。

### 只保留ModelGateway透明重试

否决原因：Gateway不知道Turn是否仍Running、conversation是否变化，也不能裁决Steer、Cancel、revocation和Compaction；这些事实属于SessionExecutor。

### 对所有typed ModelCallError统一重试三次

否决原因：`RequestOutcomeUnknown`、`StreamInterrupted`、认证、quota、配置和协议错误不是普通transient failure，blind replay可能重复provider work、内容或计费。
