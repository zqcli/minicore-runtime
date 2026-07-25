# Compaction架构设计

日期：2026-07-25

状态：当前权威架构（设计已冻结，实现进行中）

## 目的

本文定义MiniCore的conversation compaction模块，回答：

- 何时判断下一次模型请求需要压缩；
- 如何选择需要摘要的连续prefix和原样保留的suffix；
- UserMessage、Assistant Continue、完整ToolRound、final AssistantMessage和已有summary如何保持协议完整；
- 如何通过PromptSet和ModelGateway执行portable SummaryModel调用；
- Compaction entry何时append/apply并触发conversation Replace；
- 压缩后如何重建ConversationSeed和private AgentLoop；
- Steer、Cancel、Workspace revocation、retry、write outcome unknown和restart如何处理；
- Workspace/Agent/Session instructions、Skill metadata和动态contribution如何保留。

[Runtime Interface](runtime-interface.md)不提供公开`CompactSession`协议或standalone/manual maintenance state。本文也不定义split-turn双摘要、provider-native opaque artifact、hierarchical summary tree、long-term memory、physical JSONL vacuum或provider tokenizer实现。

相关权威文档：

- [Conversation与SessionStorage架构设计](conversation-storage.md)
- [Session Execution架构设计](session-execution.md)
- [Turn执行上下文架构设计](turn-execution-context.md)
- [Prompt子系统架构设计](prompt.md)
- [ModelGateway架构设计](model-gateway.md)
- [ADR 0107](../adr/0107-compaction-uses-strict-stable-suffix.md)
- [Runtime Interface与公开协议架构设计](runtime-interface.md)

## 决策摘要

- Compaction是crate-internal纯planning/validation模块，不是Runtime Service或领域entity；
- SessionExecutor是orchestration、request arbitration和writer调用的唯一owner；
- SessionStorage是唯一durable truth；
- 只在active Turn的`NeedModel`安全点执行automatic compaction；
- trigger包括soft context pressure、Prompt local context overflow和provider context overflow；
- 不在final response后执行post-turn compaction，也不提供standalone/manual compaction；
- cut基于model-visible stable unit，不基于裸JSONL行；
- summarized range是连续prefix，retained range是连续suffix；
- cut不能拆分UserMessage、Assistant Continue、完整ToolRound、final AssistantMessage或Compaction summary；
- active Turn initiating UserMessage及其后全部history hard-protected；
- current Turn自身过大时返回`ProtectedSuffixTooLarge`，不使用split-turn summary；
- rolling summary使用“previous summary + newly summarized units”生成一个portable summary；
- SummaryModel使用active Turn exact `TurnModelSnapshot`；
- PromptSet是SummaryModel context的唯一组装者；purpose固定为`CompactionSummary`，output contract固定为`NoToolCalls`；
- SummaryModel成功不立即改变conversation；只有StoredCompaction append/apply后Replace生效；
- append前重新验证source checkpoint、Cancel和Workspace authorization；
- 成功后从updated CommittedConversationState重建ConversationSeed和private AgentLoop segment；
- 同一个active Turn最多执行一次automatic overflow recovery；
- soft-pressure compaction失败时，只有原ModelCallRequest仍valid才允许继续未压缩调用；
- hard-overflow compaction失败时Turn Failed；
- restart不恢复summary call、retry timer、CompactionPlan或旧AgentLoop；
- 原始entries始终保留，fork到Compaction前自然得到未压缩历史。

## 同类项目取舍

| 项目 | 采用 | 不采用 |
| --- | --- | --- |
| pi | rolling previous summary、recent suffix、Tool安全cut、append-only overlay | split-turn、post-run eager compact、manual implicit abort |
| Codex | model-call前检查、overflow recovery、replacement conversation | provider-native artifact、active-Turn cross-model fallback、直接live-history replacement |
| Claude Code | near-limit auto-compaction | 未公开的cut/storage细节；不复制`/compact` |
| Rig | rolling carry-over、window policy、orphan ToolResult保护 | load-path同步压缩、process-only watermark、system-role summary |
| Grok Build | Compaction与Session/Sampler分离 | checkout中不可验证的具体算法 |

