# Compaction

> **Pre-refactor implementation contract.** 当前目标设计见[Compaction架构设计](../refactor/compaction.md)和[ADR 0027](../adr/0027-compaction-uses-strict-stable-suffix.md)。目标架构使用SessionExecutor、by-entry StoredCompaction、strict stable-unit cut和portable rolling summary；本文中的SessionRuntime、SessionWriteBatch、manual/post-run和ProviderNative描述不再是MVP权威规则。

`Compaction` 是会话级上下文压缩能力。MVP baseline 把当前会话路径上的旧历史摘要成一条 provider-neutral、模型可见的压缩摘要消息，并保留最近上下文；后期也可按当前模型 capability 使用 provider-native compact endpoint 生成 model-bound context replacement，使后续 Agent 运行在较小上下文中继续。按 Transcript-First 决策，`Compaction` 只产出 cut/protection/directive；模型上下文组装仍由 Prompt 完成，压缩 commit 后必须从 committed session path 重建新的 `ConversationSeed`。

一句话边界：

```text
Compaction computes cut/protection/directive for session context.
It does not assemble model context, drive Rig AgentRun, execute tools, scan resources, or write UI state.
```

## 设计决定

压缩由 `SessionRuntime` 编排，不由 `Driver` 执行。

```text
SessionRuntime
  → reads current root-to-leaf SessionEntry path
  → compaction.rs prepares cut point, protected EntryIds and summary directive
  → resolves CompactionMethod from trigger, user preference and model capabilities
  → Prompt assembles the CompactionSummary model context
  → ModelGateway generates portable summary or invokes a supported native compact endpoint
  → SessionHandle.commit(SessionWriteBatch::compaction(...))
  → SessionHandle rebuilds compatible context replacement + kept messages
  → SessionRuntime rebuilds ConversationSeed from committed storage
  → SessionRuntime may start Driver.drive_conversation(...) again with continue mode
```

`Driver` 只提供压缩决策所需事实：完成消息、模型调用 usage、provider error、context overflow error 和 run result。压缩触发使用 [UsageStats](usage-stats.md) 中定义的 `ContextUsage`，不是会话累计 token 消耗。压缩不是 Rig `AgentRun` 的 step，也不应该被塞进 `commit_pending_messages` 的 driver 实现里。

原因：

- 压缩改变的是 session projection，不是 Rig 协议状态机。
- 压缩要读取会话树、选择 `first_kept_entry_id`、提交 stable compaction batch、重建上下文并发 UI 事件。
- 压缩要走模型凭据、summary prompt、abort 和 auto retry，这些属于 `SessionRuntime` 和 `ModelGateway`。后期启用 hook system 时，压缩 hook 仍由 `SessionRuntime` 在压缩安全点调用。
- Rig `AgentRun` 可序列化，但序列化状态包含已累积的 conversation。压缩后继续 resume 旧 `AgentRun` 不能减少下一次 model input；压缩后应从重建后的 `ConversationSeed` 启动新的 continuation。

## 设计原则

- `Compaction` 只负责 token estimate、cut point、protected `EntryId` 集合、provider-neutral source selection、method plan rules、summary directive 和结果校验。
- `SessionRuntime` 负责 manual/auto orchestration、phase、abort、模型调用、事件和 post-run 顺序。
- `SessionHandle` / `SessionWriter` / `SessionStorage` 负责提交 compaction batch 和重建 root-to-leaf context。
- `Driver` / Rig Agent loop 不拥有压缩；overflow recovery 后从重建出的 `ConversationSeed` 启动新的 continuation。
- running 时提交的 `Compact` defer 到 post-run safe point，不隐式 abort active run。

pi coding-agent、LangChain 等实现可以作为摘要格式和 cut-point 行为的参考对象，但不构成 API、事件顺序或隐式 abort 行为的兼容承诺。MVP `SummaryModel` 的模型可见压缩消息固定为 user-role summary message：

```text
The conversation history before this point was compacted into the following summary:

<summary>
...
</summary>
```

该摘要替代 ordered committed conversation 的较早部分，不进入 system prompt。

## 其他项目参考

