# Prompt

> 状态：pre-refactor implementation contract。当前PromptService、PromptSet和ordered ModelInstruction/ModelMessage `AssembledModelContext`以[Prompt子系统架构设计](../refactor/prompt-subsystem.md)为权威。

`Prompt` 是 MiniCore 的无状态提示词与模型上下文组装深模块，对应未来的 `prompt.rs` / `prompt/`。它是 **唯一模型可见上下文组装 seam**：所有进入模型的 system prompt、工具 schema、已提交会话消息、用户输入展开结果、动态 context、输出契约和调用目的，都必须经过 Prompt 的 typed API 组装、排序、校验和 fingerprint。

Prompt 不持有 `ResourceManager`、`Tools`、`SessionStorage`、provider、auth、queue 或 context provider handle。`SessionRuntime` 仍是 Pull Master：它决定何时捕获资源和工具 profile、何时读取 committed conversation、何时消费队列、何时收集 transient context，然后把这些 owner 已经捕获或授权的 typed inputs 传给 Prompt。Prompt 只做确定性纯组装。

一句话边界：

```text
ResourceManager captures stable resource input.
Tools captures stable tool input.
Prompt prepares messages and assembles model-visible context.
```

## 设计定位

当前 prompt 相关输入来自明确 owner：

```text
ResourceManager  → TurnResourceSnapshot       // 非工具稳定资源输入
Tools            → TurnToolProfile            // prompt_view + run-only executor + shared fingerprint
SessionStorage   → ordered committed conversation
SessionRuntime   → PromptIntent、delivery、run safe-point plan、purpose
future context owners → ContextMaterialContribution
ModelGateway     → provider payload / role mapping / auth injection boundary
```

Prompt 公开三个核心 seam：

```text
Prompt.prepare_message_turn(PromptResourceView + ToolPromptView)
  -> PreparedMessageTurn

PreparedMessageTurn.compose_user_message(PromptIntent)
  -> CanonicalUserMessage

Prompt.assemble_model_context(ModelContextProfile + ordered committed conversation + transient context + purpose)
  -> AssembledModelContext
```

`PreparedMessageTurn` 暴露的 `ModelContextProfile` 是窄 profile：它绑定 system prompt、active tool schemas、工具 fingerprint、贡献来源和 profile fingerprint，但不暴露资源 current pointer、工具 owner、storage 或 provider handle。

这些 owner 不主动调用 Prompt，也不向 Prompt 推送失效通知。新的目标 user turn / work chain 边界由 `SessionRuntime` 统一拉取稳定输入：

```text
ResourceManager.capture_turn_resources(..., typed owner projections)
Tools.capture_turn_tools(...)
SessionRuntime reads ordered committed conversation when needed
  → Prompt.prepare_message_turn(...)
  → PreparedMessageTurn
  → PreparedMessageTurn.compose_user_message(PromptIntent)
  → CanonicalUserMessage
  → commit user message if this is model-visible user input
  → Prompt.assemble_model_context(ModelContextProfile, ordered committed conversation, transient context, purpose)
  → AssembledModelContext
```

MVP 不执行 turn 内 profile mutation。后续合法 safe point 不得再次读取 `ResourceManager` current pointer，也不得用 Tools 的单独 getter 拼装工具字段；只能基于 active captured resource/tool input 或明确 step override，在同一 actor transaction 中整体替换 `ModelContextProfile` 和 future tool executor，并保持 fingerprint 一致。

这样 resource reload、active tools 变化、model change 和 queued prompt 不会分别触发互相竞争的局部 prompt rebuild。同一 work chain 的 profile 固定；automatic retry、overflow recovery 和同一 `RunId` segment rollover 复用同一 captured input。

## 模块结构

建议代码布局：

```text
src/
  prompt.rs
  prompt/
    prepared_turn.rs
    system.rs
    intent.rs
    model_context.rs
    validation.rs
    provenance.rs
```

公开 facade 保持很小：

