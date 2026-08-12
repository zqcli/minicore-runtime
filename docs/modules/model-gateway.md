# ModelGateway架构设计

日期：2026-08-08

状态：当前权威架构（M6.2 scripted foundation、M8.1最小ToolCall、M10 CompactionSummary purpose/budget request validation与ActiveTurnTask orchestration，以及crate-private Structured output foundation已实现；M12 protocol reality gate已关闭并因Rust 1.85拒绝Rig production dependency；M14 `OpenAiResponsesProviderAdapter`与`AnthropicMessagesProviderAdapter`、provider-native Structured mapping、默认离线loopback contract suites、host-only dynamic credential/catalog installation及explicit ignored live smoke harness已实现，stateless full-request wire policy由ADR 0141冻结；production Tool/Sandbox中首个narrow OS-backed `read_file` slice已由ADR 0143实现，实际real-credential smoke run与其余Tool/Sandbox adapters仍待实现）

## 目的

本文定义MiniCore的provider-neutral模型调用模块，回答：

- ActiveTurnTask如何从`AssembledModelContext`发起一次模型调用；
- Model identity、definition revision、capabilities和effective limits如何在Turn内固定；
- System、User、Assistant和Tool role如何映射到不同provider；
- ToolSpec、tool choice、OutputContract、reasoning和attachments如何编码；
- streaming delta、finalized response、usage、finish reason和provider metadata如何规范化；
- 单次provider attempt与Session logical retry如何区分；
- cancellation、authentication、secret redaction、跨Session并发调用和provider rate limit反馈如何治理；
- provider prompt cache、connection reuse和continuation如何保持完整logical input等价性；
- M14 stateless full-request wire policy（[ADR 0141](../adr/0141-provider-calls-are-stateless-full-request.md)）为何冻结为omission：每次`generate_model_turn`至多调用一次`ProviderAdapter::execute`（owner validation/pre-send cancellation/`AuthMissing`在调用adapter前以typed error terminal），独立地发送零或一个HTTP POST——adapter编码/build失败或adapter级pre-send cancellation发生在那一次`execute`内部且不产生POST——若发送POST则携带完整full request，显式cache annotation与continuation保持intentionally disabled/omitted；
- M12 Rig 0.40.0 evidence揭示了哪些provider协议事实与SDK损失点，以及为何production `ProviderAdapter`直接拥有HTTP/SSE mapping。

本文不定义：

- Prompt transcript construction、conversation visibility或canonical live/stored facts → `ModelMessage` conversion；
- ActiveTurnTask logical retry、compaction orchestration或Turn terminal规则；本文只摘要Gateway terminal error与该policy的边界，具体规则以Session Execution和ADR 0119为权威；
- Tool execution、approval或ToolResult持久化；
- Runtime公开model catalog/query/event协议；其safe view以[Runtime Interface](runtime-interface.md)为权威；
- provider-native compaction artifact的持久化格式；
- 完整pricing、billing ledger或成本审计。

