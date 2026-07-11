# RuntimeHooks

`RuntimeHooks` 是 MiniCore harness runtime core 的内部扩展点系统。它在运行时关键安全点开放受控干预能力，让内置策略、测试 harness、可信 package 或后续 extension 能观察、改写、拒绝、补充或替换某些运行时决策，而不需要 fork MiniCore 或绕过 `AgentRuntime` / `SessionRuntime`。

它不是 `AgentRuntimeProtocol`，不是 UI plugin API，也不是事件流。Hook 不能直接发布 `agent_runtime_protocol::Event`，不能直接读写 session storage，不能直接执行工具，不能直接读取凭据。Hook 只能返回 typed decision / patch / replacement；最终状态变化和 UI 可见事件仍由拥有对应状态机的 `AgentRuntime` 或 `SessionRuntime` 归约。

当前 MVP 不实现 hook system，也不把 `RuntimeHookRegistry` 放进当前开发阶段。本文定义的是后期能力的边界和 owner 规则，用来防止后续扩展点绕过已经收敛的 runtime、model、tool、command 和 persistence owner。

## Hook And Event

```text
Hook
  = runtime 在事实发生前后开放的受控干预点
  = 可以观察或影响后续行为
  = 不进入 UI event stream

Event
  = runtime 已经发生的公开事实
  = 下游 CLI/TUI/GUI reducer 的输入
  = 拥有 sequence / event_id / routing / reconnect 语义
```

示例：

```text
ToolBeforePolicy hook
  → returns Deny { reason }

SessionRuntime
  → applies denial
  → emits tool_call_finished { is_error: true }
  → emits message_tool_result_appended
```

Hook 影响最终会发生什么；event 告诉下游最终发生了什么。下游 UI/CLI 不应直接消费 hook，也不应注册能绕过策略、沙箱、凭据或会话持久化的 hook。

## Hook 面

以下 hook 面来自 MiniCore 当前 owner 分层和安全点需求：

- `input`：用户输入到达后、进入 agent 前可 transform 或 handled。
- `before_agent_start`：用户 prompt 展开后、agent loop 启动前可追加 custom message 或替换 system prompt。
- `context`：每次模型调用前可替换 AgentMessage context。
- `before_provider_request` / `after_provider_response`：模型请求前后 hook；raw provider payload patch 必须 privileged 且脱敏，MVP 可以不开放。
- `tool_call` / `tool_result`：工具执行前阻止或改写结果。
- resource discovery 不作为 hook；资源更新统一走 `ResourceManager` ensure/reload/recompose pipeline。
- `session_before_switch` / `session_before_fork` / `session_before_compact` / `session_before_tree`：会话级 gate 和 provider hook。
- `message_end`、`agent_start/end`、`turn_start/end`、`tool_execution_start/update/end`：agent lifecycle observer / transform。

这些 hook 点必须遵守统一边界：

- 不允许 hook 直接发布 UI event。
- 不允许 hook 直接 mutate session storage。
- 不允许 hook 直接执行 tool 或读 credential。
- 不允许工具参数原地改写后跳过校验；任何 args rewrite 必须重新 schema validate 和重新走 policy。
- 不把 TUI/GUI UI primitive 放进 core hook context；用户交互应通过 UI-safe command result / command output 或下游 adapter 能力表达，不能通过 hook 注入完整 `AgentCommand`。

pi coding-agent、Codex 和其他 Agent runtime 的扩展机制可以作为覆盖面参考，但 MiniCore 不承诺兼容其 hook 名称、payload、执行顺序或插件接口；本文件定义的 typed result、owner 和 failure policy 才是权威契约。

## Hook 类型

```rust
pub enum HookKind {
    Observer,
    Transform,
    Gate,
    Provider,
    Registrar,
}
```

| 类型 | 是否改变行为 | 用途 |
| --- | --- | --- |
| `Observer` | 否 | telemetry、audit、diagnostics、indexing |
| `Transform` | 是 | 改写 input、context、system prompt、provider payload、tool result |
| `Gate` | 是 | deny tool、cancel compaction、cancel session switch/delete |
| `Provider` | 是 | 提供 resource paths、compaction result、branch summary |
| `Registrar` | 是 | 注册 tools、command metadata 或 extension capabilities；provider/model 注册默认走 `ProviderRegistry` / settings |

