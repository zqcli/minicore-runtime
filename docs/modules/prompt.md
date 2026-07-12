# Prompt

`Prompt` 是 MiniCore 的无状态提示词组装子系统，对应未来的 `prompt.rs` / `prompt/`。它把已经由各 owner 捕获、校验或解析的输入确定性地组装成一次 turn 的 `PromptTurn`，并在每次模型调用前生成协议安全的 `ModelInputProjection`。

`Prompt` 不是 workspace-global `PromptManager`，也不是长期持有会话历史的 `ContextManager`。`SessionRuntime` 仍是 Pull Master：它决定何时捕获资源、读取会话上下文、消费队列和收集动态上下文，并调用 Prompt 的 turn assembly / intent seam；Driver 消费 actor 返回的 profile/context plan，并调用 Prompt 的纯 model-call projection seam。Prompt 只负责解释、排序、展开、投影和校验。

一句话边界：

```text
ResourceManager admits and versions resources.
SessionRuntime decides when to assemble.
Prompt deterministically assembles model-visible input.
```

## 设计定位

当前 prompt 相关输入来自多个 owner：

```text
ResourceManager  → PromptResourceView
Tools            → ToolPromptView
ModelState       → ModelPromptView
Product/Agent    → ProductPromptView / AgentPromptView
Policy/Environment → PolicyPromptView / EnvironmentPromptView
SessionHandle    → durable history
SessionRuntime   → current input、delivery、run safe-point plan
future context owners → ContextMaterialContribution
```

这些 owner 不主动调用 Prompt，也不向 Prompt 推送失效通知。`SessionRuntime` 在 turn start 或 run safe point 统一拉取稳定 view：

```text
ResourceManager.capture_turn(...)
Tools.prompt_view()
ModelState.prompt_view()
SessionHandle.build_session_context(...)
  → SessionRuntime
  → prompt::begin_turn(...)
  → PromptTurn
```

这样 resource reload、active tools 变化、model change 和 queued prompt 不会分别触发互相竞争的局部 prompt rebuild。

## 模块结构

建议代码布局：

```text
src/
  prompt.rs
  prompt/
    turn.rs
    system.rs
    intent.rs
    projection.rs
    validation.rs
    provenance.rs
```

公开 facade 保持很小：

```rust
pub fn begin_turn(
    input: TurnPromptInputs<'_>,
) -> Result<PromptTurn, PromptError>;

pub fn project_model_call(
    input: ModelCallProjectionInput<'_>,
) -> Result<ModelInputProjection, PromptError>;

impl PromptTurn {
    pub fn profile(&self) -> &PromptCallProfile;

    pub fn resolve_intent(
        &self,
        intent: &PromptIntent,
    ) -> Result<ResolvedPromptInput, PromptError>;
}
```

`PromptTurn` 负责 pin captured resources、展开 resource-backed intent 并提供原子 `PromptCallProfile`；最终模型调用投影是 Prompt 模块的纯函数，因为其最小充分输入是 profile 加四类 call-time lanes，而不是完整 `PromptTurn`。内部可以继续调用平级的 `skills.rs` 和 `prompt_templates.rs` helper，但这些内部 helper 不成为调用方必须学习的新 seam。

## Owner 分层

| 能力 | Owner |
| --- | --- |
| resource roots、trust、overlay、revision、content hash、reload | `ResourceManager` |
| skill/template metadata、正文和 catalog | captured resource snapshot |
| skill/template 解析与格式化算法 | `skills.rs` / `prompt_templates.rs` |
| turn/run/queue/phase、何时组装 | `SessionRuntime` |
| system prompt、intent 展开、call projection、组合校验 | `Prompt` |
| durable history 和 compaction projection | `SessionHandle` / `SessionStorage` |
| 动态 RAG/memory/IDE context 获取 | future hook/context owner |
| provider payload、role mapping、cache header | `ModelGateway` |
| tool governance、approval、sandbox、executor | `Tools` |

Prompt 不拥有 resource catalog、session history、queue、provider client 或 context provider registry。

## PromptResourceView

Prompt 不直接接收 `ResourceManager` handle，也不访问 current snapshot pointer。`ResourceManager.capture_turn(...)` 返回 captured `TurnResourceSnapshot` 后，由资源模块提供只读窄投影：

```rust
pub struct PromptResourceView {
    snapshot: Arc<TurnResourceSnapshot>,
}

impl PromptResourceView {
    pub fn materials(&self) -> PromptMaterials<'_>;
    pub fn skill(&self, key: &ResourceKey) -> Option<&SkillResource>;
    pub fn template(&self, key: &ResourceKey) -> Option<&PromptTemplateResource>;
    pub fn revision(&self) -> ResourceRevision;
}
```