LangChain `SummarizationMiddleware` 在 `before_model` middleware 中触发摘要：按 tokens/messages/fraction 判断是否需要 summarize，选择 cutoff，把旧消息摘要成一条 `HumanMessage("Here is a summary of the conversation to date:\n\n...")`，然后用 `RemoveMessage(REMOVE_ALL_MESSAGES)` 替换状态为 `summary message + preserved messages`。它还会避免切开 AI/tool message pairs，并会裁剪 summary-generation 输入。

Pydantic AI 把 history 作为 `message_history` 传入新 run，并提醒：如果历史来自 compaction pipeline 且没有 round-trip system prompt，需要显式 reinject system prompt。它也强调 history 是可信服务端状态，未信任边界传来的 history 要 sanitize，特别是移除未解决 tool calls。对本项目的启发是：系统提示词不要依赖历史消息保留；压缩后的消息序列必须是协议安全的，不能留下 orphan tool result 或 unresolved tool call。

AutoGen 的 Memory 协议通过 `update_context` 在模型调用前修改 agent 的 `model_context`，例如把检索出的 memory 作为 system message 插入。它说明 memory/context injection 属于 agent context 层，而不是工具执行层或模型 provider 层。本项目压缩摘要也是 context projection，不是工具调用。

Claude Code 公开文档把 auto-compaction 描述为接近 context limit 时总结 conversation history，并提供 `/compact Focus ...` 形式的自定义压缩指令。它还建议把长指令迁移到 skills，以降低基础上下文。对本项目的启发是：支持手动 `Compact { instructions }`，并允许后续从项目资源中加载 compact instructions，但不要把资源加载和压缩执行混在一起。

## 模块 Interface

未来对应 `compaction.rs`。它提供压缩算法、摘要 prompt 构建、消息序列化和结果类型，但不持有 session storage、provider client、runtime event bus 或 Rig 类型。

```rust
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    pub summary_input_token_limit: Option<u64>,
    pub summary_max_output_tokens: u64,
}

pub enum CompactionMethod {
    SummaryModel,
    ProviderNative,         // post-MVP reserved
    DeterministicReduction, // bounded fallback
}

pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

pub struct CompactionPreparation {
    pub first_kept_entry_id: EntryId,
    pub protected_entry_ids: Vec<EntryId>,
    pub messages_to_summarize: Vec<MessageRecord>,
    pub turn_prefix_messages: Vec<MessageRecord>,
    pub is_split_turn: bool,
    pub tokens_before: u64,
    pub previous_summary: Option<String>,
    pub file_ops: CompactionFileOps,
    pub settings: CompactionSettings,
}

pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: EntryId,
    pub tokens_before: u64,
    pub estimated_tokens_after: Option<u64>,
    pub details: CompactionDetails,
    pub usage: Option<PersistedModelCallUsage>,
}
```

`protected_entry_ids` 是硬不变量：这些 entry 不进入 `messages_to_summarize`，也不能被 split-turn prefix summary 吞掉。典型来源包括刚提交、即将启动本次 run 的 `CanonicalUserMessage` entry，以及 overflow recovery 中必须保留的 committed suffix。

`CompactionResult.usage` 使用 durable `PersistedModelCallUsage`。`ModelGateway` 返回的 runtime `ModelCallUsage` 必须由 `SessionRuntime` 去除 `raw_provider_usage` 后再放入 result；hook-provided summary 没有模型调用时为 `None`。

当前 `CompactionResult { summary, ... }` 是 `SummaryModel` baseline。后期 `ProviderNative` 若返回 GPT 类加密 context item，需要把 replacement 扩展为 portable summary 或 model-bound opaque artifact；上层只保存 opaque envelope 和 compatibility metadata，不解析或记录 payload。原始 session entries 继续保留，以便模型切换、artifact 拒绝或 capability 变化时重新压缩。具体 replacement enum 和 wire/storage 字段不属于 BR-053，留待 ProviderNative 开工时定型。

推荐函数：

```rust
pub fn estimate_context_tokens(messages: &[MessageRecord]) -> ContextUsageEstimate;
pub fn should_compact(tokens: u64, context_window: u64, settings: &CompactionSettings) -> bool;

pub fn prepare_compaction(
    path_entries: &[SessionEntry],
    settings: &CompactionSettings,
) -> Result<Option<CompactionPreparation>, CompactionError>;

pub fn build_summary_directive(
    preparation: &CompactionPreparation,
    instructions: Option<&str>,
) -> CompactionSummaryDirective;

pub fn build_turn_prefix_summary_directive(
    preparation: &CompactionPreparation,
) -> Option<CompactionSummaryDirective>;

pub fn finish_compaction(
    preparation: CompactionPreparation,
    history_summary: String,
    turn_prefix_summary: Option<String>,
) -> CompactionResult;

pub fn compaction_summary_to_message(entry: &CompactionEntry) -> MessageRecord;
```

