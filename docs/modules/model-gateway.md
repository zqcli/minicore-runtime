# ModelGateway

`ModelGateway` 是 MiniCore 中唯一负责真实模型调用的运行时边界。它复用 Rig 的 provider/client/streaming 能力，但不把 Rig provider 类型、模型凭据、raw provider payload 或 provider SDK 错误暴露给 `SessionRuntime`、`Driver`、下游 UI 或会话持久化。

一句话边界：

```text
ModelGateway owns model invocation governance.
Rig owns provider protocol implementation.
```

## 设计决定

MiniCore 不重新实现 OpenAI、Anthropic、Gemini 或 OpenAI-compatible / Anthropic-compatible HTTP client。provider protocol、stream parsing、provider-specific request/response shape 尽量复用 Rig；MiniCore 在 Rig 外包一层 `ModelGateway`，集中处理 provider/model 选择、凭据解析、custom base URL、fallback、usage 归一化、错误分类和 cancellation。Provider hook 是后期 `RuntimeHooks` 能力；它的 owner 仍是 `ModelGateway`，不属于 `SessionRuntime` 或 `Driver`。

```text
SessionRuntime::ModelState
  → TurnState.model: ActiveModel
  → Driver builds ModelCallRequest { model: ModelSelection, ... }
  → DriverHost::call_model(...)
  → ModelGateway
      → ProviderRegistry.resolve(ModelSelection)
      → AuthStore.resolve(auth_ref)
      → private Rig provider adapter
      → normalized stream / usage / error
```

这不是把 Rig provider API 透传给上层。除私有 adapter，例如 `model_gateway/rig.rs`，其他模块不应导入 `rig::providers::*`。

## 实现分期

`ModelGateway` 是 `Driver` 的早期依赖，不能等到 usage/context usage 阶段才稳定。实现顺序采用“两层”策略：先落最小稳定 spine，再补完整 provider 能力。

阶段 4/5 前必须稳定的 `ModelGateway` spine：

- provider-neutral 类型：`ModelSelection`、`ModelCallPurpose`、`ModelCallRequest`、`ModelCallResult`、`ModelCallErrorKind`、`ModelCallUsage` 的字段、purpose 传播和 redaction 规则。
- `ModelGateway.call_model(request, sink, cancel)` 主 seam。
- 最小 `ProviderRegistry.resolve(ModelSelection)`：可以只支持 builtin fake/minimal provider 和最小 capability projection，但调用方必须通过 registry，不允许在 `Driver` / `SessionDriverHost` 中 match provider id。
- 最小 `AuthStore.resolve(AuthRef)`：可以使用测试 secret 或受控 env resolver，但 secret material 只能出现在 `AuthStore` / `ModelGateway` 内部。
- fake/minimal provider adapter 或私有 Rig adapter：用于证明 text streaming、cancellation、error mapping 和 `ModelStreamSink` 形状。

阶段 9 才补齐的扩展能力：

- user-global custom provider 配置、`ProviderProtocol`、custom `base_url`、`api_model_name` 和完整 capability validation。
- keychain / OAuth / runtime override 等完整 auth 来源。
- fallback chain、model invalid fallback diagnostics 和 provider-specific retry refinement。
- provider-specific usage extraction、`UsageStats`、`ContextUsageView` 和成本/上下文窗口展示。
- 为后期 provider hook 预留 redacted summary seam；raw provider payload patch 不进入当前 MVP，只有在 adapter shape 稳定且可脱敏后才开放。

禁止的临时路径：阶段 5 不允许让 `Driver`、`SessionDriverHost` 或 `SessionRuntime` 直接读取环境变量、构造 Rig provider client、解析 provider error 字符串、保存 provider API model name 为执行身份，或把 raw provider payload / raw usage 塞进 event、snapshot 或 session JSONL。为了跑通 text-only driver，也必须走 `ModelGateway` spine。

## 模块归属

```text
WorkspaceServices
  ├─ SettingsStore / user-global EffectiveSettings
  ├─ ProviderRegistry / user-global provider catalog
  ├─ AuthStore / user-global credentials boundary
  └─ ModelGateway / shared model invocation boundary

SessionRuntime
  ├─ ModelState                 // session-scoped model selection
  └─ DriverHost::call_model     // delegates to ModelGateway

Driver
  └─ copies ModelSelection into ModelCallRequest only

ModelGateway internals
  └─ rig_provider_adapter       // private Rig provider/client usage
```

