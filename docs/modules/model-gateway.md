# ModelGateway架构设计

日期：2026-07-25

状态：当前权威架构（设计已冻结，生产实现待启动）

## 目的

本文定义MiniCore的provider-neutral模型调用模块，回答：

- SessionExecutor如何从`AssembledModelContext`发起一次模型调用；
- Model identity、definition revision、capabilities和effective limits如何在Turn内固定；
- System、User、Assistant和Tool role如何映射到不同provider；
- ToolSpec、tool choice、OutputContract、reasoning和attachments如何编码；
- streaming delta、finalized response、usage、finish reason和provider metadata如何规范化；
- 单次provider attempt与Session logical retry如何区分；
- cancellation、authentication、secret redaction、跨Session并发调用和provider rate limit反馈如何治理；
- provider prompt cache、connection reuse和continuation如何保持Transcript-First等价性；
- Rig 0.40.0可以为provider协议映射与单次attempt调用复用哪些能力，以及这些能力如何被限制在private `ProviderAdapter`内。

本文不定义：

- Prompt内容、conversation visibility或`MessageRecord → ModelMessage`转换；
- Session logical retry、compaction orchestration或Turn terminal规则；本文只摘要Gateway terminal error与该policy的边界，具体规则以Session Execution和ADR 0119为权威；
- Tool execution、approval或ToolResult持久化；
- Runtime公开model catalog/query/event协议；其safe view以[Runtime Interface](runtime-interface.md)为权威；
- provider-native compaction artifact的持久化格式；
- 完整pricing、billing ledger或成本审计。

相关权威文档：

- [Prompt子系统架构设计](prompt.md)
- [Turn执行模块与执行上下文架构设计](turn-execution-context.md)
- [Session Execution架构设计](session-execution.md)
- [Conversation与SessionStorage架构设计](conversation-storage.md)
- [Runtime Interface与公开协议架构设计](runtime-interface.md)
- [ADR 0104：SessionStorage是durable truth](../adr/0104-session-storage-is-durable-truth.md)
- [ADR 0106：ModelGateway使用一个深异步调用interface](../adr/0106-model-gateway-is-single-deep-operation.md)
- [ADR 0119：模型调用使用Session逻辑重试](../adr/0119-model-calls-use-session-logical-retries.md)
- [ADR 0120：失败由事实拥有模块分类，恢复由执行拥有者决定](../adr/0120-failures-stay-with-owning-modules.md)
- [ADR 0125：ModelGateway不设置本地模型调用Permit](../adr/0125-model-gateway-has-no-local-call-permits.md)

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
- MVP中每次`generate_model_turn`只执行一个provider attempt，Rig和底层provider SDK automatic retry固定为0；
- ModelGateway不执行transparent retry、401 refresh-and-resend或WebSocket → HTTP transport fallback；
- SessionExecutor只对同一个immutable request执行最多3次AgentRun logical retry；CompactionSummary最多1次；
- cross-model fallback不是ModelGateway transparent behavior；
- streaming progress是process-local observer data，不进入SessionStorage；
- finalized response或typed error是一次gateway调用唯一terminal result；
- provider-reported usage随成功assistant response保存；失败attempt usage在MVP中只属于ModelGateway internal telemetry；
- authentication secret、raw headers、raw request/response body和provider SDK类型不越过ModelGateway seam；
- prompt cache、connection reuse、`previous_response_id`和incremental request只是wire optimization；
- 所有optimization必须能退回完整`AssembledModelContext`请求；
- ProviderAdapter是private internal seam，首批实现为RigProviderAdapter和ScriptedProviderAdapter；
- RigProviderAdapter只负责具体provider的request/stream/response/error映射与单次attempt执行；model resolution、request validation、auth policy与credential resolution、progress lifecycle、cache/continuation policy和terminal result归一化均由ModelGateway拥有；
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

结论：Rig适合作为private provider adapter实现，不适合作为MiniCore ModelGateway interface或domain type来源。

## 对比结论

| 维度 | pi | Codex | Grok Build | Rig 0.40.0 | MiniCore决策 |
| --- | --- | --- | --- | --- | --- |
| model invocation seam | AgentLoop调用pi-ai stream | ModelClientSession stream | 独立sampler crate | CompletionModel stream | 一个`generate_model_turn` |
| provider state | Model/registry + provider module | thread/turn client state | sampler内部 | provider client/model | Gateway内部connection pool |
| Prompt assembly | AgentSession/AgentLoop context | Turn构造Responses input | chat state/sampling types | CompletionRequest | 只接受PromptSet输出 |
| streaming | typed content events | typed Responses events | ACP/update stream | typed assistant content stream | droppable progress + one terminal result |
| retry owner | AgentSession为主 | client/turn loop协作 | sampler retry | caller决定 | Gateway single attempt，SessionExecutor logical retry |
| transport fallback | provider-specific | WebSocket → HTTP | backend显式配置 | adapter-specific | MVP不启用 |
| cross-model fallback | model selection层 | 不作为transparent stream fallback | 未确认 | 无统一语义 | active Turn内禁止 |
| continuation/cache | provider compat/cache flags | strict prefix + previous response | 未确认 | provider additional params | strict equivalence，full request fallback |
| usage | assistant response usage | terminal response usage | telemetry/model usage | generic Usage | 成功response usage durable |
| error taxonomy | helper + string patterns | CodexErr mapping | typed sampler errors | CompletionError | MiniCore typed recovery classes |
| auth | registry + AuthStorage | AuthManager + refresh | API key/OAuth/OIDC | provider client setup | Gateway-private AuthStore |

## Interface方案比较

### 方案A：一个深异步operation

```rust
ModelGateway::generate_model_turn(request, progress, cancel)
    -> Result<ModelCallResult, ModelCallError>
```