`CompactionSummaryDirective` 是 `Compaction` 产出的摘要指令、目标消息和输出预算，不是给 `ModelGateway` 的请求，也不是 Rig run，也不是最终 `AssembledModelContext`：

```rust
pub struct CompactionSummaryDirective {
    pub messages: Vec<MessageRecord>,
    pub instruction: String,
    pub max_output_tokens: u64,
}
```

`Compaction` 只拥有 cut point、protected entry set、summary instruction、摘要目标输入和 `summary_max_output_tokens`。它不选择 provider/model，不决定 thinking/stream policy，不分配 `ModelCallId` / `RunId`，不调用 `Prompt.assemble_model_context(...)`，也不构造 `ModelCallRequest`。

`SessionRuntime` 收到 directive 后，使用当前 `ModelState` 或 user-global settings 中的摘要模型选择，决定 thinking/stream policy，分配 correlation id，并把 directive 交给 Prompt 的唯一模型上下文组装 seam：active/pre-run work chain 复用当前 `ModelContextProfile` 和稳定 conversation prefix；standalone compaction 先通过 `prepare_message_turn()` 产生确定性 profile。`Prompt.assemble_model_context(profile, directive.messages + directive.instruction, purpose = CompactionSummary) -> AssembledModelContext`，随后构造唯一请求：`ModelCallRequest { purpose: ModelCallPurpose::CompactionSummary, input: assembled_context, max_output_tokens: Some(directive.max_output_tokens), ... }`。调用策略禁用 tool execution；最终仍由 [ModelGateway](model-gateway.md) 解析 provider、auth、fallback、usage 和错误分类。

`estimate_context_tokens()` 是 `ContextUsage` fallback helper，不是成本统计。它应遵循 [UsageStats](usage-stats.md) 的规则：优先使用最近一次有效 assistant provider usage，再估算后续消息；没有 provider usage 时才估算完整模型可见上下文。

压缩阈值使用：

```text
should_compact(context_usage.current_tokens, context_usage.context_window, settings)
```

不要用 `SessionStatsView.total_usage` 触发压缩；会话累计消耗不会因为压缩下降，不能代表下一次模型请求的上下文占用。

`SessionRuntime` 实现 `CompactionSummarizer` adapter，内部调用 ModelGateway：

```rust
#[async_trait]
pub trait CompactionSummarizer {
    async fn summarize(
        &self,
        directive: CompactionSummaryDirective,
        cancel: CancellationToken,
    ) -> Result<String, CompactionError>;
}
```

## 压缩准备

`prepare_compaction()` 输入当前 leaf 的 root-to-leaf path，而不是 UI messages。它必须使用 `SessionEntry`，因为压缩边界需要稳定的 `EntryId`。

流程：

1. 如果 path 为空，或最后一条已经是 `compaction`，返回 `None`。
2. 从后往前找到最新 `SessionEntry::Compaction`。
3. 如果存在上一条 compaction：
   - `previous_summary = prev.summary`。
   - `boundary_start` 是上一条 `first_kept_entry_id` 在 path 中的位置；如果找不到，退化为上一条 compaction 之后。
4. 如果不存在上一条 compaction：`boundary_start = 0`。
5. 用 `session::load_committed_conversation(path)` 得到当前 committed conversation，并按 `ContextUsage` 规则估算 `tokens_before`。
6. 从 path 尾部向前累计估算 token，选择能保留约 `keep_recent_tokens` 的 cut point。
7. cut point 必须是协议安全点：不能从 tool result 开始保留，也不能制造 orphan tool result 或 unresolved tool call。
8. 如果 cut point 落在一个用户 turn 中间，记录 `turn_prefix_messages`，并设置 `is_split_turn = true`。
9. 收集 `messages_to_summarize`，排除旧 compaction entry 本身和所有 `protected_entry_ids`，但保留 branch summary、custom message 和 context-visible runtime message。
10. 从被摘要消息中提取文件操作，用于 summary 尾部的 `<read-files>` / `<modified-files>`。

