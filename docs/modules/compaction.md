# Compaction架构设计

日期：2026-07-25

状态：当前权威架构（设计已冻结，生产实现待启动）

## 目的

本文定义MiniCore的conversation compaction模块，回答：

- 何时判断下一次模型请求需要压缩；
- 如何在conversation prefix或active-Turn completed prefix中选择安全摘要范围，并保留原样recent tail；
- UserMessage、Assistant Continue、完整ToolRound、final AssistantMessage和已有summary如何保持协议完整；
- 如何通过PromptSet和ModelGateway执行portable SummaryModel调用；
- Compaction entry何时append/apply并触发conversation Replace；
- 压缩后如何重建ConversationSeed和private AgentLoop；
- Steer、Cancel、SecurityRevoked、retry、write outcome unknown和restart如何处理；
- Workspace/Agent/Session instructions、Skill metadata和动态contribution如何保留。

[Runtime Interface](runtime-interface.md)不提供公开`CompactSession`协议或standalone/manual maintenance state。本文也不定义provider-native opaque artifact、hierarchical summary tree、long-term memory、physical JSONL vacuum或provider tokenizer实现。

相关权威文档：

- [Conversation与SessionStorage架构设计](conversation-storage.md)
- [Session Execution架构设计](session-execution.md)
- [Turn执行上下文架构设计](turn-execution-context.md)
- [Prompt子系统架构设计](prompt.md)
- [ModelGateway架构设计](model-gateway.md)
- [ADR 0112](../adr/0112-compaction-supports-active-turn-checkpoints.md)
- [Runtime Interface与公开协议架构设计](runtime-interface.md)

## 决策摘要

- Compaction是crate-internal纯planning/validation模块，不是Runtime Service或领域entity；
- SessionExecutor是orchestration、request arbitration和writer调用的唯一owner；
- SessionStorage是唯一durable truth；
- 只在active Turn的`NeedModel`安全点执行automatic compaction；
- trigger包括soft context pressure、Prompt local context overflow和provider context overflow；
- 不在final response后执行post-turn compaction，也不提供standalone/manual compaction；
- cut基于model-visible stable unit，不基于裸JSONL行；
- Compaction scope分为pre-Turn `ConversationPrefix`和`ActiveTurnCompletedPrefix`；
- cut不能拆分UserMessage、Assistant Continue、完整ToolRound、final AssistantMessage或Compaction summary；
- active Turn的initiating input和已append Steer UserMessage保持原文；每个instruction segment内已经完成的早期stable units可以滚动为`ActiveTurnCheckpoint`；
- leading conversation summary最多一个，每个active instruction segment最多一个effective checkpoint；
- rolling summary使用“previous effective summary/checkpoint + newly summarized units”生成一个portable replacement；
- SummaryModel使用active Turn exact `TurnModelSnapshot`；
- summary输出预算在plan阶段与pinned `EffectiveModelLimits`和summary call context可行性求交；
- PromptSet是SummaryModel context的唯一组装者；purpose固定为`CompactionSummary`，output contract固定为`NoToolCalls`；
- SummaryModel成功不立即改变conversation；只有StoredCompaction append/apply后Replace生效；
- append前重新验证source checkpoint和Cancel/SecurityRevoked control；
- 成功后从updated CommittedConversationState重建ConversationSeed和private AgentLoop segment；
- 同一active Turn允许在frontier推进后再次compact，但受单Turn次数、minimum reclaim和same-source hard-recovery规则约束；
- soft-pressure compaction失败时，只有原ModelCallRequest仍valid才允许继续未压缩调用；
- hard-overflow compaction失败时Turn Failed；
- restart不恢复summary call、retry timer、CompactionPlan或旧AgentLoop；
- 原始entries始终保留，fork到Compaction前自然得到未压缩历史。

## 同类项目取舍

| 项目 | 采用 | 不采用 |
| --- | --- | --- |
| pi | rolling previous summary、recent suffix、Tool安全cut、turn-prefix summary | post-run eager compact、manual implicit abort |
| Codex | model-call前检查、overflow recovery、replacement conversation | provider-native artifact、active-Turn cross-model fallback、直接live-history replacement |
| Claude Code | near-limit auto-compaction | 未公开的cut/storage细节；不复制`/compact` |
| Rig | rolling carry-over、window policy、orphan ToolResult保护 | load-path同步压缩、process-only watermark、system-role summary |
| Grok Build | Compaction与Session/Sampler分离 | checkout中不可验证的具体算法 |

MiniCore采用：

```text
Portable Rolling Summary
+ Stable-Unit Safe Cut
+ Leading Conversation Summary
+ Anchored Active-Turn Segment Checkpoints
+ Model-Aware Summary Budget
+ One StoredCompaction Entry
+ Bounded Frontier Advancement
```

## Ownership

```text
SessionExecutor
├─ decides trigger
├─ owns CurrentCompactionState
├─ starts RunningOperation::CompactConversation
├─ calls PromptSet and ModelGateway
├─ arbitrates Steer/Cancel/SecurityRevoked
└─ append/applies StoredCompaction

Compaction module
├─ context budget calculation
├─ stable-unit projection
├─ cut/protection planning
├─ summary directive construction
├─ summary input reduction
└─ result/commit candidate validation

PromptSet
├─ exposes deterministic CompactionSummaryAssemblyBasis
└─ assembles CompactionSummary context

ModelGateway
└─ performs one provider-neutral SummaryModel call

SessionStorage
└─ validates StoredCompaction scope/frontier and emits trusted Replace delta
```

Compaction module不拥有SessionWriter、CommittedConversationState mutation、ModelGateway、PromptService、Runtime events、CancellationToken lifecycle或Turn terminal state。

## Module Interface

Compaction是concrete crate-internal module，不建立public trait：

```rust
pub(crate) struct Compaction;

impl Compaction {
    pub(crate) fn plan(
        input: CompactionPlanningInput<'_>,
    ) -> Result<CompactionPlan, CompactionError>;

    pub(crate) fn build_summary_directive(
        plan: &CompactionPlan,
    ) -> CompactionSummaryDirective;

    pub(crate) fn validate_summary(
        plan: CompactionPlan,
        result: ModelCallResult,
    ) -> Result<CompactionCommitCandidate, CompactionError>;
}
```

