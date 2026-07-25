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
- provider-internal retry、transport fallback与Session logical retry如何区分；
- cancellation、authentication、secret redaction、并发配额和rate limit如何治理；
- provider prompt cache、connection reuse和continuation如何保持Transcript-First等价性；
- Rig 0.40.0可以复用哪些能力，哪些差异必须留在private adapter。

本文不定义：

- Prompt内容、conversation visibility或`MessageRecord → ModelMessage`转换；
- Session logical retry、compaction orchestration或Turn terminal规则；
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

## 决策摘要

已经确定：

- `MiniCoreRuntime`拥有一个共享`Arc<ModelGateway>`；
- ModelGateway不保存current Session、current Turn或UI selected model；
- `ModelGateway::resolve_for_turn(...)`在Turn capture期间返回immutable `TurnModelSnapshot`；
- `ModelGateway::generate_model_turn(...)`是唯一真实模型调用interface；
- `ModelCallRequest.input`只能是PromptSet产生的`AssembledModelContext`；
- ModelGateway只编码完整provider-neutral context，不重新决定conversation visibility；
- `TurnModelSnapshot`固定exact model definition、capabilities、effective limits和generation policy；
- active Turn内不允许静默替换provider/model identity；
- provider retry只复用同一个immutable request；
- transparent retry只允许在adapter证明request未执行、provider明确拒绝或支持idempotent resume时发生；
- WebSocket → HTTP等transport fallback可以发生，但必须保持同一exact model identity和request semantics；
- cross-model fallback不是ModelGateway transparent behavior；
- streaming progress是process-local observer data，不进入SessionStorage；
- finalized response或typed error是一次gateway调用唯一terminal result；
- provider-reported usage随成功assistant response保存；失败attempt usage在MVP中只属于ModelGateway internal telemetry；
- authentication secret、raw headers、raw request/response body和provider SDK类型不越过ModelGateway seam；
- prompt cache、connection reuse、`previous_response_id`和incremental request只是wire optimization；
- 所有optimization必须能退回完整`AssembledModelContext`请求；
- ProviderAdapter是private internal seam，首批实现为RigProviderAdapter和ScriptedProviderAdapter；
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
| retry owner | AgentSession为主 | client/turn loop协作 | sampler retry | caller决定 | provider retry在Gateway，logical retry在Executor |
| transport fallback | provider-specific | WebSocket → HTTP | backend显式配置 | adapter-specific | 同model transport fallback允许 |
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
- provider attempt、connection、auth、retry、cache和continuation全部具有locality；
- 与SessionExecutor的`RunningOperation`自然对齐；
- fake adapter可以通过同一interface测试；
- 删除Gateway会把大量provider复杂性重新散落到caller，满足deep module deletion test。

代价：

- implementation内部较深；
- 需要private planner、adapter、connection和retry seams辅助测试；
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
- concurrency permit和continuation不能安全地在prepare阶段预留；
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
   ├─ ModelConcurrencyController
   ├─ private ProviderAdapter
   ├─ ProviderConnectionPool
   ├─ ContinuationCache
   ├─ RetryPolicy
   └─ RedactionPolicy

SessionExecutor
└─ RunningOperation::GenerateModelResponse
   ├─ immutable ModelCallRequest
   ├─ scoped ProgressEventPublisher<ModelProgressEvent>
   └─ CancellationToken
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

impl ModelGateway {
    pub(crate) fn resolve_for_turn(
        &self,
        request: ResolveTurnModelRequest,
    ) -> Result<TurnModelSnapshot, ModelResolutionError>;