Hook 能改变行为，但只能通过明确 hook point 的 typed result 改变。不要给 hook 一个万能 `ctx` 去随意调用 runtime internals。

## Registry And Ownership

```text
AgentRuntime
  ├─ owns RuntimeHookRegistry as a future runtime service
  ├─ invokes only workspace/runtime lifecycle hooks
  └─ emits official runtime events after applying results

Command / CommandManager
  ├─ invokes command catalog / resolve / UI-safe output hooks
  └─ never manufactures AgentCommand payloads from hook output

SessionRuntime
  ├─ holds future session-scoped RuntimeHookRegistry view
  ├─ invokes run / prompt / context / queue / compaction / persistence hooks
  └─ emits official session/run/tool/message events after applying results

Tools
  ├─ invokes tool governance hooks inside Tools::invoke_batch(...)
  └─ revalidates schema / sandbox / policy after any hook rewrite

ModelGateway
  ├─ invokes model/provider boundary hooks inside call_model(...)
  └─ keeps credentials, raw provider payload and provider SDK errors inside the gateway
```

`RuntimeHookRegistry` 可以保存 handler 集合、capability metadata、source info、timeout 策略和 error policy。它不拥有业务状态机，也不拥有 event metadata。

Owner 规则：谁拥有该安全点的不变量，谁调用 hook、应用 typed result、重新校验并负责 diagnostics。`Driver` 不调用 hook；它只把 Rig safe point 交还给 `DriverHost` / `SessionRuntime`。`RuntimeHookRegistry` 不拥有任何业务流程。后期 trust / capability gate 必须由 hook owner 在调用时按当前上下文计算；session-scoped hook 使用该 `SessionRuntime` 的 fixed workspace/cwd 和对应 resource trust summary，不能用 focused session 或全局默认 cwd。

Hook source 建议分级：

```rust
pub enum HookSource {
    Builtin,
    Test,
    UserGlobal,
    WorkspaceTrusted,
    Package,
}
```

Hook capability 建议显式声明：

```rust
pub enum HookCapability {
    Observe,
    DiscoverResources,
    RegisterCommand,
    RegisterTool,
    PatchCommandOutput,
    PatchPrompt,
    ReplacePrompt,
    TransformContext,
    PatchProviderPayload,
    RewriteToolArgs,
    DenyTool,
    ProvideCompactionResult,
    CancelSessionOperation,
    ObservePersistence,
}
```

默认策略：

- observer hook 可以更宽松；失败只进入 diagnostics。
- gate / transform / provider hook 必须受 trust 和 capability 控制。
- provider payload、context replacement、tool args rewrite、system prompt replace 和 session delete gate 都应是 privileged capability。

## Hook Context

每个 hook 拿到受限 context：

```rust
pub struct RuntimeHookContext {
    pub workspace_id: Option<WorkspaceId>,
    pub session_id: Option<SessionId>,
    pub run_id: Option<RunId>,
    pub command_id: Option<CommandId>,
    pub cwd: PathBuf,
    pub source: HookSource,
    pub trust: TrustLevel,
    pub capabilities: HookCapabilities,
    pub cancel: CancellationToken,
}
```

可提供受控读取能力：

- snapshot summary。
- resource summaries。
- session stats / context usage。
- current model / thinking / active tools summaries。
- append diagnostics helper。

不能提供：

- raw `EventBus.emit`。
- raw `SessionStorage` / JSONL handle。
- `ToolExecutor` handle。
- model credentials / `AuthStore` secret material。
- raw `ResourceManager` scanner bypass。
- Rig `AgentRun` object。

## Typed Results

不要用一个万能 `serde_json::Value` 表达 hook 结果。每个 hook point 应有 typed result。

通用形状：

```rust
pub enum HookControl {
    Continue,
    Cancel { reason: String },
    Deny { reason: String },
}

pub enum HookPatch<T> {
    Unchanged,
    Replace(T),
    Patch(T),
}
```

示例：

```rust
pub enum InputHookDecision {
    Continue,
    Transform { input: UserInput },
    Handled { output: Option<CommandOutput> },
    Reject { reason: String },
}

pub enum ToolHookDecision {
    Continue,
    Deny { reason: String },
    RewriteArgs { args: serde_json::Value },
    RequireApproval { reason: String },
    AbortRun { reason: String },
}

pub enum BeforeCompactDecision {
    Continue,
    Cancel { reason: String },
    PatchInstructions { instructions: String, replace: bool },
    ProvideResult { result: CompactionResult },
}
```