MVP 可以先只允许切在 user/custom/bash turn boundary。完整版本再支持 split turn。

## 摘要指令与 Profile 复用

摘要调用复用 active/pre-run work chain 的 `ModelContextProfile` 和尽可能长的稳定 conversation prefix；standalone compaction 通过 `prepare_message_turn()` 生成确定性 profile。`Compaction` 只构建 `CompactionSummaryDirective`，Prompt 把 directive instruction 作为最后一条 typed user message 追加，并用 `OutputContract::NoToolCalls` 禁止工具调用。这样 system/tool/profile 前缀保持稳定，但摘要调用不会执行工具；provider adapter 无法表达该 contract 时必须返回 `UnsupportedCapability`，不能静默放开工具。

摘要 instruction：

```text
You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.
```

初次摘要 user prompt：

```text
The preceding messages are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
...

## Constraints & Preferences
...

## Progress
### Done
...

### In Progress
...

### Blocked
...

## Key Decisions
...

## Next Steps
...

## Critical Context
...

Keep each section concise. Preserve exact file paths, function names, and error messages.
```

增量摘要在 prompt 中追加上一条摘要：

```text
<previous-summary>
{previous_summary}
</previous-summary>
```

然后要求模型保留已有信息、加入新消息、更新 Progress 和 Next Steps。

如果 split turn，需要第二个 summary request：

```text
This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.

Summarize the prefix to provide context for the retained suffix:

## Original Request
...

## Early Progress
...

## Context for Suffix
...
```

最终 summary 合并为：

```text
{history_summary}

---

**Turn Context (split turn):**

{turn_prefix_summary}
```

然后追加文件操作区块：

```text
<read-files>
path/a.rs
path/b.rs
</read-files>

<modified-files>
path/c.rs
</modified-files>
```

摘要目标选择规则：

- user/custom/bash/branch summary 由 Prompt 的 `MessageRecord -> ModelMessage` seam 转成协议安全内容。
- assistant text、thinking 摘要、tool calls 保留关键内容。
- tool result 必须截断，默认保留开头并标记被截断字符数。
- image 内容只保留占位描述和来源摘要，不把二进制或大 base64 放入摘要 prompt。
- 不序列化 provider metadata、UI 展示状态或凭据。

## 压缩后的 Prompt 组装

上下文重建的权威规则在 [SessionManager / SessionStorage](session-manager.md)；本节只说明 `Compaction` 产出的摘要如何成为模型可见消息，以及为什么它不是 system prompt。

压缩不是删除旧 entry，而是通过 `Compaction` batch 提交一条 `SessionEntry::Compaction`。上下文重建负责把 append-only history 投影成模型可见 messages。

```rust
pub enum SessionEntry {
    Message { base: EntryBase, message: MessageRecord },
    Compaction {
        base: EntryBase,
        summary: String,
        first_kept_entry_id: EntryId,
        tokens_before: u64,
        estimated_tokens_after: Option<u64>,
        details: CompactionDetails,
        usage: Option<PersistedModelCallUsage>,
        from_hook: bool,
    },
    // ...
}
```

`SessionHandle.load_committed_conversation()` / `session::load_committed_conversation(path)` 规则：

```text
if no compaction on path:
  messages = all context-visible messages on path

if latest compaction exists:
  messages = [compaction_summary_message]
  messages += context-visible entries before compaction, starting at first_kept_entry_id
  messages += context-visible entries after compaction
```

也就是说，如果一次压缩发生在第 100 条 entry，且 `first_kept_entry_id` 指向第 80 条：

```text
entry 1..79     summarized into compaction summary
entry 80..99    kept recent suffix before the compaction entry
entry 100       compaction entry itself
entry 101..end  normal messages after compaction
```

模型实际看到：

```text
system: {ModelContextProfile.system_prompt}
user:   The conversation history before this point was compacted into the following summary:
        <summary>...</summary>
...     kept messages from first_kept_entry_id onward
...     messages after compaction
```

压缩摘要进入 committed conversation / 后续 `ConversationSeed`，不进入 `ModelContextProfile` 的长期 system prompt。

推荐内部消息类型：