当前M6.2 foundation与M8.1最小ToolCall路径已运行：Runtime仍默认拥有empty gateway/catalog root；Prompt proof、retained exact model snapshot、pinned-estimator context-limit preflight、single scripted attempt、progress、terminal response validation、delivery-aware typed error和cancel/terminal线性化可运行。M10新增`CompactionSummary` purpose，并要求assembly budget proof、`NoToolCalls`、empty ToolSpec和explicit max output exact匹配，仍复用同一个single-attempt Gateway。crate-private Structured foundation已实现：`OutputContract::Structured` contract由绑定exact `TurnModelSnapshot`的constructor创建（capability与`max_schema_bytes` cap校验、schema v1 subset验证），optional name按[stable symbolic key](wire-schema.md#stable-symbolic-keys)共同floor限制为1..64 bytes，`ModelCallRequest` constructor按proof复验exact model/OutputContract绑定，Gateway对terminal执行exact JSON object parse与本地schema validation（`Refused` bypass；`UnexpectedToolCall`/`IncompleteResponse`/`InvalidStructuredOutput` precedence），并由ScriptedProviderAdapter端到端conformance覆盖；无JSON repair、type coercion或Markdown code-fence extraction。`RateLimited`与其他允许logical retry的transient reason一样，只有`NotSent | RejectedBeforeExecution`可以保留；unsafe delivery fail closed为`RequestOutcomeUnknown | StreamInterrupted`。M14 production provider/install slices现已实现：exact `reqwest = 0.13.4`关闭default features并只启用`json + rustls + stream`，共享private client construction、bounded body drain与增量SSE framing，client显式关闭retry、redirect和ambient proxy；`OpenAiResponsesProviderAdapter`独立编码Responses instructions/items/Tools/NoToolCalls/Structured/reasoning/service tier，仅以`response.completed + status=completed + response.model exact匹配pinned private API model name`为success terminal；`AnthropicMessagesProviderAdapter`独立编码Messages system/messages/Tools/Structured+adaptive effort/service tier，仅以valid `message_start.message.model` exact匹配加non-empty `message_delta.stop_reason`为success terminal，并保存thinking/signature/redacted artifact、cumulative usage/cache、request ID、typed envelope、delivery与cancellation truth，`x-api-key` HeaderValue显式标记sensitive。35+38个默认离线测试分别通过真实`ModelGateway.generate_model_turn → ProviderAdapter`与`127.0.0.1:0` HTTP路径验证。public host-only `ProviderCredential`/`CredentialSource`、explicit model descriptor、endpoint policy与`ModelProviderConfig`已接入`MiniCoreRuntimeConfig::with_model_provider`：未配置时Runtime保持empty catalog；trusted host显式安装后catalog只保存nonsecret exact definition，credential在每次attempt前动态解析，missing/cancel在adapter前保持`AuthMissing|Cancelled/NotSent`。stable `ModelId`与private API model name完全分离，后者不进入Session/Wire/provenance。installation 11个、credential 3个与Runtime focused tests及真实Rust 1.85 all-target check均通过；两个public Runtime-path live smoke tests已在stable/真实Rust 1.85编译，默认2 ignored且不读env/访问network。仍pending：任何public requester/Wire/SessionDefinition structured字段、Runtime/ActiveTurnTask structured激活（ordinary AgentRun路径仍`output_contract=None`，toolful Turn在普通ToolRound后的第二次Structured call未激活，Compaction仍固定`NoToolCalls`）、完整generic ToolSpec schema、实际real-credential smoke run以及除ADR 0143 narrow `read_file`外的其余production Tool/Sandbox adapters。显式cache annotation、`previous_response_id`/incremental input与continuation不是pending实现：M14 wire policy（[ADR 0141](../adr/0141-provider-calls-are-stateless-full-request.md)）已冻结为有意omission——每次`generate_model_turn`至多调用一次`ProviderAdapter::execute`（owner validation/pre-send cancellation/`AuthMissing`在调用adapter前以typed error terminal，零execute/零POST；adapter编码/build失败或adapter级pre-send cancellation为一次execute/零POST），独立地发送零或一个HTTP POST，若发送POST则携带完整full request，没有optimization-specific fallback POST、重试或continuation state；这些省略不声称provider不做automatic caching/retention，MiniCore只是不请求/不控制，正确性从不依赖它们。

相关权威文档：

- [Prompt子系统架构设计](prompt.md)
- [Turn执行模块与执行上下文架构设计](turn-execution-context.md)
- [Session Execution架构设计](session-execution.md)
- [Conversation Recording与Replay架构设计](conversation-storage.md)
- [Runtime Interface与公开协议架构设计](runtime-interface.md)
- [ADR 0126：Turn执行使用async loop，Session记录采用inline best-effort append](../adr/0126-turn-execution-is-async-and-session-recording-is-best-effort.md)
- [ADR 0106：ModelGateway使用一个深异步调用interface](../adr/0106-model-gateway-is-single-deep-operation.md)
- [ADR 0119：模型调用使用Session逻辑重试](../adr/0119-model-calls-use-session-logical-retries.md)
- [ADR 0120：失败由事实拥有模块分类，恢复由执行拥有者决定](../adr/0120-failures-stay-with-owning-modules.md)
- [ADR 0125：ModelGateway不设置本地模型调用Permit](../adr/0125-model-gateway-has-no-local-call-permits.md)
- [ADR 0138：Production Provider baseline只采用已验证的Rig协议合同](../adr/0138-production-provider-baseline-uses-verified-rig-contracts.md)
- [ADR 0139：Rust 1.85下Rig只作为独立证据harness](../adr/0139-rig-is-evidence-only-under-rust-1-85.md)

## 决策摘要

已经确定：

- `MiniCoreRuntime`拥有一个共享`Arc<ModelGateway>`；
- 共享Gateway支持多个Session直接并发调用，不设置Runtime global、provider route、model或auth-principal调用permit；
- ModelGateway不保存current Session、current Turn或UI selected model；
- `ModelGateway::resolve_for_turn(...)`在Turn capture期间返回`Arc<TurnModelSnapshot>`；
- `ModelGateway::generate_model_turn(...)`是唯一真实模型调用interface；
- `ModelCallRequest.input`只能是PromptSet产生的`AssembledModelContext`；
- ModelGateway只编码完整provider-neutral context，不重新决定conversation visibility；
- `TurnModelSnapshot`固定exact model definition、capabilities、effective limits和generation policy；
- active Turn内不允许静默替换provider/model identity；
- MVP中每次`generate_model_turn`只执行一个provider attempt，adapter与private HTTP client automatic retry固定为0；
- ModelGateway不执行transparent retry、401 refresh-and-resend或WebSocket → HTTP transport fallback；
- ActiveTurnTask只对同一个immutable request执行最多3次AgentRun logical retry；CompactionSummary最多1次；
- cross-model fallback不是ModelGateway transparent behavior；
- streaming progress是process-local observer data，不进入Conversation Storage；
- finalized response或typed error是一次gateway调用唯一terminal result；
- provider-reported usage随成功assistant response进入live state并成为record candidate；recording degraded时可能无法跨restart恢复；失败attempt usage只属于ModelGateway internal telemetry；
- authentication secret、raw headers、raw request/response body和provider SDK类型不越过ModelGateway seam；
- prompt cache、connection reuse、`previous_response_id`和incremental request只是wire optimization，且M14有意不请求/不控制它们（[ADR 0141](../adr/0141-provider-calls-are-stateless-full-request.md)）：每次`generate_model_turn`至多调用一次`ProviderAdapter::execute`，独立地发送零或一个HTTP POST，若发送POST则携带完整full request，没有optimization-specific fallback POST、重试或continuation state；
- 任何未来optimization若要激活，必须退回完整`AssembledModelContext`请求并满足ADR 0141的独立门槛（provider-specific evidence/ADR、稳定credential binding与tenant/session privacy scope、canonical full-wire successor proof、retention/billing policy、one-POST reconciliation）；
- ProviderAdapter是private internal seam，首批实现为`ScriptedProviderAdapter`、`OpenAiResponsesProviderAdapter`与`AnthropicMessagesProviderAdapter`；
- 两个production adapter各自负责其provider request/SSE/response/error映射与单次attempt执行；model resolution、request validation、auth policy与credential resolution、progress lifecycle和terminal result归一化均由ModelGateway拥有；M14不实现cache/continuation policy（ADR 0141：显式cache annotation与continuation为有意omission，credential由request绑定的`CredentialSource`在每次attempt动态解析）；
- 默认Runtime不安装provider；trusted host通过validated `ModelProviderConfig`显式安装route与model descriptors。credential source与catalog availability分离，secret不进入definition/catalog；同一个retained Turn snapshot的每次Gateway invocation都会重新resolve source（一个installed source代表一个稳定nonsecret credential binding，token可逐attempt轮换，静默切换binding identity属于配置变更）；
- 不增加`ModelStep`、`ModelAttempt`领域entity、provider session public object或第二conversation state。

## 同类项目研究

### 研究范围

本轮直接检查：

- pi coding agent及其`pi-ai`、`pi-agent-core`安装包；
- OpenAI Codex Rust实现的`ModelClient`、Responses API和retry代码；
- Grok Build可取得的sampler/config/package metadata和已研究的Session调用流程；
- Rig 0.40.0的completion、streaming、provider和sans-I/O AgentRun源码。

Claude Code和Cursor只使用可观察行为作为背景，不推断其未公开provider实现。

### pi

关键实现：

```text
pi AgentSession
→ pi-agent-core AgentLoop
→ pi-ai streamSimple(model, context, options)
→ API-specific provider stream
```

pi的主要特点：

- `Model`同时描述provider、API protocol、base URL、context window、max tokens、reasoning、cost和compat flags；
- provider stream使用typed `start → delta* → done/error`事件；
- text、thinking和tool call都有start/delta/end；
- `AbortSignal`贯穿AgentSession、AgentLoop和provider request；
- ModelRegistry组合builtin model、custom model和provider override；
- AuthStorage支持API key、OAuth refresh、environment和runtime override；
- context overflow和transient provider error分开分类；
- automatic retry和compaction由AgentSession编排，不完全属于provider module；
- provider差异通过OpenAI/Anthropic compat flags处理。

可借鉴：

- typed stream事件；
- reasoning/tool-call block保持原始顺序；
- model-level capability和thinking映射；
- auth refresh与provider catalog分离；
- context overflow与transient retry分离。

不采用：

- Model descriptor直接携带base URL、headers等执行细节并跨调用层传播；
- caller可传任意provider compat JSON；
- provider error靠上层字符串模式判断；
- AgentSession同时拥有retry、compaction、message state和provider orchestration。

### OpenAI Codex

关键实现：

```text
Session / TurnContext
→ ModelClient
→ ModelClientSession
→ Responses WebSocket或HTTP SSE
→ ResponseStream
→ Turn event loop
```

Codex的主要特点：

- `ModelClient`保存thread级provider/auth/telemetry和connection policy；
- `ModelClientSession`保存turn级WebSocket和sticky turn state；
- Responses WebSocket优先，失败后切换HTTP；
- connection retry使用exponential backoff和jitter；
- 401可以触发一次auth refresh；
- prompt cache key和session identity关联；
- incremental request只有在previous request/response和current full request满足严格prefix关系时使用；
- cancellation通过stream drop和CancellationToken传播；
- usage在terminal response event规范化；
- provider request ID、response ID和timing只进入受控telemetry/metadata。

可借鉴：

- transport fallback与model fallback分离；
- continuation必须证明full-request prefix equivalence；
- connection/session state留在model client内部；
- retry delay带jitter并尊重provider requested delay；
- cancellation-aware stream mapping；
- auth refresh只执行有界次数。

不采用：

- 把turn-scoped provider session暴露给SessionExecutor；
- 让provider sticky state成为Turn恢复来源；
- 以OpenAI Responses协议形状定义MiniCore-owned interface；
- 默认假设所有provider都支持`previous_response_id`。

### Grok Build

当前可取得的sampler源码不完整，因此这里只采用可验证的crate和产品结构：

- `xai-grok-sampler`是独立sampling/inference crate，声明HTTP streaming和retry，不依赖shell；
- `xai-grok-sampling-types`保存纯sampling数据类型；
- model config显式声明`chat_completions | responses | messages` backend；
- model registry记录context window、temperature、top-p和API backend；
- authentication支持API key、OAuth/OIDC、session token和external provider；
- integration测试依赖真实mock HTTP server而不是只mock函数；
- streaming update与chat/session state分层。

可借鉴：

- sampling module与Session/shell解耦；
- provider protocol必须显式配置，不能根据base URL猜测；
- 使用真实mock HTTP server验证SSE、retry、cancel和redaction；
- telemetry使用closed typed schema并在emit时redact。

不推断：

- 未取得源码中的exact retry state machine；
- 未公开的provider fallback和continuation内部规则；
- Grok产品内部credential storage实现。

### Rig 0.40.0

Rig提供：

- `CompletionModel::completion/stream`；
- provider-neutral `CompletionRequest`；
- System/User/Assistant、Reasoning、ToolCall和ToolResult content；
- `ToolChoice::Auto | None | Required | Specific`；
- structured output schema；
- streaming `cancel/pause/resume`；
- input/output/cache-write/cache-read/reasoning/tool-use usage；
- OpenAI Responses reasoning、prompt cache、previous response和service tier参数；
- Anthropic thinking/signature和cache control；
- generic client `base_url` override；
- sans-I/O `AgentRunStep::CallModel | CallTools | Done`。

Rig 0.40.0的integration gaps：

- generic `CompletionResponse`没有统一finish reason；
- provider-specific raw response类型不同；
- `CompletionRequest.additional_params`是raw JSON escape hatch；
- provider error taxonomy不等于MiniCore recovery taxonomy；
- stream cancellation使用Rig `AbortHandle`，需要桥接MiniCore CancellationToken；
- prompt cache、reasoning和structured output仍需provider-specific typed mapping；
- generic interface不能完整表达所有provider response metadata。
- crate source使用Rust 1.85不支持的let-chain与trait-upcasting语法；0.36.0–0.40.0均不能作为直接MSRV降级。

结论：Rig evidence适合验证provider合同和SDK损失点，不适合作为MiniCore Rust 1.85 production dependency，也不适合作为ModelGateway interface或domain type来源。M14由两个private direct adapters拥有协议wire mapping。

## 对比结论

| 维度 | pi | Codex | Grok Build | Rig 0.40.0 | MiniCore决策 |
| --- | --- | --- | --- | --- | --- |
| model invocation seam | AgentLoop调用pi-ai stream | ModelClientSession stream | 独立sampler crate | CompletionModel stream | 一个`generate_model_turn` |
| provider state | Model/registry + provider module | thread/turn client state | sampler内部 | provider client/model | 每个installation一个adapter-owned无状态reqwest client（reload复用，普通transport pooling eligibility） |
| Prompt assembly | AgentSession/AgentLoop context | Turn构造Responses input | chat state/sampling types | CompletionRequest | 只接受PromptSet输出 |
| streaming | typed content events | typed Responses events | ACP/update stream | typed assistant content stream | droppable progress + one terminal result |
| retry owner | AgentSession为主 | client/turn loop协作 | sampler retry | caller决定 | Gateway single attempt，ActiveTurnTask logical retry |
| transport fallback | provider-specific | WebSocket → HTTP | backend显式配置 | adapter-specific | MVP不启用 |
| cross-model fallback | model selection层 | 不作为transparent stream fallback | 未确认 | 无统一语义 | active Turn内禁止 |
| continuation/cache | provider compat/cache flags | strict prefix + previous response | 未确认 | provider additional params | 有意omission（ADR 0141）：始终full request，无continuation/显式cache请求 |
| usage | assistant response usage | terminal response usage | telemetry/model usage | generic Usage | 成功response usage durable |
| error taxonomy | helper + string patterns | CodexErr mapping | typed sampler errors | CompletionError | MiniCore typed recovery classes |
| auth | registry + AuthStorage | AuthManager + refresh | API key/OAuth/OIDC | provider client setup | host-injected `CredentialSource`，per-attempt resolve |

## Interface方案比较

### 方案A：一个深异步operation

```rust
ModelGateway::generate_model_turn(request, progress, cancel)
    -> Result<ModelCallResult, ModelCallError>
```

优点：

- caller只理解完整request、progress和terminal result；
- provider attempt、connection、auth、delivery-state mapping、cache和continuation全部具有locality；
- 与ActiveTurnTask中的一次普通async await自然对齐；
- fake adapter可以通过同一interface测试；
- 删除Gateway会把大量provider复杂性重新散落到caller，满足deep module deletion test。

代价：

- implementation内部较深；
- 需要private planner、adapter和connection seams辅助测试；
- progress publisher必须明确non-authoritative语义。

### 方案B：公开prepare/execute两阶段

```rust
ModelGateway::prepare_model_turn(request) -> PreparedModelTurn
PreparedModelTurn::execute(progress, cancel) -> Result<...>
```

问题：

- caller必须理解prepared value何时过期；
- auth、provider config和capability在prepare还是execute时解析会产生双重时序；
- dropped prepared value引入无业务价值的lifecycle；
- credential、connection和continuation runtime state不能安全地在prepare阶段预留；
- 删除PreparedModelTurn并把private plan放回generate内部不会损失能力。

结论：不作为外部interface。可以作为ModelGateway private implementation structure。

### 方案C：公开ModelTurnSession和async stream

```rust
ModelGateway::open_turn(...) -> ModelTurnSession
ModelTurnSession::generate(...) -> ModelCallStream
```

问题：

- ActiveTurnTask需要管理第二个turn-scoped provider lifecycle；
- caller必须理解stream terminal frame、drop、poisoned continuation和session reset；
- provider session容易被误认为Turn truth或recovery source；
- 若把provider session暴露给caller，logical retry和Steer后的新request会迫使ActiveTurnTask管理第二套provider session lifecycle；
- connection/continuation完全可以留在Gateway内部并用full request重新证明兼容性。

结论：不作为外部interface。Codex式WebSocket session可以作为private adapter优化。

### 选择

采用方案A。

ModelGateway是一个深模块：对外只有Turn model resolution和一次完整模型调用两个操作；provider variation通过private adapters处理。

## Ownership

```text
MiniCoreRuntime
└─ Arc<ModelGateway>
   ├─ ProviderCatalog（ModelCatalogView）
   └─ private ProviderAdapter（每installation一个，持有其reqwest client）

ActiveTurnTask
├─ await ModelGateway::generate_model_turn
│  ├─ immutable ModelCallRequest + logical_retry_count
│  └─ scoped ProgressEventPublisher<ModelProgressEvent>
├─ cancellation-aware retry sleep
│  └─ same ModelCallRequest + ConversationRevision/control_generation
└─ async CompactionSummary call
   └─ immutable CompactionPlan + ModelCallRequest
```

规则：

- ProviderCatalog和ProviderAdapter是ModelGateway implementation details；AuthStore/ProviderConnectionPool/ContinuationCache/auth-principal类型不存在——credential由request绑定的host-owned `CredentialSource`在每次attempt解析，连接政策就是adapter-owned的无状态reqwest client；
- Runtime公开model query以后通过MiniCoreRuntime facade取得safe catalog view；
- ActiveTurnTask不直接resolve auth、base URL、API model name或provider protocol；
- ToolService、PromptService和SkillService不调用ModelGateway；
- ModelGateway不读Conversation Storage，不append entry，不发布Runtime durable event；
- ModelGateway没有current Session、current Turn、current cwd或current model字段。

## External Interface

```rust
pub struct ModelGateway {
    // private
}

pub(crate) struct ModelCatalogView {
    // private validated retained definitions and selection index
}

impl ModelGateway {
    pub(crate) async fn initialize(
        &self,
    ) -> Result<Arc<ModelCatalogView>, ModelResolutionError>;

    pub(crate) async fn build_reload_candidate(
        &self,
    ) -> Result<Arc<ModelCatalogView>, ModelResolutionError>;

    pub(crate) fn resolve_for_turn(
        &self,
        catalog: Arc<ModelCatalogView>,
        request: ResolveTurnModelRequest,
    ) -> Result<Arc<TurnModelSnapshot>, ModelResolutionError>;

    pub(crate) async fn generate_model_turn(
        &self,
        request: Arc<ModelCallRequest>,
        progress: ProgressEventPublisher<ModelProgressEvent>,
        cancel: CancellationToken,
    ) -> Result<ModelCallResult, ModelCallError>;
}
```

`initialize()`构建并返回第一个immutable catalog root。`build_reload_candidate()`准备并validate新catalog但不发布。ModelGateway不保存current catalog pointer，也没有publish方法；Runtime把candidate放入完整`SharedResourceRoots`后一次publication。`resolve_for_turn`只访问Turn admission已捕获的catalog Arc，不读取credential，也不在capture期间隐式执行remote I/O。

`generate_model_turn`最多执行一个完整provider attempt。调用期间可以等待request前credential resolve/refresh和stream，但不经过Gateway本地模型调用permit；它由ActiveTurnTask直接await，SessionExecutor control actor继续处理ingress。provider terminal error返回ActiveTurnTask后，本次Gateway operation结束；Gateway不内联retry timer。

不提供：

```text
open_provider_session
prepare_model_turn
begin_model_call
poll_model_call
finish_model_call
raw_provider_client
provider_stream
```

## Model Identity

### ModelSelection

```rust
pub struct ModelSelection {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
}
```

`ModelSelection`是SessionDefinition保存的稳定用户选择，不包含：

- API model name；
- base URL；
- provider protocol；
- auth reference或secret；
- model capability；
- current catalog publication state。

### ModelDefinitionRef

```rust
pub struct ModelDefinitionRef {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    pub definition_version: ModelDefinitionVersion,
}
```

`definition_version`是validated provider/model definition的content identity。它覆盖影响调用语义的非secret配置：

```text
provider protocol
endpoint identity
nonsecret auth binding reference identity
API model name
capabilities
advertised limits
effective token estimate rate and algorithm version
generation defaults
cache/role/tool/output mapping policy
adapter encoding version
```

它不覆盖credential内容、OAuth access token或resolved auth principal（MiniCore没有auth-principal类型）。当前host installation API显式接收non-zero definition version；host必须在protocol、endpoint、private API model name、capabilities、limits、estimate rate、generation defaults或credential binding identity等nonsecret调用语义变化时递增/替换version，单纯轮换同一binding的token不改变version。一个installed `CredentialSource`被信任为表示一个稳定nonsecret credential binding（一个account/project/tenant scope）；静默切换binding identity属于配置变更，需要新的installation/definition version/runtime config publication，MiniCore不能从credential bytes推断且没有运行时检查。MiniCore不为此计算hash/fingerprint。

### TurnModelSnapshot

```rust
pub struct TurnModelSnapshot {
    selection: ModelSelection,
    definition: ModelDefinitionRef,
    capabilities: ModelCapabilities,
    limits: EffectiveModelLimits,
    token_estimate_rate: TokenEstimateRate,
    generation: EffectiveGenerationPolicy,
    execution_ref: ModelExecutionRef,
}
```

`ModelExecutionRef`是crate-private opaque reference。它允许ModelGateway找到exact retained provider/model definition，但不暴露base URL、auth reference、headers或provider adapter类型。

TurnModelSnapshot语义：

- 在TurnExecutionContext capture期间创建；
- active Turn的每次AgentRun模型调用复用同一Snapshot；
- Steer不重新resolve Model；
- provider catalog reload只影响future Turn；
- definition已从catalog current head移除时，Gateway仍可使用Snapshot持有的retained exact definition；
- process restart不恢复active Turn，因此不承诺跨进程继续旧Snapshot；
- Input UserMessage不保存Turn-start model snapshot；实际响应使用的safe provider/model descriptor和generation metadata只随`StoredAssistantMessage`记录，不保存`ModelDefinitionVersion`或execution_ref。

### ReasoningPreference

```rust
pub enum ReasoningPreference {
    Auto,
    Disabled,
    Low,
    Medium,
    High,
}

pub struct ModelResponseSummary {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    pub reasoning: ModelReasoningSummary,
    pub service_class: ModelServiceClass,
}

pub enum ModelReasoningSummary {
    ProviderDefault,
    Disabled,
    Low,
    Medium,
    High,
}
```

`Auto`使用model definition validated default；其他variant是provider-neutral explicit preference。unsupported Disabled/effort在Turn model resolution时返回typed capability error，不由adapter silent downgrade；provider-specific effort name或extra level不能进入SessionDefinition。

`ModelResponseSummary`是普通Assistant与Compaction provenance共用的唯一portable actual-model descriptor。它保存实际ProviderId/ModelId、effective reasoning和service class，不保存ModelDefinitionVersion、endpoint、auth、client、catalog generation或private execution ref。provider-specific extra effort无法映射closed summary时model definition不可用于portable recording，不能塞进metadata raw map。P1-3负责把这些closed values映射到首版provider protocol。

### ResolveTurnModelRequest

```rust
pub struct ResolveTurnModelRequest {
    pub selection: ModelSelection,
    pub requested_reasoning: ReasoningPreference,
    pub requested_max_output_tokens: Option<NonZeroU32>,
}
```

resolution负责：

- exact selection lookup；
- capability validation；
- reasoning preference映射；
- effective context/output limits计算；
- generation defaults固定；
- safe diagnostics生成。

resolution不负责：

- auth resolution；
- provider connection；
- prompt assembly；
- model availability network probe；
- cross-model fallback。

selection不存在、definition invalid或capability不满足时，Turn admission失败。不能在active Turn开始后静默换模型。

### EffectiveGenerationPolicy

```rust
pub struct EffectiveGenerationPolicy {
    pub max_output_tokens: NonZeroU32,
    pub reasoning: EffectiveReasoningPolicy,
    pub sampling: SamplingPolicy,
    pub service_class: ModelServiceClass,
}

pub enum ModelServiceClass {
    Standard,
    Priority,
}
```

`max_output_tokens`是在model resolution时由explicit request、validated model generation default和known effective output limit闭合的ordinary AgentRun reservation。`ModelCallRequest.max_output_tokens = None`使用该值；Compaction pressure把它与Runtime `pressure_reserve_tokens`取较大值，不根据model name或provider隐式default猜测。无法形成non-zero、limit-valid effective default时Turn model resolution失败。

`EffectiveReasoningPolicy`是requested preference经过model capability映射后的provider-neutral结果；unsupported preference在resolve_for_turn时失败或按explicit Session policy降级，不能在provider adapter中临时猜测。`SamplingPolicy`只保存validated temperature/top-p等stable values；NaN、Infinity或provider不支持的组合在Turn capture时拒绝。

cache和service class属于Turn-pinned execution policy。它们不改变conversation visibility，但会影响provider request和cost。

当前M14政策（[ADR 0141](../adr/0141-provider-calls-are-stateless-full-request.md)）：显式prompt cache annotation与continuation是intentionally disabled/omitted——MiniCore不请求/不控制cache，每次invocation至多调用一次`ProviderAdapter::execute`、独立地发送零或一个POST（owner validation/pre-send cancellation/`AuthMissing`→零execute/零POST；adapter编码/build失败或adapter级pre-send cancellation→一次execute/零POST），若发送POST则携带完整full request，正确性从不依赖cache。不存在`PromptCachePolicy`实现；未来激活必须满足ADR 0141门槛（provider-specific evidence/ADR、稳定credential binding与tenant/session privacy scope、canonical full-wire successor proof、retention/billing policy、one-POST reconciliation），且不能改变conversation visibility。

## Model Capabilities

```rust
pub struct ModelCapabilities {
    pub input_modalities: InputModalities,
    pub tool_calling: ToolCallingCapabilities,
    pub structured_output: StructuredOutputCapabilities,
    pub reasoning: ReasoningCapabilities,
    pub prompt_cache: PromptCacheCapabilities,
    pub supports_streaming: bool,
    pub supports_parallel_tool_calls: bool,
    pub advertised_limits: AdvertisedModelLimits,
}
```

### InputModalities

```rust
pub struct InputModalities {
    pub text: bool,
    pub image: bool,
    pub audio: bool,
    pub document: bool,
}
```

unsupported content在provider request开始前返回`UnsupportedCapability`。MiniCore只产生System sections和conversation messages；provider无法表达System instructions时，该model definition不可用，不做role降级。

### ToolCallingCapabilities

至少描述：

```text
native tools是否支持
ToolChoice::None/Auto/Required/Specific支持情况
tool result name是否required
parallel tool calls支持情况
tool call arguments streaming支持情况
provider tool name限制
```

### StructuredOutputCapabilities

至少描述：

```text
JSON object
JSON schema
strict schema
schema限制和sanitization version
与native tools组合是否支持
```

### ReasoningCapabilities

至少描述：

```text
provider reasoning effort levels
summary支持
replayable text/summary/encrypted/signature artifact支持
reasoning token usage是否报告
```

### EffectiveModelLimits

```rust
pub struct EffectiveModelLimits {
    pub context_window_tokens: Option<NonZeroU32>,
    pub max_output_tokens: Option<NonZeroU32>,
    pub max_tool_count: Option<NonZeroU32>,
    pub max_schema_bytes: Option<NonZeroU32>,
}
```

unknown必须保持`None`。不能根据model name猜测limit，也不能把本地estimate写成provider-advertised fact。

### Token Estimate Rate

本地token估算率归validated model definition所有，并随`ModelDefinitionVersion`变化：

```rust
pub struct TokenEstimateRate {
    pub bytes_per_token: NonZeroU32,
    pub algorithm_version: u16,
}

pub struct TokenEstimator {
    rate: TokenEstimateRate,
}

impl TurnModelSnapshot {
    pub(crate) fn token_estimator(&self) -> TokenEstimator;
}
```

首版算法对canonical UTF-8/JSON bytes使用保守启发式：

```text
tokens = ceil(byte_count / bytes_per_token)
```

model definition未声明rate时，validated definition使用Runtime保守默认`bytes_per_token = 3`并产生safe diagnostic；这个effective default及algorithm version同样进入definition content identity。非文本binary content或无法canonicalize的provider extension返回typed unknown，不为soft trigger伪造数字。

纪律：

- PromptSet final validation、Compaction pressure/unit/source estimate和`CompactionSummaryAssemblyBasis.fixed_prompt_tokens`只能使用同一Turn-pinned estimator；
- 各调用点不得按model name猜测或实现自己的bytes/token常量；
- catalog reload或rate调整只影响future Turn，active Turn继续使用旧Snapshot；
- estimator是确定性纯值，不读取provider、filesystem或mutable catalog；
- estimate属于ContextUsage/planning，不进入`ModelUsage`、StoredAssistantMessage或provider-reported usage。

## AssembledModelContext Contract

PromptSet是唯一producer。assembled shape显式分开有序System sections与conversation messages：

```rust
pub(crate) struct AssembledModelContext {
    system: Arc<[PromptSection]>,
    messages: Arc<[ModelMessage]>,
    tools: Arc<[ToolSpec]>,
    output_contract: Option<OutputContract>,
    diagnostics: Arc<[PromptDiagnostic]>,
    assembly_proof: PromptAssemblyProof,
}
```

`AssembledModelContext` fields are private; this module consumes only the crate-private narrow getters frozen in Prompt. PromptSet System sections、前置User context和ToolSpec在active Turn内天然稳定；这些canonical section/message/tool boundaries只为未来provider-specific cache evidence提供可审计输入，M14不会选择cache breakpoint或发出cache annotation（ADR 0141）。ProviderAdapter是Prompt授权read-ref consumers之一；for every transcript message it must use Prompt's crate-private `ModelMessage::as_ref()` / `ModelAssistantContent::as_ref()` rather than destructuring a storage kind or private transcript kind. `AssembledModelContext`没有flat `contribution_stamps`：stamp只保存在各个User `ModelMessage`内部，任何read ref都不提供它。stamp不是provider payload、cache-control input、source locator或authorization；adapter不得据此重新读取Skill/Workspace正文或影响future cache choice。

`PromptAssemblyProof`是PromptSet生成的crate-private consistency proof，绑定ModelCallPurpose、exact TurnModelRef、OutputContract结构值和optional CompactionSummaryBudget proof。它不提供第二个caller-controlled purpose；ModelCallRequest constructor必须校验proof与request一致。

首版provider-neutral output contract至少包含：

```rust
pub enum OutputContract {
    NoToolCalls,
    Structured(StructuredOutputContract),
}

pub struct StructuredOutputContract {
    name: Option<String>,
    schema: BoundedJsonSchema,
}

impl StructuredOutputContract {
    pub fn name(&self) -> Option<&str>;
    pub fn schema(&self) -> &BoundedJsonSchema;
}
```

contract与enum都是crate-private，不是public requester/Wire/SessionDefinition类型。已实现的`StructuredOutputContract`由绑定exact `TurnModelSnapshot`的constructor创建：它保存process-local exact `TurnModelRef`、校验model capability与`max_schema_bytes` cap，并把schema v1 subset在内存中编译为schema value；compiled schema细节保持private，Debug只暴露has_name/schema_bytes。同一contract只能在绑定model上使用，`ModelCallRequest`构造时按proof复验exact model/OutputContract绑定，不能跨model复用。

`NoToolCalls`不是普通prompt text。请求中的`tools`必须为空；provider支持显式tool-choice/allowed-tools禁用时adapter同时设置该字段。若provider允许在未声明Tool时仍返回可执行ToolCall，Gateway必须返回`UnexpectedToolCall`，不能把它交给ActiveTurnTask执行。

`StructuredOutputContract` constructor要求optional name满足[stable symbolic key](wire-schema.md#stable-symbolic-keys)共同floor并进一步限制为1..64 bytes；schema必须通过BoundedJsonSchema、ModelGateway supported-keyword validation，并与exact TurnModelSnapshot `max_schema_bytes`取更小cap。schema root必须是object schema；remote ref、network lookup、unbounded regex与provider-only raw option不能进入contract。已实现constructor还会在contract构造和request构造时fail-closed复验capability与schema byte cap。

`Structured`在MVP中同样要求`tools`为空。provider-native strict schema只是第一层约束；Gateway仍必须对terminal text执行exact JSON parse和本地schema validation。MVP不做JSON repair、type coercion或Markdown code-fence extraction。需要Tool的AgentRun先完成普通ToolRound，之后再使用新的Structured `ModelCallRequest`取得terminal structured response。

当前实现的schema v1 subset精确为：root必须`type: object`；允许keyword只有`$schema`（仅root且exact Draft 2020-12 URI `https://json-schema.org/draft/2020-12/schema`）、`type`（object/array/string/number/integer/boolean/null）、`description`（string）、`properties`（递归object）、`required`（string数组、无重复）、boolean `additionalProperties`、`items`（递归）、`enum`（非空且无重复）、`const`。任何其他keyword（包括`$ref`、`$defs`、`pattern`、`anyOf`/`allOf`/`oneOf`/`not`等）fail closed为`UnsupportedSchema`，不做sanitize、改写或忽略。terminal validation对`BoundedJsonObject`执行exact JSON parse并与compiled schema本地校验：`Refused`且非空refusal text作为successful response直接bypass schema校验；`UnexpectedToolCall`（contract下出现ToolCall）优先于`IncompleteResponse`/`InvalidStructuredOutput`，后两者按Response Validation顺序形成non-retryable error。

该foundation目前仍未被Runtime调用路径激活：当前Runtime/ActiveTurnTask不构造`Structured` contract（ordinary AgentRun保持`output_contract=None`，Compaction固定`NoToolCalls`），也没有public requester/Wire/SessionDefinition structured字段；OpenAI/Anthropic provider-native schema mapping与direct production adapters已实现并由local contract suites闭合，但尚无catalog-installed requester触发这些路径。

ModelGateway可以：

- 把System sections映射到provider原生system/instructions字段；
- 把ModelMessage编码成provider message/item；
- 把ToolSpec编码成provider tool schema；
- 把OutputContract编码成tool choice、response format或JSON schema；
- 对provider schema限制执行不改变业务含义的canonical sanitization。

ModelGateway当前不能：在canonical section边界添加cache-control metadata——M14 wire policy（ADR 0141）冻结为omission，未来激活必须先满足独立门槛，且不能改变conversation visibility。

ModelGateway不能：

- 删除、增加或重新排序model-visible content；
- 从Conversation Storage重新加载messages；
- 根据provider限制自行摘要或截断conversation；
- 把diagnostic变成prompt text；
- 把ToolSpec描述与ToolSet executor重新join；
- 把OutputContract伪装为system text；
- 把尚未进入sanitized LiveConversation的draft加入payload。

无法无损映射时返回`UnsupportedCapability`或`InvalidRequest`，不能best-effort改变语义。

## ModelCallRequest

ModelGateway是`ModelCallRequest`的唯一canonical owner。Turn Execution Context、Session Execution、Prompt和Compaction只引用该类型及constructor contract，不复制字段或定义第二个request DTO。

```rust
pub struct ModelCallRequest {
    model: Arc<TurnModelSnapshot>,
    purpose: ModelCallPurpose,
    input: Arc<AssembledModelContext>,
    source_revision: ConversationRevision,
    max_output_tokens: Option<NonZeroU32>,
}

impl ModelCallRequest {
    pub(crate) fn new(
        model: Arc<TurnModelSnapshot>,
        purpose: ModelCallPurpose,
        input: Arc<AssembledModelContext>,
        source_revision: ConversationRevision,
        max_output_tokens: Option<NonZeroU32>,
    ) -> Result<Self, ModelRequestValidationError>;
}
```

请求保持最小：

- model generation defaults和effective reasoning已在TurnModelSnapshot固定；
- tool choice和structured output只存在于`input.output_contract`；
- system/messages/tools只存在于`input`；
- streaming是Gateway implementation strategy，不是model-visible caller option；
- cache policy默认由exact model definition和Runtime policy确定；
- SessionId、TurnId和control_generation由ActiveTurnTask调用scope携带；source ConversationRevision随request固定；
- request不含ModelCallId、RunId、ModelStepId或ModelAttemptId；
- request不含credential、auth reference、base URL、header或raw provider params。

constructor验证：

- assembly_proof.purpose等于request purpose；
- `assembly_proof.turn_model`等于TurnModelSnapshot的exact `TurnModelRef`；
- assembly_proof.output_contract等于input.output_contract结构值；
- assembly_proof.source_revision等于request source_revision；
- `NoToolCalls`或`Structured`时input tools为空；
- `CompactionSummary`的assembly budget proof必须存在，且proof max output等于request `max_output_tokens`；`AgentRun`不得携带该proof；
- max_output_tokens满足Snapshot effective limits。

### ModelCallPurpose

```rust
pub enum ModelCallPurpose {
    AgentRun,
    CompactionSummary,
}
```

purpose：

- 选择Prompt assembly closed variant；
- 原样传播到usage和diagnostics；
- 不是retry classification；
- 不是foreground/background状态；
- 不因request前credential refresh或Session logical retry改变。

`CompactionSummary`额外要求：

- 使用active Turn exact model identity；
- input由PromptSet的CompactionSummary variant产生；
- `OutputContract::NoToolCalls`且ToolSpec为空；
- 提供由Compaction planning基于pinned `EffectiveModelLimits`和summary call context feasibility派生的明确max_output_tokens；
- 不进入AgentRun conversation path，不把summary result解释成普通assistant response；
- 不调用provider-native compact endpoint；首版仍是普通portable model generation。

未来只有真实业务任务出现时才增加variant，例如SessionTitle；不能增加`Retry`或`Background`。

### Output Limit

`max_output_tokens`是本次purpose允许的上限：

- `None`使用TurnModelSnapshot中的effective default；
- 非空值必须大于0；
- 超过effective max时在provider调用前返回`InvalidRequest`；
- Gateway不静默clamp，因为clamp会改变调用方已经固定的policy；
- CompactionSummary应提供明确上限，并保证该值已写入同一个immutable `Arc<CompactionPlan>`和assembly proof；
- known model limit小于全局summary配置时，由Compaction plan确定性取较小值，而不是让Gateway修正；
- known context/output limit无法留下最低摘要预算时，Compaction返回`NoFeasibleSummaryBudget`，不构造ModelCallRequest；
- 若CompactionSummary仍以越界值到达constructor，`InvalidRequest`表示caller实现或plan/request contract错误，不能分类成provider故障。

## Private Provider Adapter

```rust
#[async_trait]
pub(crate) trait ProviderAdapter: Send + Sync {
    async fn execute(
        &self,
        request: ProviderAttemptRequest,
        content: ProviderContentPublisher,
        cancel: CancellationToken,
    ) -> Result<ProviderAttemptResult, ProviderAttemptError>;
}
```

该trait是ModelGateway private internal seam，不进入MiniCoreRuntime interface。一次`generate_model_turn`最多调用一次`execute`，一次`execute`只执行一个由ModelGateway规划好的provider attempt。`ProviderContentPublisher`只能发布该attempt内的content delta；adapter和底层SDK都不能自行重试。

ProviderAdapter可以：

- 把private `ProviderAttemptRequest`编码成具体provider wire request；
- 使用已经解析的model、endpoint和credential执行一次provider request；
- 桥接provider stream与attempt cancellation；
- 把provider content、finish reason、usage、metadata和typed protocol/transport error映射为`ProviderAttemptResult`或`ProviderAttemptError`。

ProviderAdapter不能：

- 选择或替换provider/model，解析catalog current value或改变Turn-pinned model identity；
- 重新组装Prompt、判断conversation visibility或执行Tool；
- 决定logical retry、执行transport fallback或terminal Turn结果（M14不存在cache/continuation policy可决定：ADR 0141冻结为stateless full-request omission）；
- 发布ModelGateway attempt lifecycle、构造最终`ModelCallResult`或把provider/transport raw type泄漏给caller。

首个production实现：

```text
OpenAiResponsesProviderAdapter
AnthropicMessagesProviderAdapter
```

首个测试实现使用：

```text
ScriptedProviderAdapter
```

它按脚本返回stream delta、terminal response或typed error，并记录收到的private request，供ActiveTurnTask、ModelGateway和Compaction共享vertical-slice tests使用。测试仍必须经过`ModelCallRequest::new → ModelGateway.generate_model_turn → ProviderAdapter`，不能让ActiveTurnTask或Compaction直接调用fake model interface。只有出现需要纯函数映射/property test的真实场景时再增加`DeterministicProviderAdapter`。

因此ProviderAdapter是一个真实seam，而不是为单一implementation增加的假抽象。

ProviderAttemptRequest可以包含：

- exact provider protocol；
- resolved API model name；
- resolved auth；
- provider-specific typed options；
- canonical encoded payload；
- single-attempt request correlation。

这些类型都必须private、non-serializable并使用redacted Debug。M14的`ProviderAttemptRequest`只携带effective max output、same request Arc与resolved credential；不携带connection/continuation candidate（ADR 0141：无此类state）。

只有private adapter可以使用provider wire DTO、HTTP/SSE transport types和typed provider options。调用方不能提交arbitrary JSON。

## Direct Provider Adapter Mapping

```text
ModelCallRequest
→ ModelGateway validate / resolve / plan attempt
→ private ProviderAttemptRequest
→ protocol-specific adapter encode并执行一个provider attempt
→ ProviderAttemptResult或ProviderAttemptError
→ ModelGateway terminal归一化
→ FinalizedAssistantResponse或ModelCallError
```

`OpenAiResponsesProviderAdapter`与`AnthropicMessagesProviderAdapter`直接拥有各自request body、SSE event、typed error envelope和terminal parser，避免generic SDK先擦除finish、delivery或metadata事实后再用旁路重建。两个adapter仍不能越过private seam，也不是ModelGateway implementation的替代品；它们不拥有provider选择、logical retry或最终错误分类，也不实现cache/continuation判定（M14没有这种判定：ADR 0141冻结为stateless full-request omission）。private HTTP client的内建retry必须显式为0。

映射要求：

- ordered System sections映射到provider-specific system/instructions字段；
- messages保持source order；
- reasoning/text/tool call保持final content order；
- ToolSpec映射到provider tool schema；
- OutputContract映射到provider tool choice/output schema typed options；
- provider-specific optional fields只由adapter内部typed builder生成；
- provider usage规范化到MiniCore ModelUsage；
- transport abort桥接MiniCore CancellationToken；
- finish reason从protocol terminal与provider-specific response字段提取；仍不可得则使用`Unknown`；optional request ID/metadata不可得时保持None；
- provider error envelope与transport error必须在adapter内转换成ProviderAttemptError。

两个adapter只共享private connection client construction、bounded body drain、numeric Retry-After、event-stream content-type check与bounded SSE framing；不能共享会抹平OpenAI/Anthropic request、terminal、typed envelope、metadata或delivery差异的generic response model。M14已把shared transport固定为exact `reqwest = 0.13.4`的`json + rustls + stream`最小features，并由真实Rust 1.85冷编译、OpenAI 35个与Anthropic 38个local loopback/focused tests验证；两个protocol parser仍完全独立。

M12 standalone Rig evidence已经验证：

- OpenAI Responses instructions与Anthropic Messages system、ordered messages、Tool schema及ToolCall identity/order；
- OpenAI reasoning summary/encrypted artifact与Anthropic thinking/signature round-trip；
- Anthropic typed cache-control及OpenAI structured output request mapping；
- unary/stream usage、provider response identity、Anthropic stop reason与OpenAI terminal status；
- custom base URL只访问test-owned `127.0.0.1:0`；
- stream cancel、fragmented SSE、transport error、drop与early EOF；
- OpenAI `response.completed`和Anthropic non-empty `message_delta.stop_reason`作为protocol terminal evidence；Rig synthetic zero-usage `Final`不作为terminal；
- public `HttpClientExt` wrapper可以在原样转发bytes时提取terminal和allowlisted headers，不改变Rig output；
- OpenAI body response ID/header `x-request-id`与Anthropic body ID/header `request-id`独立提取；
- 400/401/429/500/529 typed envelope、malformed 200和single-request行为；
- 26-case provider-neutral delivery/error matrix中的context overflow、rate limit、auth、transport、malformed response和early EOF分类。

同一gate也证明Rig 0.40.0无法由真实Rust 1.85编译，因此这些结果是协议参考和SDK反例，不是production implementation dependency。OpenAI adapter使用Responses terminal/status/incomplete details；Anthropic adapter使用typed `stop_reason`。仍不可得时使用`Unknown`，不根据文本推断。

## Streaming And Progress

`generate_model_turn`内部消费provider stream，caller不持有raw stream。provider不支持streaming时Gateway使用non-streaming completion并只返回terminal result；这不改变caller interface。

```rust
pub enum ModelProgressEvent {
    ContentDelta {
        content_index: u32,
        delta: ModelContentDelta,
    },
}
```

```rust
pub enum ModelContentDelta {
    ReasoningText(Arc<str>),
    ReasoningSummary(Arc<str>),
    Text(Arc<str>),
    ToolCallName(Arc<str>),
    ToolCallArguments(Arc<str>),
}
```

规则：

- scoped publisher是process-local；AgentRun adapter必须先无损更新`StreamingItem`和ItemId映射，之后的Host ProgressEvent enqueue才是bounded、可合并/丢弃的observer路径；CompactionSummary adapter不创建ItemId；
- publisher由ActiveTurnTask scope到SessionId/TurnId；task创建的AgentRun adapter使用`content_index`维护`StreamingItem`并分配MiniCore ItemId；
- ModelGateway只发布provider-neutral content index和delta，不创建ItemId，也不接收SessionId或TurnId；logical retry由ActiveTurnTask发布`model_retry_scheduled`；
- 连续text/reasoning delta可以合并；
- Host progress queue满可以丢弃中间delta，但不能跳过operation-local累积；
- Host progress sink关闭不取消provider调用或operation-local累积；
- cancellation由CancellationToken决定；
- partial reasoning/text/tool arguments不是durable Item；
- hidden chain-of-thought不可发布；
- provider未暴露的reasoning不可推断；
- terminal success必须返回完整FinalizedAssistantResponse；
- terminal error必须返回typed ModelCallError；
- 对AgentRun，ActiveTurnTask把finalized text/reasoning content转为同ItemId的live final mutation；ToolCall在assistant live apply后产生Started ToolInvocation，随后完成inline record attempt并发布StateEvent；CompactionSummary结果不创建Item；
- ActiveTurnTask在terminal error后丢弃StreamingItem；若启动logical retry则发布`model_retry_scheduled`清理Host临时view，Turn terminal或新Snapshot提供最终校正。

不通过progress发布：

```text
credential
raw headers/body
provider SDK error
unredacted endpoint query
full prompt
full tool schema
opaque encrypted reasoning artifact
```

## Finalized Response

```rust
pub struct ModelCallResult {
    pub response: FinalizedAssistantResponse,
    pub diagnostics: Arc<[ModelCallDiagnostic]>,
}

pub struct FinalizedAssistantResponse {
    pub model: ModelResponseSummary,
    pub response_id: Option<ProviderResponseId>,
    pub content: Arc<[FinalizedAssistantContent]>,
    pub finish_reason: ModelFinishReason,
    pub effective_max_output_tokens: NonZeroU32,
    pub usage: Option<ModelUsage>,
    pub metadata: ProviderResponseMetadata,
}
```

```rust
pub enum FinalizedAssistantContent {
    Reasoning(ReasoningContent),
    Text {
        text: Arc<str>,
    },
    ToolCall {
        provider_item_id: Option<ProviderItemId>,
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: BoundedJsonObject,
        index: u32,
    },
}
```

`content[]`保持provider finalized semantic order。`ModelProgressEvent::ContentDelta.content_index`是该terminal `content[]`的zero-based位置，不是ToolCall内部`index`；adapter必须保证stream与terminal normalization使用同一位置语义，无法关联时返回`InvalidProviderResponse`。SessionExecutor随后分配或关联ItemId并构造assistant entry。

Session Execution把validated `content[]`中ToolCall variants的出现顺序规范化为Tools-owned `ToolCall.call_index`。上面的adapter `index`只服务provider ToolCall delta/final关联，不作为Session-local mutation FIFO或model-visible ordering的第二事实源。

`ToolCallId`由ProviderAdapter归一化：provider提供native call ID时原样保留；协议没有call ID时生成只需在当前assistant response内唯一的opaque ID。native ID在同一response内重复、stream/final映射不一致或无法按provider协议回填ToolResult时返回`InvalidProviderResponse`。ToolCallId不要求Session-wide唯一，durable层使用`TurnId + ItemId + ToolCallId`关联。

### Response Validation

ModelGateway必须在构造`ModelCallResult`前执行一个纯provider-neutral validation step：

```text
ProviderAttemptResult
→ normalize provider wire
→ validate finish/content consistency
→ validate OutputContract
→ ModelCallResult或ModelCallError
```

ToolCall仅在`output_contract = None`且request tools非空时允许。validation按以下顺序执行：

1. malformed provider wire、重复/缺失terminal identity、stream与final `content_index`无法对应，返回`InvalidProviderResponse`；
2. 当前调用不允许ToolCall却出现任意ToolCall，返回`UnexpectedToolCall`；
3. 在ToolCall本来允许的调用中，`Length`或`ContentFiltered`返回`IncompleteResponse`；即使content中已经出现ToolCall也不得执行；
4. `ToolCalls`但content无ToolCall、`Stop`或`Refused`却带ToolCall，以及provider要求finish reason但terminal缺失，返回`InvalidProviderResponse`；
5. `Refused`且包含非空finalized refusal text是successful response，不执行Structured schema validation；空refusal返回`InvalidProviderResponse`；
6. `Stop`或`Unknown`且无ToolCall时必须包含非空user-visible Text；空response或reasoning-only response返回`IncompleteResponse`；
7. `Structured`必须规范化为exact一个非空Text block（Reasoning可另存），该Text可直接解析为JSON并满足schema；否则返回`InvalidStructuredOutput`；
8. `Unknown`只用于adapter已经证明该provider无法可靠提供native finish reason的情况；若协议要求finish但wire缺失，必须返回`InvalidProviderResponse`。

可解析为BoundedJsonObject但不符合matching `ToolSpec` schema的arguments不是Response Validation error。该ToolCall在允许Tool的调用中保持valid model action，随后由ToolSet preflight形成failed ToolResult；Gateway不得把它改写为`InvalidProviderResponse`或`InvalidStructuredOutput`。

规范决策表如下；若同时命中多行，按上述validation顺序选择更早的error：

| ToolCall permission | Content | Finish reason | Result |
| --- | --- | --- | --- |
| forbidden | any ToolCall | any | UnexpectedToolCall |
| allowed | ToolCall | ToolCalls / Unknown | valid response → async loop执行Tool path |
| allowed | ToolCall | Length / ContentFiltered | IncompleteResponse |
| allowed | ToolCall | Stop / Refused | InvalidProviderResponse |
| any | no ToolCall | ToolCalls | InvalidProviderResponse |
| any | no ToolCall | Length / ContentFiltered | IncompleteResponse |
| any | no ToolCall + non-empty refusal text | Refused | valid candidate response |
| any | no ToolCall + empty refusal | Refused | InvalidProviderResponse |
| any | no user-visible Text | Stop / Unknown | IncompleteResponse |
| any | user-visible Text | Stop / Unknown | validate Structured when requested → candidate response |

通过validation后，ToolCall presence决定ActiveTurnTask进入Tool path还是candidate arbitration。ModelGateway不执行Tool，也不决定Turn terminal。

### ReasoningContent

```rust
pub struct ReasoningContent {
    text: Option<Arc<str>>,
    summary: Option<Arc<str>>,
    encrypted: Option<Arc<str>>,
    signature: Option<Arc<str>>,
    provider_item_id: Option<ProviderItemId>,
}

impl ReasoningContent {
    pub fn text(&self) -> Option<&str>;
    pub fn summary(&self) -> Option<&str>;
    pub fn encrypted(&self) -> Option<&str>;
    pub fn signature(&self) -> Option<&str>;
    pub fn provider_item_id(&self) -> Option<&ProviderItemId>;
}
```

`ReasoningContent`在ModelGateway validation后就是唯一live/storage-safe reasoning artifact shape；fields/constructor保持private，Conversation Storage直接复用该type，不定义`StoredReasoning` shadow DTO。`text | summary | encrypted | signature`至少一个为Some；provider_item_id只是auxiliary correlation，不能单独构成content。text<=262,144 bytes、summary<=131,072、encrypted<=262,144、signature<=16,384，provider_item_id遵守opaque ID limit。

`ReasoningContent`完整保留上述五个fields，包括portable `provider_item_id`，是fixtures/storage冻结的唯一reasoning artifact shape。这个ID是Prompt transcript唯一允许的portable provider exception；它随`ModelAssistantContentRef::Reasoning`可读，但不让adapter制造request/response attempt identity。response ID、stream/final content index、provider ordering bookkeeping、metadata、usage及所有其他provider-attempt facts绝不进入`ReasoningContent`、`ModelMessage`或its read refs。ToolCall terminal normalization中的provider item ID/index只用于Gateway stream/final reconciliation，投影到Prompt transcript时必须丢弃。

不得：

- 保存hidden chain-of-thought；
- 根据token usage生成reasoning text；
- 把encrypted artifact当普通text展示；
- 丢失provider要求的signature/item identity；
- 在不兼容provider/model上回放opaque artifact。

### Finish Reason

```rust
pub enum ModelFinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFiltered,
    Refused,
    Unknown,
}
```

规则：

- provider native finish code映射到closed taxonomy；
- 原始code可以作为allowlisted redacted metadata保留；
- generic provider projection没有finish reason时由protocol-specific adapter从terminal response提取；
- 仍不可得时使用Unknown，不根据文本内容伪造；
- ToolCall content是通过Response Validation后ActiveTurnTask进入Tool path的主要事实；finish reason在Gateway中用于一致性校验和diagnostics；
- Length和ContentFiltered返回`IncompleteResponse`，不进入live conversation；
- provider返回finalized refusal content时使用successful response + Refused；请求在生成response前被policy拦截时返回SafetyBlocked error；
- safety/refusal不能只表现为空Stop。

### Provider Response Metadata

```rust
pub struct ProviderResponseMetadata {
    pub provider_request_id: Option<ProviderRequestId>,
    pub raw_finish_code: Option<RedactedProviderCode>,
    pub service_tier: Option<RedactedProviderCode>,
}
```

metadata使用allowlist字段。response/request ID、finish code和service tier都必须执行长度、字符集、control-character和redaction validation。禁止保存raw response、headers map、request payload、server trace或secret-bearing URL。

`response_id`可以随assistant entry持久化用于diagnostics和可能的provider replay artifact关联，但它不是conversation identity，也不能单独授权continuation。

## Usage

```rust
pub struct ModelUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub provider_total_tokens: Option<u64>,
    pub reported_cost: Option<Money>,
}
```

`Money`使用[Wire Schema Money](wire-schema.md#money)，all u64 token/count carrier使用[Wire Schema u64 rule](wire-schema.md#revisions)；`reported_cost`必须non-negative。

规则：

- provider未返回的字段保持`None`；
- `provider_total_tokens`只保存provider报告值，不通过其他字段相加伪造；Anthropic Messages在consumed contract中不报告total，因此Anthropic normalized usage恒为`None`（各provider-reported cumulative component原样保留并继续执行monotonic检查；usage finalization是infallible，individually valid的大计数器即使本地求和会溢出也不得使terminal失败）；
- locally estimated tokens属于ContextUsage projection，不进入ModelUsage；
- locally calculated price属于rebuildable cost estimate，不写`reported_cost`；
- `reported_cost`只用于provider明确返回billed cost的情况；
- successful response usage随对应assistant entry保存；
- Session total usage由assistant/compaction entries replay重建；
- 每次Gateway operation最多一个provider attempt；usage是否出现不能代替delivery-state分类，也不能把不确定outcome降级成普通transient error；
- provider若为失败attempt报告usage，该事实只进入ModelGateway internal telemetry；不放入ModelCallError、不进入Session aggregate，也不创建synthetic assistant或独立Usage entry；
- 因此MiniCore Conversation Storage不是provider billing ledger。

StoredAssistantMessage的`logical_retry_count`定义为ActiveTurnTask对同一logical call执行的logical retry数量。Gateway没有transparent retry count。

## Retry

### Single Provider Attempt

MVP中一次`generate_model_turn`至多执行一个provider attempt，并独立地至多发送一个HTTP POST：

```text
validate request（owner validation失败 → typed error terminal：0次execute、0个POST）
→ resolve/refresh credential before request（cancel/`AuthMissing` → 0次execute、0个POST）
→ ProviderAdapter.execute（至多一次）
   → adapter编码/build失败或adapter级pre-send cancellation → 1次execute、0个POST
   → 发送POST：携带完整full request（1次execute、1个POST）
→ normalize one terminal result or typed error
```

从不调用第二次`execute`，从不发送第二个POST，从不发送optimization-specific fallback POST。

adapter与private HTTP client automatic retry必须配置为0。ModelGateway不执行DNS/connect/TLS retry、429/5xx retry、401 refresh-and-resend、stream restart或WebSocket → HTTP fallback。失败后本次Gateway operation立即terminal，由ActiveTurnTask根据typed error决定是否logical retry。

每个ProviderAttemptError必须携带private delivery state：

```rust
pub(crate) enum ProviderRequestDeliveryState {
    NotSent,
    RejectedBeforeExecution,
    AcceptedNoOutput,
    OutputStarted,
    Unknown,
}
```

HTTP status或“尚无delta”本身不能决定delivery state。adapter必须根据protocol contract、provider response和transport阶段准确映射：已发送但outcome不确定时返回`RequestOutcomeUnknown`，已发布partial output后失败返回`StreamInterrupted`。该proof决定ActiveTurnTask能否安全logical retry，不能因为Gateway不内联retry而省略。

只有`NotSent`和provider明确证明未开始执行的`RejectedBeforeExecution`可以映射为默认retryable reason。`AcceptedNoOutput`若没有更强的provider terminal proof必须映射`RequestOutcomeUnknown`；不能把“还没收到delta”当作未执行证明。

每次attempt：

1. 检查cancellation；
2. resolve current credential；
3. 调用`ProviderAdapter::execute`：build provider payload、start provider request并更新delivery state；
4. consume stream；
5. normalize terminal result。

request前credential可以resolve/refresh。当前M14 `CredentialSource`是public host-only async capability：`resolve()`只构造future，future拥有全部工作且drop必须停止owner-visible work；`None`表示当前无可用credential并映射`AuthMissing/NotSent`。Gateway不读取env/home、不缓存secret、不内建refresh/singleflight；需要的bounded refresh/singleflight由trusted host source内部拥有，且不能detach未受owner跟踪的工作。provider返回401时terminal为`AuthRejected`，不在本次Gateway operation中refresh-and-resend。每次invocation（包括Session logical retry对同一`Arc<ModelCallRequest>`的再次调用）至多调用一次`ProviderAdapter::execute`，独立地发送零或一个HTTP POST（owner validation/pre-send cancellation/`AuthMissing`在调用adapter前以typed error terminal→零execute/零POST；adapter编码/build失败或adapter级pre-send cancellation为一次execute/零POST；若发送POST则携带完整full request，ADR 0141），credential逐attempt重新解析，不pin到request。

### Semantic Delta Rule

默认规则：

- `NotSent`或`RejectedBeforeExecution`映射为具体typed transient error和optional `retry_after`，交给SessionExecutor裁决；
- `AcceptedNoOutput`缺少明确pre-execution rejection proof时映射`RequestOutcomeUnknown`；
- request已经发送但尚无response，delivery outcome不确定时返回`RequestOutcomeUnknown`；
- 第一个model-visible delta发布后，普通restart可能重复text、tool call或billing，因此返回`StreamInterrupted`；
- MVP不执行exact resume或stream restart；
- progress中已经发布的StreamingItem由SessionExecutor在terminal error后丢弃，不生成Completed Item。

### Authentication

- AuthMissing不重试；
- credential在single attempt前即时resolve，必要的token refresh可以singleflight完成，不pin access token到TurnModelSnapshot；
- provider返回401后直接返回AuthRejected，不在Gateway内部重发；
- refresh log和error不得包含token。

### Rate Limit

- Gateway规范化typed `Retry-After`但不sleep；
- ActiveTurnTask只在provider hint不超过60秒时按logical retry policy等待；
- Gateway不保留retained route/principal cooldown state：当前实现没有`ProviderRateLimitState`、route/principal cooldown cache或preflight cooldown fast-fail；限流一律作为terminal `RateLimited { retry_after }`返回，由ActiveTurnTask裁决logical retry（MiniCore没有auth-principal类型，credential binding identity不进入Gateway状态）。

### Transport Fallback

MVP不执行transparent transport fallback：

```text
WebSocket → HTTP SSE
provider A/model X → provider B/model Y
model X → model Z
Responses model → capability不同的Chat Completions model
```

首个production adapter应选择一个明确transport。任何transport/model替换都留到future Turn或显式Session definition update；不能在active Turn内静默完成。

跨模型替换会改变capabilities、limits、cost和输出行为，破坏Turn-pinned exact Model。需要替换时：

- 在Session definition update或下一Turn admission前显式resolve；
- 形成新的SessionDefinitionRevision或明确的future Turn model choice；
- 当前Turn失败或由用户显式Cancel后重试；
- 不由ModelGateway悄悄完成。

### ActiveTurnTask Logical Retry

Gateway single attempt返回typed error后，ActiveTurnTask决定logical retry：

```text
current Turn still Running and TurnId matches
control_generation unchanged
ConversationRevision unchanged
same `Arc<ModelCallRequest>` is reused; no reassembly
→ close/drop previous local future result path
→ cancellation-aware sleep
→ 使用同一ModelCallRequest再次await ModelGateway
```

logical retry：

- 不阻塞SessionExecutor control actor；
- queued Steer只排队且不改变`ConversationRevision`，因此不使in-flight result或retry backoff失效；Steer只在safe point被消费并成功apply后递增revision，使旧request basis失效；
- Cancel、成功Compaction Replace或其他conversation mutation通过control/revision basis使retry失效；
- 计入StoredAssistantMessage.logical_retry_count；
- 不创建ModelAttempt entity；
- 不改变ModelCallPurpose；
- 不与旧本地Model future重叠；provider端可能继续工作或计费不等于旧future仍可回传ActiveTurnTask。

默认policy：

| Purpose | 最大logical retries | Backoff |
| --- | ---: | --- |
| AgentRun | 3 | 2s、4s、8s |
| CompactionSummary | 1 | 2s |

自动retry必须同时满足delivery proof为`NotSent`或`RejectedBeforeExecution`，且reason是`Timeout`、`TransportUnavailable`、`ProviderUnavailable`，或typed `Retry-After <= 60s`的`RateLimited`。`AcceptedNoOutput`没有明确pre-execution rejection proof时归一化为`RequestOutcomeUnknown`。`RequestOutcomeUnknown`、`StreamInterrupted`、认证、quota、配置、安全、`UnexpectedToolCall`、`InvalidStructuredOutput`、`InvalidProviderResponse`和`IncompleteResponse`默认不自动retry；`ContextOverflow`进入Compaction。完整决策见[ADR 0119](../adr/0119-model-calls-use-session-logical-retries.md)和[ADR 0120](../adr/0120-failures-stay-with-owning-modules.md)。

## Cancellation

ModelGateway在以下位置检查CancellationToken：

- resolution validation后；
- auth resolution和request前refresh前后；
- provider request开始前；
- stream read期间，直到provider terminal event被adapter接受。

Gateway success/cancel的线性化点是adapter接受完整provider terminal event：

- cancellation先被观察：中止request并返回Cancelled；
- terminal event先被接受：Gateway完成normalization并返回ModelCallResult，之后到达的cancel不把该result改写为Cancelled；
- ActiveTurnTask仍会用control_generation、ConversationRevision和Cancel/SecurityRevoked arbitration决定该result能否apply live；普通Steer不拒绝已完成result，只决定无ToolCall response保存为Continue还是Final，因此Gateway terminal先赢不等于Turn一定Completed；
- 该规则保证terminal usage、finish reason和content不会因normalization期间的timer race随机丢失。

取消行为：

```text
cancel token fires
→ 取消当前single attempt
→ 调用transport abort
→ 停止发布新progress
→ 返回ModelCallErrorReason::Cancelled
```

不能承诺：

- provider没有收到request；
- provider立即停止generation；
- provider不会计费；
- 已发布partial delta可以恢复；
- cancellation可以回滚provider-side cache。

ActiveTurnTask不detach Model future；`generate_model_turn` terminal返回或future被安全drop并关闭结果路径后，才能开始下一次logical attempt。因此正常路径不存在旧Model result在新attempt之后迟到。SessionId、TurnId、control_generation和ConversationRevision用于验证result basis。

## Error Taxonomy

```rust
pub struct ModelCallError {
    pub reason: ModelCallErrorReason,
    pub message: RedactedErrorMessage,
    pub retry_after: Option<Duration>,
    pub diagnostics: Arc<[ModelCallDiagnostic]>,
}
```

```rust
pub enum ModelCallErrorReason {
    Cancelled,
    ModelUnavailable,
    AuthMissing,
    AuthRejected,
    RateLimited,
    QuotaExceeded,
    ContextOverflow,
    UnsupportedCapability,
    InvalidRequest,
    SafetyBlocked,
    Timeout,
    TransportUnavailable,
    ProviderUnavailable,
    ProviderRejected,
    RequestOutcomeUnknown,
    StreamInterrupted,
    UnexpectedToolCall,
    InvalidStructuredOutput,
    InvalidProviderResponse,
    IncompleteResponse,
}
```

错误映射：

| Error reason | Gateway行为 | SessionExecutor默认处理 |
| --- | --- | --- |
| Cancelled | 停止attempt | Cancel/SecurityRevoked/version规则；普通Steer不取消attempt |
| ModelUnavailable | 不替换模型 | TurnFailed或要求SetModel |
| AuthMissing/AuthRejected | 不重发；401直接AuthRejected | user action / TurnFailed |
| RateLimited | 返回typed Retry-After | hint <= 60s时可logical retry |
| QuotaExceeded | 不自动retry | user action / TurnFailed |
| ContextOverflow | 不改Prompt | bounded compaction recovery |
| UnsupportedCapability | provider调用前拒绝 | configuration failure |
| InvalidRequest | provider调用前或400 mapping | implementation/config failure |
| SafetyBlocked | 不自动retry | truthful terminal response/error policy |
| Timeout | 仅`NotSent`/`RejectedBeforeExecution`时映射；single attempt terminal | logical retry或TurnFailed |
| TransportUnavailable | 仅`NotSent`/`RejectedBeforeExecution`时映射；single attempt terminal | logical retry或TurnFailed |
| ProviderUnavailable | 仅`NotSent`/`RejectedBeforeExecution`时映射；single attempt terminal | logical retry或TurnFailed |
| ProviderRejected | 不解析raw string | TurnFailed/diagnostic |
| RequestOutcomeUnknown | 不重放 | 默认TurnFailed，提示可能重复provider work/cost |
| StreamInterrupted | 不重启stream | 丢弃draft并默认TurnFailed |
| UnexpectedToolCall | 不返回assistant candidate，不执行Tool | non-retryable Model TurnFailure |
| InvalidStructuredOutput | 不修复、不coerce、不blind retry | non-retryable Model TurnFailure |
| InvalidProviderResponse | fail closed | non-retryable Model TurnFailure + diagnostic |
| IncompleteResponse | 丢弃partial candidate | non-retryable Model TurnFailure |

规则：

- caller不能解析`message`决定retry；
- provider raw error body不越过adapter；
- error message必须redacted、bounded且provider-neutral；
- provider status、request ID等只作为allowlisted diagnostic；
- ContextOverflow必须与Prompt本地size validation保留不同source；
- SafetyBlocked和Refused response的最终建模必须由adapter根据provider protocol一致处理；
- `Timeout`、`TransportUnavailable`、`ProviderUnavailable`和`RateLimited`只有在delivery proof为`NotSent`或`RejectedBeforeExecution`时才能保持该retryable reason；`AcceptedNoOutput`没有明确pre-execution rejection proof时、以及其他outcome unknown都必须映射`RequestOutcomeUnknown`，已有semantic delta必须映射`StreamInterrupted`；
- 上述四个response error发生在request已经离开`NotSent | RejectedBeforeExecution`安全状态之后，或在completed response validation期间，不满足ADR 0119的safe-delivery retry前提；SessionExecutor不得把它们重新解释为transient failure。

## Authentication And Secret Redaction

ModelGateway implementation不包含`AuthStore`、`ResolvedAuth`、`AuthBindingRef`或`AuthPrincipalIdentity`类型。credential resolution是request-bound的每attempt操作：`ModelCallRequest`不携带secret，其model execution持有crate-private `CredentialSource`引用，`generate_model_turn`在adapter执行前动态resolve（ADR 0141）。一个installed source代表一个稳定nonsecret credential binding（account/project/tenant scope）；token可逐attempt轮换，静默切换binding identity属于配置变更（新installation/definition version/runtime config publication），MiniCore不推断也不检查。不存在connection/cache隔离键或continuation compatibility principal。

secret只允许存在于：

```text
host-owned CredentialSource（secret保存在source内部）
attempt期间解析的ProviderCredential
provider client/request
actual transport request
```

secret不得进入：

```text
TurnModelSnapshot
ModelCallRequest
AssembledModelContext
ModelProgressEvent
ModelCallResult
ModelCallError
ConversationStorage
Runtime event/snapshot
Prompt/Tool/Skill
logs或Debug output
```

要求：

- `ProviderCredential`不实现revealing Debug/Display/Serialize；resolved value只在单次attempt内由Gateway转交private direct adapter做header injection；
- headers和URL query在diagnostics前redact；
- API key、OAuth token、cookie和signed URL使用typed secret wrappers；
- request前auth resolve/refresh若需要singleflight，由trusted host credential source内部实现；Gateway本身不缓存或singleflight secret，401后不重发；
- custom provider auth reference只能来自user-global trusted config；
- project Workspace不能注册base URL、headers或credential source；
- Runtime hook未来只能看到provider-neutral redacted request summary；
- raw provider payload hook默认不提供。

## Provider Catalog And Custom Providers

custom provider必须显式声明：

```rust
pub enum ProviderProtocol {
    OpenAiResponses,
    AnthropicMessages,
}
```

```rust
pub struct CustomProviderDefinition {
    pub provider_id: ProviderId,
    pub protocol: ProviderProtocol,
    pub endpoint: Url,
    pub auth_ref: AuthRef,
    pub models: Arc<[CustomModelDefinition]>,
}
```

当前实现不公开generic protocol enum或raw custom-provider DTO；host使用`ModelProviderConfig::openai_responses(...)`或`ModelProviderConfig::anthropic_messages(...)`显式选择protocol，并提供共享credential source与一个或多个`ModelProviderDescriptor`。constructor只做纯validation；`MiniCoreRuntime::open`为每个installation构建exact一个direct adapter/client与static source，后续shared reload复用该source，不重建client。

规则：

- 首个production baseline只支持`OpenAiResponses | AnthropicMessages`；两个protocol adapter、dynamic credential resolution与catalog/model source installation均已完成。默认Runtime仍为空catalog；只有trusted host显式安装的definition可解析。explicit ignored live smoke harness已实现，实际real-credential release run仍待记录；
- OpenAI Chat Completions、Gemini或其他protocol不接受为首版config；新增variant必须先有同等级loopback、delivery/error fixture与Accepted decision；
- 不根据endpoint或model name猜protocol；
- endpoint默认必须HTTPS；tests/development只能通过显式policy允许numeric loopback `127.0.0.0/8 | ::1` HTTP，hostname `localhost`和任意非loopback HTTP均拒绝；
- config load拒绝URL userinfo、query和fragment；path必须canonicalize且不得携带secret-like token；
- endpoint identity只使用canonical origin和validated base path，safe catalog view默认不显示完整custom path；
- auth_ref是nonsecret reference，只存在于Gateway implementation；
- arbitrary headers必须来自user-global trusted config并经过allowlist/redaction；
- model capability由definition声明并validate；
- capability声明错误在adapter mapping时fail closed；
- catalog refresh产生新的ModelDefinitionVersion；
- active Turn继续使用retained exact definition；
- future Turn resolve current definition；
- model listing/query返回safe view，不返回endpoint credential details。

## Prompt Cache

Prompt cache不是response memoization，也不是conversation state。

**M14 current policy（[ADR 0141](../adr/0141-provider-calls-are-stateless-full-request.md)）：显式prompt cache是intentionally disabled/omitted。** MiniCore不请求、不控制、不选择provider cache：OpenAI请求永不携带`prompt_cache_key`/`prompt_cache_retention`/`cache_control`；Anthropic请求在任何递归位置（system blocks、message/content blocks、tool definitions）永不携带`cache_control`，也永不发送`anthropic-beta` cache header。若发送POST则总是携带完整system/messages/tools。provider报告的cache read/write token（OpenAI `input_tokens_details.cached_tokens`、Anthropic `cache_read_input_tokens`/`cache_creation_input_tokens`）只作为usage evidence，不构成请求或控制。

这些省略不声称provider执行zero automatic caching或retention：provider可能自动缓存；MiniCore只是不请求/不控制，正确性从不依赖cache。cache miss、eviction或自动命中的语义差异都不改变conversation truth。

未来激活门槛（ADR 0141，未实现、不预留抽象）：

- 独立的provider-specific evidence与新ADR；
- 稳定credential binding加显式tenant/session privacy scope；
- canonical full-wire successor proof（新请求必须是旧完整请求+finalized response+exact sanitized live suffix，可审计）；
- retention/billing policy；
- 与one-POST语义的reconciliation（同一次operation内不得出现第二个POST）。

激活后仍必须遵守：cache annotation只能添加到canonical content边界；不为了cache重排、删除、复制或改写content；cache key不得包含secret或raw user content；cache miss不改变语义；provider cache key不是PromptSet、Turn或conversation identity。

## Connection Reuse And Continuation

**M14 current policy（[ADR 0141](../adr/0141-provider-calls-are-stateless-full-request.md)）：不存在`ProviderConnectionPool`或`ContinuationCache`实现，也不存在`ProviderRuntime`/`ProviderWirePlan`/`ProviderRateLimitState`类型。**

### Connection Reuse

连接政策就是adapter-owned的reqwest client：每个provider installation在`MiniCoreRuntime::open`时构建一个direct adapter（一个reqwest client），shared-resource reload复用同一source/client，不重建。这只描述普通无状态reqwest transport pooling eligibility（HTTP连接复用能力），不承诺物理socket必然复用，也不携带auth或session state；不能按SessionId保存credential-bearing client作为Session state。

### Continuation

`previous_response_id`、incremental input或sticky provider state在M14不发送、不缓存、不规划：每次invocation至多调用一次`ProviderAdapter::execute`，独立地发送零或一个HTTP POST（若发送POST则携带完整full request），没有optimization-specific fallback POST、重试或continuation state。

旧语言“provider rejects continuation then fallback full request”不能解释为同一次operation内的第二次POST——每次`generate_model_turn`至多一个POST，fallback只能作为later distinct logical request之前的新ADR规划。

未来若激活continuation，必须满足ADR 0141门槛，且至少：same provider protocol and endpoint、same ModelDefinitionRef、same purpose、same effective generation policy、same tool schemas and output contract、same adapter encoding version、previous call completed successfully、新完整logical input可证明为previous full input + previous finalized response + exact sanitized live suffix；必须使用完整provider-neutral input和private canonical equivalence proof证明prefix，不能只比较SessionId、TurnId、response_id或message count。ContinuationCache即使激活也是process-local optimization，不进入TurnExecutionContext，不通过Session recording恢复。

## Concurrent Calls And Rate Governance

ModelGateway不提供Runtime global、per-provider route、per-model或per-auth-principal模型调用permit，也不建立本地admission queue或公平调度器。该决策由[ADR 0125](../adr/0125-model-gateway-has-no-local-call-permits.md)冻结。

要求：

- 共享`Arc<ModelGateway>`支持多个Session并发执行独立provider attempt；
- 不用Gateway-wide mutex或其他长guard包围credential resolution、provider request或stream读取；
- 每个Session最多一个ActiveTurnTask，task内部最多await一个current model call，不形成跨Session容量限制；
- request前auth refresh只对同一credential执行singleflight，不阻塞无关credential的调用；
- 不存在route/principal cooldown cache或preflight cooldown fast-fail；`RateLimited`、`QuotaExceeded`和typed `Retry-After`继续规范化为terminal result，由ActiveTurnTask裁决logical retry；
- Provider SDK/HTTP connection pool的transport资源管理是private implementation detail，不成为MiniCore admission policy；
- progress publisher阻塞不能占用provider stream读取。

## Complete Call Flow

```text
Turn admission
→ shared publication gate captures Arc<ModelCatalogView>
→ ModelGateway.resolve_for_turn(captured catalog, ModelSelection + preferences)
→ Arc<TurnModelSnapshot>
→ TurnExecutionContext pins snapshot

ActiveTurnTask requests next model step
→ PromptSet.assemble(LiveConversationView, purpose, output contract)
→ AssembledModelContext
→ ActiveTurnTask creates ModelCallRequest(source_revision)
→ ModelGateway.generate_model_turn
   → validate snapshot/request binding
   → choose exact provider adapter and route
   → resolve current credential（逐attempt，ADR 0141）
   → encode full AssembledModelContext（无cache/continuation optimization；至多一次execute、独立地零或一个POST，若发送则携带完整full request）
   → provider stream
   → publish ModelProgressEvent to operation-local adapter
      → lossless StreamingItem/ItemId update
      → best-effort Host ProgressEvent enqueue
   → normalize ordered finalized content/usage/finish reason/metadata
   → return ModelCallResult
→ ModelCallResult returned to ActiveTurnTask
→ task validates Turn/control_generation/ConversationRevision and Workspace authorization
→ Tool path或candidate arbitration
→ apply assistant live + await SessionRecorder.record
```

## Failure And Recovery

### Process Restart

不恢复：

```text
provider stream
connection
resolved credential
partial draft
single-attempt state
```

process restart直接丢弃active Turn与旧TurnStatus。下一Turn从recorded conversation prefix重建sanitized live context并建立新provider request；未record tail不可恢复。M14不存在continuation state或connection pool state可恢复（ADR 0141）：每次invocation都是fresh full request。

### Outcome Ambiguity

模型调用不是Tool side effect ledger：

- transport error后provider可能已经生成或计费；
- 未取得terminal finalized response时不能创建synthetic assistant message；
- partial draft不持久化；
- request已发送但尚未开始response时映射为RequestOutcomeUnknown；已产生partial response后映射为StreamInterrupted；
- `RequestOutcomeUnknown`和`StreamInterrupted`默认禁止logical retry；ActiveTurnTask只能对Gateway已经证明delivery-safe的typed error应用ADR 0119 policy；
- duplicate assistant prevention依赖task只接受一次terminal ModelCallResult并通过live reducer应用；Session recording不提供operation key或去重permit。

### Session Recording Failure

ModelGateway成功后assistant先进入live state，再best-effort record：

```text
ModelCallResult
→ ActiveTurnTask identity/authorization/revision validation
→ apply assistant live
→ await SessionRecorder.record
→ publish final StateEvent / continue Tool path
```

encode/write失败时，assistant live mutation保留，recording health转为Degraded，随后Tool path或candidate流程继续。不能重新调用provider或重新执行Tool来补偿recording failure。

## Recording Contract

SessionRecorder可以在`StoredAssistantMessage`中保存safe `ModelResponseSummary`：actual `ProviderId + ModelId + ModelReasoningSummary + ModelServiceClass`。response ID、finish reason、effective max output、usage、logical retry和allowlisted ProviderResponseMetadata是StoredAssistantMessage的独立fields，不属于ModelResponseSummary。它用于recorded history显示和reasoning artifact解释，不要求old `ModelDefinitionVersion`或retained catalog definition在cold replay时仍可解析；active Turn和旧TurnModelSnapshot不跨restart恢复。

实际record成功的assistant entry按[Format V1 Assistant Message](../formats/conversation-jsonl-v1.md#assistant-message)保存：

```text
ModelResponseSummary
allowlisted response_id
ordered StoredAssistantContent[]（每variant内联ItemId）
normalized ModelFinishReason
effective_max_output_tokens
normalized ModelUsage
Session logical_retry_count
allowlisted ProviderResponseMetadata
```

不保存：

```text
raw provider request/response
credential
headers
base URL
provider SDK object
partial stream
provider attempt trace
ActiveTurnTask retry timer
connection/continuation state（M14不存在；每次invocation都是fresh full request）
full AssembledModelContext
```

首版automatic SummaryModel compaction把normalized result投影到[Compaction唯一拥有的`StoredCompactionModelCall`](compaction.md#summary-validation与provenance)：`ModelResponseSummary`、allowlisted response ID、usage、finish reason、requested max output、Session logical retry count和`ProviderResponseMetadata`。StoredCompaction本体保存summary与single `first_kept_entry_id` marker；automatic路径该字段必须为Some。`None`只为未来明确设计的deterministic maintenance/import保留，不是automatic overflow fallback。ModelGateway不定义第二份stored provenance type，也不接收marker/cut。

`logical_retry_count`只表示Session logical retry。Gateway没有transparent retry count。

Provider response ID和ReasoningContent opaque artifact使用Wire/Format v1 exact length、safe-character和redaction validation；oversized response在live apply前ModelGateway fail，不依赖Recorder截断。

## Performance

目标：

- `resolve_for_turn`只做local catalog lookup和validation；
- full context encoding与provider I/O在ActiveTurnTask await的Gateway operation内执行；
- adapter-owned reqwest client跨installation/reload共享（普通无状态transport pooling，非admission policy）；
- 多个Session可并发执行provider request，不经过Gateway本地admission queue；
- progress delta可以合并；
- provider stream reader不等待slow observer；
- request前auth refresh singleflight（host source内部）；
- M14没有cache/continuation optimization可失败，因此不存在fallback路径；未来optimization必须先满足ADR 0141门槛；
- 不因optimization额外复制完整conversation多次。

性能不能牺牲：

- full logical request equivalence；
- exact model pin；
- cancellation responsiveness；
- usage/finish/content order正确性；
- secret redaction；
- SessionExecutor control actor的request响应能力。

## Diagnostics

ModelCallDiagnostic是bounded、redacted、provider-neutral value：

```text
model definition ref
adapter kind
transport kind
timing category
status class
request/response allowlisted IDs
```

M14不产生cache hit/miss/unsupported或continuation used/full-request fallback诊断（ADR 0141：无cache/continuation optimization）；这些category只保留给未来激活后的diagnostic shape。

不得包含：

```text
prompt正文
user message正文
tool arguments/results
credential
raw headers/body
full URL query
opaque encrypted reasoning
```

内容级debug需要显式secure diagnostic mode，并且仍不得记录credential。

## Test Matrix

### Interface And Ownership

- 只有`resolve_for_turn`和`generate_model_turn`进入Session execution；
- ActiveTurnTask/SessionExecutor不import provider-native或transport类型；
- ModelGateway不能读取Conversation Storage或Prompt definitions；
- ProviderAdapter不能append Session entry；
- shared Gateway并发处理多个Session；
- 没有global current Session/model。

### Model Resolution

- exact provider/model selection；
- missing provider/model；
- catalog重新发布时active Snapshot保持旧definition；
- future Turn取得新definition；
- capability/limit validation；
- reasoning preference Auto/Disabled/Low/Medium/High mapping，unsupported explicit value在resolution fail且adapter不silent downgrade；
- unknown limits保持None；
- effective TokenEstimateRate进入ModelDefinitionVersion，rate/catalog更新只影响future Turn；
- definition未声明rate时使用保守默认并产生diagnostic；
- 同一个TurnModelSnapshot的estimator在PromptSet/Compaction调用点结果一致；
- non-text unknown estimate不触发soft compaction；
- project config不能注册provider/auth；
- active Turn不执行cross-model fallback；
- explicit requested/default AgentRun max output在resolve时形成non-zero effective reservation，超过known model limit时拒绝；
- Compaction pressure读取该exact reservation，不使用provider隐式default或model-name heuristic。

### Prompt Mapping

- System和User的provider mapping；
- unsupported role fail closed；
- User/Assistant/Tool order保持；
- reasoning/text/tool call content order保持；
- ReasoningContent至少一个field且各artifact byte/opaque ID limits；
- ModelResponseSummary只含portable actual provider/model/reasoning/service class；
- orphan ToolResult拒绝；
- ToolSpec schema/name mapping；
- BoundedJsonSchema exact byte/depth/node/ref/keyword boundary，remote ref拒绝；
- finalized ToolCall arguments必须是BoundedJsonObject且超limit在ModelGateway validation失败；
- ToolChoice None/Auto/Required/Specific；
- JSON schema/output contract；
- NoToolCalls和Structured携带非空tools时constructor拒绝；
- unsupported image/audio/document；
- Gateway不截断或摘要context；
- ModelGateway不修改`AssembledModelContext`；
- assembly proof purpose/model/output-contract mismatch在ModelCallRequest constructor被拒绝。
- ordinary AgentRun允许`input.output_contract = None`和`max_output_tokens = None`；
- explicit AgentRun output limit使用`Some(NonZeroU32)`且不得超过Snapshot effective limit；
- Structured与NoToolCalls只存在于`input.output_contract`，request不保存第二份contract；
- Compaction budget proof missing、purpose不匹配或request max output不相等时constructor拒绝；
- CompactionSummary的request `max_output_tokens`必须为`Some`且不超过Snapshot known limit；
- 越界CompactionSummary在provider调用前以InvalidRequest拒绝，Gateway不静默clamp。

### Streaming

- start/text/reasoning/tool-call delta；
- `content_index`与terminal `content[]`位置一致，ToolCall内部index不参与该correlation；
- AgentRun operation-local StreamingItem累积先于Host queue，delta queue满时合并/丢弃不影响ItemId映射或final result；CompactionSummary不分配ItemId；
- publisher关闭不取消call；
- cancellation停止progress；
- unexpected EOF → StreamInterrupted；
- partial draft不进入storage；
- finalized response可以完整替换observer draft；
- hidden reasoning不发布。

### Retry

- admitted request每次generate_model_turn至多调用一次ProviderAdapter.execute；owner validation、pre-send cancellation和AuthMissing在调用adapter前以typed error terminal（零次execute），adapter编码/build失败或adapter级pre-send cancellation发生在那一次execute内部且不产生POST（一次execute、零个POST），因此execute总数为0或1、POST总数为0或1，且POST总数独立于execute计数；
- adapter与private HTTP client automatic retry固定为0；
- connect failure、429、5xx和timeout都以typed error terminal返回Gateway caller；
- delivery state准确区分NotSent、RejectedBeforeExecution、AcceptedNoOutput、OutputStarted和Unknown；
- request delivery outcome unknown返回RequestOutcomeUnknown；
- first semantic delta后failure返回StreamInterrupted；
- 401不refresh-and-resend；request前credential refresh可以singleflight；
- 429返回typed Retry-After，Gateway不sleep；
- MVP不做WebSocket → HTTP fallback；
- 不允许provider/model substitution；
- AgentRun最多3次Session logical retry，backoff 2s/4s/8s；
- CompactionSummary最多1次Session logical retry，backoff 2s；
- RequestOutcomeUnknown和StreamInterrupted默认不logical retry；
- 仅`NotSent`/`RejectedBeforeExecution`的Timeout/TransportUnavailable/ProviderUnavailable允许logical retry；AcceptedNoOutput和unsafe/unknown outcome必须归一化为RequestOutcomeUnknown或StreamInterrupted；
- Retry-After <= 60s时实际delay取purpose backoff与hint较大值，超过60s不调度；
- queued Steer不使in-flight result或retry backoff失效；Steer只在safe point消费并成功apply后通过revision变化使旧request失效；Cancel通过control arbitration使retry失效。

### Cancellation

- cancel during auth resolve/request前refresh；
- cancel before request start；
- cancel during stream；
- provider terminal先于cancel时Gateway完成result；SessionExecutor仍可因version/cancel拒绝；
- transport abort桥接；
- logical retry与queued Steer场景不存在可回传的旧detached Model future；exact `control_generation`、`ConversationRevision`与same `Arc<ModelCallRequest>`用于拒绝basis已经变化的result。

### Usage And Finish

- all provider usage fields；
- missing fields保持None；
- no local estimate in ModelUsage；
- failed attempt usage只进ModelGateway internal telemetry，不进入ModelCallError或Session aggregate；
- successful response usage随assistant保存；
- logical retry_count准确记录0–3（AgentRun）或0–1（CompactionSummary）；
- automatic Compaction projection完整携带model/response ID/usage/finish/requested max output/logical retry/allowlisted metadata且总是Some；
- Stop/ToolCalls/Length/ContentFiltered/Refused/Unknown；
- NoToolCalls或Structured返回ToolCall → UnexpectedToolCall，ToolSet从未被调用；
- output_contract=None且tools为空时返回ToolCall → UnexpectedToolCall；
- output_contract=None且tools非空时，parseable但ToolSpec schema-invalid的arguments仍形成valid ToolCall并交给ToolSet preflight；
- Structured exact JSON/schema success → ModelCallResult；
- Structured non-empty syntax/schema/fence/coercion failure → InvalidStructuredOutput；
- ToolCalls无call、Stop/Refused带call、required finish缺失、stream/final index不一致 → InvalidProviderResponse；
- Length、ContentFiltered、empty/reasoning-only terminal → IncompleteResponse；
- non-empty Refused作为successful response，不经过Structured schema validation；
- empty Refused → InvalidProviderResponse；
- 上述四个response error均不logical retry且不生成Completed Item；
- generic provider projection缺finish reason时protocol-specific extraction；
- response ID长度和字符validation。

### Auth And Redaction

- AuthMissing/AuthRejected；
- request前OAuth refresh并发singleflight，401后不重发；
- Debug/Display/Serialize不泄漏secret；
- raw error body redaction；
- URL/header redaction；
- progress/result/error/storage不含credential；
- custom endpoint HTTPS、userinfo/query/fragment和canonical path policy；
- runtime trusted config source。

### Cache And Continuation

M14 wire policy（ADR 0141）的loopback evidence：

- OpenAI recursive request denylist（context-aware traversal）：wrapper从provider-wire context开始，只在adapter-emitted schema-root成员（tool definition `parameters`、Structured `text.format.schema`）进入JSON Schema context；只有schema context内的`properties`把user property-name keys当data——schema property VALUE递归、key不比较。provider-wire对象（含假设的provider-owned `properties`对象）的forbidden key比较始终active；ordinary/toolful/replay/Structured/Compaction五种代表性请求的完整JSON value递归遍历，任何protocol/schema节点上都不存在provider-owned optimization member（`previous_response_id`/`prompt_cache_key`/`prompt_cache_retention`/`cache_control`/`conversation`作为实际成员）——Structured fixture schema故意包含名为`conversation`/`cache_control`的user property，且captured provider schema中这两个property name原样保留（各自为string schema），证明无false positive、无sanitization删除；replay shape经OpenAI-truthful helper（reasoning带provider item id+text/summary、无Anthropic signature；Turn-pinned tool set含echo）并逐item断言captured wire：initial user → replayed reasoning item id → assistant text → function_call `call_replay`/echo → matching function_call_output → steer user，tools含echo定义；且`store == false`；
- Anthropic recursive request denylist（context-aware traversal）：同样只在adapter-emitted schema-root成员（tool definition `input_schema`、Structured `output_config.format.schema`）进入schema context；system blocks、message/content blocks与tool definitions递归无provider-owned `cache_control` annotation（user-authored schema property names保持为data），Structured fixture schema的`cache_control`/`conversation` user properties在captured provider schema中原样保留；replay shape经Anthropic-truthful helper（reasoning带exact text+signature、无provider item id/OpenAI-only artifact）并逐message断言captured wire：initial user → assistant thinking（text+signature）/text/tool_use `call_replay`/echo → user tool_result for `call_replay` → steer user，tools含echo定义；captured request无`anthropic-beta` header，若发送则始终完整system/messages/tools；
- 同一`Arc<ModelCallRequest>`经ModelGateway调用两次（mutable credential source）各恰好一次POST，两次body bytes逐字节相同（始终full request），auth header随两次解析到的credential变化；
- provider-reported cache read/write token只作为usage evidence，Anthropic `provider_total_tokens`恒为`None`。

未来激活门槛（未实现、不预留抽象）：

- 独立provider-specific evidence/ADR、稳定credential binding与显式tenant/session privacy scope、canonical full-wire successor proof、retention/billing policy、与one-POST语义的reconciliation；
- 激活后仍需证明：deterministic cache annotation不改变content、cache miss/full request equivalence、cache token usage、strict prefix continuation success、Steer/Cancel/Compaction/model/tool/output change使continuation失效、process restart不恢复continuation、provider拒绝continuation的处理（只能作为later distinct logical request之前规划，不能是同一operation内第二次POST）、concurrent continuation candidate race不发送错误delta。

### Multi-Session And Performance

- shared ModelGateway并发执行多个Session的provider request；
- 不存在Gateway-local model permit wait或admission fairness语义；
- one Session cancellation不影响另一个；
- 不存在route/principal cooldown cache；每个并发Session的限流都以terminal `RateLimited`返回并各自裁决；
- slow progress observer不阻塞stream；
- adapter-owned reqwest client按installation共享（普通stateless transport pooling，无auth-principal隔离键）；
- slow credential resolution/request前auth refresh不持有Gateway-wide长guard；
- 两个Session同时调用同一provider/model时都可以进入各自attempt。

### Real Adapter Integration

M12 gate evidence：

- [x] Rig OpenAI Responses loopback server；
- [x] Rig Anthropic Messages loopback server；
- [x] SSE fragmented frames、protocol terminal、early EOF与stream error；
- [x] malformed provider payload、400/401/429/500/529 status/body与single-request；
- [x] custom base URL；
- [x] tool/reasoning/usage/identity/cache-control round-trip；
- [x] cancellation、stream drop与connection close；
- [x] response metadata allowlist和canary rejection；
- [x] delivery/error fixture与queued Steer retry rule。

MVP首版只选择HTTP Responses/Messages streaming，不启用WebSocket或transport fallback。M14 OpenAI与Anthropic adapters已各自消费对应contract suite并实现owner-scoped cancellation、provider-native Structured mapping、private error conversion与exact provider-reported model binding；两者不共享terminal/parser。dynamic credential/catalog installation已接入public host config；`tests/m14_live_provider_smoke.rs`提供两个默认ignored的完整public Runtime-path harness，仍需在显式release环境使用真实credential执行并记录结果。

## Source Plan

当前实现（M14）文件布局：

```text
src/model_gateway.rs
src/model_gateway/openai_responses.rs
src/model_gateway/anthropic_messages.rs
src/model_gateway/provider_installation.rs
src/model_gateway/provider_transport.rs
```

`model_gateway.rs`拥有Gateway core、private `ProviderAdapter`/`ModelCallRequest`/`ProviderAttempt*`类型、`ProviderCatalog`与crate-private test helpers；`ScriptedProviderAdapter`是test-only实现，定义在`model_gateway.rs`内部供vertical-slice tests使用，不进入production路径。两个production adapter各自拥有protocol wire mapping（`openai_responses.rs`、`anthropic_messages.rs`），`provider_installation.rs`实现host-only dynamic credential/catalog installation（`ModelProviderConfig`、`CredentialSource`接线），`provider_transport.rs`保存两个adapter共享的protocol-neutral transport/framing。这是一个module及其private implementation文件，不建立provider crate hierarchy，除非真实build time或dependency isolation证明需要拆分。M14不新增`cache.rs`/`continuation.rs`：显式cache annotation与continuation保持omission（ADR 0141），不预留实现文件。

## Rejected Designs

### ActiveTurnTask直接调用provider或SDK

否决原因：provider type、auth、base URL、retry、usage和error会扩散到Turn execution。

### ModelGateway重新组装Prompt

否决原因：产生第二个model-visible context seam，绕过PromptSet与sanitized LiveConversation。

### 公开PreparedModelTurn

否决原因：增加stale plan和dropped prepared lifecycle，不能提高caller leverage。

### 公开ModelTurnSession

否决原因：让ActiveTurnTask管理第二个provider-session state owner；adapter-owned reqwest client保持private（每installation一个、reload复用，普通stateless transport pooling），continuation保持omission——M14不存在可private复用的continuation state，旧“connection/continuation可以private复用”的说法不成立。

### 返回raw provider stream

否决原因：caller必须理解provider event、terminal EOF、retry和partial draft；error/usage normalization失去locality。

### active Turn自动跨模型fallback

否决原因：破坏exact Model pin，改变capabilities、limits、cost和语义。

### 把provider attempt持久化为ModelAttempt entity

否决原因：attempt是transport/execution detail，不是领域事实；会扩大storage和recovery surface。

### 把partial stream写SessionRecorder

否决原因：一个finalized assistant response应对应一个record candidate；partial draft不可恢复且会制造重复usage和content ordering问题。

### 根据model name猜capability

否决原因：custom provider和alias会使行为不确定；capability必须来自validated definition。

### 使用response_id作为conversation truth

否决原因：provider state不可替代完整sanitized live conversation；continuation必须证明full logical equivalence。

## 完成检查

- [x] 定义ModelGateway ownership和deep interface。
- [x] 定义ModelSelection、ModelDefinitionRef和TurnModelSnapshot。
- [x] 定义capabilities、effective limits和role mapping。
- [x] 定义AssembledModelContext到provider request的职责。
- [x] 定义ToolSpec、tool choice和OutputContract mapping。
- [x] 定义stream progress和finalized response。
- [x] 定义usage、finish reason和provider metadata。
- [x] 定义Response Validation、Structured约束和四个non-retryable response error reason（ADR 0120）。
- [x] 定义single provider attempt与Session logical retry边界，MVP禁用Gateway transparent retry和transport fallback。
- [x] 禁止active Turn cross-model fallback。
- [x] 定义cancellation、auth、redaction和custom provider规则。
- [x] 定义cache、connection reuse和continuation等价性规则。
- [x] 定义multi-session直接并发和provider rate-limit governance（ADR 0125）。
- [x] 定义persistence、recovery、performance和test matrix。
- [x] 实现M6.2 ScriptedProviderAdapter text-only AgentRun vertical slice，包括exact request identity、ordered progress、terminal/error validation、cancellation与reload-retained adapter。
- [x] 随M8/M10扩展ScriptedProviderAdapter覆盖允许ToolCall与CompactionSummary vertical slices。
- [x] 执行Rig 0.40.0 ModelGateway reality gate，在production adapter冻结前完成OpenAI Responses/Anthropic Messages loopback、stream、terminal、metadata和delivery/error证据（ADR 0138）。
- [x] 实现ModelGateway model resolution、immutable request/proof与single-attempt scripted core。
- [x] 实现crate-private Structured foundation：`OutputContract::Structured` exact-model contract constructor（capability/`max_schema_bytes` cap、name 1..64、schema v1 subset）、`ModelCallRequest` exact-model/OutputContract proof复验、terminal exact JSON object parse与本地schema validation、`Refused` bypass及`UnexpectedToolCall`/`IncompleteResponse`/`InvalidStructuredOutput` precedence，ScriptedProviderAdapter端到端conformance。
- [x] 实现OpenAI Responses direct private adapter、provider-native Structured strict mapping、bounded SSE terminal/delivery/error/cancellation与默认离线production loopback suite；transport由真实Rust 1.85验证。
- [x] 实现host-only dynamic credential/catalog installation：redacted typed credential/source、explicit model descriptor、stable/API model identity分离、endpoint policy、Runtime source installation、missing/cancel NotSent与exact terminal model binding。
- [x] 实现两个explicit opt-in live smoke harness；默认tests只编译并报告ignored，不读取env或访问network。
- [ ] 在显式release环境执行并记录real-credential live smoke；激活public structured requester/Wire/SessionDefinition字段与Runtime/ActiveTurnTask structured调用（显式cache/continuation保持ADR 0141 omission，见下节）。
- [x] 完成OpenAI Responses与Anthropic Messages M12 mock-server contract tests，以及M14两个direct production adapter/local contract suites。
- [x] 在阶段9冻结公开model catalog/query协议。

## M14 Stateless Full-Request Policy（ADR 0141）

- [x] 冻结：每次`generate_model_turn`至多调用一次`ProviderAdapter::execute`（owner validation/pre-send cancellation/`AuthMissing`→零execute/零POST；adapter编码/build失败或adapter级pre-send cancellation→一次execute/零POST），独立地发送零或一个HTTP POST；若发送POST则携带完整full request；无optimization-specific fallback POST、重试或continuation state；
- [x] OpenAI：保留`store=false`；无`previous_response_id`/`prompt_cache_key`/`prompt_cache_retention`/`cache_control`/`conversation`/incremental-input；cached_tokens只作为usage evidence；
- [x] Anthropic：递归无provider-owned `cache_control` annotation（user-authored schema property names保持为data）、无`anthropic-beta` header、若发送则始终完整system/messages/tools；cache read/write只作为usage evidence；
- [x] `ModelUsage.provider_total_tokens`为provider-reported only；Anthropic恒为`None`（无derived sum、无overflow error路径、usage finalization infallible）；
- [x] credential逐attempt动态解析（不memoize/pin到request），one installed source=一个稳定nonsecret binding scope，token可轮换、binding identity切换是配置变更；
- [x] 文档与ADR 0106/0119/0123/0125/0138/0139 refined：旧“provider rejects continuation→fallback full request”不能是第二次POST；
- [ ] 未来激活显式cache/continuation必须另立provider-specific evidence/ADR并满足本表门槛（当前M14保持omission，不是pending实现）；
- [ ] 在显式release环境执行并记录real-credential live smoke；激活public structured requester/Wire/SessionDefinition字段与Runtime/ActiveTurnTask structured调用。