## Hook 点

### Workspace / Runtime

| Hook | 类型 | 能力 |
| --- | --- | --- |
| `ProjectTrustRequest` | Gate / Provider | decide trust / remember |
| `WorkspaceOpening` | Observer / Gate | audit 或阻止 workspace 打开 |
| `WorkspaceOpened` | Observer | telemetry / diagnostics |
| `WorkspaceClosing` | Observer | cleanup |
| `RuntimeDiagnosticsChanged` | Observer | telemetry / log |

### 资源

当前设计不定义资源 discovery / reload hook，也不预留资源回调接口形状。资源来源由 [ResourceManager](resource-manager.md) 的内置 resolver、trust gate 和 overlay policy 管理；显式 reload / startup ensure 进入 `ResourceManager` reload/recompose pipeline。未来如果确实需要 extension/package 资源声明，应另起 ADR 重新定义安全边界。

### Commands And Output

| Hook | 类型 | 能力 |
| --- | --- | --- |
| `CommandCatalogBuild` | Registrar | 注册 extension command metadata / argument hints；MVP 不允许 project-local manifest 注册可执行 handler |
| `CommandBeforeResolve` | Gate / Transform | 调整 availability 或拒绝 invocation；不能绕过 `resolve_for_execution` |
| `CommandOutputBuild` | Transform | patch `/status`、`/usage`、parse error 等 UI-safe 输出 |

Hook 只能返回 catalog/output patch；不能直接打开 UI，不能直接发 `command_output_appended`，不能制造 `AgentCommand`、handler id 或 executable action payload。所有动态 metadata 必须经过 UI-safe redaction。

### Input

| Hook | 类型 | 能力 |
| --- | --- | --- |
| `InputReceived` | Transform / Gate | transform / handled / reject |
| `InputBeforeSubmit` | Transform / Gate | prompt-like input 进入 session 前最后校验 |

`SubmitPrompt` 不默认解析 slash。`ExecuteCommandText` / `ExecuteCatalogCommand` 仍走 `CommandManager.resolve_for_execution`。Input hook 不能绕过 `CommandRunPolicy`、`PromptDelivery` admission 或 session phase guard。

### Session Lifecycle

| Hook | 类型 | 能力 |
| --- | --- | --- |
| `SessionBeforeOpen` | Gate | cancel |
| `SessionOpened` | Observer | observe |
| `SessionBeforeSwitch` | Gate | cancel |
| `SessionBeforeFork` | Gate / Transform | cancel / skip restore |
| `SessionBeforeTreeNavigate` | Gate / Provider | cancel / provide summary / override instructions |
| `SessionBeforeDelete` | Gate | cancel |
| `SessionClosed` | Observer | observe |

Hook 不直接创建 session files，不直接 mutate `SessionStorage`。`SessionManager` / `SessionRuntime` 执行最终动作并发 session events。

### Prompt / Context / System Prompt

| Hook | 类型 | 能力 |
| --- | --- | --- |
| `BeforeAgentStart` | Transform | transform/gate structured current input、patch stream options |
| `PromptBuilt` | Transform | append system section 或 privileged replace；应用后重建 `PromptCallProfile` |
| `ContextProjection` | Transform / Provider | 返回 `CurrentRun` / `CurrentCall` 的 `ContextMaterialContribution::Available/Unavailable`；privileged capability 才可 replace messages |
| `AfterTurnStateBuild` | Observer | inspect stable turn state / PromptTurn summary；不能把资源 snapshot 扩大进 `DriverTurnInput` |

提示词注入应该返回 typed result，由 `SessionRuntime` 应用后交给 Prompt 最终组装，而不是让 UI、hook 或下游代码直接提交 system/messages。