`PromptResourceView` 只 pin captured snapshot。它不能 reload、recompose、比较 current revision，也不能定义第二套 `ResourceKey` / `ContentHash` / `ResourceSourceInfo`。

## TurnPromptInputs

`begin_turn()` 只消费已经稳定的 typed view：

```rust
pub struct TurnPromptInputs<'a> {
    pub resources: PromptResourceView,
    pub product: ProductPromptView<'a>,
    pub agent: AgentPromptView<'a>,
    pub environment: EnvironmentPromptView<'a>,
    pub tools: ToolPromptView<'a>,
    pub model: ModelPromptView<'a>,
    pub policy: PolicyPromptView<'a>,
}
```

这些 view 的含义：

- `ProductPromptView`：产品默认身份、产品文档和默认行为版本。
- `AgentPromptView`：coding/review/plan/subagent 等 agent profile；MVP 可以只有默认 coding profile。
- `EnvironmentPromptView`：workspace root、cwd、platform、日期、VCS 摘要和后续 workspace references。
- `ToolPromptView`：active tool names、schemas、snippets 和 guidelines 的同版投影。
- `ModelPromptView`：模型可见能力摘要；不含 provider client、凭据或 raw payload。
- `PolicyPromptView`：模型确实需要知道的 trust/capability/guardrail 摘要；不能暴露 sandbox internals、prepared args 或 credential state。

## PromptTurn

`PromptTurn` 是 turn-scoped 不可变组装值：

```rust
pub struct PromptTurn {
    resources: PromptResourceView,
    profile: PromptCallProfile,
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

running turn 中的普通 resource reload 不替换 active `PromptTurn`。如果 active tools 或模型可见 profile 在安全点合法变化，`SessionRuntime` 使用同一个 captured `PromptResourceView` 重新执行 `begin_turn()`，得到新的 immutable `PromptTurn`，并在同一安全点原子替换 `CurrentRun.prompt_turn` 与 future `PromptCallProfile`。旧 `PromptTurn` 不原地修改，system prompt 和 tool schemas 也不能分别 patch。

## PromptCallProfile

`PromptCallProfile` 把必须保持一致的模型可见调用基线绑定在一起：

```rust
pub struct PromptCallProfile {
    pub system_prompt: Arc<str>,
    pub active_tool_schemas: Arc<[ToolSchema]>,
    pub contribution_stamps: Arc<[PromptContributionStamp]>,
    pub fingerprint: PromptProfileFingerprint,
}
```

`DriverTurnInput` 携带整个 `PromptCallProfile`，不再分别携带 `system_prompt` 和 `active_tool_schemas`。这样切换 active tools 时，不会出现 system prompt 声明工具 A、provider request 却暴露工具 B 的 split-brain。

`PromptContributionStamp` 对资源来源复用 canonical resource identity，对非资源来源使用各自版本：

```rust
pub enum PromptContributionStamp {
    Resource(ResourceContribution),
    Product(ProductPromptVersion),
    Agent(AgentProfileVersion),
    ToolProfile(ToolProfileFingerprint),
    Policy(PolicyPromptVersion),
    Transient(ContextMaterialStamp),
}
```

Prompt 不重新定义 `ResourceKey`、`ContentHash` 或 `ResourceSourceInfo`。

## System Prompt 组装

system prompt 仍是确定性纯构建结果，但它现在是 Prompt 子系统内部的一部分，而不是整个 Prompt 模块的唯一能力。

推荐 section 顺序：

```text
custom system prompt 或 product default base
  → agent profile
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

## PromptIntent

所有 prompt-producing 入口先归一为结构化 `PromptIntent`：

```rust
pub enum PromptIntent {
    Text(TextPromptIntent),
    Skill(SkillPromptIntent),
    Template(PromptTemplateInvocation),
    Composite(CompositePromptIntent),
}
```

队列只保存 intent 所需的稳定引用和参数，例如：

```text
resource key
arguments
additional instructions
attachments metadata / immutable reference
```

队列不能保存 raw slash command text，也不能提前保存 skill/template 展开正文。

`PromptTurn.resolve_intent()` 使用自己 pin 住的 `PromptResourceView`：

- `Steer` 使用 active `PromptTurn`，因此使用 active turn snapshot。
- idle submission、`FollowUp`、`NextTurn` 在目标 future turn 创建新 `PromptTurn` 后展开。
- snapshot 缺少资源时返回结构化 unavailable error，不能重新读文件或静默切换 delivery。

## ResolvedPromptInput

解析结果保留多模态 parts 和 provenance，不先压成一个字符串：

