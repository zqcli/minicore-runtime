# ADR 0145：真实Provider证据细化Direct Adapter Wire Truth

状态：Accepted
日期：2026-08-12

## 背景

M12通过standalone Rig evidence与离线loopback合同冻结了OpenAI Responses、Anthropic Messages的协议、terminal、metadata、delivery与single-attempt baseline；ADR 0139又把Rig隔离在Rust 1.88 evidence package之外，M14由两个private direct adapters拥有production wire truth。随后host-only installation与两个explicit ignored live smoke harness已经接通完整public Runtime path，但此前只完成默认离线编译，没有真实credential release run。

2026-08-12在显式release环境使用同一个已启用credential与同一个private API model，通过一个HTTPS兼容gateway分别执行OpenAI Responses与Anthropic Messages完整public Runtime smoke。初始请求暴露三个不能由离线fixture推断的production事实：

- UA-less HTTPS请求在gateway/WAF前被HTTP 403规则拒绝；普通非伪装产品User-Agent即可通过，浏览器伪装没有额外价值；
- Anthropic-compatible stream可产生没有signature字段或signature delta的visible `thinking`，其余message/content/terminal truth仍合法；
- Anthropic-compatible `message_start.message`可省略`stop_reason`与`stop_sequence`，而不是显式发送null；真正success terminal仍是后续non-empty `message_delta.delta.stop_reason`。

这些是direct adapter必须truthfully消费的wire事实，但不能成为endpoint/model特殊分支、浏览器伪装、认证fallback、retry或generic compatibility registry。

## 决策

1. **Shared transport显式发送固定产品User-Agent。** `provider_transport::build_client()`在保留`retry::never()`、`redirect::Policy::none()`与`no_proxy()`的同时，为每个direct adapter client安装`concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"))`。当前artifact wire值为`minicore-runtime/0.1.0`。该值是stable、nonsecret、provider-neutral的产品标识，不是浏览器伪装；OpenAI与Anthropic POST必须携带同一个exact值。当前不增加host-configurable UA：未来只有出现真实host consumer与identity/policy需求时才独立设计，不能为猜测提前扩大config或Wire surface。

2. **UA不改变single-attempt/full-request政策。** 固定UA只是shared client metadata；每次`generate_model_turn`仍至多一次`ProviderAdapter::execute`，独立地零或一个POST，若发送则是完整full request。不得因403/401或任何gateway响应切换UA、protocol、auth header、endpoint或发送第二个POST。

3. **Anthropic thinking start的signature是optional string。** `content_block_start.content_block.type == "thinking"`仍要求`thinking`字段存在且为string；`signature`字段可缺失，缺失表示unsigned。若字段存在则必须为string（empty可作为signed stream的start placeholder）；null、number、bool、object或array仍malformed。后续`signature_delta`仍只适用于open thinking block；即使start缺失signature，首个delta也可建立signature，后续delta按序追加，合法late signature不得丢失。

4. **Unsigned thinking被truthfully保留但不伪造成可重放artifact。** thinking block stop时：

   - empty thinking且无/non-empty-filter后的empty signature不产生domain block；
   - visible thinking + absent/empty signature归一化为`ReasoningContent{text=Some, signature=None}`；
   - visible thinking + non-empty signature保持现有exact replayable reasoning；
   - empty thinking + non-empty signature保持现有signature-only reasoning。

   Anthropic request replay只编码exact text+signature pair、signature-only或unambiguously Anthropic redacted artifact；unsigned text-only reasoning自然省略。若同一assistant message仍有representable text/tool blocks，则保留这些blocks；若只剩unsigned reasoning，则整条assistant message按既有规则drop。绝不合成signature、把thinking改写为user-visible text、或把unsigned artifact回传provider。

5. **Anthropic message_start stop fields是absent-or-null。** `message_start.message.stop_reason`与`stop_sequence`分别允许字段缺失或显式null，两者都表示start阶段尚未terminal。任何present non-null值仍是malformed provider response。start永不构成success proof；success仍只允许all blocks closed后出现non-empty `message_delta.delta.stop_reason`，`message_stop`仍never sufficient，early EOF仍truthful failure。

6. **其余Anthropic truth保持严格。** `message.type == message`、`role == assistant`、`message.model` exact匹配pinned private API model、empty start content、valid response id、required numeric start usage、strict provider indexes、block-specific字段、terminal cumulative output usage/monotonicity、metadata allowlist、delivery/cancellation与error classification全部不变。不能把本ADR解释为generic permissive parser。

7. **真实release证据不保存secret或private endpoint。** `tests/m14_live_provider_smoke.rs`继续默认`#[ignore]`、默认门禁只编译不联网。release run通过仓库外0600临时环境文件提供credential，完成后删除并unset；credential不进入命令行、panic、日志、fixture、Git或文档。仓库只记录nonsecret evidence：OpenAI Responses与Anthropic Messages、private API model `deepseek-v4-flash`、Anthropic version `2023-06-01`、日期与pass/fail结果；不记录private endpoint或response content。

## 可执行证据

- `src/model_gateway/provider_transport.rs`：artifact-derived `USER_AGENT`与locked-down client construction；
- `src/model_gateway/openai_responses.rs` / `anthropic_messages.rs`：两个loopback request-shape helpers固定同一exact UA；
- `src/model_gateway/anthropic_messages.rs`：optional signature state、late signature delta、unsigned text-only normalization/replay omission、absent-or-null start stops及严格non-null rejection；
- Anthropic focused suite覆盖unsigned receive/replay、late signature、live-wire 17 thinking deltas/zero signature delta、omitted start stop fields、present malformed values与完整Gateway loopback；
- `tests/m14_live_provider_smoke.rs`：两个public Runtime-path ignored smoke；2026-08-12显式release run中OpenAI Responses与Anthropic Messages均通过exact单测试，model为`deepseek-v4-flash`；
- milestone acceptance：`./scripts/check.sh`通过主crate library `939 passed / 3 ignored`、integration `159 passed / 3 ignored`、standalone provider-gate `25/25`及Clippy/format/docs/Wire/Store fixtures；`./scripts/check-msrv.sh`用真实Rust 1.85通过主crate同样的`939/3 + 159/3`。

## 后果

- 两个direct adapters现在既保留M12离线协议证据，也拥有一次真实HTTPS/public Runtime release run。
- WAF要求通过正常产品身份满足，不采用浏览器伪装或请求重试；未来UA配置不是当前pending实现，除非真实host需求另行冻结。
- Anthropic-compatible unsigned thinking不再使整个valid terminal失败，同时不会污染future provider replay或伪造签名。
- M14 provider real-credential smoke项关闭；M14仍未关闭，因为public Structured activation、write/network/process及其他production Tool/Sandbox adapters、mutation queue/permit、generic ToolService与concrete Prompt/Skill sources仍pending。

## 被否决的方案

- **浏览器User-Agent伪装。** 普通产品UA已解除规则，伪装增加错误身份且无证据价值。
- **403后切换UA或protocol重发。** 违反one-execute/one-POST与delivery truth。
- **为特定endpoint/model加compatibility flag。** wire事实属于协议形状，不应根据名称猜测或创建open-world registry。
- **为unsigned thinking合成空/伪signature。** 会伪造provider artifact并使future replay不truthful。
- **丢弃整个successful response。** domain已有text-only reasoning carrier与安全replay omission，可保留真实输出而不扩大public schema。
- **接受message_start non-null stop fields。** 会把start与terminal truth混淆；只有absent/null表示not stopped。