```rust
pub fn prepare_message_turn(
    input: PrepareMessageTurnInput,
) -> Result<PreparedMessageTurn, PromptError>;

pub fn assemble_model_context(
    input: AssembleModelContextInput<'_>,
) -> Result<AssembledModelContext, PromptError>;

pub struct PrepareMessageTurnInput {
    pub resources: PromptResourceView,
    pub tools: ToolPromptView,
}

impl PreparedMessageTurn {
    pub fn model_context_profile(&self) -> &ModelContextProfile;

    pub fn compose_user_message(
        &self,
        intent: &PromptIntent,
    ) -> Result<CanonicalUserMessage, PromptError>;
}
```

`PreparedMessageTurn` 负责 pin captured resource view、展开 resource-backed intent 并提供同版 `ModelContextProfile`。它不持有 `TurnToolProfile` 或 `ToolBatchInvoker`；`SessionRuntime` 只把同一 `TurnToolProfile` 中的 `ToolPromptView` 交给 Prompt，把 executor 留给 run path。最终模型上下文组装也是 Prompt 模块的纯函数，因为其最小充分输入是 profile、有序 committed conversation、transient context 和 purpose，而不是完整 `PreparedMessageTurn`。内部可以继续调用平级的 `skills.rs` 和 `prompt_templates.rs` helper，但这些 helper 不成为调用方必须学习的新 seam。

## Owner 分层

| 能力 | Owner |
| --- | --- |
| resource roots、trust、overlay、cwd revision、content hash、cwd reload | `ResourceManager` |
| runtime product/user-global defaults 初始化 | `OpenWorkspace` / `ResourceManager` |
| behavior/model/environment/policy typed projection 的 owner state | `SessionRuntime` 及其对应 owner |
| turn resource projection freeze | `ResourceManager.capture_turn_resources(...)` |
| tool governance、approval、sandbox、executor、active set | `Tools` |
| turn tool prompt-view/executor freeze | `Tools.capture_turn_tools(...)` |
| skill/template metadata、正文和 catalog | captured resource snapshot |
| skill/template 解析与格式化算法 | `skills.rs` / `prompt_templates.rs` |
| turn/run/queue/phase、何时组装 | `SessionRuntime` |
| system prompt、intent 展开、model context 组装、组合校验 | `Prompt` |
| ordered committed conversation 和 compaction projection | `SessionStorage` / `SessionHandle` |
| 动态 RAG/memory/IDE context 获取 | future hook/context owner |
| provider payload、role mapping、cache header | `ModelGateway` |

Prompt 不拥有 resource catalog lifecycle、session history lifecycle、queue、provider client、tools owner handle、storage handle 或 context provider registry。

## TurnResourceSnapshot 与 TurnToolProfile

Prompt 不直接接收 `ResourceManager` handle、完整 `TurnResourceSnapshot` 或 current snapshot pointer。`ResourceManager.capture_turn_resources(...)` 返回 captured `TurnResourceSnapshot`，`SessionRuntime` 再把其窄 `PromptResourceView` 交给 Prompt。Prompt 只能通过该 view 读取对模型安全的 materials、behavior、model、environment、policy、skill/template catalog、source info、content hash、cwd revision 和 fingerprint。

Prompt 也不直接接收 `Tools` owner handle。`Tools.capture_turn_tools(...)` 返回由 [Tools](tools.md) 定义的 `TurnToolProfile { prompt_view, executor, fingerprint }`；Prompt 只读取 `prompt_view`，run path 只读取 `executor`，三者 fingerprint 必须一致。Prompt 不能 reload、比较 current revision、读文件，也不能定义第二套 `ResourceKey` / `ContentHash` / `ResourceSourceInfo`。工具 active set、审批和 executor owner 继续属于 Tools。

## PreparedMessageTurn

`PreparedMessageTurn` 是 turn-scoped 不可变准备值：

```rust
pub struct PreparedMessageTurn {
    resources: PromptResourceView,
    profile: ModelContextProfile,
    contribution_stamps: Arc<[PromptContributionStamp]>,
    fingerprint: PromptFingerprint,
}
```

它不是长期 service：

- 不读文件。
- 不监听 reload。
- 不持有 queue 或 session storage。
- 不执行 Hook、工具或模型调用。
- 不原地 mutate。
- 不跨 turn 缓存 current resources。