- 企业 policy / coding style：优先作为 required policy view 或 `PromptBuilt` append；system replacement 需要 privileged `ReplacePrompt` capability。
- `PromptBuilt` 变化后必须重建 contribution stamps、fingerprint 和完整 `PromptCallProfile`，不能只改字符串。
- RAG / memory / issue context：成功时返回带 source、content hash、persistence 和 requirement 的 `ContextMaterial`；失败时返回同 key/source/requirement 的 `Unavailable`，不能省略失败项。`Required` 失败由 Prompt fail closed，`Optional` 失败进入 projection diagnostics。
- privileged message replacement 仍必须经过 `PromptTurn.project_model_call()`，保证没有 orphan tool result、unresolved tool call 或 current-input loss。
- resource-driven prompt material：由 `ResourceManager` 内置 resolver 提供；hook 不直接读文件，也不绕过 captured `PromptResourceView` 注入资源。

### Run Safe Points

| Hook | 类型 | 能力 |
| --- | --- | --- |
| `RunBeforeStart` | Gate / Observer | cancel 或 observe |
| `BeforeNextModelCall` | Transform / Gate | 形成组合式 `NextModelCallPlan`：drain queue、profile/model options、typed context、pause/finish |
| `BeforeRunFinish` | Gate / Transform | decide finish/pause/follow-up |
| `RunFinished` | Observer | telemetry / diagnostics |

这些 hook 必须由 `SessionRuntime` 调用。`Driver` 不直接暴露 hook 给外部；它只消费 `NextModelCallPlan`，再调用 Prompt 生成最终 `ModelInputProjection`。

### Model / Provider

这些 hook 的 owner 是 `ModelGateway`。`SessionRuntime` 只在进入 `DriverHost::call_model(...)` 前拥有 run safe point；一旦形成 `ModelCallRequest` 并进入 `ModelGateway.call_model(...)`，provider-neutral request patch、provider payload patch、provider response observer 和 usage normalization observer 都必须留在 `ModelGateway` 内。当前 MVP 不实现这些 hook；后期若先开放，也应先开放 provider-neutral `BeforeModelCall`，继续禁用 raw provider payload patch。

| Hook | 类型 | 能力 |
| --- | --- | --- |
| `BeforeModelCall` | Transform | patch model options、stream options、thinking level |
| `BeforeProviderPayload` | Transform | privileged redacted provider payload patch；当前 MVP 不开放 |
| `AfterProviderResponse` | Observer | status / allowlisted headers |
| `ProviderUsageNormalized` | Observer | telemetry |

不要暴露 API key、OAuth token、authorization header、credential store 或 raw provider response。`BeforeProviderPayload` 应是 privileged hook；如果私有 Rig adapter 无法提供稳定且脱敏的 payload 形态，就不能开放 raw provider payload patch。

### Tools

这些 hook 的 owner 是 `Tools`，但 UI-visible event、session write 和 snapshot projection 仍由 `SessionRuntime` 归约。

| Hook | 类型 | 能力 |
| --- | --- | --- |
| `ToolCallProposed` | Observer | observe model-requested tool call |
| `ToolBeforePolicy` | Gate / Transform | deny、rewrite args、require approval、abort |
| `ToolBeforeExecute` | Gate / Observer | final deny / observe |
| `ToolOutputChunk` | Observer | progress telemetry |
| `ToolAfterExecute` | Transform | patch/redact normalized result |
| `ToolResultBeforeAppend` | Transform | patch tool result message draft |

关键规则：

- `RewriteArgs` 后必须重新 schema validate。
- `RewriteArgs` 后必须重新 canonicalize path、重新构造 preview、重新走 sandbox check。
- `RewriteArgs` 后必须重新走 `ToolPolicy`。
- approval 之后不允许再改 args。
- hook 不能直接调用 `ToolExecutor`。
- tool hook failure 默认 fail closed：before-policy 出错应 deny tool 或 fail command，取决于 hook 点。

### Queue

| Hook | 类型 | 能力 |
| --- | --- | --- |
| `QueueBeforeEnqueue` | Gate / Transform | allow / transform / reject |
| `QueueBeforeDrain` | Transform | reorder / coalesce / one-at-a-time policy |
| `QueueUpdated` | Observer | observe |

Hook 不能直接 mutate queue；返回 decision，由 `SessionRuntime` 应用并发 `queue_updated`。

### Compaction

| Hook | 类型 | 能力 |
| --- | --- | --- |
| `SessionBeforeCompact` | Gate / Provider / Transform | cancel、patch instructions、provide result |
| `CompactionPromptBuilt` | Transform | privileged prompt patch |
| `CompactionResultProduced` | Observer / Transform | inspect or patch result |
| `SessionCompact` | Observer | observe final compaction entry |