模型调用、timer、writer和events都由SessionExecutor编排。

`CompactionPlanningInput`携带active Turn的`CompactionSettingsSnapshot`、pinned `EffectiveModelLimits`和PromptSet产出的`CompactionSummaryAssemblyBasis`。Compaction不读取Prompt内部section，也不自行猜测Runtime policy开销；该basis只暴露固定摘要policy/tool-contract开销的token estimate与fingerprint，不是第二个assembly实现。

## Trigger

### NeedModel Safe Point

Compaction只在AgentLoop返回`NeedModel`且当前没有Model/Tool external operation时检查：

```text
AgentLoop NeedModel
→ PromptSet assemble AgentRun context
   ├─ success：得到ModelCallRequest和context estimate
   └─ ContextOverflow：得到typed local overflow
→ evaluate compaction trigger
```

safe point包括initiating UserMessage后第一次Model前、`tool_round_completed`后下一次Model前、queued Steer append/apply后下一次Model前，以及provider ContextOverflow recovery。Sampling、WaitingApproval、WaitingForUserInput、ExecutingTools和Finishing期间不启动Compaction。

```rust
pub enum CompactionTrigger {
    ContextPressure,
    PromptContextOverflow,
    ProviderContextOverflow,
}
```

`ContextPressure`是soft trigger；其余是hard trigger。

### Context Budget

```rust
pub struct CompactionSettings {
    pub enabled: bool,
    pub soft_trigger_percent: u8,
    pub target_after_percent: u8,
    pub summary_max_output_tokens: NonZeroU32,
    pub summary_min_output_tokens: NonZeroU32,
    pub minimum_reclaimed_tokens: u64,
    pub max_compactions_per_turn: NonZeroU32,
    pub max_tool_result_summary_bytes: usize,
    pub max_summary_bytes: usize,
}
```

active Turn使用admission时捕获的immutable `CompactionSettingsSnapshot`；配置reload只影响future Turn。参与planning、次数限制和summary budget的字段都进入snapshot fingerprint，不能在同一Turn中途改变policy。

```rust
pub struct CompactionSettingsSnapshot {
    pub settings: Arc<CompactionSettings>,
    pub fingerprint: CompactionSettingsFingerprint,
}
```

推荐但不冻结的默认范围：soft trigger为usable input budget的80%–85%，target after为55%–65%。

```text
usable input budget
= context_window
- effective max_output_tokens
- runtime safety reserve
```

规则：

- context window未知时不执行soft trigger；
- max output来自当前ModelCallRequest；
- 不使用Session total usage判断context pressure；
- provider usage优先，本地estimate补充trailing committed content；
- PromptSet final validation仍是调用前权威检查；
- estimate低于阈值不保证provider接受，provider overflow仍走hard recovery。

### Compaction Summary Budget

AgentRun的usable input budget只决定“是否需要压缩”；SummaryModel调用必须使用自己的预算。`Compaction::plan`从全局设置、active Turn pinned `EffectiveModelLimits`、PromptSet固定摘要开销、reduced summary source token estimate和runtime safety reserve派生：

```rust
pub struct CompactionSummaryBudget {
    pub max_output_tokens: NonZeroU32,
    pub estimated_source_tokens: u64,
    pub fixed_prompt_tokens: u64,
    pub safety_reserve_tokens: u64,
    pub assembly_basis_fingerprint: CompactionSummaryAssemblyBasisFingerprint,
    pub fingerprint: CompactionSummaryBudgetFingerprint,
}
```

`estimated_source_tokens`覆盖全部protected context与经过有标记reduction的summary target，不得只计算将被替换的units。

```text
effective summary output
= min(
    settings.summary_max_output_tokens,
    known EffectiveModelLimits.max_output_tokens,
    context_window - fixed prompt - reduced source - safety reserve
  )
```

规则：

- `summary_min_output_tokens <= summary_max_output_tokens`，否则配置加载失败；
- 已知model output/context limit必须参与求交，未知limit保持unknown，不根据model name猜测；
- context subtraction使用checked arithmetic；固定开销、source和reserve已经占满known window时直接返回`NoFeasibleSummaryBudget`；
- 最终`max_output_tokens`不得低于`summary_min_output_tokens`，否则返回`NoFeasibleSummaryBudget`；
- 最终budget进入plan、directive和`CompactionPlanFingerprint`，request构造后不得改变；
- PromptSet仍执行最终context validation；ModelGateway仍严格拒绝越过effective limit的请求，不能静默clamp；
- summary source feasibility使用SummaryModel自己的output预留，不能复用AgentRun的effective max output基准。

final AssistantMessage后不立即auto compact。延迟到下一次NeedModel能避免无效summary调用，并使用真正的future TurnContext。

## Stable Conversation Units

Cut基于effective committed conversation order，不基于raw JSONL entry order：

```rust
pub(crate) enum StableConversationUnitKind {
    UserMessage,
    AssistantContinue,
    CompleteToolRound,
    FinalAssistant,
    CompactionSummary,
}

pub(crate) struct StableConversationUnit {
    kind: StableConversationUnitKind,
    first_entry_id: EntryId,
    last_entry_id: EntryId,
    backing_entry_ids: Arc<[EntryId]>,
    messages: Arc<[MessageRecord]>,
    estimated_tokens: u64,
    fingerprint: StableConversationUnitFingerprint,
}
```

StableConversationUnit不是领域entity，不持久化，也没有UnitId。

- `UserMessage`：一个Input或Steer UserMessage；不能拆分content或PromptContribution stamps。
- `AssistantContinue`：一个无ToolCall、model-visible、non-terminal Assistant Intermediate response。
- `CompleteToolRound`：Assistant(Intermediate)、ordered Tool messages和`tool_round_completed`；只有completion event已append/apply的round可见。
- `FinalAssistant`：一个final AssistantMessage。
- `CompactionSummary`：latest projection中的leading summary或active-Turn checkpoint；下一次同scope rolling compaction将previous effective summary与新增旧历史合成一个replacement summary。

Started、Abandoned或conversation-hidden Tool round不能成为summary source。

## Protection与Scope

始终exact-protect：

- active Turn全部UserMessage entry及其完整unit，包括initiating input和已append Steer；
- explicit caller-protected entries及其完整unit；
- Pending Interaction关联且仍需精确恢复的内容；
- Started、Abandoned、conversation-hidden或缺少`tool_round_completed`的Tool round；
- planner为recent exact tail选中的完整stable units。