`ModelGateway` 是 runtime-global 的模型调用边界，不随 session focus、cwd 或 resource reload 重建。provider settings、custom provider 声明和 auth 都来自 user-global/runtime-global 配置；项目级 settings 不允许声明 custom provider、覆盖 base URL 或引用 credentials。不同 session 通过自己的 `ModelState` 选择 `ModelSelection`，但 provider/auth 解析使用同一个 user-global `ProviderRegistry` 和 `AuthStore`。

## 不应承担

`ModelGateway` 不应：

- 执行 Agent loop 或推进 Rig `AgentRun`。
- 执行工具、审批工具或读取工具结果。
- 构建 system prompt、展开 skill 或解析 command text。
- 读写 `SessionStorage` 或发布 `agent_runtime_protocol::Event`。
- 把 raw provider payload、provider response 或 credentials 放进 snapshot/event/session JSONL。
- 让 hook 获得 `AuthStore` secret material。

这些职责分别属于 `Driver`、`Tools`、`Prompt` / `Skills` / `CommandSurface`、`SessionRuntime`、`AgentRuntimeProtocol` 和 `RuntimeHooks`。

## 核心类型

执行路径使用 provider-neutral 类型。`ModelSummary`、`ProviderSummary` 和 `AuthStatusView` 是协议 view；它们不是模型调用的权威输入。

```rust
pub struct ModelSelection {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
}

pub struct ActiveModel {
    pub selection: ModelSelection,
    pub summary: ModelSummary,
    pub capabilities: ModelCapabilities,
}

pub struct ModelState {
    pub selected: ModelSelection,
    pub thinking_level: ThinkingLevel,
    pub stream_options: StreamOptions,
    pub fallback_policy: Option<ModelFallbackPolicy>,
}
```

`ModelSelection` 是 session entry、command、event 和 request 中的稳定选择身份。`ModelSummary` 是 UI/snapshot 投影；不要用它驱动 provider 调用。

```rust
pub enum ProviderProtocol {
    OpenAiResponses,
    OpenAiChatCompletions,
    AnthropicMessages,
    Gemini,
}

pub struct ProviderSpec {
    pub provider_id: ProviderId,
    pub display_name: String,
    pub protocol: ProviderProtocol,
    pub base_url: Option<Url>,
    pub auth_ref: AuthRef,
    pub models: Vec<ModelSpec>,
}

pub struct ModelSpec {
    pub model_id: ModelId,
    pub display_name: String,
    pub api_model_name: String,
    pub capabilities: ModelCapabilities,
    pub default_options: ModelDefaultOptions,
}

pub struct ResolvedModelSpec {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    pub protocol: ProviderProtocol,
    pub base_url: Option<Url>,
    pub api_model_name: String,
    pub capabilities: ModelCapabilities,
    pub auth_ref: AuthRef,
}

struct ResolvedProviderCall {
    model: ResolvedModelSpec,
    auth: ResolvedAuth,
    request: ModelCallRequest,
}
```

`model_id` 是 MiniCore 内的稳定 ID；`api_model_name` 是真实传给 provider API / Rig provider client 的模型名。二者可以相同，也可以不同。

`ResolvedProviderCall` 是 `ModelGateway` 内部对象，不是 public API。它是少数允许同时持有 resolved model spec、resolved auth 和 provider-neutral request 的地方，生命周期应限制在一次 provider 调用内。

```rust
pub struct ModelCapabilities {
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub supports_streaming: bool,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub supports_json_schema: bool,
    pub supports_thinking: bool,
    pub supports_prompt_cache: bool,
    pub reports_usage: bool,
}
```

能力用于裁剪 thinking level、过滤 active tools、估算 context usage、决定 compaction / overflow 策略和生成 UI 可用性说明。能力缺失时必须保守处理，不应靠 provider 名称猜测。

## ProviderRegistry

`ProviderRegistry` 是 provider/model catalog，不是 provider client pool，也不是 credential store。

它负责：

- 合并 Rig 支持的内置 provider、user-global settings 中的 custom provider 和后续 user-trusted extension 声明。
- 校验 `ProviderSpec` / `ModelSpec`，包括 `provider_id`、`model_id`、`protocol`、`base_url`、`api_model_name` 和 capabilities。
- 提供 `resolve(ModelSelection) -> ResolvedModelSpec`。
- 提供 `list_providers()` / `list_models()` 的协议投影数据。
- 提供默认模型和 fallback chain 的候选解析。