```rust
pub enum MessageRecord {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    Custom(CustomMessage),
    BranchSummary(BranchSummaryMessage),
    CompactionSummary(CompactionSummaryMessage),
}

pub struct CompactionSummaryMessage {
    pub summary: String,
    pub tokens_before: u64,
    pub entry_id: EntryId,
    pub timestamp: Timestamp,
}
```

转换为 model-visible message 时，`Prompt.assemble_model_context(...)` 必须把 `MessageRecord::CompactionSummary` 映射为一条 `ModelMessage::User`，其 `content` 是一个 `ModelContentPart::Text`，文本使用下面的稳定包裹格式：`The conversation history before this point was compacted into the following summary:\n\n<summary>...</summary>`。Compaction 文档只规定语义和文本模板，不定义第二套 `ModelMessage` 类型或 provider mapping。

理由：

- user-role summary 明确表达它是 conversation history 的替代物，而不是长期行为规则，因此不应进入 system prompt。
- system prompt 仍由目标 turn 的 `ModelContextProfile` 从 captured resources 和 typed views 构建，避免 Pydantic AI 提到的 system prompt round-trip 问题。
- UI 可以把 `CompactionSummaryMessage` 渲染为折叠块，但模型看到的是明确标签包裹的摘要。

## Current User 保护

Transcript-First 不保留 durable history / current input 双 lane。刚被接收、即将触发本次 Agent run 的 canonical user message 已经位于 committed session path 中；Compaction 通过其 committed `EntryId` 将它排除在 summary target 之外，压缩 commit 后再从 committed state 构造一条 ordered `ConversationSeed`。

```text
committed conversation before protected user
  → compact if needed
  → rebuild summary + retained suffix
  → retain protected user at its committed position
  → build ConversationSeed in which it appears exactly once
```

如果 canonical user message 已通过 `UserInput` batch 提交，`SessionRuntime` 必须按 committed entry id 把它从本次 compaction candidate 中排除；它不通过第二个参数或 lane 再次传递。已提交的 Steer 同理：它通过 `CommittedConversationDelta` 成为 ordered conversation 的普通 user message，后续 compaction 再按目标 cut/protection 规则处理。

RAG、memory、IDE diagnostics 等 `CurrentCall` context 不进入 compaction source；`CurrentRun` context 是否摘要必须由其 owner 显式升级为 durable entry，不能由 Compaction 猜测。

## Run 前阈值压缩

`SessionRuntime` 在最终 `UserInput` batch commit 后、分配 `RunId` 前做同步、best-effort threshold gate。它使用 ordered committed conversation、protected entry set 和 prompt/tool baseline 的 `ContextUsage` 估算；若超过阈值，立即从 `Turn + current_run = None` 切换到独立 `Compaction` phase并发布 compaction lifecycle，再排除本次 committed user entry 执行压缩。压缩后切回 `Turn`，重建 committed conversation / `ConversationSeed` 并重新检查一次；通过后才分配 `RunId` 和发布 `run_started`。没有可压缩历史、protected user 本身过大或压缩后仍超限时返回 typed unrecoverable context-limit error，不启动幽灵 run。

该 gate 用于提前避免常见超限，不替代 `Driver` 每次 CallModel 前的最终 `Prompt.assemble_model_context(...)` 校验。tool results、Steer 或 transient context 可能让 run 内后续 call 才超限，此时走下面的 overflow recovery。

## 手动压缩流程

```text
agent_runtime_protocol::AgentCommand::Compact { instructions }
  → AgentRuntime.dispatch
  → SessionRuntime checks phase
  → if idle: start manual compaction now
  → if active work: store PendingSessionAction::Compact
      → emit queue_updated { pending_actions: [compact] }
      → current work continues unchanged
      → after terminal handling (required stable commit if any) + terminal facts + required retry chain
      → remove pending action and emit queue_updated
  → phase = compaction
  → emit `session_phase_changed` + `compaction_started { reason: manual }`
  → path = SessionStorage.get_path_to_root(current_leaf)
  → preparation = compaction::prepare_compaction(path, settings)
  → future RuntimeHookRegistry.invoke(SessionBeforeCompact)
  → future hook may cancel, patch instructions, or provide CompactionResult
  → directive = compaction::build_summary_directive(preparation, instructions)
  → Prompt.assemble_model_context(...) reuses stable profile/prefix and applies OutputContract::NoToolCalls
  → SessionRuntime constructs ModelCallRequest { purpose: CompactionSummary, input }
  → ModelGateway.generate_model_turn summarizes directive input
  → SessionHandle.commit(SessionWriteBatch::compaction(...))
  → reload/apply committed conversation and build ConversationSeed
  → update SessionRuntime committed conversation projection / next TurnState basis
  → emit `compaction_finished`
  → future RuntimeHookRegistry.invoke(SessionCompact) // observer; failure only diagnostics
  → phase = idle
```