MiniCore采用：

```text
Portable Rolling Summary
+ Strict Stable-Unit Cut
+ Contiguous Retained Suffix
+ One StoredCompaction Entry
+ Bounded Active-Turn Recovery
```

## Ownership

```text
SessionExecutor
├─ decides trigger
├─ owns CurrentCompactionState
├─ starts RunningOperation::CompactConversation
├─ calls PromptSet and ModelGateway
├─ arbitrates Steer/Cancel/revocation
└─ append/applies StoredCompaction

Compaction module
├─ context budget calculation
├─ stable-unit projection
├─ cut/protection planning
├─ summary directive construction
├─ summary input reduction
└─ result/commit candidate validation

PromptSet
└─ assembles CompactionSummary context

ModelGateway
└─ performs one provider-neutral SummaryModel call

SessionStorage
└─ validates StoredCompaction and emits trusted Replace delta
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

safe point包括initiating UserMessage后第一次Model前、`tool_round_completed`后下一次Model前、queued Steer append/apply后下一次Model前，以及provider ContextOverflow recovery。Sampling、WaitingApproval、ExecutingTools和Finishing期间不启动Compaction。

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
    pub minimum_reclaimed_tokens: u64,
    pub max_tool_result_summary_bytes: usize,
    pub max_summary_bytes: usize,
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
- `CompactionSummary`：latest projection中的已有summary；下一次rolling compaction将它与新增旧历史合成一个新summary。

Started、Abandoned或conversation-hidden Tool round不能成为summary source。

## Protection

hard-protect：

- active Turn initiating UserMessage entry及其unit；
- explicit caller-protected entries；
- 任何包含protected entry的完整unit；
- initiating UserMessage之后的全部committed model-visible history。

由于retained range必须是连续suffix，不会摘要active Turn内部的早期ToolRound。

保留current user原文可以避免summary改写任务目标、精确路径或错误文本，也避免split-turn summary和额外conversation ordering规则。

如果hard-protected suffix已超过usable input budget：

```text
CompactionError::ProtectedSuffixTooLarge
```

此时不会split current Turn、摘要或截断current UserMessage、删除ToolResult，或切换更大模型。hard overflow映射为TurnFailed；soft trigger仅在原调用仍通过Prompt validation时继续未压缩调用。

## Cut Algorithm

合法plan必须满足：

- summarized range是非空连续prefix；
- retained range是非空连续suffix；
- 两者在stable-unit boundary处相邻；
- 不拆分unit；
- retained suffix包含全部hard-protected units；
- summary source fit summary input budget；
- predicted summary + retained suffix不超过target-after budget；
- reclaimed tokens不低于minimum；
- source checkpoint和TranscriptFingerprint来自同一view。

选择过程：

```text
1. 构造ordered stable units
2. 找到active Turn initiating UserMessage unit
3. 建立hard-protected suffix lower bound
4. 根据target-after budget计算需要回收的token
5. 从较早boundary开始，保留尽可能多的recent exact units
6. predicted result仍过大时，把retained_from逐步向后移动
7. retained_from不能越过hard-protected lower bound
8. 选择第一个满足target和minimum reclaim的boundary
9. 验证summary source fit SummaryModel input budget
10. 生成CompactionPlan
```

目标是在满足安全空间的前提下，最大化原样保留的recent history。

```rust
pub enum CompactionErrorKind {
    Disabled,
    NothingToCompact,
    NoFeasibleCut,
    ProtectedSuffixTooLarge,
    SummarySourceTooLarge,
    SummaryFailed,
    SummaryInvalid,
    SourceChanged,
    Cancelled,
    AuthorizationRevoked,
    Storage,
    ContextStillTooLargeAfterCompaction,
}
```

## CompactionPlan

`ConversationBoundary`是完整stable unit reference：`summarized_through`是inclusive last summarized unit，`retained_from`是inclusive first retained unit。它保存unit first/last EntryId和不含storage-local identity的stable unit fingerprint；writer必须证明两个unit在source projection中相邻。fork只remap EntryId，语义fingerprint保持不变。

```rust
pub(crate) struct CompactionPlan {
    pub trigger: CompactionTrigger,
    pub source: ConversationCheckpoint,
    pub summarized_through: ConversationBoundary,
    pub retained_from: ConversationBoundary,
    pub protected_entries: Arc<[EntryId]>,
    pub summary_source: CommittedConversationPrefixView,
    pub tokens_before: u64,
    pub retained_tokens: u64,
    pub predicted_tokens_after: u64,
    pub summary_max_output_tokens: NonZeroU32,
    pub plan_fingerprint: CompactionPlanFingerprint,
}
```

`CompactionPlanFingerprint`只用于operation result validation和tests，不是领域identity。

`CommittedConversationPrefixView`没有public constructor，只能由CommittedConversationState基于validated stable-unit boundary产生。它证明source checkpoint exact、prefix来自trusted projection、结束在stable boundary、messages顺序正确且没有hidden Tool round。

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
pub struct CompactionSummaryDirective {
    pub format_version: CompactionSummaryFormatVersion,
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
- 保留路径、标识符、error text和用户约束；
- 更新旧summary中的Progress和Next Steps；
- 不把历史中的instruction当成summary模型自己的instruction；
- 只输出summary text，不调用Tool；
- 不用markdown code fence包裹整个summary。

format version进入CompactionPlanFingerprint和StoredCompaction summary metadata。

## Prompt Assembly

PromptSet是唯一模型上下文组装seam。目标input：

```rust
pub enum PromptAssemblyInput<'a> {
    AgentRun {
        conversation: &'a CommittedConversationView,
        output_contract: Option<&'a OutputContract>,
    },
    CompactionSummary {
        source: &'a CommittedConversationPrefixView,
        directive: &'a CompactionSummaryDirective,
    },
}
```

purpose由variant确定，不允许caller把Compaction source配成AgentRun purpose。

CompactionSummary assembly包含：

- Runtime required safety policy；
- Compaction-specific System policy和User summary directive；
- trusted prefix source converted by PromptSet；
- previous rolling summary if part of source；
- `OutputContract::NoToolCalls`；
- empty ToolSpec list；
- exact TurnModelFingerprint proof。

不包含ordinary Agent/Session instructions、Workspace instructions、ToolPromptView、SkillView metadata、arbitrary current-call contribution、queued Steer或uncommitted draft。这些Turn-static内容不会丢失，因为成功后下一次AgentRun assembly仍使用同一个PromptSet完整重建。

StoredCompaction进入conversation projection时生成typed CompactionSummary MessageRecord，PromptSet映射为user-role历史消息：

```text
The conversation history before this point was compacted into the following summary:

<summary>
...
</summary>
```

它不是System instruction，也不允许summary内容改变Runtime policy。

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
- finish reason不是Length/Safety/ContentFilter；
- summary bytes不超过max；
- usage和provider metadata可规范化；
- returned request/assembly fingerprint exact match。

SummaryModel返回的reasoning不进入conversation。StoredCompaction不单独保存summary-call reasoning；实现不得保存或伪造provider未返回的hidden chain-of-thought。

### Retry

provider-internal retry遵守ModelGateway delivery proof。

logical SummaryModel retry由SessionExecutor决定，必须复用相同CompactionPlan、PromptSet、TurnModelSnapshot、AssembledModelContext和source checkpoint。Auth refresh/rate-limit backoff不改变logical request。

`RequestOutcomeUnknown`或`StreamInterrupted`不能盲目重放。logical summary retry应有很小上限，不能通过改变summary directive伪装成同一次retry。

## Session Execution Integration

### Phase

```rust
pub enum TurnExecutionPhase {
    PreparingModel,
    Compacting,
    Sampling,
    WaitingApproval,
    ExecutingTools,
    Committing,
}
```

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
    pub plan_fingerprint: CompactionPlanFingerprint,
    pub original_model_request: Option<Arc<ModelCallRequest>>,
    pub cancellation: CancellationToken,
}
```

