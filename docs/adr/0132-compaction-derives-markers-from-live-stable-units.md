# ADR 0132：Compaction从Revision-Bound Stable Units派生Marker

状态：Accepted
日期：2026-07-31

## 背景

ADR 0124把`StoredCompaction`收窄为portable rolling summary、single `first_kept_entry_id` marker和optional safe model-call provenance；ADR 0126又把execution truth移到`LiveSessionState`，Compaction先Replace live conversation再best-effort record marker。

current planner仍只接收：

```rust
LiveConversationView {
    revision: ConversationRevision,
    messages: Arc<[ModelMessage]>,
}
```

该view没有EntryId或protocol-unit origin，无法在重复消息、完整Tool exchange和已有rolling summary上安全派生`first_kept_entry_id`。按`ModelMessage` value equality反查EntryId会在相同文本重复出现时选错边界；把Assistant ToolCall与部分ToolResult拆开又会生成provider-invalid retained suffix。

同时，`CompactionSettingsSnapshot`没有来源、字段或默认值，`StoredCompactionModelCall`也缺少ModelGateway已经要求的finish、requested max output和allowlisted provider metadata。继续实现会迫使Live reducer、Prompt、Compaction和Storage各自补一套不兼容解释。

## 决策

1. `LiveConversationView`继续只服务普通Prompt assembly，不增加EntryId、storage ordinal或Compaction policy。Live conversation reducer额外提供crate-private、immutable `LiveCompactionSourceView`：

   ```rust
   pub(crate) struct LiveCompactionSourceView {
       session_id: SessionId,
       revision: ConversationRevision,
       units: Arc<[LiveCompactionUnit]>,
   }

   pub(crate) struct LiveCompactionUnit {
       first_entry_id: EntryId,
       kind: CompactionUnitKind,
       messages: Arc<[ModelMessage]>,
   }

   pub(crate) enum CompactionUnitKind {
       RollingSummary,
       UserMessage,
       AssistantMessage,
       ToolExchange,
   }
   ```

   fields与constructors保持private。该view只表达provider-valid stable-unit order和exact live origin，不携带token estimate、settings、model limits、storage commit状态或hash。

2. stable unit规则固定为：

   - ordinary UserMessage和无ToolCall AssistantMessage各自一个unit；
   - 含ToolCall的Assistant只有在全部matching truthful ToolResult存在后，才与按assistant call顺序排列的全部ToolResult形成一个不可拆`ToolExchange` unit；其`first_entry_id`是Assistant entry；
   - current rolling summary是leading `RollingSummary` unit，其origin是安装该summary的外层`StoredSessionEntry.entry_id`；
   - incomplete、orphan、abandoned-first Tool exchange、Interaction、progress、stream draft和recording health不进入source；
   - 相同message value由不同EntryId区分；合法marker只能是unit的`first_entry_id`。

3. Compaction plan持有exact source snapshot和prefix cut index，不保存caller独立提供的prefix、suffix与marker三份事实：

   ```rust
   pub(crate) struct CompactionPlan {
       source: Arc<LiveCompactionSourceView>,
       summarized_unit_count: NonZeroUsize,
       summary_source: Arc<CompactionSummarySourceView>,
       directive: CompactionSummaryDirective,
       budget: CompactionSummaryBudget,
       // private settings/model/estimate proof
   }
   ```

   `first_kept_entry_id`只能由：

   ```text
   source.units[summarized_unit_count].first_entry_id
   或 summarized_unit_count == source.units.len() 时为None
   ```

   派生。prefix永远非空且连续；retained suffix只能是其余完整units。Prompt的CompactionSummary source只包含待摘要prefix的确定性reduced representation，不包含retained suffix。live Replace从current reducer重建retained suffix，不接受caller提供的replacement vector。