    pub(crate) async fn generate_model_turn(
        &self,
        request: ModelCallRequest,
        progress: ProgressEventPublisher<ModelProgressEvent>,
        cancel: CancellationToken,
    ) -> Result<ModelCallResult, ModelCallError>;
}
```

`resolve_for_turn`只访问已经初始化的provider/model catalog，不读取credential。catalog refresh是Runtime lifecycle操作，不在Turn capture期间隐式执行remote I/O。

`generate_model_turn`执行完整provider调用。调用期间可以等待provider并发permit、auth refresh、retry delay和stream，但它运行在`RunningOperation`中，不阻塞SessionIngress scheduler或control loop。

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
- current catalog revision。

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
    generation: EffectiveGenerationPolicy,
    fingerprint: TurnModelFingerprint,
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
- TurnContext entry保存safe `TurnModelRef`和fingerprint，不保存execution_ref。

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
- TurnModelFingerprint生成；
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

cache和service class属于Turn-pinned execution policy。它们不改变conversation visibility，但会影响provider request、cost和fingerprint。

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
    pub fingerprint: AssembledModelContextFingerprint,
    pub(crate) assembly_proof: PromptAssemblyProof,
}

```

PromptSet System sections、前置User context和ToolSpec在active Turn内天然稳定；Gateway可以利用canonical section/message/tool boundaries选择cache breakpoint，不需要额外stability flag。

`PromptAssemblyProof`是PromptSet生成的crate-private consistency proof，绑定ModelCallPurpose、TurnModelFingerprint、OutputContract hash和optional CompactionSummaryBudget proof。它不提供第二个caller-controlled purpose；ModelCallRequest constructor必须校验proof与request一致。

首版provider-neutral output contract至少包含：

```rust
pub enum OutputContract {
    NoToolCalls,
    Structured(StructuredOutputContract),
}
```

`NoToolCalls`不是普通prompt text。请求中的`tools`必须为空；provider支持显式tool-choice/allowed-tools禁用时adapter同时设置该字段。若provider允许在未声明Tool时仍返回可执行ToolCall，adapter必须拒绝该结果，不能把它交给SessionExecutor执行。

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
    model: TurnModelSnapshot,
    purpose: ModelCallPurpose,
    input: AssembledModelContext,
    max_output_tokens: Option<NonZeroU32>,
}