running turn 中的普通 resource reload 不替换 active `PreparedMessageTurn`。MVP 在 `Turn` 中继续拒绝 model、thinking、stream options、active tools 和 profile mutation，避免 `ModelContextProfile` 与 `TurnToolProfile.executor` 分裂。

后续 full version 若允许 safe-point mutation，`SessionRuntime` 必须通过明确 step override，在同一 actor transaction 中整体替换 `CurrentRun.prepared_turn` / `NextConversationStep.model_context_profile` 与 future tool executor。旧 `PreparedMessageTurn` 不原地修改，system prompt 和 tool schemas 也不能分别 patch；replacement profile、future executor 和 fingerprint 必须一致。

## ModelContextProfile

`ModelContextProfile` 把必须保持一致的模型可见调用基线绑定在一起：

```rust
pub struct ModelContextProfile {
    pub system_prompt: Arc<str>,
    pub active_tool_schemas: Arc<[ToolSchema]>,
    pub tool_profile_fingerprint: ToolProfileFingerprint,
    pub contribution_stamps: Arc<[PromptContributionStamp]>,
    pub fingerprint: ModelContextProfileFingerprint,
}
```

`DriverTurnInput` 携带整个 `ModelContextProfile`，不分别携带 `system_prompt` 和 `active_tool_schemas`。`tool_profile_fingerprint` 必须等于 captured `TurnToolProfile.fingerprint`，也供 `SessionDriverHost` 校验 replacement executor fingerprint；这样切换 active tools 时，不会出现 system prompt 声明工具 A、provider request 或执行 executor 却暴露工具 B 的 split-brain。

`PromptContributionStamp` 对资源来源复用 canonical resource identity，对非资源来源使用各自版本或 fingerprint：

```rust
pub enum PromptContributionStamp {
    Resource(ResourceContribution),
    Behavior(BehaviorVersion),
    Model(ModelPromptVersion),
    Environment(EnvironmentVersion),
    Policy(PolicyPromptVersion),
    ToolProfile(ToolProfileFingerprint),
    Transient(ContextMaterialStamp),
}
```

Prompt 不重新定义 `ResourceKey`、`ContentHash` 或 `ResourceSourceInfo`。

## System Prompt 组装

system prompt 是 `PreparedMessageTurn` 创建时的确定性纯构建结果，但它只是 model context profile 的一部分，不是整个 Prompt 模块的唯一能力。

推荐 section 顺序：

```text
custom system prompt 或 product default base
  → behavior / agent profile
  → required policy / guardrails
  → append system prompts
  → project/context files
  → environment
  → tool guidelines / tool descriptions
  → visible skill summaries
  → future MCP server instructions
```

约束：

- custom system prompt 替换 base，但不绕过 required policy sections。
- context files 保留 canonical source path。
- skill summary 仅在当前 tool profile 允许模型实际加载技能时显示。
- tools、skills、context files 和 contributions 使用稳定排序。
- 对 system-level resource 应执行大小限制、控制字符检查和必要的隐藏 Unicode tag 清洗；普通用户正文不能被任意改写。
- provider-specific role、cache-control header 和 payload shape 不在这里处理。

## PromptIntent 与 CanonicalUserMessage

所有 prompt-producing 入口先归一为结构化 `PromptIntent`：

```rust
pub enum PromptIntent {
    Text(TextIntent),
    Skill(SkillIntent),
    Template(PromptTemplateIntent),
    Composite(CompositePromptIntent),
}
```

队列只保存 intent 所需的稳定引用和参数，例如 resource key、arguments、additional instructions 和附件 metadata / immutable reference。队列不能保存 raw slash command text，也不能提前保存 skill/template 展开正文。

`PreparedMessageTurn.compose_user_message()` 使用自己 pin 住的 captured inputs：

- `Steer` 使用 active `PreparedMessageTurn`，因此使用 active turn snapshot 和 active tool profile。
- idle submission、`FollowUp`、`NextTurn` 在目标 future turn 创建新 `PreparedMessageTurn` 后展开。
- snapshot 缺少资源时返回结构化 unavailable error，不能重新读文件或静默切换 delivery。