手动 `Compact` 使用 `CommandRunPolicy::QueueAfterRun`。它绝不隐式 abort 当前 run，也不清理 pending approval、resume state 或消息队列。pending compact 是 `SessionRuntime` 持有的结构化 action，不是 follow-up/next-turn message；执行优先级高于 queued steering/follow-up continuation；`NextTurn` 不会自动启动。

同一 session 同时只允许一个 pending manual compact。重复请求返回 `CompactAlreadyQueued`，保留第一次请求的 instructions；已处于 compaction phase 时返回 `CompactionAlreadyRunning`。`AbortRun`、`ClearQueue`、session close 或 runtime shutdown 会清除 pending compact。普通 run failure 如果不再 retry，仍会执行已经排队的 compact。

如果没有可压缩内容，返回结构化错误：`AlreadyCompacted` 或 `NothingToCompact`。

`AbortCompaction` 只取消当前 summary model call、后期 ProviderNative endpoint wait 和 hook wait，不修改已写入的 session entry。

## 自动压缩流程

`SessionRuntime` 在 run 完成后执行 post-run 检查：

```text
Driver returns ConversationRunResult::Completed
  → SessionRuntime has committed the final AssistantFinal batch
  → compute ContextUsage from projected session context
  → should_compact(context_usage.current_tokens, context_window, settings)
  → run_auto_compaction(reason = Threshold, will_retry = false)
```

threshold 压缩成功后不自动重跑刚完成的回答；如果 queued steering continuation / follow-up 有消息，压缩后从最新 committed state 构造新的 `ConversationSeed` 并启动 continuation 或下一次 prompt。next-turn queue 保留到下一次显式 prompt，不自动启动 run。

如果 post-run safe point 已有 pending manual compact，先执行 manual compact，并跳过同一 safe point 的 threshold auto compaction，避免连续生成两次摘要。context overflow recovery / immediate retry chain 优先于 pending manual compact；等当前 work chain 稳定结束后再执行用户排队的压缩。

## Overflow Recovery

context overflow 是压缩和 retry 的交界点。

```text
Driver returns ConversationRunResult::Failed
  { error: DriverError::ContextLimitExceeded { source: PromptAssembly | Provider, ... } }
  → SessionRuntime verifies the error belongs to the current model/work chain
  → if overflow recovery has not been attempted in this work chain:
       emit diagnostics only; do not persist the transient partial/error message
       run_auto_compaction(reason = Overflow, will_retry = true)
       rebuild committed conversation and start a new RunId with
       ConversationSeed + continuation reason ContextOverflowRecovery
  → else:
       stop with typed ContextStillTooLargeAfterCompaction
```

`PromptAssembly` 表示本地最终模型上下文组装已超限且没有调用 provider/产生 model-call usage；`Provider` 表示 provider 已拒绝请求并可能带 attempt/usage。两者共享 recovery policy，但 diagnostics 和 usage source 不合并。transient overflow error、partial assistant 和旧 `AgentRun` 的未完成状态都不进入 `SessionWriter`，因此 retry 只从最后 committed conversation 重建，不需要删除任何 committed message。

压缩后的重试不要 resume 旧 Rig segment。应使用重建后的 `ConversationSeed` 启动新的 continuation。

overflow recovery budget 跨它创建的新 `RunId` 保留，整个 work chain 最多自动 compact-and-continue 一次。若 preparation 返回 `NothingToCompact`、protected current input 已占满有效窗口，或 recovery 后再次收到任一来源的 context limit，必须 fail closed，不能形成 compact/run 循环。

## AgentRuntimeEvents And RuntimeSnapshot

压缩相关事件沿用 `AgentRuntimeProtocol`，生命周期顺序以 [AgentRuntimeEvents](agent-runtime-events.md) 为准：