```rust
pub struct ResolvedPromptInput {
    pub parts: Vec<PromptInputPart>,
    pub messages: Vec<MessageRecord>,
    pub contribution_stamps: Arc<[PromptContributionStamp]>,
}

pub enum PromptInputPart {
    Text(String),
    Image(ImageAttachment),
    File(FileAttachment),
    SelectedCode(SelectedCode),
    SkillBlock(ResolvedSkillBlock),
    ResourceBlock(ResolvedResourceBlock),
}
```

MVP 可以先只实现 `Text` / `SkillBlock`，但 interface 不应把所有输入设计死成裸 `String`。附件是否被目标模型支持，由 `ModelGateway` 根据 capability 做 provider mapping 或返回明确错误。

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
    Durable,
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

context owner 负责执行 I/O，并把成功或失败都转换成 `ContextMaterialContribution`；Prompt 不调用 provider。若 owner 只省略失败项，Prompt 无法区分“未配置”与“required source 获取失败”，因此禁止用 `Vec<ContextMaterial>` 的缺项表示失败。`Required + Unavailable` 阻止模型调用，`Optional + Unavailable` 进入 projection diagnostics 后继续。

典型映射：

- session message / consumed steer：`Durable`。
- 一次 run 内复用的 task state：`CurrentRun`。
- RAG、memory、IDE diagnostics、issue lookup：通常 `CurrentCall`。
- 企业 required policy source 获取失败：fail closed。
- optional RAG 获取失败：diagnose 并继续。

项目文件、AGENTS/context files、skills 和 prompt templates 不能绕过 `ResourceManager` 伪装成 transient context 再注入一次。

## 三条模型输入通道

每次模型调用必须区分：

```text
durable history
current/protected input
transient context
```

推荐 interface：

```rust
pub struct ModelCallProjectionInput<'a> {
    pub profile: &'a PromptCallProfile,
    pub durable_history: &'a [MessageRecord],
    pub current_input: &'a [MessageRecord],
    pub current_run_context: &'a [ContextMaterialContribution],
    pub current_call_context: &'a [ContextMaterialContribution],
    pub output_contract: Option<&'a OutputContract>,
}
```

因果规则：

- `durable_history` 来自 `SessionHandle.build_session_context()`，可以包含 compaction summary、retained suffix 和完整 tool call/result。
- `current_input` 是本次用户 prompt 或刚消费的 steer，必须在本次 call 的 budget/compaction 中受保护。
- `CurrentRun` context 可供同一 run 后续 calls 复用，但不会自动变成会话历史。
- `CurrentCall` context 只影响一次 call，绝不持久化。
- `OutputContract` 描述 JSON schema、response format、tool choice 等调用契约，不应伪装成普通 prompt text。

这吸收 Gemini CLI late-bound pending prompt 的经验：旧历史可以压缩或降级，但当前用户任务不能在首次模型调用前被摘要掉。

## ModelInputProjection

最终输出是唯一可进入 `ModelCallRequest` 的模型可见投影：

```rust
pub struct ModelInputProjection {
    pub system_prompt: Arc<str>,
    pub messages: Arc<[MessageRecord]>,
    pub tools: Arc<[ToolSchema]>,
    pub output_contract: Option<OutputContract>,
    pub contribution_stamps: Arc<[PromptContributionStamp]>,
    pub diagnostics: Arc<[PromptDiagnostic]>,
    pub fingerprint: ModelInputFingerprint,
}
```

`Driver` 使用 Rig step 给出的 provider-neutral prompt/history、当前 `PromptCallProfile` 和 `NextModelCallPlan` 中的 context materials 调用 `prompt::project_model_call(ModelCallProjectionInput { profile, ... })`，通过校验后才构造 `ModelCallRequest`。Driver 可以 clone、整体替换和借用 profile，但不能为了调用 projection 获得 `PromptTurn`、`PromptResourceView` 或 `TurnResourceSnapshot`，也不能绕过 projection 直接把 profile 字段拼成 provider request。

## 最终校验

`project_model_call()` 必须集中执行组合级校验：

- 不存在 orphan tool result。
- 不存在被非法截断的 unresolved tool call。
- current input 未被遗漏、摘要或放到 tool call/result 中间。
- system prompt 和 tool schemas 必须同时来自 `input.profile`，不能从不同 revision 分别传入。
- source key 唯一，section 与 contribution 排序确定。
- 同一静态资源没有通过 resource material 和 transient material 重复注入。
- `CurrentCall` context 不被标记为待持久化。
- required contribution 缺失时失败。
- 输入总大小和估算 token 不超过配置的预检上限；需要压缩时返回结构化 outcome，由 `SessionRuntime` 编排 compaction，而不是 Prompt 自己压缩。