展开结果是 canonical user message，而不是裸字符串：

```rust
pub struct CanonicalUserMessage {
    pub message: MessageRecord,
    pub contribution_stamps: Arc<[PromptContributionStamp]>,
    pub fingerprint: CanonicalUserMessageFingerprint,
}

pub enum UserMessagePart {
    Text(String),
    Image(ImageAttachment),
    File(FileAttachment),
    SelectedCode(SelectedCode),
    SkillBlock(ComposedSkillBlock),
    ResourceBlock(ComposedResourceBlock),
}
```

`UserMessagePart` 是 `MessageRecord` 的 canonical content part，不是与 `message` 并列的第二份状态。`compose_user_message()` 可以在实现内部使用临时 part IR，但返回值只保留一条 canonical `MessageRecord`。MVP 可以先只实现 `Text` / `SkillBlock`，但 interface 不应把所有输入设计死成裸 `String`。附件是否被目标模型支持，由 Prompt 根据 captured model capability 校验，并由 `ModelGateway` 做 provider encoding；两者都不能重新解释这条 message 的语义。

显式 skill 调用遵循：slash skill / catalog selection 先解析为 `PromptIntent::Skill`；目标 turn 从 captured skill body 展开成一条 `CanonicalUserMessage`；该 message 作为普通 user message 提交到 session storage。它不是 system prompt，也不是隐藏上下文。

## User turn / work chain capture 规则

`SessionRuntime` 对一个新的显式 user turn / work chain 捕获一次 `TurnResourceSnapshot`、一次 `TurnToolProfile`，并创建一次 baseline `PreparedMessageTurn`。

复用同一 captured turn 的情况：

- automatic retry；
- context overflow compaction recovery；
- active `Steer`；
- 同一 `RunId` 下 Rig segment rollover。

必须重新捕获的情况：

- `FollowUp` 启动后续 work；
- `NextTurn` 被下一次显式 prompt 消费；
- 新 idle prompt；
- 其它新的显式 user turn / work chain。

Resource reload 只影响新的显式 user turn / work chain。已经展开并持久化的 skill/template invocation 是历史 user message，后续 reload 不改写它。

相同 captured input 必须产生相同 `PreparedMessageTurn`、`CanonicalUserMessage`、`ModelContextProfile` 和 `AssembledModelContext` fingerprint。同一 work chain 的 profile 固定。

## ContextMaterial

动态 context 不是 ResourceManager 的静态资源，也不能用无来源字符串表达：

```rust
pub struct ContextMaterial {
    pub key: ContextMaterialKey,
    pub content: MessageContent,
    pub source: ContextSource,
    pub persistence: ContextPersistence,
    pub requirement: ContextRequirement,
    pub content_hash: ContentHash,
}

pub enum ContextPersistence {
    CurrentRun,
    CurrentCall,
}

pub enum ContextRequirement {
    Required,
    Optional,
}

pub enum ContextMaterialContribution {
    Available(ContextMaterial),
    Unavailable {
        key: ContextMaterialKey,
        source: ContextSource,
        persistence: ContextPersistence,
        requirement: ContextRequirement,
        diagnostic: PromptDiagnostic,
    },
}
```

context owner 负责执行 I/O，并把成功或失败都转换成 `ContextMaterialContribution`；Prompt 不调用 provider。若 owner 只省略失败项，Prompt 无法区分“未配置”与“required source 获取失败”，因此禁止用 `Vec<ContextMaterial>` 的缺项表示失败。`Required + Unavailable` 阻止模型调用，`Optional + Unavailable` 进入 diagnostics 后继续。

项目文件、AGENTS/context files、skills 和 prompt templates 不能绕过 `ResourceManager` 伪装成 transient context 再注入一次。

## 模型上下文组装

每次模型调用只接受三类输入：

```text
ModelContextProfile
ordered committed conversation
transient context + purpose
```

推荐 interface：

```rust
pub struct AssembleModelContextInput<'a> {
    pub profile: &'a ModelContextProfile,
    pub committed_conversation: &'a [MessageRecord],
    pub transient_context: &'a [ContextMaterialContribution],
    pub output_contract: Option<&'a OutputContract>,
    pub purpose: ModelCallPurpose,
}
```