它不负责：

- 读取 API key 或 OAuth token。
- 构造 Rig provider client。
- 执行模型调用。
- 发布 model changed 事件。

custom provider 必须显式声明 protocol：

```rust
pub struct CustomProviderConfig {
    pub provider_id: ProviderId,
    pub display_name: String,
    pub protocol: ProviderProtocol,
    pub base_url: Url,
    pub auth_ref: AuthRef,
    pub models: Vec<CustomModelConfig>,
}

pub struct CustomModelConfig {
    pub model_id: ModelId,
    pub api_model_name: String,
    pub display_name: Option<String>,
    pub capabilities: ModelCapabilities,
}
```

不要用“只要填了 base_url 就自动猜 OpenAI-compatible”。显式 `ProviderProtocol` 让 provider request、tool schema、usage extraction 和错误分类都能有确定映射。

custom provider 是 user-global 能力。项目级资源、项目级 settings 或未信任工作区不能声明 custom provider，也不能把默认模型指向项目提供的 base URL。这避免打开项目后把用户凭据发送到项目控制的 provider endpoint。

## AuthStore

`AuthStore` 是凭据解析边界。`ProviderRegistry` 只保存 `AuthRef`，不保存 secret material。

```rust
pub enum AuthRef {
    Env { var: String },
    Named { key: String },
    OAuth { account_id: String, provider_id: ProviderId },
    RuntimeOverride { key: String },
}

pub trait AuthStore: Send + Sync {
    fn status(&self, auth_ref: &AuthRef) -> AuthStatusView;
    fn resolve(&self, auth_ref: &AuthRef) -> Result<ResolvedAuth, AuthError>;
}
```

`ResolvedAuth` 只允许存在于 `ModelGateway` 内部和随后的 Rig provider client / HTTP request 中。它不得实现泄漏 secret 的 `Debug` / `Display`。

secret 只允许存在于：

```text
AuthStore
ModelGateway internal ResolvedAuth / ResolvedProviderCall
Rig provider client/request
actual HTTP request
```

secret 不得出现在：

```text
Command
TurnState
DriverTurnInput
DriveRequest
ModelCallRequest
DriverEvent
agent_runtime_protocol::Event
RuntimeSnapshot
SessionEntry / JSONL
RuntimeHookContext
diagnostics 明文
logs
```

## ModelCallRequest

`ModelCallRequest` 是 `Driver` / `SessionRuntime` 给 `ModelGateway` 的唯一 provider-neutral 请求。它携带模型选择、消息、工具 schema、输出限制和调用目的，但不携带 credentials、raw headers、Rig provider 类型或 raw provider payload。`Compaction` 不构造第二套模型请求；它只产出 `CompactionSummaryMaterial`，由 `SessionRuntime` 补齐调用策略后构造本类型。

```rust
pub enum ModelCallPurpose {
    AgentRun,
    CompactionSummary,
}

pub struct ModelCallRequest {
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub call_id: ModelCallId,
    pub purpose: ModelCallPurpose,
    pub model: ModelSelection,
    pub messages: Vec<MessageRecord>,
    pub system_prompt: Option<String>,
    pub tools: Vec<ToolSchema>,
    pub thinking_level: ThinkingLevel,
    pub stream_options: StreamOptions,
    pub output_contract: Option<OutputContract>,
    pub max_output_tokens: Option<u64>,
}
```

`ModelCallPurpose` 是模型调用业务目的的唯一权威类型，并原样传播到 `ModelCallUsage` 和 future `SessionEntry::Usage`；不存在第二套 `UsagePurpose`，也不允许在 usage/persistence 层重新分类。

`Retry` 不是 purpose。provider fallback / retry 由 `ModelCallAttempt` 表达，session/run 级 retry 由 `RetryReason`、`DriveEntry::Retry` 或 future call lineage 表达；重试后的调用仍保留原 purpose。`Background` 也不是 purpose：某个 session 因 focus 切换而在后台继续运行时，它的调用仍是 `AgentRun`。未来如果增加 branch summary、session title 等真实模型任务，应增加明确业务变体，而不是恢复模糊的 `Background`。

