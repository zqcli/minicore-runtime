# ADR 0141: Provider调用是Stateless Full-Request Wire Policy

状态：Accepted
日期：2026-08-12

## 背景

M14两个direct provider adapters（`OpenAiResponsesProviderAdapter`、`AnthropicMessagesProviderAdapter`）已经实现，但既有文档仍把`ProviderConnectionPool`、`ContinuationCache`、`AuthStore`/auth-principal identity描述为ModelGateway implementation内容，并把“provider拒绝continuation后fallback full request”写成仿佛一次`generate_model_turn`内部可以先发优化请求、再在拒绝后补发第二个完整请求。这与M14实际实现的wire事实冲突：每次`generate_model_turn`至多调用一次`ProviderAdapter::execute`（owner validation/pre-send cancellation/`AuthMissing`在调用adapter前以typed error terminal，因此为0或1次）；独立地，每次invocation发送零或一个HTTP POST——adapter编码/build失败或adapter级pre-send cancellation发生在那一次`execute`内部且不产生POST；若发送POST则携带完整full request，从不发送optimization-specific fallback POST，也没有重试或continuation state。

同时，Anthropic adapter的usage归一化曾用本地`checked_add`把input/cache-read/cache-write/output相加合成`provider_total_tokens`，并因和溢出而fail closed。这违反[ModelGateway架构](../modules/model-gateway.md#usage)的规则：`provider_total_tokens`只保存provider报告值，不通过其他字段相加伪造。Anthropic Messages在consumed contract中不报告total，因此归一化usage必须恒为`None`。

本ADR把M14 stateless full-request wire policy冻结为显式决策，并细化ADR 0106、0119、0123、0125、0138、0139中的相关语言。

## 决策

1. **每次invocation执行零或一次`ProviderAdapter::execute`；独立地，发送零或一个HTTP POST；若发送则携带完整full request。** 每次`generate_model_turn`至多调用一次`ProviderAdapter::execute`：owner validation、pre-send cancellation和`AuthMissing`都在调用adapter之前以typed error terminal（零次execute、零个POST）；adapter编码/build失败或adapter级pre-send cancellation发生在那一次`execute`内部，同样零个POST（一次execute、零个POST）；若`ProviderAdapter::execute`发送了POST，则它总是携带完整full request并消费一个terminal。从不调用第二次`execute`，从不发送第二个POST，从不发送optimization-specific fallback POST；不存在adapter/gateway内重试，不存在continuation state。旧语言“provider rejects continuation then fallback full request”不能解释为同一次operation内的第二次POST：M14的政策是omission（根本不再请求continuation），任何未来fallback只能作为later distinct logical request之前的新ADR规划（ADR 0119的Session logical retry是另一条路径，每次retry都是一次新的完整invocation）。

2. **连接政策就是adapter-owned的reqwest client。** 每个provider installation在`MiniCoreRuntime::open`时构建一个adapter（一个reqwest client），shared-resource reload复用同一source/client，不重建。这只描述普通无状态reqwest transport pooling eligibility（HTTP连接复用能力），不承诺物理socket必然复用，也不携带auth或session state。不存在`ProviderConnectionPool`、`ProviderRuntime`、`ProviderWirePlan`或`ProviderRateLimitState`类型的实现，也没有connection/session隔离键。

3. **OpenAI Responses wire。** 保留`store=false`；永不发送`previous_response_id`、`prompt_cache_key`、`prompt_cache_retention`、`cache_control`、`conversation`/sticky-provider-session字段或incremental-input优化。provider报告的`input_tokens_details.cached_tokens`等只作为usage evidence，不构成请求或控制。

4. **Anthropic Messages wire。** 永不发送`cache_control`（system blocks、message/content blocks、tool definitions递归均不出现）和`anthropic-beta` cache header；若发送POST则总是携带完整system/messages/tools。provider报告的`cache_read_input_tokens`/`cache_creation_input_tokens`只作为usage evidence。

5. **Credential逐attempt动态解析。** 每个`generate_model_turn` invocation仍在调用adapter前解析`CredentialSource`，不把credential或body memoize/pin到`ModelCallRequest`；既有credential rotation行为保持不变。一个installed source被信任为表示一个稳定nonsecret credential binding（一个account/project/tenant scope）；token内容可以逐attempt轮换，但静默切换binding identity属于配置变更，需要新的installation/definition version/runtime config publication。MiniCore不能从credential bytes推断binding identity，也不为此新增类型或运行时检查（见[Provider Installation](../modules/model-gateway.md#provider-catalog-and-custom-providers)与[ModelProviderDescriptor](../modules/model-gateway.md#modeldefinitionref)版本义务）。

6. **omission不等于否认provider自动缓存。** 上述省略不声称provider不做任何automatic caching或retention；MiniCore只是不请求/不控制这些优化，正确性从不依赖它们。

7. **未来激活门槛。** 显式cache annotation或continuation优化若要激活，必须（a）先有独立的provider-specific evidence与新ADR；（b）稳定credential binding加显式tenant/session privacy scope；（c）canonical full-wire successor proof（新请求必须是旧完整请求+finalized response+exact sanitized live suffix，且证明方式可被审计）；（d）retention/billing policy；（e）与one-POST语义的reconciliation（同一次operation内不得出现第二个POST）。在此之前，这些优化保持intentionally disabled/omitted，不是pending implementation。

8. **usage truth。** `ModelUsage.provider_total_tokens`只保存provider报告值。Anthropic Messages在consumed contract中不报告total，因此Anthropic normalized usage恒为`None`；各provider-reported cumulative component（input/output/thinking/cache read/cache write）原样保留并继续执行monotonic检查。usage finalization是infallible：非常大的individually valid counters即使本地求和会溢出也不得使terminal失败。

## 后果

- 每次`generate_model_turn`的调用与网络行为完全确定：至多一次`ProviderAdapter::execute`，独立地零或一个HTTP POST；若发送POST则携带完整full request，无fallback、无重试、无continuation。loopback contract suite对成功路径的“exactly one POST”断言（与pre-send cancellation/`AuthMissing`/owner validation的零execute/零POST路径，以及adapter编码/build失败的零POST路径）成为one-POST policy的可执行证据。
- 同一`Arc<ModelCallRequest>`被复用（Session logical retry路径）时，每次invocation重新resolve credential并重新编码完整body；body bytes在所有attempt间相同（无incremental变体），auth header随解析到的credential变化。
- Anthropic usage不再合成total，消除了sum-overflow fail-closed路径；`provider_total_tokens`在该协议上恒为`None`。
- 文档中关于connection pool、continuation cache和auth principal的current-implementation描述被删除；这些机制不存在，不预留abstraction。
- ADR 0106、0119、0123、0125、0138、0139中与本政策相关的旧语言以本ADR为准；其single-attempt、Session logical retry、no-local-permit和full-request equivalence原则继续有效。
