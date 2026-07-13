# Prompt

`Prompt` 是 MiniCore 的无状态提示词组装子系统，对应未来的 `prompt.rs` / `prompt/`。它把已经由各 owner 捕获、校验或解析的输入确定性地组装成一次 turn 的 `PromptTurn`，并在每次模型调用前生成协议安全的 `ModelInputProjection`。

`Prompt` 不是 workspace-global `PromptManager`，也不是长期持有会话历史的 `ContextManager`。`SessionRuntime` 仍是 Pull Master：它决定何时捕获资源、读取会话上下文、消费队列和收集动态上下文，并调用 Prompt 的 turn assembly / intent seam；Driver 消费 actor 返回的 profile/context plan，并调用 Prompt 的纯 model-call projection seam。Prompt 只负责解释、排序、展开、投影和校验。

一句话边界：

```text
ResourceManager admits and versions stable resources.
SessionRuntime decides when to assemble.
Prompt deterministically assembles model-visible input.
```

## 设计定位

当前 prompt 相关输入来自明确 owner：

```text
ResourceManager  → PromptResourceView     // 所有非工具稳定 Prompt 输入
Tools            → ToolPromptView         // 工具模型可见 schemas/snippets/guidelines
SessionHandle    → durable history
SessionRuntime   → current input、delivery、run safe-point plan
future context owners → ContextMaterialContribution
ModelGateway     → provider payload / role mapping / auth injection boundary
```

`PromptResourceView` 是所有非工具稳定 Prompt 输入的唯一 seam。它暴露：

```text
materials
behavior
model
environment
policy
skill/template catalog access
fingerprint
```

`ToolPromptView` 保持 Tools 独立，不并入 `PromptResourceView`。这避免工具 active set、审批、executor、future `ToolBatchInvoker` 与资源 current-pointer 语义混在一起。

这些 owner 不主动调用 Prompt，也不向 Prompt 推送失效通知。新的目标 user turn / work chain 边界由 `SessionRuntime` 统一拉取稳定 view：

```text
ResourceManager.capture_turn(..., typed owner projections)
Tools.capture_profile_baseline() -> ToolProfileBaseline { prompt, invoker, fingerprint }
SessionHandle.build_session_context(...)
  → SessionRuntime
  → prompt::assemble_turn(PromptTurnSpec { resources, tools })
  → PromptTurn
```

MVP 不执行 turn 内 profile mutation。后续合法 safe point 不得再次调用 `capture_turn(...)` 或读取 ResourceManager current pointer；它只能以 active `TurnResourceSnapshot` 为 parent 构造 `StepResourceSnapshot` / step override，并从 `Tools.capture_profile_baseline()` 得到完整 replacement baseline，再组装新的 `PromptTurn`。

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
pub fn assemble_turn(
    spec: PromptTurnSpec,
) -> Result<PromptTurn, PromptError>;

pub fn project_model_call(
    input: ModelCallProjectionInput<'_>,
) -> Result<ModelInputProjection, PromptError>;

pub struct PromptTurnSpec {
    pub resources: PromptResourceView,
    pub tools: ToolPromptView,
}

impl PromptTurn {
    pub fn profile(&self) -> &PromptCallProfile;

    pub fn resolve_intent(
        &self,
        intent: &PromptIntent,
    ) -> Result<ResolvedPromptInput, PromptError>;
}
```

`PromptTurn` 负责 pin captured resources、展开 resource-backed intent 并提供原子 `PromptCallProfile`；最终模型调用投影是 Prompt 模块的纯函数，因为其最小充分输入是 profile 加 call-time lanes，而不是完整 `PromptTurn`。内部可以继续调用平级的 `skills.rs` 和 `prompt_templates.rs` helper，但这些内部 helper 不成为调用方必须学习的新 seam。

## Owner 分层

| 能力 | Owner |
| --- | --- |
| resource roots、trust、overlay、cwd revision、content hash、cwd reload | `ResourceManager` |
| runtime product/user-global defaults 初始化 | `OpenWorkspace` / `ResourceManager` |
| behavior/model/environment/policy typed projection 的 owner state | `SessionRuntime` 及其对应 owner |
| prompt-safe turn projection freeze | `ResourceManager.capture_turn(...)` |
| tool governance、approval、sandbox、executor、active set | `Tools` |
| tool prompt schemas/snippets/guidelines projection | `ToolPromptView` from `Tools` |
| skill/template metadata、正文和 catalog | captured resource snapshot |
| skill/template 解析与格式化算法 | `skills.rs` / `prompt_templates.rs` |
| turn/run/queue/phase、何时组装 | `SessionRuntime` |
| system prompt、intent 展开、call projection、组合校验 | `Prompt` |
| durable history 和 compaction projection | `SessionHandle` / `SessionStorage` |
| 动态 RAG/memory/IDE context 获取 | future hook/context owner |
| provider payload、role mapping、cache header | `ModelGateway` |

Prompt 不拥有 resource catalog、session history、queue、provider client、tools owner handle 或 context provider registry。

## PromptResourceView

Prompt 不直接接收 `ResourceManager` handle，也不访问 current snapshot pointer。`ResourceManager.capture_turn(...)` 返回 captured `TurnResourceSnapshot` 后，由资源模块提供只读窄投影：

```rust
pub struct PromptResourceView {
    snapshot: Arc<TurnResourceSnapshot>,
}