优点：

- caller只理解完整request、progress和terminal result；
- provider attempt、connection、auth、delivery-state mapping、cache和continuation全部具有locality；
- 与SessionExecutor的`RunningOperation`自然对齐；
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

- SessionExecutor需要管理第二个turn-scoped lifecycle；
- caller必须理解stream terminal frame、drop、poisoned continuation和session reset；
- provider session容易被误认为Turn truth或recovery source；
- 若把provider session暴露给caller，logical retry和Steer后的segment rebuild会迫使SessionExecutor管理第二套session lifecycle；
- connection/continuation完全可以留在Gateway内部并用full request重新证明兼容性。

结论：不作为外部interface。Codex式WebSocket session可以作为private adapter优化。

### 选择

采用方案A。

ModelGateway是一个深模块：对外只有Turn model resolution和一次完整模型调用两个操作；provider variation通过private adapters处理。

## Ownership

```text
MiniCoreRuntime
└─ Arc<ModelGateway>
   ├─ ProviderCatalog
   ├─ AuthStore
   ├─ private ProviderAdapter
   ├─ ProviderConnectionPool
   ├─ ContinuationCache
   ├─ ProviderRateLimitState
   └─ RedactionPolicy

SessionExecutor
├─ RunningOperation::GenerateModelResponse
│  ├─ immutable ModelCallRequest + logical_retry_count
│  └─ scoped ProgressEventPublisher<ModelProgressEvent>
├─ RunningOperation::WaitForModelRetry
│  └─ same ModelCallRequest + ready_at + typed resume basis
└─ RunningOperation::CompactConversation
   └─ immutable CompactionSummary ModelCallRequest + logical_retry_count
```

规则：

- ProviderCatalog、AuthStore和ProviderAdapter是ModelGateway implementation details；
- Runtime公开model query以后通过MiniCoreRuntime facade取得safe catalog view；
- SessionExecutor不直接resolve auth、base URL、API model name或provider protocol；
- AgentLoop不直接调用ModelGateway；
- ToolService、PromptService和SkillService不调用ModelGateway；
- ModelGateway不读SessionStorage，不append entry，不发布Runtime durable event；
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

`generate_model_turn`最多执行一个完整provider attempt。调用期间可以等待request前credential resolve/refresh和stream，但不经过Gateway本地模型调用permit；它运行在`RunningOperation`中，不阻塞SessionIngress scheduler或control loop。provider terminal error返回SessionExecutor后，本次Gateway operation结束；Gateway不内联retry timer。

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

它不覆盖credential内容、OAuth access token或resolved auth principal。`ModelExecutionRef`持有private nonsecret AuthBindingRef；credential和opaque principal identity在每个attempt即时解析。

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
- process restart不恢复unfinished Turn，因此不承诺跨进程继续旧Snapshot；
- Input UserMessage内联的`StoredTurnStart`只保存safe provider/model descriptor和generation settings，不保存`ModelDefinitionVersion`或execution_ref。

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
    pub reasoning: EffectiveReasoningPolicy,
    pub sampling: SamplingPolicy,
    pub prompt_cache: PromptCachePolicy,
    pub service_class: ModelServiceClass,
}

pub enum PromptCachePolicy {
    Auto,
    Disabled,
}

pub enum ModelServiceClass {
    Standard,
    Priority,
}
```

`EffectiveReasoningPolicy`是requested preference经过model capability映射后的provider-neutral结果；unsupported preference在resolve_for_turn时失败或按explicit Session policy降级，不能在provider adapter中临时猜测。`SamplingPolicy`只保存validated temperature/top-p等stable values；NaN、Infinity或provider不支持的组合在Turn capture时拒绝。

cache和service class属于Turn-pinned execution policy。它们不改变conversation visibility，但会影响provider request和cost。

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
pub struct AssembledModelContext {
    pub system: Arc<[PromptSection]>,
    pub messages: Arc<[ModelMessage]>,
    pub tools: Arc<[ToolSpec]>,
    pub output_contract: Option<OutputContract>,
    pub contribution_stamps: Arc<[PromptContributionStamp]>,
    pub diagnostics: Arc<[PromptDiagnostic]>,
    pub(crate) assembly_proof: PromptAssemblyProof,
}

```

PromptSet System sections、前置User context和ToolSpec在active Turn内天然稳定；Gateway可以利用canonical section/message/tool boundaries选择cache breakpoint，不需要额外stability flag。

`PromptAssemblyProof`是PromptSet生成的crate-private consistency proof，绑定ModelCallPurpose、exact TurnModelRef、OutputContract结构值和optional CompactionSummaryBudget proof。它不提供第二个caller-controlled purpose；ModelCallRequest constructor必须校验proof与request一致。

首版provider-neutral output contract至少包含：

```rust
pub enum OutputContract {
    NoToolCalls,
    Structured(StructuredOutputContract),
}
```

`NoToolCalls`不是普通prompt text。请求中的`tools`必须为空；provider支持显式tool-choice/allowed-tools禁用时adapter同时设置该字段。若provider允许在未声明Tool时仍返回可执行ToolCall，Gateway必须返回`UnexpectedToolCall`，不能把它交给SessionExecutor执行。

`Structured`在MVP中同样要求`tools`为空。provider-native strict schema只是第一层约束；Gateway仍必须对terminal text执行exact JSON parse和本地schema validation。MVP不做JSON repair、type coercion或Markdown code-fence extraction。需要Tool的AgentRun先完成普通ToolRound，之后再使用新的Structured `ModelCallRequest`取得terminal structured response。

ModelGateway可以：