soft trigger可以暂存尚未发送的original request。任何Steer、Cancel、revocation或conversation change都使它失效。

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
→ mark overflow recovery attempted
→ Compaction::plan(trigger = ProviderContextOverflow)
→ phase = Compacting
→ start CompactConversation
```

启动Compaction本身不推进`execution_version`，因此soft failure在没有control或conversation变化时仍可复用original request。成功append/apply Replace后才推进version。Cancel和revocation立即推进version并取消operation；Compacting期间到达的Steer只排队并使saved original request不再可发送，Steer UserMessage append/apply也不推进execution_version。

同一个active Turn最多一个automatic overflow recovery；该turn-lifetime flag保存在`CurrentTurnExecution`，不保存在operation-local CurrentCompactionState。soft compaction不消耗allowance；provider或local hard overflow开始recovery时立即标记已使用，即使summary后续失败也不能开启第二次hard recovery。

### RunningOperation

```rust
RunningOperation::CompactConversation {
    turn_id,
    execution_version,
    source,
    plan_fingerprint,
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
- WorkspaceAuthorizationRevoked先out-of-band revoke lease，再设置同一`EmergencyControl`的revocation signal；不等待普通lane容量；
- PrepareForUnload通过`LifecycleControl`停止new admission；grace deadline到期时转为fail-closed Cancel；
- ResolveInteraction与Compaction无关，不应存在由Compaction创建的Interaction。

Compaction启动前、结果返回后和StoredCompaction append前都必须观察最新emergency epoch并重新检查Workspace authorization。Cancel/revocation已生效时丢弃未commit summary并进入Interrupted处理。Steer本身不让已生成summary失真，因为它尚未append；成功commit后再compose/append Steer，然后重新assemble AgentRun。

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
WorkspaceCommitAuthorization valid
summary result valid
committed prefix中不存在覆盖同一cut的StoredCompaction（靠状态判断去重，非operation key冲突检测）
```

commit顺序：

```text
process all earlier queued control requests
→ winning Cancel/revocation：discard candidate并进入cleanup
→ validate candidate and current execution version
→ acquire WorkspaceCommitAuthorization
→ revalidate source checkpoint/control state while authorization is held
→ SessionWriter.append(StoredCompaction draft)
→ SessionWrite OutcomeUnknown时保守终结本次compaction，下次触发按committed prefix重新规划
→ apply CommittedConversationDelta::Replace
→ release authorization
```

Compaction summary本身不读取Workspace文件，但其active Turn和后续AgentRun依赖captured Workspace authorization，因此security revocation必须在线性化点前获胜。

### Success Flow

```text
StoredCompaction append/apply
→ CommittedConversationState checkpoint advances
→ increment execution_version
→ Replace messages = [new summary] + [retained contiguous suffix]
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
- 同一overflow recovery allowance已消耗；
- TurnFailed with `ContextStillTooLargeAfterCompaction`；
- 不无限compact-and-retry。

## Failure Policy

| Failure | Soft pressure | Hard overflow |
| --- | --- | --- |
| NothingToCompact / NoFeasibleCut | 原request仍valid时继续 | TurnFailed |
| ProtectedSuffixTooLarge | 原request通过validation时继续 | TurnFailed |
| SummarySourceTooLarge | 原request仍valid时继续 | TurnFailed |
| summary auth/rate/transport exhausted | 原request仍valid时继续 | TurnFailed |
| summary invalid/length/tool call | 原request仍valid时继续 | TurnFailed |
| source changed | 不使用candidate并fail closed | fail closed；不在同一operation内重计划 |
| Cancelled/revoked | 进入Turn interruption cleanup | 进入Turn interruption cleanup |
| storage failure | Session unavailable/Turn failure规则 | 同左 |
| still too large after commit | TurnFailed | TurnFailed |

soft fallback使用原ModelCallRequest必须满足conversation checkpoint、assembly fingerprint、execution version和control state全部未变；否则重新assemble，不能发送stale request。

`SourceChanged`不做transparent replan。合法Steer在Compacting期间尚未append，Cancel/revocation会推进version；因此source意外变化表示实现race或未经授权的projection advance，必须结束当前candidate。新的planning只能由SessionExecutor在重新进入NeedModel后作为新的明确操作启动；hard overflow allowance不会因此重置。

## StoredCompaction

权威storage shape：

```rust
pub struct StoredCompaction {
    pub turn_id: Option<TurnId>,
    pub source: ConversationCheckpoint,
    pub summarized_through: ConversationBoundary,
    pub summary: CompactionSummary,
    pub retained_from: ConversationBoundary,
    pub protected_entries: Arc<[EntryId]>,
    pub model_call: Option<StoredCompactionModelCall>,
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

`StoredCompactionModelCall`保存exact model、response id、usage、finish reason、provider metadata、logical retry count和assembled context fingerprint。失败attempt usage只进入ModelGateway telemetry，不写入Session totals。

### Writer Validation

SessionWriter必须验证：

- source checkpoint位于selected ancestry并等于current checkpoint；
- summarized_through和retained_from都是stable boundaries；
- summarized prefix非空、retained suffix连续且两者相邻；
- protected entries都存在于selected ancestry；
- protected entries所属完整unit全部位于retained suffix；
- 任何ToolRound都未被拆分；
- summary text非空且hash匹配；
- automatic SummaryModel entry有model_call；
- turn_id存在时Turn仍Running且与source path一致；
- 基于committed prefix状态的compaction去重规则成立。

caller提供的raw replacement messages不是StoredCompaction字段。trusted projector根据source、boundaries和summary确定性构造Replace delta，避免caller提交任意history rewrite。

### Projection

```text
old effective conversation
= [summarized prefix] + [retained suffix]

Compaction Replace
= [CompactionSummary message] + [same retained suffix]
```

Compaction entry本身推进ConversationCheckpoint和TranscriptFingerprint。下一次rolling compaction的source只看effective conversation，不重新展开更早已被覆盖的raw entries。

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
- target remap全部nested EntryId和ConversationBoundary references；
- replay在child中重建同一个summary + retained suffix；
- 不复制Compaction operation、AgentLoop、provider continuation或Workspace lease。

Compaction entry不暴露成独立public fork anchor，但selected message path上的Compaction自然随path生效。

## Events和Usage

automatic compaction可以发布process-local progress：

```text
CompactionStarted
CompactionProgress
CompactionCompleted
CompactionFailed
```

public event使用[Runtime Interface](runtime-interface.md)定义的per-session StateEvent与ProgressEvent。事件必须在对应committed fact apply后发布；progress可合并/丢弃，不进入JSONL或SessionCursor。

Session usage由assistant entries和`StoredCompaction.model_call` replay重建。SummaryModel usage属于Session total，但不属于assistant Item或普通Agent response usage。

## Security

- summary source只来自CommittedConversationPrefixView；
- 历史中的prompt injection只是待摘要data，不获得System authority；
- PromptSet不能把summary text插入Runtime instruction section；
- secret redaction在写StoredCompaction前执行；
- provider raw error/body不进入summary或JSONL；
- Workspace revocation在线性化点前阻止append；
- compaction不重新读取live Workspace绕过captured snapshot；
- no-tools contract防止summary产生外部副作用。

## Invariants

必须成立：

```text
COMP-001 Compaction source来自committed conversation
COMP-002 summarized prefix和retained suffix连续且相邻
COMP-003 cut只在stable-unit boundary
COMP-004 ToolRound不能拆分
COMP-005 active Turn initiating UserMessage保留
COMP-006 StoredCompaction append/apply前旧conversation仍权威
COMP-007 summary调用只经过PromptSet和ModelGateway
COMP-008 SummaryModel不能调用Tool
COMP-009 active Turn exact model identity不变
COMP-010 source checkpoint变化使candidate失效
COMP-011 Cancel/revocation可在append前获胜
COMP-012 Replace由trusted projector构造，不接收caller raw history
COMP-013 rolling compaction只产生一个effective summary
COMP-014 raw historical entries不被改写
COMP-015 restart不恢复旧summary operation
COMP-016 同一Turn automatic overflow recovery有界
COMP-017 static Prompt/Tool/Skill/Workspace instructions由后续AgentRun重新注入
COMP-018 Compaction不是Turn、Item或Interaction
```

## Tests

### Stable Cut Property Tests

随机生成合法conversation entry sequences，验证：

- 任意plan都不拆分ToolRound；
- summarized/retained ranges无重叠、无空洞；
- protected unit始终位于retained suffix；
- Replace后不存在orphan ToolResult；
- rolling N次后effective conversation仍只有一个leading summary；
- replay和live apply得到相同TranscriptFingerprint。

### Scenario Tests

至少覆盖：

1. 多个completed Turns后第一次Model前soft compact；
2. previous summary +新增Turns rolling compact；
3. active Turn含多个ToolRounds但initiating UserMessage全部保护；
4. protected suffix自身过大；
5. Tool messages durable但缺少`tool_round_completed`；
6. summary result返回ToolCall；
7. summary finish reason Length；
8. summary期间Steer排队；
9. summary期间Cancel；
10. summary期间Workspace revocation；
11. source checkpoint在result返回前变化；
12. write OutcomeUnknown后按committed prefix状态确认（compaction已存在则跳过）；
13. crash after append before apply；
14. provider ContextOverflow后一次成功recovery；
15. compact后仍overflow并TurnFailed；
16. soft compact失败后原request仍valid；
17. fork到Compaction前后；
18. large ToolResult head/tail reduction metadata；
19. static Workspace/Skill/Tool instructions未写入summary但下一次AgentRun仍存在；
20. encrypted reasoning不进入summary正文。

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

### Split-Turn Summary

优点是可以压缩超长active Turn；缺点是需要turn-prefix summary、current Turn continuation标记、额外cut/recovery/fork规则。设计选择明确报告`ProtectedSuffixTooLarge`。

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

1. 实现stable-unit projection和property tests；
2. 实现context pressure和CompactionPlan；
3. 扩展PromptAssemblyInput和CompactionSummaryDirective；
4. 在SessionExecutor加入`Compacting`和CurrentCompactionState；
5. 接入ModelGateway SummaryModel call；
6. 完成StoredCompaction writer validation和trusted Replace delta；
7. 完成replay/fork/crash tests；
8. 完成soft/hard failure和overflow allowance；
9. 建立summary quality fixtures；
10. 接入Runtime per-session StateEvent/ProgressEvent并完成公开contract tests。

## 完成检查

- [x] 确定Compaction ownership和边界。
- [x] 确定NeedModel trigger和soft/hard overflow分类。
- [x] 确定stable-unit cut和current UserMessage protection。
- [x] 确定rolling summary和连续suffix。
- [x] 确定PromptSet/ModelGateway summary调用。
- [x] 确定`TurnExecutionPhase::Compacting`和control arbitration。
- [x] 确定append/apply线性化和Replace projection。
- [x] 确定retry、failure、restart和fork。
- [x] 确定static instruction不进入summary。
- [x] 确定不做split-turn、manual、hierarchical或provider-native compaction。
- [x] 确定Runtime protocol不公开manual CompactSession。
- [ ] 实现Compaction module。
- [ ] 实现SessionExecutor integration。
- [ ] 实现Prompt/ModelGateway summary path。
- [ ] 实现storage/projector/replay/fork tests。
- [ ] 完成summary quality evaluation。
