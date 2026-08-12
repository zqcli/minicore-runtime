# ADR 0138：Production Provider baseline只采用已验证的Rig协议合同

状态：Partially Superseded by ADRs 0139 and 0141
日期：2026-08-11

Refinement note：本ADR冻结的OpenAI Responses/Anthropic Messages协议、terminal、metadata、single-attempt与delivery/error合同继续有效；`rig-core`作为主crate dev/production dependency及M14 `RigProviderAdapter`的决定已被ADR 0139取代。Rig 0.40.0现只存在于standalone stable-only evidence harness，M14改为两个direct provider adapters。

> 2026-08-12：[ADR 0141](0141-provider-calls-are-stateless-full-request.md)细化single-attempt wire语义：M14 adapter绝不发送optimization-specific fallback POST。“provider拒绝continuation→fallback full request”的旧语言不能解释为同一`generate_model_turn`内的第二次POST；当前政策是omission（不请求continuation），未来fallback只能在新ADR下作为later distinct logical request之前规划。

## 背景

ModelGateway已经冻结provider-neutral request/result/error、single provider attempt、Session logical retry和private `ProviderAdapter` seam，但production `RigProviderAdapter`开始前仍有三个现实风险：文档列出未经验证的protocol；Rig generic API会丢失部分finish、terminal与response metadata证据；provider建议retry与MiniCore delivery-safe logical retry并不等价。

M12使用exact `rig-core = 0.40.0`、真实`127.0.0.1:0` HTTP/1.1 loopback server和公开`HttpClientExt` seam执行reality gate。所有测试离线、无credential/ambient home config、无sleep/timeout/yield absence proof，server thread由poison connection确定性结束并join。

## 决策

1. 首个production baseline的`ProviderProtocol`支持集只有：

   ```rust
   pub enum ProviderProtocol {
       OpenAiResponses,
       AnthropicMessages,
   }
   ```

   OpenAI Chat Completions、Gemini和其他protocol不进入首版enum或available catalog。未来增加variant必须先有同等级loopback contract、delivery/error fixture和独立decision；不能根据endpoint或model name猜protocol。M12关闭不使任何provider立即available，production catalog仍等待M14 adapter。

2. Rig保持ModelGateway private implementation detail。`rig-core`在M12仅是exact dev-dependency，Rig Agent、Conversation、Message、Usage、retry、provider request/response和error类型都不能进入Prompt、Conversation Storage、Runtime public DTO或public Wire。M14可以在private adapter内使用Rig provider-specific types和公开`HttpClientExt`，不能扩大provider-neutral seam来迁就SDK。

3. 一次`generate_model_turn`最多执行一个provider attempt。M12 unary、streaming、429、500和529 probes均证明一次completion/stream invocation只产生一个HTTP request；Rig/SDK automatic retry按观察合同视为0。M14不得启用Rig retry policy、429/5xx retry、stream restart、401 refresh-and-resend或transport fallback。

4. Rig generic stream的`Final`不是MiniCore terminal proof。M14 private HTTP wrapper必须在原始SSE bytes进入Rig parser的同时增量观察protocol terminal：

   - OpenAI Responses成功需要`response.completed`；
   - Anthropic Messages成功需要`message_delta`携带non-empty `stop_reason`；不能额外要求`message_stop`，因为Rig 0.40.0在terminal delta后可以停止polling；
   - provider error event优先否决success；
   - connection EOF、transport error或stream drop在未观察terminal时都不是成功；
   - Rig在两协议early EOF后都会合成zero-usage `Final`，该值不得覆盖缺失terminal的事实。

   Observer只保存bounded typed evidence并原样转发bytes，不重写provider payload，也不把raw stream交给caller。MiniCore cancellation桥接Rig/transport abort；取消后不得把partial content当terminal response。

5. finish与identity只从typed/provider-specific事实提取。OpenAI使用Responses terminal event、response `status`与可用的incomplete details；Anthropic使用`stop_reason`。ToolCall content继续是进入Tool path的主事实。无法取得finish时使用`Unknown`，不得根据assistant文本猜测。response body ID与HTTP request ID是两个独立optional事实；二者不相等也不改变conversation identity。