- 把System sections映射到provider原生system/instructions字段；
- 把ModelMessage编码成provider message/item；
- 把ToolSpec编码成provider tool schema；
- 把OutputContract编码成tool choice、response format或JSON schema；
- 对provider schema限制执行不改变业务含义的canonical sanitization；
- 在canonical section边界添加cache-control metadata。

ModelGateway不能：

- 删除、增加或重新排序model-visible content；
- 从SessionStorage重新加载messages；
- 根据provider限制自行摘要或截断conversation；
- 把diagnostic变成prompt text；
- 把ToolSpec描述与ToolSet executor重新join；
- 把OutputContract伪装为system text；
- 把未committed draft加入payload。

无法无损映射时返回`UnsupportedCapability`或`InvalidRequest`，不能best-effort改变语义。

## ModelCallRequest

```rust
pub struct ModelCallRequest {
    model: Arc<TurnModelSnapshot>,
    purpose: ModelCallPurpose,
    input: AssembledModelContext,
    max_output_tokens: Option<NonZeroU32>,
}

impl ModelCallRequest {
    pub(crate) fn new(
        model: Arc<TurnModelSnapshot>,
        purpose: ModelCallPurpose,
        input: AssembledModelContext,
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
- SessionId、TurnId、execution_version和OperationType由外层RunningOperation携带；
- request不含ModelCallId、RunId、ModelStepId或ModelAttemptId；
- request不含credential、auth reference、base URL、header或raw provider params。

constructor验证：

- assembly_proof.purpose等于request purpose；
- `assembly_proof.turn_model`等于TurnModelSnapshot的exact `TurnModelRef`；
- assembly_proof.output_contract等于input.output_contract结构值；
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
- 不进入AgentLoop，不把summary result解释成普通assistant response；
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

- 把private `ProviderAttemptRequest`编码成具体provider/Rig typed request；
- 使用已经解析的model、endpoint和credential执行一次provider request；
- 桥接provider stream与attempt cancellation；
- 把provider content、finish reason、usage、metadata和SDK error映射为`ProviderAttemptResult`或`ProviderAttemptError`。

ProviderAdapter不能：

- 选择或替换provider/model，解析catalog current value或改变Turn-pinned model identity；
- 重新组装Prompt、判断conversation visibility或执行Tool；
- 决定logical retry、执行transport fallback、cache/continuation policy或terminal Turn结果；
- 发布ModelGateway attempt lifecycle、构造最终`ModelCallResult`或把Rig raw type泄漏给caller。

首个production实现：

```text
RigProviderAdapter
```

首个测试实现使用：

```text
ScriptedProviderAdapter
```

它按脚本返回stream delta、terminal response或typed error，并记录收到的private request，供SessionExecutor、ModelGateway和Compaction共享vertical-slice tests使用。测试仍必须经过`ModelCallRequest::new → ModelGateway.generate_model_turn → ProviderAdapter`，不能让SessionExecutor或Compaction直接调用fake model interface。只有出现需要纯函数映射/property test的真实场景时再增加`DeterministicProviderAdapter`。

因此ProviderAdapter是一个真实seam，而不是为单一implementation增加的假抽象。

ProviderAttemptRequest可以包含：

- exact provider protocol；
- resolved API model name；
- resolved auth；
- provider-specific typed options；
- canonical encoded payload；
- connection/continuation candidate；
- single-attempt request correlation。

这些类型都必须private、non-serializable并使用redacted Debug。

只有private adapter可以使用Rig provider types和`CompletionRequest.additional_params`。调用方不能提交arbitrary JSON。

## Rig Adapter Mapping

```text
ModelCallRequest
→ ModelGateway validate / resolve / plan attempt
→ private ProviderAttemptRequest
→ RigProviderAdapter encode并执行一个provider attempt
→ ProviderAttemptResult或ProviderAttemptError
→ ModelGateway terminal归一化
→ FinalizedAssistantResponse或ModelCallError
```

RigProviderAdapter不被限制为generic `CompletionModel::stream`。当generic类型擦除finish reason、request ID或reasoning artifact时，adapter可以使用Rig公开的provider-specific request/response types；这些类型仍不能越过private adapter。

RigProviderAdapter是provider attempt adapter，不是ModelGateway implementation的替代品。它不拥有provider选择、logical retry、cache/continuation判定或最终错误分类。Rig和provider SDK的内建retry必须显式配置为0；spike无法证明这一点时不得冻结production adapter。

映射要求：

- ordered System sections映射到Rig preamble或provider-specific system/instructions字段；
- messages保持source order；
- reasoning/text/tool call保持final content order；
- ToolSpec映射到Rig ToolDefinition；
- OutputContract映射到ToolChoice/output_schema/provider typed options；
- Rig `additional_params`只由adapter内部typed builder生成；
- Rig Usage规范化到MiniCore ModelUsage；
- Rig AbortHandle桥接MiniCore CancellationToken；
- generic finish reason缺失时读取受支持provider-specific terminal response；仍不可得则使用`Unknown`；optional request ID/metadata不可得时保持None；
- Rig CompletionError只在adapter内存在，必须转换成ProviderAttemptError。

如果Rig generic和provider-specific API都无法保留MiniCore要求的semantic content、tool identity或usage，spike必须阻止该provider adapter并提出targeted follow-up ADR；不能把Rig raw types泄漏给caller。直接重写provider HTTP client不属于当前accepted baseline。

Rig spike必须验证：

- OpenAI Responses和Anthropic Messages的role映射；
- reasoning text/summary/encrypted/signature round-trip；
- tool call index/name/arguments和content order；
- stream EOF、cancel和usage terminal行为；
- custom base URL；
- typed cache-control；
- finish reason提取；
- provider request/response ID提取；
- context overflow与rate-limit分类。

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
- publisher由RunningOperation scope到SessionId/TurnId；SessionExecutor创建的AgentRun adapter使用`content_index`维护`StreamingItem`并分配MiniCore ItemId；
- ModelGateway只发布provider-neutral content index和delta，不创建ItemId，也不接收SessionId或TurnId；Session logical retry由SessionExecutor发布`model_retry_scheduled`；
- 连续text/reasoning delta可以合并；
- Host progress queue满可以丢弃中间delta，但不能跳过operation-local累积；
- Host progress sink关闭不取消provider调用或operation-local累积；
- cancellation由CancellationToken决定；
- partial reasoning/text/tool arguments不是durable Item；
- hidden chain-of-thought不可发布；
- provider未暴露的reasoning不可推断；
- terminal success必须返回完整FinalizedAssistantResponse；
- terminal error必须返回typed ModelCallError；
- 对AgentRun，SessionExecutor把finalized text/reasoning content转为同ItemId的FinalItemCandidate；ToolCall走assistant entry append/apply后的Started ToolInvocation projection，只有正式Item append/apply后才发布对应StateEvent；CompactionSummary结果不创建Item；
- SessionExecutor在terminal error后丢弃StreamingItem；若启动logical retry则发布`model_retry_scheduled`清理Host临时view，Turn terminal或新Snapshot提供最终校正。

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
    pub model: ModelDefinitionRef,
    pub response_id: Option<ProviderResponseId>,
    pub content: Arc<[FinalizedAssistantContent]>,
    pub finish_reason: ModelFinishReason,
    pub usage: Option<ModelUsage>,
    pub metadata: ProviderResponseMetadata,
}
```