每个active UserMessage之后已经完整提交的早期Assistant Continue和CompleteToolRound不再自动hard-protect。保留全部active UserMessage原文可以避免summary改写任务目标、Steer约束、精确路径或错误文本；checkpoint只描述对应instruction segment中已经完成的工作。

```rust
pub enum CompactionScope {
    ConversationPrefix {
        summarized_through: ConversationBoundary,
        retained_from: ConversationBoundary,
    },
    ActiveTurnCompletedPrefix {
        anchor_user_message: ConversationBoundary,
        previous_checkpoint: Option<ConversationBoundary>,
        summarized_through: ConversationBoundary,
        retained_from: ConversationBoundary,
    },
}
```

`ConversationPrefix`保持原有leading rolling summary：被摘要范围是active Turn之前的连续prefix，retained range是其后的连续exact suffix。

`ActiveTurnCompletedPrefix`把summary放在selected exact active-Turn UserMessage anchor之后。anchor可以是initiating input或已append Steer。第一次从anchor后的第一个eligible stable unit开始且不能跨越下一个active UserMessage；后续从该segment的`previous_checkpoint`之后继续，只能向后推进。`previous_checkpoint`指向当前effective projection中的checkpoint unit；该checkpoint所覆盖的原始范围（covered-through provenance）由其backing `StoredCompaction`派生，不能把两者混成一个boundary。previous checkpoint与新完成units被合成为一个replacement checkpoint，同一segment不会形成checkpoint链。

只有exact active UserMessage anchors、真实protected units和要求保留的recent tail本身仍超过usable input budget时返回：

```text
CompactionError::ProtectedRegionTooLarge
```

该错误不再由“所有active-Turn历史都被政策性保护”触发。不会摘要未完成ToolRound、截断current UserMessage或切换active Turn模型。

## Cut Algorithm

所有合法plan必须满足：

- cut只在stable-unit boundary；
- 不拆分任何unit或ToolRound；
- source checkpoint和TranscriptFingerprint来自同一个trusted view；
- summary source fit派生后的`CompactionSummaryBudget`；
- predicted result满足target-after或至少达到`minimum_reclaimed_tokens`并推进frontier；
- protected unit不进入summary coverage；
- summary coverage、anchor、previous checkpoint及其covered-through provenance和retained tail无重叠、无空洞。

scope-specific规则：

- `ConversationPrefix`的summarized range为非空连续prefix，结束在active Turn initiating UserMessage之前；
- `ActiveTurnCompletedPrefix`必须保留anchor UserMessage原文，只覆盖anchor之后、下一个active UserMessage之前已经完整提交的连续eligible units；
- active scope的`retained_from`紧邻`summarized_through`，并保留尽可能多的recent exact units；
- `previous_checkpoint`存在时必须等于current effective projection中的该segment checkpoint；其backing compaction记录的covered-through provenance必须位于selected ancestry，且新coverage紧接该provenance之后单调前进。

选择过程：

```text
1. 从CommittedConversationState构造ordered stable units
2. 找到active Turn全部exact UserMessage anchors及各instruction segment的previous checkpoint与covered-through frontier
3. 标记exact-protected units与minimum recent tail
4. 构造ConversationPrefix和ActiveTurnCompletedPrefix候选
5. 对每个候选执行ToolResult summary-input reduction并派生CompactionSummaryBudget
6. 优先选择能一次达到target-after且保留最多recent exact history的候选
7. 若没有单一候选达到target，但存在能推进frontier且达到minimum reclaim的候选，选择回收量最大的候选
8. 无eligible range时返回NoFeasibleCut；真实protected region过大时返回ProtectedRegionTooLarge
9. known model/context limit无法留下最低摘要输出时返回NoFeasibleSummaryBudget
10. 生成fingerprinted CompactionPlan
```

一次成功Compaction后重新assemble。若仍有压力，只能在source/frontier已经推进且单Turn预算仍有余额时规划下一次scope；不能对同一source和frontier无界重试。

```rust
pub enum CompactionErrorKind {
    Disabled,
    NothingToCompact,
    NoFeasibleCut,
    ProtectedRegionTooLarge,
    NoFeasibleSummaryBudget,
    SummarySourceTooLarge,
    SummaryFailed,
    SummaryInvalid,
    SourceChanged,
    Cancelled,
    SecurityRevoked,
    Storage,
    ContextStillTooLargeAfterCompaction,
}
```

## CompactionPlan

`ConversationBoundary`是完整stable unit reference。`CompactionScope`中的`summarized_through`是本次source effective projection中inclusive last newly covered unit，`retained_from`是inclusive first exact-retained unit；active scope还保存selected active UserMessage anchor和该segment的optional `previous_checkpoint`。`previous_checkpoint`是当前effective checkpoint unit的boundary；它所代表的原始coverage frontier必须从backing `StoredCompaction`的scope provenance派生。boundary保存unit first/last EntryId和不含storage-local identity的stable unit fingerprint；writer必须证明scope内的相邻、provenance和单调关系。fork只remap EntryId，语义fingerprint保持不变。

```rust
pub(crate) struct CompactionPlan {
    pub trigger: CompactionTrigger,
    pub source: ConversationCheckpoint,
    pub scope: CompactionScope,
    pub protected_entries: Arc<[EntryId]>,
    pub summary_source: CommittedCompactionSourceView,
    pub tokens_before: u64,
    pub retained_tokens: u64,
    pub predicted_tokens_after: u64,
    pub summary_budget: CompactionSummaryBudget,
    pub plan_fingerprint: CompactionPlanFingerprint,
}
```

`CompactionPlanFingerprint`覆盖trigger、source checkpoint/TranscriptFingerprint、CompactionSettingsSnapshot fingerprint、scope/anchor/previous-checkpoint/coverage-provenance boundaries、protected entries、summary source fingerprint、token estimates、effective summary budget和summary format version。它只用于operation result validation和tests，不是领域identity。