```text
session_phase_changed { phase: compaction }
compaction_started { reason }                  // session coordinate is on outer Event
SessionHandle.commit(SessionWriteBatch::compaction(...)) // internal, no public persistence event
compaction_finished { result, aborted, will_retry }
usage_updated { context_usage }
if immediate retry/continuation:
  session_phase_changed { phase: turn }
  run_started
else:
  session_phase_changed { phase: idle }
  session_settled { next_turn_count }
```

立即执行且没有后续工作时，推荐顺序是 `session_phase_changed(compaction)` → `compaction_started` → internal `SessionHandle.commit(SessionWriteBatch::compaction(...))` → `compaction_finished` → `usage_updated` → `session_phase_changed(idle)` → `session_settled`。running 时提交的 manual compact 先发 `queue_updated(pending compact)`；当前 work chain 结束后发 `queue_updated(remove pending compact)`，再进入上述 compaction 顺序。如果 `will_retry = true` 或压缩后将立即启动 queued steering/follow-up continuation，则必须改为 `compaction_finished` → `usage_updated` → `session_phase_changed(turn)` → `run_started`，不能经过 `Idle` 或先发 `session_settled`。需要等待 retry delay 时进入 `RetryBackoff`。`NextTurn` queue 可以保留且不阻止 settled。

压缩后的 `usage_updated` 主要更新 `ContextUsageView`，不减少 `SessionStatsView.total_usage`。

建议 view 类型：

```rust
pub struct CompactionResultView {
    pub summary_preview: String,
    pub first_kept_entry_id: EntryId,
    pub tokens_before: u64,
    pub estimated_tokens_after: Option<u64>,
    pub from_hook: bool,
}
```

快照中 `messages` 应包含 UI 可渲染的 `CompactionSummary` view，但可以默认折叠，不把完整 summary 放进列表预览。需要展开时可读取对应 session entry。

## Hooks

压缩相关 hook 是后期能力，完整边界见 [RuntimeHooks](runtime-hooks.md)。启用后，`SessionRuntime` 在压缩模型调用前触发 `SessionBeforeCompact`，在压缩条目 commit、projection 更新和 `compaction_finished` 发布后触发 `SessionCompact` observer；hook 不直接写 session entry，也不直接发布 `compaction_finished`。

`SessionBeforeCompact` 的结果应保持 typed：

```rust
pub enum BeforeCompactDecision {
    Continue,
    Cancel { reason: String },
    PatchInstructions { instructions: String, replace: bool },
    ProvideResult { result: CompactionResult },
}
```

所有 hook 返回的 `CompactionResult` 都必须经过校验：`first_kept_entry_id` 必须在当前 path 上，summary 不能为空，不能包含未受信任的外部文件内容引用。

## 设计约束

- 如果把摘要放进 system prompt，会污染行为指令层，也会让 system prompt 与 session history 纠缠；保持 user-role summary。
- 如果让 `Driver` 自动压缩，driver 会被迫理解 session tree、持久化、hooks 和 retry；这会把一个深模块变浅。
- 如果压缩后 resume 旧 `AgentRun`，上下文不会真的变小；必须从重建后的 session context 重新 continue。
- 如果 cut point 允许落在 tool result 上，会生成 provider 不接受的 orphan tool result；cut point 必须协议安全。
- 如果 overflow error 被误写入 session 并参与重试，模型会看到一次无意义失败；transient overflow diagnostics 和 partial assistant 不得进入 writer。
- 如果 summary generation 不限制输入，大型工具输出会让压缩本身 overflow；必须序列化并截断 tool results，后续支持 chunked summary。

## 不应承担

`Compaction` 模块不应：

- 调用 Rig `AgentRun`。
- 构造 `ModelCallRequest` 或选择模型调用策略。
- 执行工具或读取工作区文件。
- 持有 provider registry、API key 或 fallback policy。
- 绕过 `SessionWriter` 直接追加 session entry 或移动 leaf。
- 发送 UI event。
- 修改 system prompt。
- 扫描 skills、resources 或 prompt templates。

这些职责分别属于 `Driver`、`SessionRuntime`、`Tools`、`ModelGateway`、`SessionHandle` / `SessionStorage`、`SessionRuntime`、`Prompt` 和 `ResourceManager`。