```rust
pub enum FinalizedAssistantContent {
    Reasoning(FinalizedReasoning),
    Text {
        text: Arc<str>,
    },
    ToolCall {
        provider_item_id: Option<ProviderItemId>,
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: Value,
        index: u32,
    },
}
```

`content[]`保持provider finalized semantic order。`ModelProgressEvent::ContentDelta.content_index`是该terminal `content[]`的zero-based位置，不是ToolCall内部`index`；adapter必须保证stream与terminal normalization使用同一位置语义，无法关联时返回`InvalidProviderResponse`。SessionExecutor随后分配或关联ItemId并构造assistant entry。

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

可解析为JSON `Value`但不符合matching `ToolSpec` schema的arguments不是Response Validation error。该ToolCall在允许Tool的调用中保持valid model action，随后由ToolSet preflight形成failed ToolResult；Gateway不得把它改写为`InvalidProviderResponse`或`InvalidStructuredOutput`。

规范决策表如下；若同时命中多行，按上述validation顺序选择更早的error：

| ToolCall permission | Content | Finish reason | Result |
| --- | --- | --- | --- |
| forbidden | any ToolCall | any | UnexpectedToolCall |
| allowed | ToolCall | ToolCalls / Unknown | valid response → NeedTools |
| allowed | ToolCall | Length / ContentFiltered | IncompleteResponse |
| allowed | ToolCall | Stop / Refused | InvalidProviderResponse |
| any | no ToolCall | ToolCalls | InvalidProviderResponse |
| any | no ToolCall | Length / ContentFiltered | IncompleteResponse |
| any | no ToolCall + non-empty refusal text | Refused | valid refusal response → Finished |
| any | no ToolCall + empty refusal | Refused | InvalidProviderResponse |
| any | no user-visible Text | Stop / Unknown | IncompleteResponse |
| any | user-visible Text | Stop / Unknown | validate Structured when requested → Finished |

通过validation后，ToolCall presence才是AgentLoop路由到`NeedTools`的主要事实；无ToolCall response路由到candidate `Finished`。ModelGateway不执行Tool，也不决定Turn terminal。

### FinalizedReasoning

```rust
pub struct FinalizedReasoning {
    pub text: Option<Arc<str>>,
    pub summary: Option<Arc<str>>,
    pub encrypted: Option<Arc<str>>,
    pub signature: Option<Arc<str>>,
    pub provider_item_id: Option<ProviderItemId>,
}
```

只保存provider实际返回且允许replay/display的artifact。不得：

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
- generic Rig response没有finish reason时使用provider-specific adapter提取；
- 仍不可得时使用Unknown，不根据文本内容伪造；
- ToolCall content是通过Response Validation后AgentLoop决定NeedTools的主要事实；finish reason在Gateway中用于一致性校验和diagnostics；
- Length和ContentFiltered返回`IncompleteResponse`，不进入AgentLoop；
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

规则：

- provider未返回的字段保持`None`；
- `provider_total_tokens`只保存provider报告值，不通过其他字段相加伪造；
- locally estimated tokens属于ContextUsage projection，不进入ModelUsage；
- locally calculated price属于rebuildable cost estimate，不写`reported_cost`；
- `reported_cost`只用于provider明确返回billed cost的情况；
- successful response usage随对应assistant entry保存；
- Session total usage由assistant/compaction entries replay重建；
- 每次Gateway operation最多一个provider attempt；usage是否出现不能代替delivery-state分类，也不能把不确定outcome降级成普通transient error；
- provider若为失败attempt报告usage，该事实只进入ModelGateway internal telemetry；不放入ModelCallError、不进入Session aggregate，也不创建synthetic assistant或独立Usage entry；
- 因此MiniCore SessionStorage不是provider billing ledger。

StoredAssistantMessage的`retry_count`定义为SessionExecutor对同一logical call执行的logical retry数量。Gateway没有transparent retry count。

## Retry

### Single Provider Attempt

MVP中一次`generate_model_turn`最多执行一个provider attempt：

```text
validate request
→ resolve/refresh credential before request
→ preflight/cooldown typed terminal error：0次ProviderAdapter.execute
  或
→ ProviderAdapter.execute exactly once
→ normalize one terminal result or typed error
```