6. M14 private HTTP wrapper只读取以下response header allowlist：

   - OpenAI：`x-request-id`、`retry-after`、`openai-processing-ms`；
   - Anthropic：`request-id`、`retry-after`。

   request ID经过既有长度/字符/redaction validation后可进入`ProviderResponseMetadata`；`retry-after`只进入typed retry hint；processing time只属于internal telemetry。其他header、cookie、authorization、完整header map、raw body和canary字段不可表示。Rig正常response/error解析仍消费原body；wrapper不能改变成功输出或usage。

7. provider-neutral error与delivery分类的唯一M12 fixture是[`error-mapping-v1.json`](../fixtures/provider-gate-m12/error-mapping-v1.json)。分类只读取status、typed error type/code、transport stage、semantic-output-started、terminal evidence和bounded retry hint，不匹配human message。

   - 只有`NotSent`或provider明确证明未开始执行的`RejectedBeforeExecution`可保留transient reason供Session logical retry；
   - OpenAI typed 429和Anthropic typed 429属于pre-execution rejection；`Retry-After <= 60s`才允许retry；
   - Anthropic HTTP 529 `overloaded_error`属于typed pre-execution rejection；HTTP 200 stream中的同类error已经accepted，归为`AcceptedNoOutput → RequestOutcomeUnknown`；
   - OpenAI 500/503与Anthropic 500/504即使provider或SDK建议retry，也因delivery未知归为`RequestOutcomeUnknown`，不能自动重发；
   - first semantic output后任何transport/provider failure归为`OutputStarted → StreamInterrupted`；
   - malformed 200 response归为`InvalidProviderResponse`且terminal；
   - OpenAI只有exact `context_length_exceeded` code映射`ContextOverflow`并进入Compaction；Anthropic `invalid_request_error`没有稳定overflow subtype时保持`InvalidRequest`，不得解析message prose。

8. queued Steer不改变in-flight request的`ConversationRevision`，也不使result或retry backoff失效。Steer只在safe point成功apply后通过revision变化使旧request basis失效；Cancel/SecurityRevoked仍通过control arbitration立即阻止retry。

## 可执行证据

- `provider-gate/tests/m12_rig_openai_responses.rs`：OpenAI instructions/messages/tools/reasoning/structured schema、identity/order/status/usage、base URL和500 single-request；
- `provider-gate/tests/m12_rig_anthropic_messages.rs`：Anthropic system/messages/tools/thinking/signature/cache-control、identity/stop reason/usage、base URL和500 single-request；
- `provider-gate/tests/m12_rig_openai_streaming.rs`与`provider-gate/tests/m12_rig_anthropic_streaming.rs`：完整stream、usage、cancel、early EOF和500 single-request；
- `provider-gate/tests/m12_rig_terminal_evidence.rs`：fragmented SSE、terminal/EOF/error/drop可区分的公开`HttpClientExt` seam；
- `provider-gate/tests/m12_rig_response_metadata.rs`：成功/error路径的header allowlist、body ID/header ID独立性与canary rejection；
- `provider-gate/tests/m12_rig_error_envelopes.rs`：两协议400/401 typed envelope、malformed 200 fail-closed和single-request；
- `tests/m12_provider_error_matrix.rs`：26-case closed taxonomy、delivery normalization、retry/Compaction不变量与no-message-classification；
- `session_execution::tests::steer_queued_during_agent_run_retry_backoff_is_consumed_after_success`：queued Steer规则。

## 后果

V4-P1-3关闭，M14可以在M13 Tool/Sandbox gate完成后实现两个独立production adapters。M14必须复用本ADR的protocol、terminal、metadata和delivery evidence，并让production adapter contract tests消费同一fixture；不能用Rig synthetic `Final`、HTTP status alone或“尚无delta”替代delivery proof。

本ADR不实现credential resolution、connection/cache/continuation policy、provider-native Structured schema mapping/sanitization、public structured requester或production adapter，也不批准真实network/credential测试成为默认suite。它细化ADR 0106、0119、0120与0125；single-attempt、Session logical retry和Gateway no-local-permit原则不变。