`AgentRun` 请求必须来自已校验的 `ModelInputProjection`，通常带 `tools`、完整 system prompt 和可选 `OutputContract`；`max_output_tokens` 可以为空并使用模型默认值。`CompactionSummary` 请求不带 tools/output contract，不复用 Agent run system prompt，只使用 compaction summary prompt，并把 `CompactionSummaryMaterial.max_output_tokens` 写入请求。

`max_output_tokens` 表示调用方期望的输出上限，不是 provider 已验证值。`ModelGateway` 在 provider capability validation 时必须保证它大于 0，并按 `ModelCapabilities.max_output_tokens` 拒绝或确定性裁剪；最终生效值可进入 redacted attempt/diagnostic summary，但不能由 `Compaction` 直接读取 provider capability。

## ModelCallResult

```rust
pub struct ModelCallResult {
    pub call_id: ModelCallId,
    pub actual_model: ModelSelection,
    pub finish_reason: ModelFinishReason,
    pub message: MessageRecord,
    pub usage: Option<ModelCallUsage>,
    pub attempts: Vec<ModelCallAttempt>,
    pub response_summary: ProviderResponseSummary,
}

pub struct ModelCallAttempt {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    pub api_model_name: String,
    pub status: ModelCallAttemptStatus,
    pub error_kind: Option<ModelCallErrorKind>,
}

pub struct ProviderResponseSummary {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    pub status_code: Option<u16>,
    pub request_id: Option<String>,
    pub finish_reason: Option<String>,
    pub usage_source: UsageSource,
}
```

`ProviderResponseSummary` 是安全摘要，不是 raw provider response。默认不进入 session storage；如果进入 diagnostics，也必须 redacted。

错误分类使用 MiniCore taxonomy，禁止上层解析 provider 字符串：

```rust
pub enum ModelCallErrorKind {
    AuthMissing,
    AuthInvalid,
    RateLimited,
    ContextOverflow,
    UnsupportedCapability,
    SafetyBlocked,
    Transport,
    Provider,
    Cancelled,
    Protocol,
}
```

`SessionRuntime` 根据 `ModelCallErrorKind::ContextOverflow` 触发 overflow compaction recovery；根据 transient 分类决定 retry。它不解析 Rig/provider error 文本。

## 调用生命周期

### Startup

```text
AgentRuntime initializes WorkspaceServices
  → SettingsStore loads user-global provider/model settings
  → ProviderRegistry builds builtin + user-global custom catalog
  → AuthStore prepares env/keychain/oauth/runtime overrides
  → ModelGateway is created as shared runtime-global invocation boundary
```

### Session restore

```text
SessionHandle.build_session_context()
  → replays latest ModelChange { provider_id, model_id }
  → SessionRuntime initializes ModelState
  → invalid selection falls back through ProviderRegistry default/fallback policy
  → diagnostics records fallback reason
  → RuntimeSnapshot exposes model_fallback_message if needed
```

`ModelChange` 中保存的是 MiniCore `provider_id` / `model_id`，不是 Rig provider type，也不是 provider API model name。

### SetModel

```text
AgentCommand::SetModel { provider_id, model_id }
  → AgentRuntime routes to SessionRuntime
  → SessionRuntime checks CommandRunPolicy / session phase guard
  → ProviderRegistry.resolve(ModelSelection)
  → clamp thinking/options against ModelCapabilities
  → ModelState.selected = selection
  → SessionHandle.append_model_change(provider_id, model_id)
  → emit session_model_changed
  → persistence_save_point
```

如果当前 run 正在进行，MVP 可以拒绝切换；完整版本可写入 pending session writes，并在 `before_next_model_call` 安全点 patch future model call。运行中的 provider request 不被中途替换。

### SubmitPrompt

```text
SubmitPrompt / prompt-like intent
  → SessionRuntime captures TurnResourceSnapshot and creates PromptTurn
  → PromptTurn.resolve_intent(...) returns ResolvedPromptInput
  → SessionRuntime persists current input and builds TurnState
  → SessionRuntime projects DriverTurnInput { model, prompt: PromptCallProfile, thinking_level, stream_options }
  → Driver.drive_run(...)
  → AgentRunStep::CallModel
  → Driver applies NextModelCallPlan
  → Prompt projects durable history + protected current input + transient context
  → ModelInputProjection validation succeeds
  → Driver builds ModelCallRequest { model: ModelSelection, projection fields, ... }
  → DriverHost::call_model
  → ModelGateway.call_model
      → user-global ProviderRegistry.resolve(selection)
      → user-global AuthStore.resolve(auth_ref)
      → future BeforeModelCall hook
      → private Rig provider adapter builds provider request/client
      → future optional privileged redacted provider payload hook
      → provider streaming
      → usage normalization
      → future AfterProviderResponse / ProviderUsageNormalized hooks
  → Driver feeds ModelCallResult back into Rig AgentRun
  → SessionRuntime persists messages and emits usage/run events
```