Rig和底层provider SDK automatic retry必须配置为0。ModelGateway不执行DNS/connect/TLS retry、429/5xx retry、401 refresh-and-resend、stream restart或WebSocket → HTTP fallback。失败后本次Gateway operation立即terminal，由SessionExecutor根据typed error决定是否logical retry。

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

HTTP status或“尚无delta”本身不能决定delivery state。adapter必须根据protocol contract、provider response和transport阶段准确映射：已发送但outcome不确定时返回`RequestOutcomeUnknown`，已发布partial output后失败返回`StreamInterrupted`。该proof决定SessionExecutor能否安全logical retry，不能因为Gateway不内联retry而省略。

只有`NotSent`和provider明确证明未开始执行的`RejectedBeforeExecution`可以映射为默认retryable reason。`AcceptedNoOutput`若没有更强的provider terminal proof必须映射`RequestOutcomeUnknown`；不能把“还没收到delta”当作未执行证明。

每次attempt：

1. 检查cancellation；
2. resolve current credential和opaque auth principal identity；
3. build provider payload；
4. start provider request并更新delivery state；
5. consume stream；
6. normalize terminal result。

request前credential可以resolve/refresh。provider返回401时terminal为`AuthRejected`，不在本次Gateway operation中refresh-and-resend。

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
- SessionExecutor只在provider hint不超过60秒时按logical retry policy等待；
- rate-limit state按provider route和auth principal的opaque identity隔离；
- active cooldown只对对应route/principal快速返回`RateLimited { retry_after }`，Gateway本身不sleep，也不影响无关provider。

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

### Session Logical Retry

Gateway single attempt返回typed error后，SessionExecutor决定logical retry：

```text
current Turn still Running and TurnId matches
execution_version unchanged
ConversationCheckpoint.entry_id unchanged
current operation/control basis unchanged
same `Arc<ModelCallRequest>` is reused; no reassembly
→ schedule timer
→ 确认旧RunningOperation已经terminal并从SessionExecutor移除
→ 使用同一ModelCallRequest启动新的唯一current RunningOperation
```

logical retry：

- 不阻塞SessionIngress scheduler或control loop；
- Steer、Cancel、成功Compaction Replace或conversation change使其失效；
- 计入StoredAssistantMessage.retry_count；
- 不创建ModelAttempt entity；
- 不改变ModelCallPurpose；
- 不与旧本地Model future重叠；provider端可能继续工作或计费不等于旧future仍可回传SessionExecutor。

默认policy：

| Purpose | 最大logical retries | Backoff |
| --- | ---: | --- |
| AgentRun | 3 | 2s、4s、8s |
| CompactionSummary | 1 | 2s |

自动retry必须同时满足delivery proof为`NotSent`或`RejectedBeforeExecution`，且reason是`Timeout`、`TransportUnavailable`、`ProviderUnavailable`，或typed `Retry-After <= 60s`的`RateLimited`。`AcceptedNoOutput`没有明确pre-execution rejection proof时归一化为`RequestOutcomeUnknown`。`RequestOutcomeUnknown`、`StreamInterrupted`、认证、quota、配置、安全、`UnexpectedToolCall`、`InvalidStructuredOutput`、`InvalidProviderResponse`和`IncompleteResponse`默认不自动retry；`ContextOverflow`进入Compaction。完整决策见[ADR 0119](../adr/0119-model-calls-use-session-logical-retries.md)和[ADR 0120](../adr/0120-failures-stay-with-owning-modules.md)。

## Cancellation

ModelGateway在以下位置检查CancellationToken：

- resolution validation后；
- concurrency wait期间；
- auth resolution和request前refresh前后；
- provider request开始前；
- stream read期间，直到provider terminal event被adapter接受。

Gateway success/cancel的线性化点是adapter接受完整provider terminal event：

- cancellation先被观察：中止request并返回Cancelled；
- terminal event先被接受：Gateway完成normalization并返回ModelCallResult，之后到达的cancel不把该result改写为Cancelled；
- SessionExecutor仍会用execution_version和Cancel/SecurityRevoked arbitration决定该result能否append；普通Steer不拒绝已完成result，只决定无ToolCall response保存为Continue还是Final，因此Gateway terminal先赢不等于Turn一定Completed；
- 该规则保证terminal usage、finish reason和content不会因normalization期间的timer race随机丢失。

取消行为：

```text
cancel token fires
→ 取消concurrency wait或当前single attempt
→ 调用Rig stream.cancel或transport abort
→ 停止发布新progress
→ 返回ModelCallErrorReason::Cancelled
```

不能承诺：

- provider没有收到request；
- provider立即停止generation；
- provider不会计费；
- 已发布partial delta可以恢复；
- cancellation可以回滚provider-side cache。

SessionExecutor不detach Model future；`generate_model_turn` terminal返回或future被安全drop并关闭结果路径后，才能开始下一次logical operation。因此正常路径不存在旧Model result在新operation之后迟到。SessionId、TurnId、execution_version和OperationType仍用于验证result basis与实现错误。

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
- `Timeout`、`TransportUnavailable`和`ProviderUnavailable`只有在delivery proof为`NotSent`或`RejectedBeforeExecution`时才能保持该retryable reason；`AcceptedNoOutput`没有明确pre-execution rejection proof时、以及其他outcome unknown都必须映射`RequestOutcomeUnknown`，已有semantic delta必须映射`StreamInterrupted`；
- 上述四个response error发生在request已经离开`NotSent | RejectedBeforeExecution`安全状态之后，或在completed response validation期间，不满足ADR 0119的safe-delivery retry前提；SessionExecutor不得把它们重新解释为transient failure。