impl ModelCallRequest {
    pub(crate) fn new(
        model: TurnModelSnapshot,
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
- assembly_proof.turn_model_fingerprint等于TurnModelSnapshot fingerprint；
- assembly_proof.output_contract_hash等于input.output_contract canonical hash；
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

- 是Prompt assembly和fingerprint输入；
- 原样传播到usage和diagnostics；
- 不是retry classification；
- 不是foreground/background状态；
- 不因transport fallback、auth refresh或logical retry改变。

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
- Gateway不静默clamp，因为clamp会改变调用方已经fingerprint的policy；
- CompactionSummary应提供明确上限，并保证该值已写入`CompactionPlanFingerprint`和assembly proof；
- known model limit小于全局summary配置时，由Compaction plan确定性取较小值，而不是让Gateway修正；
- known context/output limit无法留下最低摘要预算时，Compaction返回`NoFeasibleSummaryBudget`，不构造ModelCallRequest；
- 若CompactionSummary仍以越界值到达constructor，`InvalidRequest`表示caller实现或fingerprint contract错误，不能分类成provider故障。

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

该trait是ModelGateway private internal seam，不进入MiniCoreRuntime interface。`ProviderContentPublisher`只能发布attempt内的content delta；AttemptStarted、RetryScheduled和AttemptDiscarded由ModelGateway本身发布，adapter不能伪造retry lifecycle。

真实实现：

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
- attempt ordinal。

这些类型都必须private、non-serializable并使用redacted Debug。

只有private adapter可以使用Rig provider types和`CompletionRequest.additional_params`。调用方不能提交arbitrary JSON。

## Rig Adapter Mapping

```text
ModelCallRequest
→ validate TurnModelSnapshot
→ encode AssembledModelContext
→ Rig generic或provider-specific request API
→ provider stream
→ FinalizedAssistantResponse
```

RigProviderAdapter不被限制为generic `CompletionModel::stream`。当generic类型擦除finish reason、request ID或reasoning artifact时，adapter可以使用Rig公开的provider-specific request/response types；这些类型仍不能越过private adapter。

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
    AttemptStarted {
        ordinal: NonZeroU16,
    },
    ContentDelta {
        ordinal: NonZeroU16,
        content_index: u32,
        delta: ModelContentDelta,
    },
    RetryScheduled {
        next_ordinal: NonZeroU16,
        delay: Duration,
        reason: ModelCallErrorKind,
    },
    AttemptDiscarded {
        ordinal: NonZeroU16,
        reason: AttemptDiscardReason,
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

- progress publisher是bounded、non-authoritative和process-local；
- publisher由RunningOperation scope到SessionId/TurnId/Item draft identity；
- ModelGateway本身不接收SessionId或TurnId；
- 连续text/reasoning delta可以合并；
- queue满可以丢弃中间delta；
- progress publisher关闭不取消provider调用；
- cancellation由CancellationToken决定；
- partial reasoning/text/tool arguments不是durable Item；
- hidden chain-of-thought不可发布；
- provider未暴露的reasoning不可推断；
- terminal success必须返回完整FinalizedAssistantResponse；
- terminal error必须返回typed ModelCallError；
- SessionExecutor使用terminal result清理或替换UI draft。

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
    pub transparent_retry_count: u32,
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

`content[]`保持provider finalized semantic order。SessionExecutor随后分配或关联ItemId并构造assistant entry。

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
- ToolCall content是AgentLoop决定NeedTools的主要事实，finish reason用于一致性校验和diagnostics；
- Length不自动当Completed，需要Session execution根据OutputContract和AgentLoop规则处理；
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
- transparent retry只允许delivery state证明NotSent/RejectedBeforeExecution，或provider支持idempotent replay/exact resume；usage是否出现不能作为retry safety判断；
- provider若为失败attempt报告usage，该事实只进入ModelGateway internal telemetry；不放入ModelCallError、不进入Session aggregate，也不创建synthetic assistant或独立Usage entry；
- 因此MiniCore SessionStorage不是provider billing ledger。

`transparent_retry_count`只记录Gateway内部额外attempt数量。StoredAssistantMessage的`retry_count`定义为SessionExecutor对同一logical call执行的logical retry数量，不混入transparent retry。

## Retry

### Provider-Internal Retry

ModelGateway可以处理：

- DNS/connect/TLS失败；
- provider connection establishment失败；
- 有界401 auth refresh；
- adapter确认没有开始model execution的provider rejection和short rate-limit delay；
- WebSocket upgrade失败后切换HTTP；
- provider-supported idempotent replay或exact resume。

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

HTTP status或“尚无delta”本身不能决定delivery state。只有adapter根据protocol contract、provider response和transport阶段证明`NotSent`或`RejectedBeforeExecution`时才能普通transparent retry。`AcceptedNoOutput`和`OutputStarted`只有在provider支持相同idempotency key或exact resume时才能继续；`Unknown`必须返回`RequestOutcomeUnknown`。

必须满足：

```text
same TurnModelSnapshot
same ModelCallPurpose
same AssembledModelContext
same max_output_tokens
same provider/model identity
```

每次`generate_model_turn`在Gateway内部创建process-local ProviderInvocationId。支持idempotency key的provider在transparent retry中复用该key；该ID不进入ModelCallRequest、领域模型或SessionStorage。

每次attempt：

1. 检查cancellation；
2. resolve current credential和opaque auth principal identity，不持有model concurrency permit；
3. 等待global/provider/model/auth-principal concurrency permits；
4. build provider payload；
5. start provider request并更新delivery state；
6. consume stream；
7. normalize terminal result；
8. release permits。

retry delay和auth refresh期间不持有model concurrency permit。provider返回401时先释放permits，再进入singleflight refresh，然后创建新attempt。

### Semantic Delta Rule

默认规则：

- delivery state为NotSent或RejectedBeforeExecution时，可以按policy transparent retry；
- AcceptedNoOutput或OutputStarted只有provider-supported idempotent replay/exact resume时才能transparent继续；
- request已经发送但尚无response，delivery outcome不确定时返回`RequestOutcomeUnknown`，不能仅因没有delta就blind retry；
- 第一个model-visible delta发布后，普通restart可能重复text、tool call或billing，因此返回`StreamInterrupted`；
- 只有provider adapter能证明exact resume且不会重复semantic content时才允许继续同一attempt；
- MVP Rig adapter不假设支持exact resume；
- progress中已经发布的partial draft由SessionExecutor在terminal error后丢弃。

### Authentication Retry

- AuthMissing不重试；
- AuthRejected可以触发一次singleflight refresh；
- refresh成功后只重试一次；
- refresh失败或第二次401返回AuthRejected；
- credential每个attempt即时resolve，不pin access token到TurnModelSnapshot；
- refresh log和error不得包含token。

### Rate Limit

- 尊重typed Retry-After；
- 无Retry-After时使用bounded exponential backoff + jitter；
- total attempts和elapsed time都有限制；
- cancellation中止backoff；
- rate-limit state按provider route和auth principal的opaque identity隔离；
- 不把一个provider的cooldown应用到无关provider。

### Transport Fallback

允许：

```text
same OpenAI Responses model
WebSocket → HTTP SSE
```

不允许作为transparent fallback：

```text
provider A/model X → provider B/model Y
model X → model Z
Responses model → capability不同的Chat Completions model
```

跨模型替换会改变capabilities、limits、cost和输出行为，破坏Turn-pinned exact Model。需要替换时：

- 在Session definition update或下一Turn admission前显式resolve；
- 形成新的SessionDefinitionRevision或明确的future Turn model choice；
- 当前Turn失败或由用户显式Cancel后重试；
- 不由ModelGateway悄悄完成。

### Session Logical Retry

Gateway返回exhausted typed error后，SessionExecutor决定logical retry：

```text
TurnId/execution_version unchanged
ConversationCheckpoint unchanged
TurnExecutionContext unchanged
purpose/output contract/effective max_output_tokens unchanged
AssembledModelContextFingerprint unchanged
→ schedule timer
→ 确认旧RunningOperation已经terminal并从SessionExecutor移除
→ 使用同一ModelCallRequest启动新的唯一current RunningOperation
```

logical retry：

- 不阻塞SessionIngress scheduler或control loop；
- Steer、Cancel、Compaction或conversation change使其失效；
- 计入StoredAssistantMessage.retry_count；
- 不创建ModelAttempt entity；
- 不改变ModelCallPurpose。
- 不与旧本地Model future重叠；provider端可能继续工作或计费不等于旧future仍可回传SessionExecutor。

## Cancellation

ModelGateway在以下位置检查CancellationToken：

- resolution validation后；
- concurrency wait期间；
- auth resolution和refresh前后；
- retry delay期间；
- provider request开始前；
- stream read期间，直到provider terminal event被adapter接受。

Gateway success/cancel的线性化点是adapter接受完整provider terminal event：

- cancellation先被观察：中止request并返回Cancelled；
- terminal event先被接受：Gateway完成normalization并返回ModelCallResult，之后到达的cancel不把该result改写为Cancelled；
- SessionExecutor仍会用execution_version和Cancel/revocation arbitration决定该result能否append；普通Steer不拒绝已完成result，只决定无ToolCall response保存为Continue还是Final，因此Gateway terminal先赢不等于Turn一定Completed；
- 该规则保证terminal usage、finish reason和content不会因normalization期间的timer race随机丢失。

取消行为：

```text
cancel token fires
→ 停止新attempt
→ 取消retry timer/concurrency wait
→ 调用Rig stream.cancel或transport abort
→ 停止发布新progress
→ 返回ModelCallErrorKind::Cancelled
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
    pub kind: ModelCallErrorKind,
    pub message: RedactedErrorMessage,
    pub retry_after: Option<Duration>,
    pub transparent_retry_count: u32,
    pub diagnostics: Arc<[ModelCallDiagnostic]>,
}
```

```rust
pub enum ModelCallErrorKind {
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
    ProtocolViolation,
}
```

错误映射：

| Error kind | Gateway行为 | SessionExecutor默认处理 |
| --- | --- | --- |
| Cancelled | 停止attempt | Cancel/revocation/version规则；普通Steer不取消attempt |
| ModelUnavailable | 不替换模型 | TurnFailed或要求SetModel |
| AuthMissing/AuthRejected | 有界refresh后返回 | user action / TurnFailed |
| RateLimited | 有界retry | 可按logical retry policy等待 |
| QuotaExceeded | 不自动retry | user action / TurnFailed |
| ContextOverflow | 不改Prompt | bounded compaction recovery |
| UnsupportedCapability | provider调用前拒绝 | configuration failure |
| InvalidRequest | provider调用前或400 mapping | implementation/config failure |
| SafetyBlocked | 不自动retry | truthful terminal response/error policy |
| Timeout | 有界retry | logical retry或TurnFailed |
| TransportUnavailable | 有界retry/transport fallback | logical retry或TurnFailed |
| ProviderUnavailable | 有界retry | logical retry或TurnFailed |
| ProviderRejected | 不解析raw string | TurnFailed/diagnostic |
| RequestOutcomeUnknown | 不transparent replay | explicit logical retry policy，提示可能重复provider work/cost |
| StreamInterrupted | 不blind restart | logical retry only after draft discard |
| ProtocolViolation | fail closed | TurnFailed + diagnostic |

规则：

- caller不能解析`message`决定retry；
- provider raw error body不越过adapter；
- error message必须redacted、bounded且provider-neutral；
- provider status、request ID等只作为allowlisted diagnostic；
- ContextOverflow必须与Prompt本地size validation保留不同source；
- SafetyBlocked和Refused response的最终建模必须由adapter根据provider protocol一致处理。

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
- auth refresh使用singleflight，避免并发请求重复refresh；
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
- cache key可以基于opaque runtime salt + fingerprints；
- cache miss不改变语义；
- cache eviction直接发送full request；
- cache read/write token只作为usage；
- PromptSet fingerprint不等于provider cache key；
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

必须使用完整provider-neutral input或其canonical hash sequence证明prefix；不能只比较SessionId、TurnId、response_id或message count。

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

## Concurrency And Rate Governance

ModelGateway提供：

```text
global model-call limit
per-provider route limit
per-model limit
optional per-auth-principal limit
```

要求：

- permit wait cancellation-aware；
- fairness至少保证FIFO admission或等价无饥饿策略；
- 不在retry backoff期间持有permit；
- streaming request持有permit到terminal/cancel；
- auth refresh有独立singleflight，不占用全部model permits；
- provider cooldown只影响对应route/principal；
- 一个Session不能耗尽全部Runtime capacity；
- limits是Runtime policy，不进入ModelCallPurpose或conversation fingerprint；
- progress publisher阻塞不能占用provider stream读取。

## Complete Call Flow

```text
Turn admission
→ ModelGateway.resolve_for_turn(ModelSelection + preferences)
→ TurnModelSnapshot
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
   → wait global/provider/model/auth-principal concurrency permits
   → optional cache/continuation optimization with equivalence proof
   → provider stream
   → publish droppable progress
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
retry timer
ContinuationCache
resolved credential
partial draft
transparent attempt state
```

unfinished Turn按Session recovery规则终止。下一Turn从SessionStorage durable truth重新assemble full context并建立新provider request。

### Outcome Ambiguity

模型调用不是Tool side effect ledger：

- transport error后provider可能已经生成或计费；
- 未取得terminal finalized response时不能创建synthetic assistant message；
- partial draft不持久化；
- request已发送但尚未开始response时映射为RequestOutcomeUnknown；已产生partial response后映射为StreamInterrupted；
- SessionExecutor可以在不重放Tool且logical call basis不变时决定retry；
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

Conversation storage中的`TurnModelRef`固定为`ModelSelection + ModelDefinitionVersion + TurnModelFingerprint`，因此catalog revision变化后仍能解释historical model identity和reasoning replay compatibility。

成功assistant entry保存：

```text
TurnModelRef
allowlisted response_id
ordered finalized content[]
normalized ModelUsage
normalized ModelFinishReason
Session logical retry_count
AssembledModelContextFingerprint
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
transparent attempt list
retry timer
connection/continuation state
full AssembledModelContext
```

首版automatic SummaryModel compaction把model/response/usage/finish/logical-retry、requested max output、CompactionSummaryBudget fingerprint和assembled context fingerprint保存到`StoredCompaction.model_call`，因此该字段必须为Some。`None`只为未来明确设计的standalone/deterministic maintenance保留，不是automatic overflow fallback。

`retry_count`只表示Session logical retry。Gateway transparent retry count可以进入current-host diagnostics，但不作为domain lifecycle。

Provider response ID和reasoning opaque artifact必须有长度限制和redaction validation。

## Performance

目标：

- `resolve_for_turn`只做local catalog lookup和validation；
- full context encoding与provider I/O在RunningOperation执行；
- connection pool和HTTP client跨Session共享；
- provider/model concurrency有界；
- progress delta可以合并；
- provider stream reader不等待slow observer；
- retry backoff不占permit；
- auth refresh singleflight；
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
attempt ordinal
timing category
status class
retry reason
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
- catalog revision变化时active Snapshot保持旧definition；
- future Turn取得新definition；
- capability/limit validation；
- reasoning preference映射；
- unknown limits保持None；
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
- unsupported image/audio/document；
- Gateway不截断或摘要context；
- AssembledModelContextFingerprint不被修改；
- assembly proof purpose/model/output-contract mismatch在ModelCallRequest constructor被拒绝。
- Compaction budget proof missing、purpose不匹配或request max output不相等时constructor拒绝；
- CompactionSummary的effective max_output_tokens不超过Snapshot known limit；
- 越界CompactionSummary在provider调用前以InvalidRequest拒绝，Gateway不静默clamp。

### Streaming

- start/text/reasoning/tool-call delta；
- delta queue满时合并/丢弃不影响final result；
- publisher关闭不取消call；
- cancellation停止progress；
- unexpected EOF → StreamInterrupted；
- partial draft不进入storage；
- finalized response可以完整替换observer draft；
- hidden reasoning不发布。

### Retry

- connect failure before request send透明重试；
- NotSent/RejectedBeforeExecution允许policy retry；
- AcceptedNoOutput/OutputStarted只在idempotent replay/exact resume时继续；
- request delivery outcome unknown不透明重试；
- generic 408/429/5xx或no-delta不能单独证明safe retry；
- 401 singleflight refresh一次；
- 429 Retry-After；
- exponential backoff + jitter上限；
- retry delay不持有permit；
- first semantic delta后普通failure不blind retry；
- WebSocket → HTTP保持same model/request；
- 不允许provider/model substitution；
- exhausted error交给Session logical retry；
- Steer/Cancel使logical retry失效。

### Cancellation

- cancel before permit；
- cancel during permit wait；
- cancel duringauth resolve/refresh；
- cancel during backoff；
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
- logical retry_count与transparent retry count分离；
- Stop/ToolCalls/Length/ContentFiltered/Refused/Unknown；
- generic Rig缺finish reason时provider-specific extraction；
- response ID长度和字符validation。

### Auth And Redaction

- AuthMissing/AuthRejected；
- OAuth refresh并发singleflight；
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

- global/provider/model limits；
- fairness/no starvation；
- one Session cancellation不影响另一个；
- one provider cooldown不影响另一个；
- slow progress observer不阻塞stream；
- connection pool按endpoint/auth principal隔离；
- slow credential resolution/auth refresh不持有或耗尽model permits；
- per-auth-principal permit在opaque principal解析后获取。

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
src/model_gateway/concurrency.rs
src/model_gateway/retry.rs
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
- [x] 定义provider retry、transport fallback和logical retry边界。
- [x] 禁止active Turn cross-model fallback。
- [x] 定义cancellation、auth、redaction和custom provider规则。
- [x] 定义cache、connection reuse和continuation等价性规则。
- [x] 定义multi-session concurrency和rate governance。
- [x] 定义persistence、recovery、performance和test matrix。
- [ ] 实现ScriptedProviderAdapter并通过阶段6–8 ordinary/compaction vertical slices。
- [ ] 尽早执行Rig 0.40.0 ModelGateway integration spike，在production adapter冻结前完成。
- [ ] 实现ModelGateway和Rig provider adapters。
- [ ] 完成OpenAI Responses与Anthropic Messages mock-server tests。
- [x] 在阶段9冻结公开model catalog/query协议。