impl PromptResourceView {
    pub fn materials(&self) -> PromptMaterials<'_>;
    pub fn behavior(&self) -> &BehaviorProjection;
    pub fn model(&self) -> &ModelPromptProjection;
    pub fn environment(&self) -> &EnvironmentProjection;
    pub fn policy(&self) -> &PolicyProjection;
    pub fn skill(&self, key: &ResourceKey) -> Option<&SkillResource>;
    pub fn template(&self, key: &ResourceKey) -> Option<&PromptTemplateResource>;
    pub fn fingerprint(&self) -> &TurnResourceFingerprint;
}
```

`PromptResourceView` 只 pin captured snapshot。它不能 reload、比较 current revision、读文件，也不能定义第二套 `ResourceKey` / `ContentHash` / `ResourceSourceInfo`。

MVP `environment()` 只包含 workspace root、fixed cwd、platform、date/time/timezone 和 interaction capabilities。VCS 摘要不属于 MVP environment；需要 VCS 信息时必须由独立 context owner 或缓存 snapshot 提供 typed `ContextMaterialContribution`，Prompt 和 ResourceManager 都不直接执行 Git I/O。

## PromptTurnSpec

`assemble_turn()` 只消费两个稳定 view：

```rust
pub struct PromptTurnSpec {
    pub resources: PromptResourceView,
    pub tools: ToolPromptView,
}
```

`resources` 提供 materials / behavior / model / environment / policy / skill / template / fingerprint。`tools` 提供 active tool names、schemas、snippets、guidelines 和 tool profile fingerprint。不要再为 product、agent、environment、model 或 policy 定义独立无 owner prompt view；这些非工具输入都通过 `PromptResourceView` 的 typed projections 暴露。

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

running turn 中的普通 resource reload 不替换 active `PromptTurn`。MVP 在 `Turn` 中继续拒绝 model、thinking、stream options、active tools 和 profile mutation，避免 `PromptCallProfile` 与 `ToolBatchInvoker` baseline 分裂。

后续 full version 若允许 safe-point mutation，`SessionRuntime` 必须通过 `StepResourceSnapshot` 或明确 step override，在同一 actor transaction 中整体替换 `CurrentRun.prompt_turn` / `NextModelCallPlan.prompt_profile` 与 future `ToolBatchInvoker`。旧 `PromptTurn` 不原地修改，system prompt 和 tool schemas 也不能分别 patch；replacement profile、future invoker 和 fingerprint 必须一致。

## PromptCallProfile

`PromptCallProfile` 把必须保持一致的模型可见调用基线绑定在一起：

```rust
pub struct PromptCallProfile {
    pub system_prompt: Arc<str>,
    pub active_tool_schemas: Arc<[ToolSchema]>,
    pub tool_profile_fingerprint: ToolProfileFingerprint,
    pub contribution_stamps: Arc<[PromptContributionStamp]>,
    pub fingerprint: PromptProfileFingerprint,
}
```

`DriverTurnInput` 携带整个 `PromptCallProfile`，不再分别携带 `system_prompt` 和 `active_tool_schemas`。`tool_profile_fingerprint` 必须等于组装时 `ToolPromptView.fingerprint`，也供 `SessionDriverHost` 校验 replacement `ToolBatchInvoker.fingerprint()`；这样切换 active tools 时，不会出现 system prompt 声明工具 A、provider request 或执行 invoker 却暴露工具 B 的 split-brain。

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

system prompt 仍是确定性纯构建结果，但它现在是 Prompt 子系统内部的一部分，而不是整个 Prompt 模块的唯一能力。

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

队列只保存 intent 所需的稳定引用和参数，例如 resource key、arguments、additional instructions 和附件 metadata / immutable reference。队列不能保存 raw slash command text，也不能提前保存 skill/template 展开正文。

`PromptTurn.resolve_intent()` 使用自己 pin 住的 `PromptResourceView`：

- `Steer` 使用 active `PromptTurn`，因此使用 active turn snapshot。
- idle submission、`FollowUp`、`NextTurn` 在目标 future turn 创建新 `PromptTurn` 后展开。
- snapshot 缺少资源时返回结构化 unavailable error，不能重新读文件或静默切换 delivery。

## User turn / work chain capture 规则

`SessionRuntime` 对一个新的显式 user turn / work chain capture 一次 `TurnResourceSnapshot` 并创建一次 baseline `PromptTurn`。

复用同一 captured turn 的情况：

- automatic retry；
- context overflow compaction recovery；
- active `Steer`；
- 同一 `RunId` 下 Rig segment rollover。

必须新 capture 的情况：

- `FollowUp` 启动后续 work；
- `NextTurn` 被下一次显式 prompt 消费；
- 新 idle prompt；
- 其它新的显式 user turn / work chain。

Resource reload 只影响新的显式 user turn / work chain。已经展开并持久化的 skill/template invocation 是历史 user message，后续 reload 不改写它。

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
- 输入总大小和估算 token 不超过配置的最终投影上限；超限时返回结构化 `PromptError::ContextLimitExceeded { estimated_input_tokens, effective_limit }`，Prompt 不调用 provider，也不自己压缩。

这里是每次 `AgentRunStep::CallModel` 前的 call-projection validation，不是 `SubmitPrompt` admission。它覆盖首次调用、tool result 后续调用和 Steer rollover；因此可能发生在 `run_started` 之后。

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

同一安全点可以同时消费 steer、追加 transient context 并继续运行。MVP 拒绝 model/thinking/stream/active-tools/profile mutation；full version 若启用 safe-point mutation，`prompt_profile` 只能是用 active captured turn 或 step override 原子重建后的完整 profile。`current_call_context` 只参与本次 `ModelInputProjection`。

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

因此 MVP 只实现无状态 Prompt 子系统和 immutable `PromptTurn`。未来只有在多个异步 context provider、跨 call working set、动态 token budget、后台 distillation 等真实需求出现后，才考虑 session-scoped `ContextWorkspace`。

## 性能原则

- system prompt 每个 `PromptTurn` 构建一次。
- 使用 `Arc<str>` / `Arc<[ToolSchema]>` 复用稳定 profile。
- 未发生 context projection 时可以复用已有消息 slice/Arc。
- 不在 MVP 引入 Prompt LRU 或全局 cache；模型调用成本远高于确定性字符串拼装。
- 稳定排序和 fingerprint 为后续 provider cache 留接口，但 provider cache headers 仍由 `ModelGateway` 处理。

## 测试重点

- 同输入产生相同 system prompt、profile fingerprint 和 model-input fingerprint。
- active turn cwd rev-1、current cwd rev-2 时，Steer 仍解析 rev-1。
- FollowUp / NextTurn / new idle prompt 在 future `PromptTurn` 中解析 rev-2。
- automatic retry、overflow compaction recovery 和同 RunId segment rollover 复用原 turn resources。
- active snapshot 缺少 skill/template 时明确失败，不重新读磁盘。
- MVP 在 active turn 中拒绝 model/thinking/stream/active-tools/profile mutation。
- full-version safe-point replacement 同时替换 `PromptCallProfile`、future `ToolBatchInvoker` 和 fingerprint。
- Driver 只用 `PromptCallProfile` 即可完成 projection，不需要构造或持有 `PromptTurn` / resource snapshot。
- compaction summary + retained suffix + current prompt 顺序稳定，current prompt 不被摘要。
- tool call/result 完整性校验覆盖 abort/retry/compaction 后上下文。
- `CurrentCall` context 不进入 session storage。
- required/optional context failure policy 可测，失败项不会因 vector 缺项而丢失。
- template + 多 skill + attachment + output contract 的组合顺序确定。
- Prompt 不调用 `ResourceManager.current_*`、不读文件、不持有 provider/auth/queue/storage handle。