### Tool-call continuation

```text
Rig CallTools
  → DriverHost::invoke_tool_batch
  → SessionRuntime
  → Tools policy / approval / grants / executor
  → Driver feeds tool_results back to Rig
  → next Rig CallModel
  → Driver builds a new ModelCallRequest with the same or patched ModelSelection
```

工具 schema 与 system prompt 必须来自同一个 `PromptCallProfile` / `ModelInputProjection` 后再进入 `ModelCallRequest`；工具执行绝不经过 `ModelGateway`，也不由 Rig high-level runner 自动执行。

### Compaction summary

```text
SessionRuntime compaction flow
  → compaction::build_summary_material(...)
  → CompactionSummaryMaterial { system_prompt, messages, max_output_tokens }
  → SessionRuntime selects summary model / thinking / stream policy
  → ModelCallRequest { purpose: CompactionSummary, tools: [], max_output_tokens: Some(...) }
  → ModelGateway.call_model
  → CompactionResult
```

压缩摘要调用使用同一套 auth、usage、error、cancellation 和 hook redaction 规则，但不进入 `Driver.drive_run()`。

## Hooks

模型/provider 边界 hook 的 owner 是 `ModelGateway`，不是 `SessionRuntime`，也不是 `Driver`。`SessionRuntime` 只拥有进入 `DriverHost::call_model(...)` 前的 run safe point，例如 `BeforeNextModelCall`、typed context collection、队列处理和 `PromptCallProfile` rebuild；Prompt 拥有最终 provider-neutral model-input projection 与协议校验。当前 MVP 不实现 provider hook；本节只是固定后期 hook 的边界，避免 BR-010 中的双 owner。

推荐阶段：

```text
BeforeModelCall
  provider-neutral request/options patch; no credentials

BeforeProviderPayload
  privileged, redacted, adapter-private shape; MVP may keep disabled

AfterProviderResponse
  observer; status code, request id, allowlisted headers only

ProviderUsageNormalized
  observer; receives ModelCallUsage / source / purpose
```

`BeforeProviderPayload` 是最高风险 hook。除非 Rig adapter 能提供不含 auth headers 的安全 payload，并且 MiniCore 愿意把该 payload shape 作为受控内部扩展契约，否则不得开放 raw provider payload patch。后期若先接入模型 hook，也应先接 provider-neutral `BeforeModelCall`。

Hook 不能读取 `AuthStore`，不能看到 authorization header，不能保存 raw provider response，不能绕过 provider capability validation。

## Usage And Events

`ModelGateway` 输出单次模型调用的 `ModelCallUsage`。`Driver` 可以在一次 `drive_run()` 内聚合多次模型调用，最终返回 run-level `UsageSummary`；`SessionRuntime` 更新 `CurrentRunUsage`、`SessionUsageStats` 和 `ContextUsageView`。

```text
ModelGateway
  → ModelCallUsage
Driver
  → current drive_run UsageSummary
SessionRuntime
  → usage_updated
  → run_finished { usage }
  → RuntimeSnapshot.active_session.session_stats / context_usage
```

`ModelCallUsage.raw_provider_usage` 如果保留，只能作为 internal redacted diagnostic，不进入 `AgentRuntimeProtocol`，不进 session JSONL，不进 hook context。

`ModelGateway` 不发布 UI event。模型文本 delta 通过 `ModelStreamSink` 进入 `DriverEvent`，再由 `SessionRuntime` 归约为 `message_assistant_*` 和 `usage_updated`。

## Skill And Tool Injection

技能有两个路径：

```text
explicit SkillPromptIntent
  → target PromptTurn.resolve_intent() formats captured skill body as user message

available skill summaries
  → PromptResourceView.materials
  → prompt::begin_turn(...) builds PromptCallProfile
```

技能不直接选择 provider/model，也不接触 auth。