因果规则：

- `committed_conversation` 来自 `CommittedConversationState`；该热视图在 session open/recovery 时由 `SessionHandle.load_committed_conversation()` 建立，并且只应用成功 commit 返回的 delta。它按当前 leaf 的 committed stable batch 顺序排列，可以包含 compaction summary、retained suffix、完整 tool call/result 以及已经提交的 canonical user message。
- 新用户输入或 active steer 必须先通过 `compose_user_message(...)` 形成 `CanonicalUserMessage` 并成功提交，再出现在 ordered committed conversation 中；Prompt 不维护单独 protected input lane。
- `CurrentRun` context 可供同一 run 后续 calls 复用，但不会自动变成会话历史。
- `CurrentCall` context 只影响一次 call，绝不持久化。
- `OutputContract` 描述 JSON schema、response format、tool choice 等调用契约，不应伪装成普通 prompt text。
- `OutputContract::NoToolCalls` 可在保留稳定 tool-schema/profile 前缀时禁止本次调用产生工具调用；provider 无法保证时返回 capability error。

## AssembledModelContext

最终输出是唯一可进入 `ModelCallRequest` 的模型可见上下文：

```rust
pub struct AssembledModelContext {
    pub system_prompt: Arc<str>,
    pub messages: Arc<[ModelMessage]>,
    pub tools: Arc<[ToolSchema]>,
    pub output_contract: Option<OutputContract>,
    pub contribution_stamps: Arc<[PromptContributionStamp]>,
    pub diagnostics: Arc<[PromptDiagnostic]>,
    pub fingerprint: AssembledModelContextFingerprint,
}
```

`Driver` 使用 run-local `LiveConversation`、当前 `ModelContextProfile` 和 `NextConversationStep` 中的 transient context 调用 `Prompt.assemble_model_context(...)`，通过校验后才构造 `ModelCallRequest`。`MessageRecord -> ModelMessage` 的唯一转换发生在这里；Driver 可以 clone、整体替换和借用 profile，但不能为了组装模型请求获得 `PreparedMessageTurn`、resource snapshot 或 tool owner，也不能绕过 Prompt 直接把 profile 字段拼成 provider request。

## 最终校验

`assemble_model_context()` 必须集中执行组合级校验：

- 不存在 orphan tool result。
- 不存在被非法截断的 unresolved tool call。
- 本次已经提交的 canonical user message 未被遗漏、摘要或放到 tool call/result 中间。
- system prompt 和 tool schemas 必须同时来自 `input.profile`，不能从不同 revision 分别传入。
- source key 唯一，section 与 contribution 排序确定。
- 同一静态资源没有通过 resource material 和 transient material 重复注入。
- `CurrentCall` context 不被标记为待持久化。
- required contribution 缺失时失败。
- 输入总大小和估算 token 不超过配置的最终组装上限；超限时返回结构化 `PromptError::ContextLimitExceeded { estimated_input_tokens, effective_limit }`，Prompt 不调用 provider，也不自己压缩。

这里是每次 `AgentRunStep::CallModel` 前的 context assembly validation，不是 `SubmitPrompt` admission。它覆盖首次调用、tool result 后续调用和 Steer rollover；因此可能发生在 `run_started` 之后。

相同输入必须得到相同 fingerprint。fingerprint 用于 diagnostics、`ResourceQuery::GetEffectivePrompt`、测试和后续 provider prompt cache 分段判断，不是 secret-bearing payload。

## 与 Driver Safe Point 的关系

`DriverHost.commit_pending_messages()` 在下一次模型调用前返回组合式 step，而不是互斥 decision：

`NextConversationStep` 的权威字段由 [Driver](driver.md) 定义。Prompt 只关心其中已验证的 replacement `ModelContextProfile` 和单一 `transient_context` 列表；只有成功 commit 返回的 `CommittedConversationDelta` 才能先被 Driver 应用到 `LiveConversation`，之后才允许下一次 `assemble_model_context()`。MVP 拒绝 model/thinking/stream/active-tools/profile mutation；full version 若启用 safe-point mutation，replacement profile 只能用 active captured input 或 step override 原子重建。scope 保存在每个 context material 内，接口不再拆成 run/call 两个 vector。