4. Token estimation不进入reducer source。Compaction使用Turn-pinned `TokenEstimator`估算units、reduced summary source和directive；plan保存所用estimate/budget proof。PromptSet提供两个窄basis：

   - `AgentRunCompactionAssemblyBasis`：next AgentRun中conversation之外的fixed input cost，以及future rolling-summary message envelope cost；
   - `CompactionSummaryAssemblyBasis`：required summary System policy、`NoToolCalls`与empty ToolSpec的fixed cost和typed structural basis。

   两个basis与Compaction model basis必须来自同一个TurnExecutionContext和exact `TurnModelSnapshot`。不新增Compaction fingerprint、snapshot ID或第二assembly实现。

5. Runtime startup config拥有一个validated `CompactionSettings`；Turn admission把immutable `CompactionSettingsSnapshot`捕获到`TurnExecutionContext`。MVP不提供Compaction独立hot reload、per-Agent/per-Session override、runtime command或durable setting。future per-Session policy必须进入`SessionDefinition`并产生revision语义。

   首版字段与默认值固定为：

   | Field | Type | Default |
   | --- | --- | ---: |
   | `enabled` | `bool` | `true` |
   | `pressure_reserve_tokens` | `NonZeroU32` | `4096` |
   | `summary_min_output_tokens` | `NonZeroU32` | `512` |
   | `summary_max_output_tokens` | `NonZeroU32` | `2048` |
   | `minimum_reclaimed_tokens` | `NonZeroU32` | `2048` |
   | `max_compactions_per_turn` | `NonZeroU8` | `4` |
   | `summary_safety_reserve_tokens` | `NonZeroU32` | `512` |

   Runtime initialization要求`summary_min_output_tokens <= summary_max_output_tokens`。全部token arithmetic使用checked `u64`；overflow不能wrap或saturate成可行plan。

6. Compaction pressure/plan只接收完整private input：exact source、captured settings、Prompt bases、Turn-pinned model limits/estimator/effective AgentRun output reservation、trigger和本Turn已经started的Compaction operation数量。trigger固定为`ProactivePressure | PromptContextOverflow | ProviderContextOverflow`。

   proactive pressure使用：

   ```text
   estimated AgentRun input
   = AgentRun fixed input
   + sum(all stable-unit estimates)

   effective headroom
   = max(
       settings.pressure_reserve_tokens,
       TurnModelSnapshot effective AgentRun output reservation
     )
   ```

   known input加headroom达到context window时为`Recommended`；input本身达到或超过window，或Prompt/provider已经返回ContextOverflow时为`Required`。unknown context或unknown unit estimate不触发proactive Compaction；hard overflow下对应`UnknownContextLimit`或`UnestimableSource`并fail closed。

7. planner按oldest-prefix到newest依次尝试cut，选择第一个可行cut，因此保留最大exact suffix。每个candidate必须同时满足：

   ```text
   summary max output
   = min(
       configured summary maximum,
       known model output maximum,
       context window
         - summary fixed prompt
         - reduced source
         - directive
         - summary safety reserve
     )
   ```

   `summary max output >= configured summary minimum`；并且：

   ```text
   estimated post-Replace AgentRun input
   = AgentRun fixed input
   + rolling-summary message envelope
   + summary max output
   + retained stable-unit estimates

   estimated post-Replace input + effective headroom <= context window
   estimated before - estimated post-Replace >= minimum_reclaimed_tokens
   ```

   summary source中的大ToolResult可以按`CompactionSummaryFormatVersion`执行确定性reduction，但durable/live ToolResult不改写，Tool exchange unit不拆分。没有可行candidate时返回typed Compaction error；ModelGateway不clamp。

8. `max_compactions_per_turn`计算已经成功assemble immutable summary request，并把exact plan/request安装为current Compaction operation、即将开始summary logical call chain的次数。ActiveTurnTask在该原子task-local安装点递增，随后才允许第一次Gateway调用；同一request的一次logical retry不再计数。pressure、planning、Prompt assembly或request construction失败均不计数。Recommended Compaction不可行且ordinary AgentRun仍valid时可以跳过并发布diagnostic；Required Compaction不可行时Turn按ContextOverflow failure收口。