## Authentication And Secret Redaction

ModelGateway implementation包含：

```text
ProviderCatalog
AuthStore
ResolvedAuth
ProviderClientFactory
```

AuthBindingRef固定credential source/account binding，不包含secret。ResolvedAuth另外产生opaque AuthPrincipalIdentity，用于per-principal concurrency、connection/cache隔离和continuation compatibility；该identity不进入TurnModelSnapshot、diagnostics或storage。若同一AuthBindingRef在active Turn期间解析为不同principal，Gateway必须清除旧connection/continuation candidate，不能跨principal复用provider state。

secret只允许存在于：

```text
AuthStore
ModelGateway private ResolvedAuth
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
SessionStorage
Runtime event/snapshot
Prompt/Tool/Skill
logs或Debug output
```

要求：

- ResolvedAuth不实现revealing Debug/Display/Serialize；
- headers和URL query在diagnostics前redact；
- API key、OAuth token、cookie和signed URL使用typed secret wrappers；
- request前auth resolve/refresh使用singleflight，避免并发请求重复refresh；401后不重发；
- custom provider auth reference只能来自user-global trusted config；
- project Workspace不能注册base URL、headers或credential source；
- Runtime hook未来只能看到provider-neutral redacted request summary；
- raw provider payload hook默认不提供。

## Provider Catalog And Custom Providers

custom provider必须显式声明：

```rust
pub enum ProviderProtocol {
    OpenAiResponses,
    OpenAiChatCompletions,
    AnthropicMessages,
    Gemini,
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

规则：

- 不根据endpoint猜protocol；
- endpoint必须HTTPS，localhost/development exception需要显式runtime policy；
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

Gateway可以根据exact model definition和canonical instruction/tool/message boundaries选择provider cache-control：

- stable System sections和前置User context；
- stable ToolSpec集合；
- provider允许的conversation prefix；
- provider-specific cache retention。

规则：

- cache annotation只能添加到canonical content边界；
- 不为了cache重排、删除、复制或改写content；
- cache key不得包含secret或raw user content；
- cache key可以基于opaque runtime salt和private canonical encoding；
- cache miss不改变语义；
- cache eviction直接发送full request；
- cache read/write token只作为usage；
- provider cache key不是PromptSet、Turn或conversation identity；
- cache policy变化不改变conversation truth。

## Connection Reuse And Continuation

ProviderConnectionPool和ContinuationCache都属于ModelGateway implementation。

### Connection Reuse

允许按以下safe identity复用connection：

```text
provider protocol
endpoint identity
auth principal opaque identity
transport configuration
```

不能按SessionId保存credential-bearingclient作为Session state。

### Continuation

`previous_response_id`、incremental input或sticky provider state只在以下条件全部满足时使用：

```text
same provider protocol and endpoint
same ModelDefinitionRef
same auth principal opaque identity
same purpose
same effective generation policy
same tool schemas and output contract
same adapter encoding version
previous call completed successfully
new full logical input可证明为previous full input + previous finalized response + exact committed suffix
```

必须使用完整provider-neutral input和private canonical equivalence proof证明prefix；不能只比较SessionId、TurnId、response_id或message count。

以下情况清除continuation candidate并发送full request：

- process restart；
- Cancel或Steer丢弃previous response；
- previous stream partial/error；
- Compaction Replace projection；
- model definition变化；
- ToolSpec或OutputContract变化；
- capability/encoding version变化；
- prefix proof失败；
- provider拒绝continuation。

ContinuationCache不是durable truth，不进入TurnExecutionContext，不通过SessionStorage恢复。

## Concurrent Calls And Rate Governance

ModelGateway不提供Runtime global、per-provider route、per-model或per-auth-principal模型调用permit，也不建立本地admission queue或公平调度器。该决策由[ADR 0125](../adr/0125-model-gateway-has-no-local-call-permits.md)冻结。

要求：

- 共享`Arc<ModelGateway>`支持多个Session并发执行独立provider attempt；
- 不用Gateway-wide mutex或其他长guard包围credential resolution、provider request或stream读取；
- 每个Session最多一个current model `RunningOperation`由SessionExecutor保证，不形成跨Session容量限制；
- request前auth refresh只对同一credential执行singleflight，不阻塞无关credential的调用；
- provider cooldown只对对应route/principal fast-fail，不在Gateway内等待；
- provider `RateLimited`、`QuotaExceeded`和typed `Retry-After`继续规范化为terminal result，由SessionExecutor裁决logical retry；
- Provider SDK/HTTP connection pool的transport资源管理是private implementation detail，不成为MiniCore admission policy；
- progress publisher阻塞不能占用provider stream读取。

## Complete Call Flow

```text
Turn admission
→ shared publication gate captures Arc<ModelCatalogView>
→ ModelGateway.resolve_for_turn(captured catalog, ModelSelection + preferences)
→ Arc<TurnModelSnapshot>
→ TurnExecutionContext pins snapshot

AgentLoop NeedModel
→ PromptSet.assemble(CommittedConversationView, purpose, output contract)
→ AssembledModelContext
→ SessionExecutor creates ModelCallRequest
→ RunningOperation::GenerateModelResponse
→ ModelGateway.generate_model_turn
   → validate snapshot/request binding
   → choose exact provider adapter and route
   → resolve credential and opaque auth principal
   → encode full AssembledModelContext
   → optional cache/continuation optimization with equivalence proof
   → provider stream
   → publish ModelProgressEvent to operation-local adapter
      → lossless StreamingItem/ItemId update
      → best-effort Host ProgressEvent enqueue
   → normalize ordered finalized content/usage/finish reason/metadata
   → return ModelCallResult