## 与 Compaction 的关系

Compaction 不属于 Prompt 子系统：

```text
Compaction
  → 决定 cut point、protected EntryIds、CompactionSummaryDirective

Prompt
  → 复用稳定 ModelContextProfile / conversation prefix
  → 追加 directive instruction，并应用 OutputContract::NoToolCalls
  → assemble_model_context(..., purpose = CompactionSummary)

SessionRuntime / ModelGateway
  → generate_model_turn → commit Compaction
  → apply committed delta → build new ConversationSeed
```

压缩摘要是 committed conversation 的替代消息，不是 system prompt。当前启动边界上新提交的 canonical user message 不参与同一边界上的压缩。

## 与 RuntimeHooks 的关系

当前 MVP 不实现 hooks。后期启用时：

- hook owner 获取动态材料并返回 typed `ContextMaterialContribution` 或受控 profile patch。
- `SessionRuntime` 在 owner safe point 应用 hook result，并把完整 profile/context plan 回复 Driver；Driver 随后调用 `Prompt.assemble_model_context(...)` 做最终组装和重新校验。
- privileged context replacement 也必须回到 `Prompt.assemble_model_context(...)`，不能直接提交 provider request。
- resource-driven material 仍必须来自 captured `TurnResourceSnapshot`。

Prompt 不持有 `RuntimeHookRegistry`，也不异步调用 hook。

## 为什么不是 PromptManager / ContextManager

当前没有独立状态需要长期 manager 持有：resources、history、queues、tools、model 和 provider 都已有 owner。长期 PromptManager 会复制 revision/cache invalidation；长期 ContextManager 会与 append-only session history 和 Compaction 形成第二份 context source of truth。

因此 MVP 只实现无状态 Prompt 子系统、immutable `PreparedMessageTurn` 和纯 `assemble_model_context`。未来只有在多个异步 context provider、跨 call working set、动态 token budget、后台 distillation 等真实需求出现后，才考虑 session-scoped `ContextWorkspace`。

## 性能原则

- system prompt 每个 `PreparedMessageTurn` 构建一次。
- 使用 `Arc<str>` / `Arc<[ToolSchema]>` 复用稳定 profile。
- 未发生 context assembly 时可以复用已有消息 slice/Arc。
- 不在 MVP 引入 Prompt LRU 或全局 cache；模型调用成本远高于确定性字符串拼装。
- 稳定排序和 fingerprint 为后续 provider cache 留接口，但 provider cache headers 仍由 `ModelGateway` 处理。

## 测试重点

- 同 captured input 产生相同 system prompt、profile fingerprint、canonical user message fingerprint 和 assembled context fingerprint。
- active turn cwd rev-1、current cwd rev-2 时，Steer 仍解析 rev-1。
- FollowUp / NextTurn / new idle prompt 在 future `PreparedMessageTurn` 中解析 rev-2。
- automatic retry、overflow compaction recovery 和同 RunId segment rollover 复用原 turn resources 和 tool profile。
- active snapshot 缺少 skill/template 时明确失败，不重新读磁盘。
- MVP 在 active turn 中拒绝 model/thinking/stream/active-tools/profile mutation。
- full-version safe-point replacement 同时替换 `ModelContextProfile`、future tool executor 和 fingerprint。
- Driver 只用 `ModelContextProfile` 即可完成 context assembly，不需要构造或持有 `PreparedMessageTurn` / resource snapshot。
- compaction summary + retained suffix + committed canonical user message 顺序稳定，新用户消息不被摘要。
- tool call/result 完整性校验覆盖 abort/retry/compaction 后上下文。
- `CurrentCall` context 不进入 session storage。
- required/optional context failure policy 可测，失败项不会因 vector 缺项而丢失。
- template + 多 skill + attachment + output contract 的组合顺序确定。
- Prompt 不调用 ResourceManager current pointer、不读文件、不持有 provider/auth/queue/storage handle。