压缩摘要是 session context projection，不是 system prompt。Hook 不能留下 orphan tool call/result，不能污染 overflow retry context。

### Persistence

| Hook | 类型 | 能力 |
| --- | --- | --- |
| `AfterSavePoint` | Observer | sync、index、backup、telemetry |

MVP 不建议开放 `BeforeSessionWrite`。session write 是一致性边界，外部 hook 不应改 entry id、parent id、leaf 或 raw JSONL。

### Usage And Diagnostics

| Hook | 类型 | 能力 |
| --- | --- | --- |
| `UsageUpdated` | Observer | telemetry / budget warning |
| `ContextUsageComputed` | Observer / privileged config patch | observe 或调整 estimator config |
| `DiagnosticEmitted` | Observer / Transform | tag / add detail |
| `DiagnosticsChanged` | Observer | telemetry |

UI 仍只消费 `usage_updated`、diagnostics events 和 snapshot；hook 不直接向 UI 报 usage。

## 执行顺序

建议稳定执行顺序：

```text
Builtin hooks
  → test hooks
  → user/global hooks
  → workspace trusted hooks
  → package hooks
```

冲突规则：

- Gate 类 hook：deny/cancel wins。
- Tool policy：most restrictive wins；approval requirement wins over plain allow。
- Transform 类 hook：按顺序 chain，每次 transform 后在对应 owner 中重新 validate。
- Provider 类 hook：必须定义合并规则，例如 resources paths append、compaction result first-wins 或 highest-priority-wins。
- Observer 类 hook：错误只进入 diagnostics，不影响主流程。

## Error / Timeout / Cancellation

每个 hook point 必须有 error policy、timeout 和 cancellation。

```rust
pub enum HookErrorPolicy {
    IgnoreAndDiagnose,
    FailCommand,
    DenyTool,
    CancelOperation,
}
```

建议：

| Hook 类型 | 默认错误策略 |
| --- | --- |
| Observer | ignore + diagnostics |
| Command output patch | keep original + diagnostics |
| Input transform | reject command 或 continue，按 source/capability 配置 |
| Tool before policy | deny tool |
| Tool after result | keep original result + diagnostics |
| Before provider payload | fail model call 或 keep original，按 trust/capability 配置 |
| Before session switch/delete | cancel operation |
| Compaction provide result | cancel compaction 或 continue original，按 hook point 配置 |

## 不应 Hook 出来的边界

不要暴露：

- `EventBus.emit(...)`。
- `event_id` / `sequence` 分配。
- `agent_runtime_protocol::Event` 外层记录构造。
- raw `SessionStorage` / JSONL write。
- `SessionStorage` leaf/index mutation。
- `ToolExecutor` direct handle。
- `ModelGateway` raw credentials。
- `AuthStore` secret material。
- `ResourceManager` raw file scanner bypass。
- Rig `AgentRun` object。
- `DriverEvent` direct stream。
- `Tools` 未归一化内部进度。
- `persistence_save_point` 伪造。

## 后期需求和演进

当前 MVP 不实现 hook system。核心流程应先通过直接 owner 逻辑和测试稳定下来：`SessionRuntime` 直接编排 run/prompt/compaction/persistence，`Tools` 直接治理工具，`ModelGateway` 直接治理 provider 调用，`Command` / `CommandManager` 直接处理 command output。

后期第一批 hook points 应只接入已经稳定的 owner 流程：

- `BeforeAgentStart`
- `PromptBuilt`
- `ContextProjection`（优先 typed `ContextMaterialContribution`）
- `ToolBeforePolicy`
- `ToolAfterExecute`
- `SessionBeforeCompact`
- `AfterSavePoint`
- `CommandOutputBuild`

再后续开放 trusted package / extension hooks：

- command pack / candidate provider registration
- command output patch
- observer hooks
- tool registration

最后才开放 privileged hooks：

- raw provider payload patch（仅在可以保证 redacted 且 adapter shape 稳定后）
- context replacement
- tool args rewrite
- system prompt replacement
- session delete/switch gates
- before-session-write 类能力

MiniCore 的原则是：先把 hook seam 作为 harness core 的可测试能力做深，再决定哪些 hook source 可以被下游产品或 extension 暴露给用户。