工具有两个模型可见路径：

```text
system prompt snippets/guidelines
provider tool schemas in ModelCallRequest.tools
```

`ModelGateway` 可以把 `ToolSchema` 转成 provider/Rig 需要的 provider-specific tool schema，但不能执行工具。若模型不支持 tools，`SessionRuntime` / `ActiveToolSet` 应根据 `ModelCapabilities.supports_tools` 过滤或禁用 active tools，并给 UI 诊断。

## Custom Provider Examples

OpenAI-compatible：

```text
provider_id = "acme-openai"
protocol = OpenAiChatCompletions
base_url = "https://api.acme.example/v1"
auth_ref = Env("ACME_API_KEY")
model_id = "qwen3-coder"
api_model_name = "Qwen/Qwen3-Coder"
```

Anthropic-compatible：

```text
provider_id = "acme-anthropic"
protocol = AnthropicMessages
base_url = "https://anthropic-proxy.acme.example"
auth_ref = Named("acme-anthropic")
model_id = "claude-compatible"
api_model_name = "claude-sonnet-4-5"
```

两者都走同一路径：

```text
ProviderRegistry
  → ModelGateway
  → private Rig provider adapter with overridden base_url / api_model_name / auth
```

## Grilling 结论

需要被测试和持续追问的边界：

- 阶段 5 的 text-only driver 是否已经只通过 `ModelGateway.call_model(...)`，没有临时 provider/auth 路径。
- Rig provider API 是否支持 custom `base_url`、headers、streaming、usage extraction、tool schema 和 cancellation。
- `Driver` 是否真的不 import provider registry/auth，也不解析 provider errors。
- `BeforeProviderPayload` 是否能做到 redacted；做不到就不开。
- `ModelSummary` 是否只在 protocol/snapshot 中出现，执行路径是否只用 `ModelSelection`。
- `ModelCallUsage` 和 run-level `UsageSummary` 是否不会重复计数。
- compaction summary 是否使用 `ModelCallPurpose::CompactionSummary`，且不带 tools、不复用 Agent run system prompt。
- `ModelCallRequest.purpose` 是否原样进入 `ModelCallUsage` / future `SessionEntry::Usage`，没有 `UsagePurpose` 转换层。
- provider fallback、session retry 和后台 session 运行是否都不会把 purpose 改写为 `Retry` / `Background`。
- session restore 中失效模型是否有明确 fallback / diagnostics / snapshot 展示，而不是静默换模型。

## 必测项

- early spine：`DriverHost::call_model -> ModelGateway.call_model(ModelCallRequest)` 是阶段 5 唯一模型调用路径；`Driver` / `SessionDriverHost` 不 match provider id、不读 env、不构造 Rig provider client。
- `ProviderRegistry.resolve(ModelSelection)`：builtin/custom、invalid provider、invalid model、capability projection。
- minimal provider registry：阶段 4/5 可只支持 fake/minimal provider，但仍必须走 `ProviderRegistry` / `AuthStore` seam。
- `SetModel`：phase guard、session entry、`session_model_changed`、snapshot 当前模型一致。
- auth redaction：snapshot/event/session entry/diagnostics/hook context 不含 API key、OAuth token、authorization header。
- custom provider：OpenAI-compatible / Anthropic-compatible 的 `base_url + api_model_name + auth_ref` 能传到 Rig adapter。
- custom provider 来源：项目级资源/settings 不能注册 custom provider、覆盖 base URL 或引用新的 `AuthRef`；只有 user-global 配置或后续 user-trusted extension 可以声明 custom provider。
- submit flow：`TurnState.model.selection -> DriverTurnInput.model -> ModelCallRequest.model -> ProviderRegistry.resolve` 不丢失。
- error taxonomy：auth missing、rate limit、context overflow、cancelled 不靠字符串解析给上层。
- usage：多次 `model -> tools -> model` 的 `ModelCallUsage` 聚合成一次 run `UsageSummary`，不重复计数。
- purpose：`AgentRun` / `CompactionSummary` 从 request 到 usage/persistence 原样传播；retry/fallback 只改变 attempt/lineage metadata。
- compaction：`CompactionSummaryMaterial` 由 `SessionRuntime` 转成 `ModelCallPurpose::CompactionSummary` 请求，无 tools，带明确 `max_output_tokens`，usage 归入 compaction purpose。