→ OperationResult(SessionId + TurnId + execution_version + OperationType)
→ SessionExecutor validates identity/version and Workspace authorization
→ AgentLoop.accept_model_response
→ NeedTools或Finished
→ SessionExecutor append/apply assistant entry
```

## Failure And Recovery

### Process Restart

不恢复：

```text
provider stream
connection
ContinuationCache
resolved credential
partial draft
single-attempt state
```

unfinished Turn按Session recovery规则终止。下一Turn从SessionStorage durable truth重新assemble full context并建立新provider request。

### Outcome Ambiguity

模型调用不是Tool side effect ledger：

- transport error后provider可能已经生成或计费；
- 未取得terminal finalized response时不能创建synthetic assistant message；
- partial draft不持久化；
- request已发送但尚未开始response时映射为RequestOutcomeUnknown；已产生partial response后映射为StreamInterrupted；
- `RequestOutcomeUnknown`和`StreamInterrupted`默认禁止logical retry；SessionExecutor只能对Gateway已经证明delivery-safe的typed error应用ADR 0119 policy；
- duplicate assistant prevention依赖只有terminal ModelCallResult才能append；重复append的防护靠committed prefix状态（该assistant entry是否已存在）判断，不依赖durable operation key。

### Projection/Storage Failure

ModelGateway成功不等于assistant durable：

```text
ModelCallResult
→ SessionExecutor identity/authorization validation
→ append assistant entry
→ apply projections
```

append失败时：

- 不让AgentLoop继续到Tool execution；
- NotCommitted可以重试同一assistant draft；
- SessionWrite OutcomeUnknown时保守终结、恢复靠committed prefix状态判断，不按operation key解析；
- 不能重新调用provider来“确认”storage结果；
- provider response只在current operation内保留到append outcome确定。

## Persistence Contract

Conversation storage保存safe `StoredModelDescriptor`：实际`ProviderId + ModelId`、必要generation settings和allowlisted provider metadata。它用于历史显示和reasoning artifact解释，不要求old `ModelDefinitionVersion`或retained catalog definition在cold replay时仍可解析；unfinished Turn也不跨restart恢复旧TurnModelSnapshot。

成功assistant entry保存：

```text
StoredModelDescriptor
allowlisted response_id
ordered finalized content[]
normalized ModelUsage
normalized ModelFinishReason
Session logical retry_count
allowlisted provider metadata
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
SessionExecutor `WaitForModelRetry` timer
connection/continuation state
full AssembledModelContext
```

首版automatic SummaryModel compaction把provider/model、usage、finish、logical retry、requested max output和allowlisted provider metadata保存到`StoredCompaction.model_call`，并由StoredCompaction本体保存summary与single `first_kept_entry_id` marker，因此automatic路径该字段必须为Some。`None`只为未来明确设计的deterministic maintenance/import保留，不是automatic overflow fallback。

`retry_count`只表示Session logical retry。Gateway没有transparent retry count。

Provider response ID和reasoning opaque artifact必须有长度限制和redaction validation。

## Performance

目标：

- `resolve_for_turn`只做local catalog lookup和validation；
- full context encoding与provider I/O在RunningOperation执行；
- connection pool和HTTP client跨Session共享；
- 多个Session可并发执行provider request，不经过Gateway本地admission queue；
- progress delta可以合并；
- provider stream reader不等待slow observer；
- request前auth refresh singleflight；
- cache/continuation failure快速退回full request；
- 不因optimization额外复制完整conversation多次。

性能不能牺牲：

- full logical request equivalence；
- exact model pin；
- cancellation responsiveness；
- usage/finish/content order正确性；
- secret redaction；
- SessionExecutor的request响应能力。

## Diagnostics

ModelCallDiagnostic是bounded、redacted、provider-neutral value：

```text
model definition ref
adapter kind
transport kind
timing category
status class
cache hit/miss/unsupported
continuation used/full-request fallback
request/response allowlisted IDs
```

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
- SessionExecutor/AgentLoop不import Rig provider类型；
- ModelGateway不能读取SessionStorage或Prompt definitions；
- ProviderAdapter不能append Session entry；
- shared Gateway并发处理多个Session；
- 没有global current Session/model。

### Model Resolution

- exact provider/model selection；
- missing provider/model；
- catalog重新发布时active Snapshot保持旧definition；
- future Turn取得新definition；
- capability/limit validation；
- reasoning preference映射；
- unknown limits保持None；
- effective TokenEstimateRate进入ModelDefinitionVersion，rate/catalog更新只影响future Turn；
- definition未声明rate时使用保守默认并产生diagnostic；
- 同一个TurnModelSnapshot的estimator在PromptSet/Compaction调用点结果一致；
- non-text unknown estimate不触发soft compaction；
- project config不能注册provider/auth；
- active Turn不执行cross-model fallback。

### Prompt Mapping

- System和User的provider mapping；
- unsupported role fail closed；
- User/Assistant/Tool order保持；
- reasoning/text/tool call content order保持；
- orphan ToolResult拒绝；
- ToolSpec schema/name mapping；
- ToolChoice None/Auto/Required/Specific；
- JSON schema/output contract；
- NoToolCalls和Structured携带非空tools时constructor拒绝；
- unsupported image/audio/document；
- Gateway不截断或摘要context；
- ModelGateway不修改`AssembledModelContext`；
- assembly proof purpose/model/output-contract mismatch在ModelCallRequest constructor被拒绝。
- Compaction budget proof missing、purpose不匹配或request max output不相等时constructor拒绝；
- CompactionSummary的effective max_output_tokens不超过Snapshot known limit；
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

- admitted request每次generate_model_turn只调用一次ProviderAdapter.execute；preflight validation或active route/principal cooldown可以在provider调用前以typed error terminal，因此总调用数为0或1；
- Rig和provider SDK automatic retry固定为0；
- connect failure、429、5xx和timeout都以typed error terminal返回Gateway caller；
- delivery state准确区分NotSent、RejectedBeforeExecution、AcceptedNoOutput、OutputStarted和Unknown；
- request delivery outcome unknown返回RequestOutcomeUnknown；
- first semantic delta后failure返回StreamInterrupted；
- 401不refresh-and-resend；request前credential refresh可以singleflight；
- 429返回typed Retry-After，Gateway不sleep；
- active cooldown零provider attempt并返回typed Retry-After，但仍算当前logical call chain的一次Gateway invocation；若该调用由retry启动，对应retry_count已经消耗；
- MVP不做WebSocket → HTTP fallback；
- 不允许provider/model substitution；
- AgentRun最多3次Session logical retry，backoff 2s/4s/8s；
- CompactionSummary最多1次Session logical retry，backoff 2s；
- RequestOutcomeUnknown和StreamInterrupted默认不logical retry；
- 仅`NotSent`/`RejectedBeforeExecution`的Timeout/TransportUnavailable/ProviderUnavailable允许logical retry；AcceptedNoOutput和unsafe/unknown outcome必须归一化为RequestOutcomeUnknown或StreamInterrupted；
- Retry-After <= 60s时实际delay取purpose backoff与hint较大值，超过60s不调度；
- Steer/Cancel使logical retry失效。

### Cancellation

- cancel during auth resolve/request前refresh；
- cancel before request start；
- cancel during stream；
- provider terminal先于cancel时Gateway完成result；SessionExecutor仍可因version/cancel拒绝；
- Rig AbortHandle桥接；
- logical retry与Steer场景不存在可回传的旧detached Model future；execution_version用于拒绝basis已经变化的result并检测实现错误。

### Usage And Finish

- all provider usage fields；
- missing fields保持None；
- no local estimate in ModelUsage；
- failed attempt usage只进ModelGateway internal telemetry，不进入ModelCallError或Session aggregate；
- successful response usage随assistant保存；
- logical retry_count准确记录0–3（AgentRun）或0–1（CompactionSummary）；
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
- generic Rig缺finish reason时provider-specific extraction；
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

- deterministic cache annotation不改变content；
- cache miss/full request equivalence；
- cache token usage；
- strict prefix continuation success；
- Steer/Cancel/Compaction/model/tool/output change使continuation失效；
- process restart不恢复continuation；
- provider拒绝continuation后full request fallback；
- concurrent continuation candidate race不发送错误delta。

### Multi-Session And Performance

- shared ModelGateway并发执行多个Session的provider request；
- 不存在Gateway-local model permit wait或admission fairness语义；
- one Session cancellation不影响另一个；
- one provider cooldown只fast-fail对应route/principal，不影响另一个；
- slow progress observer不阻塞stream；
- connection pool按endpoint/auth principal隔离；
- slow credential resolution/request前auth refresh不持有Gateway-wide长guard；
- 两个Session同时调用同一provider/model时都可以进入各自attempt。

### Real Adapter Integration

- Rig OpenAI Responses mock server；
- Rig Anthropic Messages mock server；
- SSE fragmented frames；
- WebSocket upgrade failure；
- malformed provider payload；
- rate-limit/status/body mapping；
- custom base URL；
- tool/reasoning/usage round-trip；
- cancellation and connection close。

## Source Plan

推荐实现：

```text
src/model_gateway.rs
src/model_gateway/model.rs
src/model_gateway/request.rs
src/model_gateway/response.rs
src/model_gateway/error.rs
src/model_gateway/catalog.rs
src/model_gateway/auth.rs
src/model_gateway/cache.rs
src/model_gateway/continuation.rs
src/model_gateway/redaction.rs
src/model_gateway/provider.rs
src/model_gateway/provider/rig.rs
src/model_gateway/provider/scripted.rs
```

这是一个module及其private implementation文件，不建立provider crate hierarchy，除非真实build time或dependency isolation证明需要拆分。

## Rejected Designs

### SessionExecutor直接调用Rig provider

否决原因：provider type、auth、base URL、retry、usage和error会扩散到Session execution。

### ModelGateway重新组装Prompt

否决原因：产生第二个model-visible context seam，破坏Transcript-First。

### 公开PreparedModelTurn

否决原因：增加stale plan和dropped prepared lifecycle，不能提高caller leverage。

### 公开ModelTurnSession

否决原因：让SessionExecutor管理第二个turn-scoped state owner；connection/continuation可以private复用。

### 返回raw provider stream

否决原因：caller必须理解provider event、terminal EOF、retry和partial draft；error/usage normalization失去locality。

### active Turn自动跨模型fallback

否决原因：破坏exact Model pin，改变capabilities、limits、cost和语义。

### 把provider attempt持久化为ModelAttempt entity

否决原因：attempt是transport/execution detail，不是领域事实；会扩大storage和recovery surface。

### 把partial stream写SessionStorage

否决原因：一个finalized assistant response应对应一个entry；partial draft不可恢复且会制造重复usage和content ordering问题。

### 根据model name猜capability

否决原因：custom provider和alias会使行为不确定；capability必须来自validated definition。

### 使用response_id作为conversation truth

否决原因：provider state不可替代完整committed transcript；continuation必须证明full logical equivalence。

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
- [ ] 实现ScriptedProviderAdapter并通过阶段6–8 ordinary/compaction vertical slices。
- [ ] 尽早执行Rig 0.40.0 ModelGateway integration spike，在production adapter冻结前完成。
- [ ] 实现ModelGateway和Rig provider adapters。
- [ ] 完成OpenAI Responses与Anthropic Messages mock-server tests。
- [x] 在阶段9冻结公开model catalog/query协议。