9. automatic SummaryModel路径的唯一provenance schema由Compaction拥有：

   ```rust
   pub struct StoredCompactionModelCall {
       pub model: ModelResponseSummary,
       pub response_id: Option<ProviderResponseId>,
       pub usage: Option<ModelUsage>,
       pub finish_reason: ModelFinishReason,
       pub requested_max_output_tokens: NonZeroU32,
       pub logical_retry_count: u8,
       pub metadata: ProviderResponseMetadata,
   }
   ```

   `validate_summary()`从exact plan与validated `ModelCallResult`构造该值：model/result必须匹配Turn-pinned model；finish只能是`Stop | Unknown`；结果必须规范化为exact一个non-empty Text block，Reasoning不写summary；ToolCall、Refused或empty output拒绝；`logical_retry_count <= 1`；requested max output只能取plan budget。automatic Compaction始终写`model_call = Some(...)`。`None`只为future明确设计的deterministic maintenance/import保留，不是overflow fallback。

10. apply顺序固定为：

    ```text
    validate current Turn/control/exact plan/request/session/revision
    → derive marker from plan cut
    → LiveSessionState allocates Compaction EntryId + binds parent
    → Replace live conversation with
         new RollingSummary(origin = new Compaction EntryId)
         + exact retained units
    → increment ConversationRevision once
    → await SessionRecorder.record(the same StoredSessionEntry)
    → publish/continue
    ```

    recording failure不回滚Replace。queued Steer本身不改变revision；只有safe point真正apply的Steer、Assistant、complete Tool exchange或其他Compaction会使旧plan stale。

11. cold replay在当前effective recorded stable-unit projection上解释marker。`Some(id)`只有在它exact匹配index大于0的unit `first_entry_id`时有效；指向ToolResult内部、missing/orphan/ignored entry、已经被旧Compaction替换的unit或第一个unit均忽略并报告bounded diagnostic。`None`表示覆盖全部非空effective units。valid replayed summary的新unit origin是该Compaction外层entry ID；后续valid Compaction仍可在先前ignored marker之后生效。

## 后果

- duplicate message、parallel Tool completion和repeated rolling summary都可以用exact EntryId boundary测试，不再依赖message equality。
- Live conversation reducer只承担协议分组与origin；Compaction集中承担model-aware估算、reduction、cut、budget和provenance，Prompt仍是唯一assembly seam。
- plan保留immutable source snapshot会增加当前Compaction operation的短期内存，但不复制第二份mutable conversation，也不进入wire/storage。
- 默认值是Runtime policy，不是durable history identity；future调整只影响新Runtime/new Turn capture。
- 首版要求known context window和可估算source才能执行automatic hard-overflow recovery；不为unknown model limits猜值。

## 被否决的方案

### 扩大LiveConversationView

否决原因：普通Prompt assembly不需要EntryId/kind；扩大后会把storage origin泄漏到所有model call，并让Compaction policy污染唯一ordinary conversation view。

### 从ModelMessage反查EntryId

否决原因：重复文本、等价结构化内容和Tool exchange跨多entry时不唯一，无法构造replay-safe marker。

### 把estimated_tokens存进LiveCompactionUnit

否决原因：unit protocol identity属于reducer，estimate属于Turn-pinned model policy。若不同时携带estimator identity会漂移；若携带又会让LiveConversation projection变成model-specific planning state。Compaction在exact snapshot上计算并持有estimate即可。

### 独立保存summary prefix、retained suffix和first_kept_entry_id

否决原因：三份caller-controlled boundary可以互相矛盾。source + cut可以确定性派生全部结果，接口更深。

### 增加retained-suffix target或checkpoint chain

否决原因：first feasible cut已经最大化exact suffix；minimum reclaim和post-Replace fit提供有界进展。MVP不恢复ADR 0112的scope/frontier/protected-entry复杂度。

## 修订关系

本ADR细化ADR 0124的single prefix marker、ADR 0126的live-first Replace/recording顺序和ADR 0123的immutable plan/exact refs。它保留ADR 0112的model-aware summary budget与deterministic ToolResult reduction，但不恢复被ADR 0124删除的active checkpoint、scope、coverage frontier或protected-entry schema。
