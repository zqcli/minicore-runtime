# ADR 0112: Compaction支持active-Turn checkpoint与模型感知预算

状态：Accepted
日期：2026-07-25

## 背景

ADR 0107要求保留active Turn的initiating UserMessage及其后的连续model-visible suffix。该规则保证current user request和ToolRound协议完整，但也使一个长agentic Turn中已经完成的早期ToolRound永远不能压缩。代码搜索、文件读取、构建和测试日志会让该suffix单调增长，即使pre-Turn历史已经全部摘要，仍会在目标编码场景中触发`ProtectedSuffixTooLarge`并使Turn失败。

同时，`CompactionSettings.summary_max_output_tokens`原样进入plan和directive，没有与active Turn固定的`EffectiveModelLimits`求交。若全局摘要输出上限高于当前模型上限，`ModelCallRequest::new`会在provider调用前拒绝每一个Compaction请求。Gateway保持严格校验是正确的，缺失的是Compaction planning seam上的effective budget。

MiniCore仍需保持SessionStorage durable truth、完整ToolRound cut、单一SessionExecutor/Writer、portable summary和append-before-Replace。问题不要求建立第二份conversation truth，而是要求StoredCompaction能够表达active Turn内部已经完成的进度checkpoint，并让预算在请求fingerprint前闭合。

## 决策

1. Compaction仍是crate-internal planning/validation模块，SessionExecutor仍是唯一orchestration owner，SessionStorage仍是唯一durable truth。SummaryModel仍使用active Turn exact `TurnModelSnapshot`、`ModelCallPurpose::CompactionSummary`和`OutputContract::NoToolCalls`。
2. cut仍只发生在model-visible stable-unit boundary，不拆分UserMessage、Assistant Continue、完整ToolRound、final AssistantMessage或已有Compaction summary。Started、Pending、Abandoned、conversation-hidden或缺少`tool_round_completed`的Tool round永不成为摘要覆盖范围。
3. automatic Compaction支持两种scope：
   - `ConversationPrefix`：把active Turn之前的连续conversation prefix滚动为一个leading summary，保留recent exact suffix；
   - `ActiveTurnCompletedPrefix`：把active Turn中的每个exact UserMessage（initiating input或已append Steer）视为一个instruction-segment anchor，在该anchor之后把已经完整提交的早期units滚动为一个`ActiveTurnCheckpoint`，并保留该segment的recent exact tail。
4. `ActiveTurnCompletedPrefix`的模型可见投影固定为：

   ```text
   optional leading conversation summary / exact pre-Turn tail
   → exact initiating UserMessage
   → optional checkpoint for initiating segment
   → exact Steer UserMessage
   → optional checkpoint for that Steer segment
   → ...
   → recent exact active-Turn tail
   ```

   active scope的summary request把current effective prefix through selected anchor作为protected context，帮助模型理解“继续”等依赖历史的任务，但该prefix不进入本次summary coverage，也不能被checkpoint替换或改写。若pre-Turn context导致第一个segment的summary request不可行，必须先执行`ConversationPrefix`，不能静默省略。
5. 每个active instruction segment拥有独立、单调的coverage frontier。第一次覆盖anchor UserMessage之后的一段完整stable units，且不能跨越下一个active-Turn UserMessage；后续scope使用指向当前effective checkpoint unit的`previous_checkpoint`，再从该checkpoint backing compaction派生covered-through provenance，并只允许把其后的新完成连续units合并为一个replacement checkpoint。checkpoint boundary与原始coverage frontier不得混为一个字段；frontier不得后退、交叉或留下空洞，每个segment至多一个effective checkpoint。
6. initiating input和所有已append Steer UserMessage都保持原文和相对位置。planner优先保留recent exact tail；explicit caller-protected entries、Pending Interaction相关内容、未完成ToolRound和任何包含它们的完整stable unit都必须位于未摘要区域。只有这些真实protected regions自身仍超过usable budget时才返回`ProtectedRegionTooLarge`；已经完成的早期Assistant/ToolRound不再因“位于initiating UserMessage之后”自动受保护。
7. 大ToolResult在summary request representation中继续使用带tool name、call identity、outcome、head/tail、原始字节数、content hash和omitted bytes的确定性reduction。durable Tool message不改写；checkpoint覆盖完整ToolRound，不能产生孤立ToolCall或ToolResult。首版不把无摘要的任意截断直接安装为conversation结果。
8. `StoredCompaction`保存typed `CompactionScope`、source checkpoint、coverage boundaries/provenance、protected entries、portable summary和model-call provenance。caller不能提交raw replacement messages；trusted projector根据scope确定性构造leading-summary Replace或anchor-after checkpoint Replace。live apply、replay和fork必须生成相同`TranscriptFingerprint`。
9. active Turn在admission时捕获immutable `CompactionSettingsSnapshot`，配置reload只影响future Turn。automatic compaction在同一Turn内可以重复，但必须有界：
   - 同一个`source checkpoint + scope frontier`最多启动一次hard-overflow recovery；
   - 成功append/apply并推进frontier后，只有新增完整stable units达到`minimum_reclaimed_tokens`才可再次compact；
   - `CompactionSettings.max_compactions_per_turn`限制单Turn总次数；
   - compact后没有推进frontier或仍无可行cut时fail closed，不进行无界compact-and-retry。