`CommittedCompactionSourceView`没有public constructor，只能由CommittedConversationState基于validated scope和stable-unit boundaries产生。它分别携带不进入coverage的protected context与真正待摘要的messages，并证明source checkpoint exact、coverage来自trusted projection、boundaries合法、messages顺序正确且没有hidden Tool round。active scope的protected context包含current effective prefix through selected UserMessage anchor，使“继续处理上一个决定”之类请求仍可正确解释；第一个segment的pre-Turn context过大时必须先执行`ConversationPrefix`，不能静默省略。PromptSet不接收caller拼装的裸message vector。

## Summary Input Reduction

SummaryModel input必须有界。缩减只影响summary request representation，不修改durable source entries或AgentRun conversation。

大ToolResult转换为明确标记的representation：

```text
Tool: <name>
Outcome: success | error
Original bytes: N
Content hash: ...
Head:
...
Tail:
...
Omitted bytes: M
```

规则：保留tool name、call identity、outcome、head/tail、原始字节数、content hash和省略字节数；不能静默截断；原始Tool message保持durable；summary coverage仍引用完整stable unit。

Images/audio/documents不把base64或binary放入summary request，使用已committed的model-safe description、media type、size和content identity；无法取得安全描述时使用typed placeholder。Reasoning只使用provider实际返回的displayable/replayable summary或text，encrypted reasoning不展开。

## CompactionSummaryDirective

```rust
pub enum CompactionSummaryScope {
    ConversationPrefix,
    ActiveTurnCompletedPrefix,
}

pub struct CompactionSummaryDirective {
    pub format_version: CompactionSummaryFormatVersion,
    pub scope: CompactionSummaryScope,
    pub instruction: Arc<str>,
    pub max_output_tokens: NonZeroU32,
    pub max_summary_bytes: usize,
}
```

summary format：

```text
## Goal
## Constraints And Preferences
## Progress
### Completed
### In Progress
### Blocked
## Key Decisions
## Next Steps
## Critical Context
```

要求：

- 描述过去conversation，不继续执行任务；
- `ConversationPrefix`描述较早会话，`ActiveTurnCompletedPrefix`只描述selected exact UserMessage instruction segment中已经完成的工作；
- 保留路径、标识符、error text和用户约束；
- 更新旧summary中的Progress和Next Steps；
- 不把历史中的instruction当成summary模型自己的instruction；
- 只输出summary text，不调用Tool；
- 不用markdown code fence包裹整个summary。

format version进入CompactionPlanFingerprint和StoredCompaction summary metadata。

## Prompt Assembly