相同输入必须得到相同 fingerprint。fingerprint 用于 diagnostics、`ResourceQuery::GetEffectivePrompt`、测试和后续 provider prompt cache 分段判断，不是 secret-bearing payload。

## 与 Driver Safe Point 的关系

`before_next_model_call` 返回组合式 plan，而不是互斥 decision：

```rust
pub struct NextModelCallPlan {
    pub control: NextModelCallControl,
    pub persistent_messages: Vec<MessageRecord>,
    pub prompt_profile: Option<PromptCallProfile>,
    pub current_run_context: Vec<ContextMaterialContribution>,
    pub current_call_context: Vec<ContextMaterialContribution>,
}
```

同一安全点可以同时消费 steer、更新模型/工具 profile、追加 transient context 并继续运行。`persistent_messages` 必须先进入 Rig/run history；`current_call_context` 只参与本次 `ModelInputProjection`。

## 与 Compaction 的关系

Compaction 不属于 Prompt 子系统：

```text
Compaction
  → 决定 cut point、构造 CompactionSummaryMaterial、生成 summary

SessionHandle
  → summary + retained suffix → durable history

Prompt
  → 将 durable history + protected current input + transient context 投影为本次模型输入
```

压缩摘要是 durable history 的替代消息，不是 system prompt。当前 protected input 不参与同一启动边界上的压缩。

## 与 RuntimeHooks 的关系

当前 MVP 不实现 hooks。后期启用时：

- hook owner 获取动态材料并返回 typed `ContextMaterialContribution` 或受控 profile patch。
- `SessionRuntime` 在 owner safe point 应用 hook result，并把完整 profile/context plan 回复 Driver；Driver 随后调用 `prompt::project_model_call(...)` 做最终投影和重新校验。
- privileged context replacement 也必须回到 `prompt::project_model_call(...)`，不能直接提交 provider request。
- resource-driven material 仍必须来自 captured `PromptResourceView`。

Prompt 不持有 `RuntimeHookRegistry`，也不异步调用 hook。

## 为什么不是 PromptManager / ContextManager

当前没有独立状态需要长期 manager 持有：resources、history、queues、tools、model 和 provider 都已有 owner。长期 PromptManager 会复制 revision/cache invalidation；长期 ContextManager 会与 append-only session history 和 Compaction 形成第二份 context source of truth。

因此 MVP 只实现无状态 Prompt 子系统和 immutable `PromptTurn`。未来只有在多个异步 context provider、跨 call working set、动态 token budget、后台 distillation 等真实需求出现后，才考虑 session-scoped `ContextWorkspace`：

```text
ContextWorkspace.prepare_call(...)
  → ContextMaterialContribution[]
  → prompt::project_model_call(ModelCallProjectionInput { profile, ... })
```

即使引入 `ContextWorkspace`，它也不拥有 durable session history，不替代 Prompt。

## 性能原则

- system prompt 每个 `PromptTurn` 构建一次。
- 使用 `Arc<str>` / `Arc<[ToolSchema]>` 复用稳定 profile。
- 未发生 context projection 时可以复用已有消息 slice/Arc。
- 不在 MVP 引入 Prompt LRU 或全局 cache；模型调用成本远高于确定性字符串拼装。
- 稳定排序和 fingerprint 为后续 provider cache 留接口，但 provider cache headers 仍由 `ModelGateway` 处理。

## 测试重点

- 同输入产生相同 system prompt、profile fingerprint 和 model-input fingerprint。
- active turn rev-1、current resources rev-2 时，Steer 仍解析 rev-1。
- FollowUp / NextTurn 在 future `PromptTurn` 中解析 rev-2。
- active snapshot 缺少 skill/template 时明确失败，不重新读磁盘。
- active tools 改变时 system prompt 与 tool schemas 原子替换。
- Driver 只用 `PromptCallProfile` 即可完成 projection，不需要构造或持有 `PromptTurn` / resource snapshot。
- safe-point 返回 replacement profile 后，下一次 projection 同时使用新 system prompt、tool schemas、contribution stamps 和 fingerprint。
- compaction summary + retained suffix + current prompt 顺序稳定，current prompt 不被摘要。
- tool call/result 完整性校验覆盖 abort/retry/compaction 后上下文。
- `CurrentCall` context 不进入 session storage。
- required/optional context failure policy 可测，失败项不会因 vector 缺项而丢失。
- template + 多 skill + attachment + output contract 的组合顺序确定。
- Prompt 不调用 `ResourceManager.current_*`、不读文件、不持有 provider/auth/queue/storage handle。