10. PromptSet通过窄的`CompactionSummaryAssemblyBasis`暴露fixed summary policy/output-contract开销的token estimate与fingerprint，不暴露或复制完整assembly实现。planning阶段用该basis派生一个immutable `CompactionSummaryBudget`，输入至少包括Turn-pinned Compaction设置、pinned `EffectiveModelLimits`、summary source的reduced token estimate、candidate directive、PromptSet固定摘要开销和runtime safety reserve。最终输出上限满足：

    ```text
    effective summary output
    = min(
        configured summary maximum,
        known model output maximum,
        context window - fixed summary prompt - reduced source - safety reserve
      )
    ```

    未知limit保持unknown，不根据model name猜测；已知limit必须参与求交。最终预算写入`CompactionPlan`、`CompactionSummaryDirective`和`CompactionPlanFingerprint`。
11. `CompactionSettings`同时提供`summary_min_output_tokens`。若已知模型/context限制不能留下该最低输出空间，planning返回`NoFeasibleSummaryBudget`；它是Compaction domain error，不得落成Gateway `InvalidRequest`。PromptSet仍做最终context validation，ModelGateway仍拒绝超出effective model limit的请求且绝不静默clamp。plan、Prompt assembly proof、`ModelCallRequest` constructor和SessionExecutor append gate负责验证临时budget/plan/directive/model-limit一致性；SessionStorage的`validate_and_project`只验证durable entry能够重建的scope、boundary、hash、checkpoint和coverage provenance，cold replay不声称重新验证已经消失的plan或limits。
12. pre-Turn leading summary和active-Turn checkpoint都是user-role historical checkpoint，不获得System authority。普通Agent/Session/Workspace/Tool/Skill静态内容不写入summary，由下一次AgentRun assembly从同一个TurnExecutionContext重新注入。
13. 首版仍不提供standalone/manual compaction、hierarchical summary tree、provider-native opaque artifact、active-Turn cross-model fallback或load-path模型调用。

## 后果

- 长编码Turn可以在完整ToolRound安全点持续释放早期搜索结果、文件内容和测试日志，不再把常见agentic负载归类为不可恢复边缘情况。
- active Turn中的initiating input和Steer都保持原文，未完成副作用和Pending Interaction保持精确；安全保护从“initiating UserMessage之后的一切”收窄为exact UserMessage anchors与真正不能摘要的region。
- projection和writer validation比单一prefix/suffix复杂：必须验证scope、anchor、previous checkpoint及其covered-through provenance、coverage连续性和checkpoint placement，但复杂度集中在Compaction与SessionStorage seam，不扩散给调用方。
- 一个effective conversation最多有一个leading history summary，并且每个active instruction segment最多一个checkpoint；同一segment重复压缩不会形成summary链。
- Gateway继续作为严格执行模块，不承担Compaction policy。预算错误在请求构造前以Compaction domain error暴露。
- 极端情况下，exact active UserMessage anchors、未完成ToolRound或最低保留tail本身仍可能超过模型窗口，此时`ProtectedRegionTooLarge`是诚实且不可避免的失败。

## 修订关系

本ADR取代[ADR 0107](0107-compaction-uses-strict-stable-suffix.md)。ADR 0107中关于durable truth、stable-unit cut、portable summary、PromptSet/ModelGateway唯一调用路径、append/apply后Replace、exact active-Turn model、control arbitration、restart/fork和不公开manual compaction的决策继续保留；连续retained suffix、active Turn全量hard-protect和单Turn仅一次overflow recovery由本ADR修订。

## 被否决的方案

### 继续保护整个active Turn

实现最简单，但会让目标编码任务随ToolRound增长确定性失败，不能作为v1主路径。

### 只扩大context window或切换更大模型

只能推迟溢出，并违反active Turn exact model pin；无法解决无界日志增长。

### UI提示用户开启新Turn

可以作为最终降级，但会人为切断当前Tool/Interaction流程，不能代替runtime内部checkpoint。

### 让ModelGateway静默clamp摘要输出

会改变调用方已经fingerprint的policy，并让plan中的token feasibility与真实请求不一致。预算必须在Compaction planning阶段闭合。