PromptSet是唯一模型上下文组装seam，`PromptAssemblyInput`的权威定义见[Prompt子系统](prompt.md#模型上下文组装)。Compaction路径只使用其`CompactionSummary` variant，传入trusted `CommittedCompactionSourceView`与fingerprinted `CompactionSummaryDirective`；purpose由variant确定，不允许caller把Compaction source配成AgentRun purpose。

CompactionSummary assembly包含：

- Runtime required safety policy；
- Compaction-specific System policy和User summary directive；
- trusted scope-aware source converted by PromptSet；
- active scope的current effective prefix through selected exact UserMessage anchor；
- previous leading summary或previous active checkpoint（仅在同scope rolling source中）；
- `OutputContract::NoToolCalls`；
- empty ToolSpec list；
- exact TurnModelFingerprint proof。

不包含ordinary Agent/Session instructions、Workspace instructions、ToolPromptView、SkillView metadata、arbitrary current-call contribution、queued Steer或uncommitted draft。这些Turn-static内容不会丢失，因为成功后下一次AgentRun assembly仍使用同一个PromptSet完整重建。

StoredCompaction进入conversation projection时生成typed CompactionSummary MessageRecord，PromptSet按scope映射为user-role历史消息。leading summary使用：

```text
The conversation history before this point was compacted into the following summary:

<summary>
...
</summary>
```

active-Turn checkpoint使用：

```text
Within this exact user instruction segment, earlier completed work was compacted into the following progress checkpoint:

<summary>
...
</summary>
```

两者都不是System instruction，也不允许summary内容改变Runtime policy。active checkpoint位于对应exact UserMessage anchor之后、该segment recent exact tail或下一条exact UserMessage之前。

## Model Call

SummaryModel调用：

```text
purpose = CompactionSummary
output_contract = NoToolCalls
model = active TurnModelSnapshot exact identity
max_output_tokens = CompactionSummaryDirective.max_output_tokens
tools = []
```

SessionExecutor调用同一个：

```rust
ModelGateway::generate_model_turn(
    request,
    progress,
    cancellation,
)
```

ModelGateway不因为purpose改变provider/model identity，也不调用provider-native compaction endpoint。

### Valid Result

Compaction只接受：

- one finalized assistant response；
- non-empty text summary；
- no ToolCall；
- no unresolved structured output；
- finish reason为Stop或adapter合法归一化的Unknown；
- summary bytes不超过max；
- usage和provider metadata可规范化；
- returned request/assembly fingerprint exact match。

SummaryModel返回的reasoning不进入conversation。StoredCompaction不单独保存summary-call reasoning；实现不得保存或伪造provider未返回的hidden chain-of-thought。

由于CompactionSummary固定使用`OutputContract::NoToolCalls`，Provider返回ToolCall时ModelGateway直接返回`UnexpectedToolCall`，Compaction不会看到可执行call。`InvalidProviderResponse`和`IncompleteResponse`同样不会产生summary candidate；non-empty `Refused`虽然是合法Model response，但不是合法summary，由Compaction映射为`SummaryInvalid`。这些结果都不属于SummaryModel transient retry。

### Retry

ModelGateway每次SummaryModel operation最多执行一个provider attempt；Rig和底层provider SDK automatic retry固定为0。

logical SummaryModel retry由SessionExecutor决定，默认最多1次并使用2秒backoff。`RunningOperation::CompactConversation`形成并持有exact `Arc<ModelCallRequest>`及其request proof；进入`WaitForModelRetry`时同时移动该Arc与`ModelRetryResume::CompactionSummary { source, scope, plan_fingerprint }`，恢复Compaction operation时不重新plan或assemble。由此复用相同CompactionPlan、PromptSet、TurnModelSnapshot、AssembledModelContext、summary directive和source checkpoint；request前credential refresh不改变logical request。

只有Gateway已证明`NotSent`或`RejectedBeforeExecution`，且reason是`Timeout`、`TransportUnavailable`、`ProviderUnavailable`，或typed `Retry-After <= 60s`的`RateLimited`时，才允许该一次retry。`AcceptedNoOutput`没有明确pre-execution rejection proof时按`RequestOutcomeUnknown`处理；`RequestOutcomeUnknown`、`StreamInterrupted`、`UnexpectedToolCall`、`InvalidProviderResponse`和`IncompleteResponse`不能重放，也不能通过改变summary directive伪装成同一次retry。完整规则见[ADR 0119](../adr/0119-model-calls-use-session-logical-retries.md)和[ADR 0120](../adr/0120-failures-stay-with-owning-modules.md)。

## Session Execution Integration

### Phase

`TurnExecutionPhase`的权威枚举由[Agent与Session生命周期](agent-session-lifecycle.md#turn-status-与-execution-phase)定义；Compaction integration只使用其中的`Compacting`，不在本模块重复声明完整枚举。

`Compacting`期间：

```text
TurnStatus = Running
SessionExecutionState = Running
RunningOperation = CompactConversation
```

phase是transient observer projection，不写入JSONL。

### Current State

```rust
pub(crate) struct CurrentCompactionState {
    pub trigger: CompactionTrigger,
    pub source: ConversationCheckpoint,
    pub scope: CompactionScope,
    pub plan_fingerprint: CompactionPlanFingerprint,
    pub original_model_request: Option<Arc<ModelCallRequest>>,
    pub cancellation: CancellationToken,
}
```

soft trigger可以暂存尚未发送的original request。任何Steer、Cancel、SecurityRevoked或conversation change都使它失效。

### Start Flow

```text
NeedModel
→ assemble AgentRun context
→ classify no pressure / soft pressure / hard local overflow

no pressure
→ start GenerateModelResponse

soft/hard pressure
→ Compaction::plan
→ phase = Compacting
→ start CompactConversation operation
```

provider overflow路径：

```text
GenerateModelResponse returns ContextOverflow
→ validate TurnId/version/request fingerprint
→ reserve hard-recovery basis(source checkpoint + current scope frontier)
→ Compaction::plan(trigger = ProviderContextOverflow)
→ phase = Compacting
→ start CompactConversation
```

启动Compaction本身不推进`execution_version`，因此soft failure在没有control或conversation变化时仍可复用original request。成功append/apply Replace后才推进version。Cancel和SecurityRevoked立即推进version并取消operation；Compacting期间到达的Steer只排队并使saved original request不再可发送，Steer UserMessage append/apply也不推进execution_version。

`CurrentTurnExecution`保存successful compaction count和last hard-recovery basis，不把它们放在operation-local CurrentCompactionState。同一个`source checkpoint + scope frontier`（active scope中包含previous checkpoint及其covered-through provenance）最多启动一次hard recovery；失败不能在同一basis透明重试。成功append/apply会推进source或frontier，之后只有新增eligible stable units达到`minimum_reclaimed_tokens`且尚未达到`max_compactions_per_turn`时，才允许在同一Turn再次compact。soft compaction也计入成功次数，但不会占用未实际启动的hard-recovery basis。

### RunningOperation

```rust
RunningOperation::CompactConversation {
    turn_id,
    execution_version,
    source,
    scope,
    plan_fingerprint,
    summary_request,
    logical_retry_count,
    cancel,
}
```

output：

```rust
OperationOutput::Compaction(
    Result<CompactionCommitCandidate, CompactionOperationError>
)
```

operation不能append JSONL，也不能apply projection。

### Control Arbitration

Compacting期间：

- Steer进入existing pending Steer FIFO，不立即append；
- FollowUp保持FollowUpQueue语义；
- Cancel通过`EmergencyControl` sticky signal立即触发operation cancellation token；Executor观察signal后推进execution_version；
- SecurityRevoked按current target generation设置同一`EmergencyControl`的sticky signal；不等待普通lane容量；
- PrepareForUnload通过`LifecycleControl`停止new admission；grace deadline到期时转为fail-closed Cancel；
- ResolveInteraction与Compaction无关，不应存在由Compaction创建的Interaction。

Compaction启动前、结果返回后和StoredCompaction append前都必须观察最新emergency epoch。Cancel/SecurityRevoked已生效时丢弃未commit summary并进入Interrupted处理。Steer本身不让已生成summary失真，因为它尚未append；成功commit后再compose/append Steer，然后重新assemble AgentRun。

### Commit Validation

SessionExecutor append前必须验证：

```text
SessionId exact
TurnId exact and Running
execution_version exact
phase = Compacting
source ConversationCheckpoint exact
source TranscriptFingerprint exact
plan fingerprint exact
TurnExecutionContext exact
TurnModelSnapshot exact
PromptSet exact
no winning Cancel
no winning SecurityRevoked
TurnControlGate controlled reservation valid
summary result valid
CompactionScope anchor/previous checkpoint/coverage provenance与current effective projection exact
committed path中不存在覆盖同一scope frontier的StoredCompaction（靠状态判断去重，非operation key冲突检测）
```

commit顺序：

```text
process all earlier queued control requests
→ winning Cancel/SecurityRevoked：discard candidate并进入cleanup
→ validate candidate and current execution version
→ reserve TurnControlGate controlled append
→ revalidate source checkpoint/control state while reservation is held
→ SessionWriter.append(StoredCompaction draft)
→ SessionWrite OutcomeUnknown时保守终结本次compaction，下次触发按committed prefix重新规划
→ apply scope-aware CommittedConversationDelta::Replace
→ release reservation
```

Compaction summary本身不读取Workspace文件，但它仍属于active Turn logical execution；因此Cancel/SecurityRevoked与StoredCompaction commit通过同一个TurnControlGate first-wins。

### Success Flow

```text
StoredCompaction append/apply
→ CommittedConversationState checkpoint advances
→ increment execution_version
→ ConversationPrefix: Replace = [new leading summary] + [retained exact suffix]
→ ActiveTurnCompletedPrefix: Replace = [prefix through exact anchor] + [new checkpoint] + [retained exact active tail]
→ rebuild ConversationSeed
→ rebuild private AgentLoop segment
→ phase = PreparingModel
→ steer FIFO非空时pop_front一条并append/apply
→ assemble a new AgentRun ModelCallRequest
→ continue the same TurnExecutionContext
```

Compaction summary output不交给AgentLoop当作assistant response。旧AgentLoop segment、provider connection和continuation全部丢弃。

成功不会：

- 开启新Turn；
- 改变TurnExecutionContext；
- 改变TurnModelSnapshot；
- 重新捕获Workspace/Skill/Tool/Prompt；
- 生成Assistant Item；
- 生成Compaction Item；
- 发布Turn terminal fact。

### Post-Commit Validation

下一次AgentRun assembly仍执行完整PromptSet validation。若compacted context仍overflow：

- 本次Compaction entry保持durable；
- 若另一个scope或已经推进后的active frontier存在eligible range、达到minimum reclaim且单Turn次数仍有余额，重新进入一个明确的Compaction planning cycle；
- 若没有新frontier、没有可行cut或次数耗尽，TurnFailed with `ContextStillTooLargeAfterCompaction`；
- 不在同一source/frontier上无限compact-and-retry。

## Failure Policy

| Failure | Soft pressure | Hard overflow |
| --- | --- | --- |
| NothingToCompact / NoFeasibleCut | 原request仍valid时继续 | TurnFailed |
| ProtectedRegionTooLarge | 原request通过validation时继续 | TurnFailed |
| NoFeasibleSummaryBudget | 原request仍valid时继续 | TurnFailed；保留Compaction domain error |
| SummarySourceTooLarge | 原request仍valid时继续 | TurnFailed |
| summary auth/rate/transport exhausted | 原request仍valid时继续 | TurnFailed |
| summary Refused/invalid，或Gateway返回UnexpectedToolCall/InvalidProviderResponse/IncompleteResponse（CompactionSummary不使用Structured） | 原request仍valid时继续 | TurnFailed |
| source changed | 不使用candidate并fail closed | fail closed；不在同一operation内重计划 |
| Cancelled/SecurityRevoked | 进入Turn interruption cleanup | 进入Turn interruption cleanup |
| storage failure | Session unavailable/Turn failure规则 | 同左 |
| still too large after commit | TurnFailed | TurnFailed |

soft fallback使用原ModelCallRequest必须满足conversation checkpoint、assembly fingerprint、execution version和control state全部未变；否则重新assemble，不能发送stale request。

`SourceChanged`不做transparent replan。合法Steer在Compacting期间尚未append，Cancel/SecurityRevoked会推进version；因此operation result对应的source意外变化表示实现race或未经授权的projection advance，必须结束当前candidate。新的planning只能由SessionExecutor在重新进入NeedModel后作为新的明确操作启动；同一hard-recovery basis仍视为已使用。

## StoredCompaction

本节是`StoredCompaction`及其nested schema的权威定义；[Conversation Storage](conversation-storage.md#compaction-entry)只拥有append/replay validation和projection，不重复声明这些类型。

```rust
pub struct ConversationBoundary {
    pub unit_first_entry_id: EntryId,
    pub unit_last_entry_id: EntryId,
    pub unit_fingerprint: StableConversationUnitFingerprint,
}

pub struct StableConversationUnitFingerprint(pub ContentHash);

pub struct StoredCompaction {
    pub turn_id: Option<TurnId>,
    pub source: ConversationCheckpoint,
    pub scope: CompactionScope,
    pub summary: CompactionSummary,
    pub protected_entries: Arc<[EntryId]>,
    pub model_call: Option<StoredCompactionModelCall>,
}

pub struct StoredCompactionModelCall {
    pub model: TurnModelRef,
    pub response_id: Option<String>,
    pub usage: Option<ModelUsage>,
    pub finish_reason: ModelFinishReason,
    pub provider_metadata: StoredProviderResponseMetadata,
    pub logical_retry_count: u32,
    pub requested_max_output_tokens: NonZeroU32,
    pub summary_budget_fingerprint: CompactionSummaryBudgetFingerprint,
    pub assembled_context_fingerprint: AssembledModelContextFingerprint,
}
```

automatic active-Turn compaction约束：

```text
turn_id = Some(active TurnId)
model_call = Some(...)
method = SummaryModel
```

`turn_id = None`和`model_call = None`保留给未来standalone/deterministic maintenance，不由automatic active-Turn流程产生。

`CompactionSummary`至少保存：

```rust
pub struct CompactionSummary {
    pub format_version: CompactionSummaryFormatVersion,
    pub text: Arc<str>,
    pub content_hash: ContentHash,
    pub tokens_before: u64,
    pub estimated_tokens_after: u64,
}
```

`StoredCompactionModelCall`保存exact model、response id、usage、finish reason、provider metadata、logical retry count、requested max output、summary budget fingerprint和assembled context fingerprint。`logical_retry_count`范围为0–1；trusted writer和cold replay semantic validation都拒绝更大值。失败attempt usage只进入ModelGateway telemetry，不写入Session totals。

### Append and Durable Entry Validation

`Compaction::validate_summary`和SessionExecutor append gate在调用`SessionWriter.append`前验证当前Turn仍可提交candidate：

- source checkpoint、TranscriptFingerprint、Turn/Context/Model/Prompt和plan fingerprint exact；
- candidate的scope anchor、`previous_checkpoint`、coverage provenance与current effective projection exact，且committed path没有同一scope frontier的已提交compaction；
- ModelCall request、directive、assembly proof、summary budget和pinned model limits exact一致；
- control/authorization仍有效，automatic entry满足`turn_id`和SummaryModel约束。

`SessionStorage::validate_and_project`（append与cold replay共用）只验证entry自身可以重建的durable关系：

- source checkpoint位于selected ancestry并等于current checkpoint；
- scope中的全部boundary都是source effective projection上的stable boundaries；
- `ConversationPrefix`的summarized prefix非空、retained suffix连续且两者相邻，且coverage不越过active Turn anchor；
- `ActiveTurnCompletedPrefix`的anchor是current Turn exact initiating或Steer UserMessage，coverage位于anchor之后且不跨越下一条active UserMessage；
- active scope的`previous_checkpoint`（若存在）必须与该instruction segment current effective checkpoint exact match；解析其backing `StoredCompaction`得到covered-through provenance，确认新coverage在selected ancestry中紧接该provenance并只向后推进，且retained tail或下一UserMessage紧邻coverage；
- protected entries都存在于selected ancestry，所属完整unit不进入summary coverage，且任何ToolRound都未被拆分；
- durable `StoredCompactionModelCall`中的requested output、budget fingerprint和summary text/hash可round-trip；cold replay不重新声称验证已经消失的plan/directive/model limits；
- automatic SummaryModel entry有model_call，turn_id存在时Turn仍Running且与source path一致；
- 基于committed prefix状态的compaction去重规则成立。

caller提供的raw replacement messages不是StoredCompaction字段。trusted projector根据source、typed scope、boundaries和summary确定性构造Replace delta，避免caller提交任意history rewrite。

### Projection

```text
ConversationPrefix：

old = [summarized prefix] + [retained exact suffix]
new = [leading CompactionSummary] + [same retained exact suffix]

ActiveTurnCompletedPrefix：

old = [prefix through selected exact UserMessage] + [optional previous checkpoint] + [newly completed units] + [retained exact suffix]
new = [same prefix through selected exact UserMessage] + [replacement segment checkpoint] + [same retained exact suffix]
```

Compaction entry本身推进ConversationCheckpoint和TranscriptFingerprint。下一次rolling compaction的source只看effective conversation，不重新展开更早已被覆盖的raw entries。effective conversation最多包含一个leading summary，并且每个active instruction segment最多一个checkpoint。

## Prompt、Workspace、Skill和Tool保留

### Turn-Static Inputs

以下内容不写入summary：

```text
Runtime safety policy
Agent instructions
Session instructions
Workspace instructions
Tool definitions/guidelines
SkillView metadata
TurnModelSnapshot policy
```

它们已经被active Turn的PromptSet/ToolSet/SkillView固定，并在每次AgentRun assembly重新注入。把它们再摘要会造成重复、过时内容和summary劫持风险。

### Dynamic Contributions

已经规范化并随UserMessage/Steer/Tool message持久化的动态contribution属于conversation fact，可以进入summary source。未append contribution、live filesystem state、current skill file内容或Tool progress不能进入summary。

### Exact Resume Limitation

Compaction不能补偿缺失的Prompt或Tool recovery material。same-Turn cold resume仍依赖Prompt和Tool execution basis可重建；已经committed的Skill正文属于conversation fact，未committed Skill不由summary保存或恢复。

## Recovery

### Crash Before Append

```text
summary operation running or completed in memory
→ no StoredCompaction
→ old conversation remains authoritative
→ restart does not resume summary call
→ unfinished Turn按conservative recovery Interrupted
```

### Crash During Append

遵守storage的append/恢复规则（EntryId + parent_id + committed-prefix状态判断）：

- incomplete tail可由exclusive writer recovery truncate；
- 恢复时读committed prefix，若目标cut的StoredCompaction已存在则跳过，不重复写；
- 不做in-run replay-by-key，也不依赖同operation key不同payload的冲突检测；
- 不重新调用SummaryModel来解析storage outcome unknown。

### Crash After Append Before Apply

```text
StoredCompaction durable
→ replay validates entry
→ projector deterministically applies Replace
→ current Turn仍按unfinished-Turn recovery终止
```

Compaction效果保留；不恢复旧AgentLoop、provider stream或RunningOperation。

### Recovery Never Invents

recovery不得：

- 生成synthetic summary；
- 自动重新调用SummaryModel；
- 猜测cut boundary；
- 重新读取current Workspace形成历史summary；
- 丢弃已经durable的Compaction；
- 把hidden incomplete ToolRound放入conversation。

## Fork

Compaction是selected entry path上的overlay：

- fork到Compaction前anchor得到旧conversation；
- fork到Compaction后anchor复制Compaction entry及其referenced source/protected identities；
- target remap全部nested EntryId、anchor、frontier和ConversationBoundary references；
- replay在child中按scope重建同一个leading summary或active checkpoint与exact tail；
- 不复制Compaction operation、AgentLoop、provider continuation、WorkspaceSnapshot或process-local security signal。

Compaction entry不暴露成独立public fork anchor，但selected message path上的Compaction自然随path生效。

## Events和Usage

automatic compaction可以发布process-local progress：

```text
CompactionStarted
CompactionProgress
CompactionCompleted
CompactionFailed
```

public event使用[Runtime Interface](runtime-interface.md)定义的per-session StateEvent与ProgressEvent。事件必须在对应committed fact apply后发布；progress可合并/丢弃，不进入JSONL或任何公开恢复序列。

Session usage由assistant entries和`StoredCompaction.model_call` replay重建。SummaryModel usage属于Session total，但不属于assistant Item或普通Agent response usage。

## Security

- summary source只来自CommittedCompactionSourceView；
- 历史中的prompt injection只是待摘要data，不获得System authority；
- PromptSet不能把summary text插入Runtime instruction section；
- secret redaction在写StoredCompaction前执行；
- provider raw error/body不进入summary或JSONL；
- SecurityRevoked可在线性化点前通过TurnControlGate阻止append；
- compaction不重新读取live Workspace绕过captured snapshot；
- no-tools contract防止summary产生外部副作用。

## Invariants

必须成立：

```text
COMP-001 Compaction source来自committed conversation
COMP-002 scope coverage、anchor、frontier和retained tail关系合法
COMP-003 cut只在stable-unit boundary
COMP-004 ToolRound不能拆分
COMP-005 active Turn initiating与Steer UserMessage原文和相对位置保留
COMP-006 StoredCompaction append/apply前旧conversation仍权威
COMP-007 summary调用只经过PromptSet和ModelGateway
COMP-008 SummaryModel不能调用Tool
COMP-009 active Turn exact model identity不变
COMP-010 source checkpoint变化使candidate失效
COMP-011 Cancel/SecurityRevoked可在append前获胜
COMP-012 Replace由trusted projector构造，不接收caller raw history
COMP-013 最多一个leading summary且每个active instruction segment最多一个checkpoint
COMP-014 raw historical entries不被改写
COMP-015 restart不恢复旧summary operation
COMP-016 same-source hard recovery和单Turncompaction次数有界
COMP-017 static Prompt/Tool/Skill/Workspace instructions由后续AgentRun重新注入
COMP-018 Compaction不是Turn、Item或Interaction
COMP-019 每个active instruction segment的summarized frontier只单调前进
COMP-020 effective summary budget与pinned model known limits一致并进入fingerprint
```

## Tests

### Stable Cut Property Tests

随机生成合法conversation entry sequences，验证：

- 任意plan都不拆分ToolRound；
- scope coverage、anchor、frontier和retained ranges无重叠、无空洞；
- protected unit始终位于summary coverage之外；
- Replace后不存在orphan ToolResult；
- leading rolling N次后仍只有一个leading summary；
- 每个active instruction segment rolling N次后frontier单调且仍只有一个segment checkpoint；
- replay和live apply得到相同TranscriptFingerprint。

### Scenario Tests

至少覆盖：

1. 多个completed Turns后第一次Model前soft compact；
2. previous summary +新增Turns rolling compact；
3. initiating segment含多个completed ToolRounds并生成第一个checkpoint；
4. previous segment checkpoint +新completed ToolRounds滚动推进frontier；
5. initiating与Steer UserMessage都保持exact且checkpoint不跨越下一UserMessage；
6. 多个Steer segment各自最多一个checkpoint且相对顺序保持；
7. Pending/Started/incomplete ToolRound不进入coverage；
8. true protected region自身过大；
9. global summary max大于pinned model output limit时plan确定性下调；
10. context剩余空间低于summary minimum时返回NoFeasibleSummaryBudget；
11. unknown model limit保持unknown且不按model name猜测；
12. Tool messages durable但缺少`tool_round_completed`；
13. summary result返回ToolCall时Gateway返回UnexpectedToolCall，Compaction不取得candidate；
14. summary finish reason Length时Gateway返回IncompleteResponse，Compaction不取得candidate；
15. summary期间Steer排队；
16. summary期间Cancel；
17. summary期间SecurityRevoked；
18. source checkpoint在result返回前变化；
19. write OutcomeUnknown后按committed path状态确认（compaction已存在则跳过）；
20. crash after append before apply；
21. provider ContextOverflow后成功推进frontier并在新增work后再次compact；
22. 同一source/frontier hard recovery不重复；
23. max_compactions_per_turn耗尽后仍overflow并TurnFailed；
24. soft compact失败后原request仍valid；
25. fork到leading summary和不同segment checkpoint前后；
26. large ToolResult head/tail reduction metadata；
27. static Workspace/Skill/Tool instructions未写入summary但下一次AgentRun仍存在；
28. encrypted reasoning不进入summary正文。

### Quality Evaluation

算法正确不代表summary质量足够。建立可重复fixture，人工或grader检查：

- goal/constraints preservation；
- file path和symbol preservation；
- completed/in-progress区分；
- decision和reason preservation；
- unresolved failure preservation；
- hallucination和obsolete next-step rate；
- summary token ratio。

质量评估不能取代storage/cut property tests。

## Rejected Alternatives

### 强制结束当前Turn并开启continuation Turn

实现上可以把长任务切成多个用户可见Turn，但会人为终止当前Tool/Interaction lifecycle，并改变Steer、Cancel、usage和terminal语义。仅作为真正无可行cut时的产品降级，不代替active-Turn checkpoint。

### Hierarchical Summary Tree

适合极长Session，但增加summary frontier、引用、失效和repair。先用rolling summary收集真实质量与成本数据。

### Provider-Native First

可能更忠实保留provider-specific state，但artifact不可移植，且要求provider/model/auth continuity。以portable summary作为durable representation。

### Post-Run Eager Compaction

可以降低下一次Turn latency，但会为不再继续的Session支付成本，并使用旧Turn模型策略。只在真正NeedModel时执行。

### Direct Live-History Replacement

实现简单，但进程崩溃后无法证明replacement已持久化。MiniCore必须append/apply后生效。

### Load-Path Compaction

会让Session load等待外部模型调用，并混淆read/recovery和new durable write。load只replay existing Compaction entry。

### Summary As System Message

容易赋予历史内容额外authority。Compaction summary是user-role historical checkpoint。

### Deterministic Truncation As Automatic Fallback

任意丢弃旧message会破坏任务连续性。只允许有标记的summary-input ToolResult reduction，不把它当conversation compaction result。

## Implementation Sequence

Compaction的纯planning/projection可以先做模块测试，但summary model path不能在SessionExecutor和ModelGateway之后另行接入。阶段6–8共享以下交付顺序：

1. 实现stable-unit projection、scope/frontier、context pressure、model-aware budget和property tests；
2. 冻结`PromptAssemblyInput::CompactionSummary`、`CompactionSummaryDirective`和共用`ModelCallRequest::new`契约；
3. 通过ModelGateway的`ScriptedProviderAdapter`闭环普通AgentRun；
4. 在同一harness加入`Compacting`、summary call、StoredCompaction append/apply、trusted Replace和AgentRun reassembly；
5. 完成soft/hard failure、same-source recovery、per-Turn compaction budget、replay/fork/crash tests；
6. 并行完成Rig spike，再接入RigProviderAdapter和provider mock-server tests；
7. 建立summary quality fixtures；
8. 接入Runtime per-session StateEvent/ProgressEvent并完成公开contract tests。

步骤3和4是共同完成门槛；不允许Compaction使用第二个summary request类型或绕过ModelGateway。

## 完成检查

- [x] 确定Compaction ownership和边界。
- [x] 确定NeedModel trigger和soft/hard overflow分类。
- [x] 确定stable-unit cut、current UserMessage protection和active-Turn checkpoint。
- [x] 确定leading summary、active frontier与scope-aware projection。
- [x] 确定pinned-model-aware summary budget与domain error。
- [x] 确定PromptSet/ModelGateway summary调用。
- [x] 确定`TurnExecutionPhase::Compacting`和control arbitration。
- [x] 确定append/apply线性化和Replace projection。
- [x] 确定retry、failure、restart和fork。
- [x] 确定static instruction不进入summary。
- [x] 确定不强制split为新Turn，不做manual、hierarchical或provider-native compaction。
- [x] 确定Runtime protocol不公开manual CompactSession。
- [ ] 实现Compaction module。
- [ ] 通过ScriptedProviderAdapter完成overflow → summary → append/apply → reassemble → AgentRun vertical slice。
- [ ] 实现SessionExecutor integration。
- [ ] 实现共用Prompt/ModelGateway summary path。
- [ ] 实现storage/projector/replay/fork tests。
- [ ] 完成summary quality evaluation。
