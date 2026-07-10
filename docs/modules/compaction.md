# Compaction

`Compaction` 是会话级上下文压缩能力。它把当前会话路径上的旧历史摘要成一条模型可见的压缩摘要消息，并保留最近上下文，使后续 Agent 运行在较小上下文中继续。

一句话边界：

```text
Compaction transforms session context.
It does not drive Rig AgentRun, execute tools, scan resources, or write UI state.
```

## 设计决定

压缩由 `SessionRuntime` 编排，不由 `Driver` 执行。

```text
SessionRuntime
  → reads current root-to-leaf SessionEntry path
  → compaction.rs prepares cut point and summary prompts
  → ModelGateway generates summary
  → SessionHandle appends SessionEntry::Compaction
  → SessionHandle rebuilds context from SessionStorage as summary + kept messages
  → SessionRuntime may start Driver again with DriveEntry::Continue
```

`Driver` 只提供压缩决策所需事实：完成消息、模型调用 usage、provider error、context overflow error 和 run result。压缩触发使用 [UsageStats](usage-stats.md) 中定义的 `ContextUsage`，不是会话累计 token 消耗。压缩不是 Rig `AgentRun` 的 step，也不应该被塞进 `before_next_model_call` 的 driver 实现里。

原因：

- 压缩改变的是 session projection，不是 Rig 协议状态机。
- 压缩要读取会话树、选择 `first_kept_entry_id`、追加 session entry、重建上下文并发 UI 事件。
- 压缩要走模型凭据、summary prompt、abort 和 auto retry，这些属于 `SessionRuntime` 和 `ModelGateway`。后期启用 hook system 时，压缩 hook 仍由 `SessionRuntime` 在压缩安全点调用。
- Rig `AgentRun` 可序列化，但序列化状态包含已累积的 conversation。压缩后继续 resume 旧 `AgentRun` 不能减少下一次 model input；压缩后应从重建后的 session context 启动新的 `DriveEntry::Continue`。

## 来自 pi 的生产经验

pi 的压缩路径是：

```text
AgentSession
  owns manual/auto compaction orchestration

core/compaction
  owns token estimate, cut point, summary prompt, summary generation helpers

SessionManager
  owns appendCompaction + buildSessionContext

agent-loop / Agent
  does not own compaction
```

关键行为：

- pi 的 `AgentSession.compact()` 会 abort 当前 agent，发 `compaction_start`，读取 `SessionManager.getBranch()`，调用 `prepareCompaction()`，允许 extension 在 `session_before_compact` hook 中取消或提供摘要，然后 `appendCompaction()`，最后 `buildSessionContext()` 并写回 agent state。MiniCore 只借鉴压缩准备和上下文重建，不沿用进程内 API 的隐式 abort：公开 `Compact` 在 running 时必须 defer 到 post-run safe point。
- `_checkCompaction()` 在 agent run 结束后或提交新 prompt 前检查 threshold 和 overflow。
- `_runAutoCompaction()` 处理自动压缩、context overflow recovery、`willRetry` 和事件。
- `core/compaction` 的职责是纯压缩逻辑与摘要生成；源码注释明确说 session manager handles I/O, after compaction session is reloaded。
- `SessionManager.buildSessionContext()` 消费最新 compaction entry，把摘要消息放在重建上下文最前面，然后接上被保留的旧消息和压缩后的新消息。

pi 的模型可见压缩消息不是 system prompt，而是一个 user-role summary message：

```text
The conversation history before this point was compacted into the following summary:

<summary>
...
</summary>
```

本项目沿用这个方向。

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

pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

pub struct CompactionPreparation {
    pub first_kept_entry_id: EntryId,
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
}
```

推荐函数：

```rust
pub fn estimate_context_tokens(messages: &[MessageRecord]) -> ContextUsageEstimate;
pub fn should_compact(tokens: u64, context_window: u64, settings: &CompactionSettings) -> bool;

pub fn prepare_compaction(
    path_entries: &[SessionEntry],
    settings: &CompactionSettings,
) -> Result<Option<CompactionPreparation>, CompactionError>;

pub fn build_summary_material(
    preparation: &CompactionPreparation,
    instructions: Option<&str>,
) -> CompactionSummaryMaterial;

pub fn build_turn_prefix_summary_material(
    preparation: &CompactionPreparation,
) -> Option<CompactionSummaryMaterial>;

pub fn finish_compaction(
    preparation: CompactionPreparation,
    history_summary: String,
    turn_prefix_summary: Option<String>,
) -> CompactionResult;

pub fn compaction_summary_to_message(entry: &CompactionEntry) -> MessageRecord;
```

`CompactionSummaryMaterial` 是 `Compaction` 产出的摘要内容和输出预算，不是给 `ModelGateway` 的请求，也不是 Rig run：

```rust
pub struct CompactionSummaryMaterial {
    pub system_prompt: String,
    pub messages: Vec<MessageRecord>,
    pub max_output_tokens: u64,
}
```

`Compaction` 只拥有 cut point、summary instructions、summary system prompt、模型可见摘要输入和 `summary_max_output_tokens`。它不选择 provider/model，不决定 thinking/stream policy，不分配 `ModelCallId` / `RunId`，也不构造 `ModelCallRequest`。

`SessionRuntime` 收到 material 后，使用当前 `ModelState` 或 user-global settings 中的摘要模型选择，决定 thinking/stream policy，分配 correlation id，并构造唯一请求：`ModelCallRequest { purpose: ModelCallPurpose::CompactionSummary, tools: [], max_output_tokens: Some(material.max_output_tokens), ... }`。最终仍由 [ModelGateway](model-gateway.md) 解析 provider、auth、fallback、usage 和错误分类。

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
        material: CompactionSummaryMaterial,
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
5. 用 `session::build_session_context(path)` 得到当前模型上下文，并按 `ContextUsage` 规则估算 `tokens_before`。
6. 从 path 尾部向前累计估算 token，选择能保留约 `keep_recent_tokens` 的 cut point。
7. cut point 必须是协议安全点：不能从 tool result 开始保留，也不能制造 orphan tool result 或 unresolved tool call。
8. 如果 cut point 落在一个用户 turn 中间，记录 `turn_prefix_messages`，并设置 `is_split_turn = true`。
9. 收集 `messages_to_summarize`，排除旧 compaction entry 本身，但保留 branch summary、custom message 和 context-visible runtime message。
10. 从被摘要消息中提取文件操作，用于 summary 尾部的 `<read-files>` / `<modified-files>`。

MVP 可以先只允许切在 user/custom/bash turn boundary。完整版本再支持 split turn。

## 摘要 Prompt

摘要调用不应复用 Agent run 的 system prompt，也不应暴露工具。`Compaction` 先构建 `CompactionSummaryMaterial`；`SessionRuntime` 再将它转换为 `ModelCallRequest { purpose: CompactionSummary, tools: [], max_output_tokens: Some(...) }`，并交给 `ModelGateway` 执行。

系统提示词：

```text
You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.
```

初次摘要 user prompt：

```text
<conversation>
{serialized_conversation}
</conversation>

The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

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

序列化规则：

- user/custom/bash/branch summary 转成可读文本。
- assistant text、thinking 摘要、tool calls 保留关键内容。
- tool result 必须截断，默认保留开头并标记被截断字符数。
- image 内容只保留占位描述和来源摘要，不把二进制或大 base64 放入摘要 prompt。
- 不序列化 provider metadata、UI 展示状态或凭据。

## 压缩后的 Prompt 组装

上下文重建的权威规则在 [SessionManager / SessionStorage](session-manager.md)；本节只说明 `Compaction` 产出的摘要如何成为模型可见消息，以及为什么它不是 system prompt。

压缩不是删除旧 entry，而是追加一条 `SessionEntry::Compaction`。上下文重建负责把 append-only history 投影成模型可见 messages。

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
        from_hook: bool,
    },
    // ...
}
```

`SessionHandle.build_session_context()` / `session::build_session_context(path)` 规则：

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
system: {prompt::build_system_prompt(...)}
user:   The conversation history before this point was compacted into the following summary:
        <summary>...</summary>
...     kept messages from first_kept_entry_id onward
...     messages after compaction
```

压缩摘要进入 `TurnState.messages`，不进入 `TurnState.system_prompt`。

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

转换为 Rig/provider message 时：

```rust
MessageRecord::CompactionSummary(m) => ModelMessage::User {
    content: format!(
        "The conversation history before this point was compacted into the following summary:\n\n<summary>\n{}\n</summary>",
        m.summary,
    ),
}
```

理由：

- user-role summary 最接近 pi 和 LangChain 的做法。
- 它是 conversation history 的替代物，不是长期行为规则，因此不应进入 system prompt。
- system prompt 仍由 `prompt.rs` 每次重建，避免 Pydantic AI 提到的 system prompt round-trip 问题。
- UI 可以把 `CompactionSummaryMessage` 渲染为折叠块，但模型看到的是明确标签包裹的摘要。

## 手动压缩流程

```text
agent_runtime_protocol::AgentCommand::Compact { instructions }
  → AgentRuntime.dispatch
  → SessionRuntime checks phase
  → if idle: start manual compaction now
  → if active work: store PendingSessionAction::Compact
      → emit queue_updated { pending_actions: [compact] }
      → current work continues unchanged
      → after terminal facts + save point + required retry chain
      → remove pending action and emit queue_updated
  → phase = compaction
  → emit `session_phase_changed` + `compaction_started { reason: manual }`
  → path = SessionStorage.get_path_to_root(current_leaf)
  → preparation = compaction::prepare_compaction(path, settings)
  → future RuntimeHookRegistry.invoke(SessionBeforeCompact)
  → future hook may cancel, patch instructions, or provide CompactionResult
  → material = compaction::build_summary_material(preparation, instructions)
  → SessionRuntime constructs ModelCallRequest { purpose: CompactionSummary, tools: [] }
  → ModelGateway summarizes material
  → append SessionEntry::Compaction
  → rebuild SessionContext
  → update SessionRuntime message projection / next TurnState basis
  → future RuntimeHookRegistry.invoke(SessionCompact)
  → emit `compaction_finished`
  → phase = idle
```

手动 `Compact` 使用 `CommandPhasePolicy::DeferUntilPostRun`。它绝不隐式 abort 当前 run，也不清理 pending approval、resume state 或消息队列。pending compact 是 `SessionRuntime` 持有的结构化 action，不是 follow-up/next-turn message；执行优先级高于普通 queued continuation。

同一 session 同时只允许一个 pending manual compact。重复请求返回 `CompactAlreadyQueued`，保留第一次请求的 instructions；已处于 compaction phase 时返回 `CompactionAlreadyRunning`。`AbortRun`、`ClearQueue`、session close 或 runtime shutdown 会清除 pending compact。普通 run failure 如果不再 retry，仍会执行已经排队的 compact。

如果没有可压缩内容，返回结构化错误：`AlreadyCompacted` 或 `NothingToCompact`。

`AbortCompaction` 只取消 summary model call 和 hook wait，不修改已写入的 session entry。

## 自动压缩流程

`SessionRuntime` 在 run 完成后执行 post-run 检查：

```text
Driver returns DriveResult::Completed
  → SessionRuntime persists assistant/tool messages
  → compute ContextUsage from projected session context
  → should_compact(context_usage.current_tokens, context_window, settings)
  → run_auto_compaction(reason = Threshold, will_retry = false)
```

threshold 压缩成功后不自动重跑刚完成的回答；如果 follow-up/steering/next-turn queue 有消息，压缩后再启动 `DriveEntry::Continue` 或下一次 prompt。

如果 post-run safe point 已有 pending manual compact，先执行 manual compact，并跳过同一 safe point 的 threshold auto compaction，避免连续生成两次摘要。context overflow recovery / immediate retry chain 优先于 pending manual compact；等当前 work chain 稳定结束后再执行用户排队的压缩。

## Overflow Recovery

context overflow 是压缩和 retry 的交界点。

```text
Driver returns DriveResult::Failed { error: ContextOverflow, messages }
  → SessionRuntime verifies error belongs to current model
  → if overflow recovery has not been attempted:
       persist UI-visible failure if desired, but exclude it from retry context
       run_auto_compaction(reason = Overflow, will_retry = true)
       start a new drive_run with DriveEntry::Continue { reason: ContextOverflowRecovery }
  → else:
       emit `compaction_finished` with recovery failure and stop
```

因为本项目使用 append-only session storage，不能像 pi 那样只从 mutable `agent.state.messages` 中移除错误消息。若要把 overflow assistant error 保存在 session 里，必须显式标记它不参与模型上下文：

```rust
pub enum ContextVisibility {
    ModelVisible,
    UiOnly,
}
```

或把 overflow failure 保存成 diagnostic/custom entry，而不是普通 assistant message。不要把 transient overflow error 留在重试 prompt 中。

压缩后的重试不要使用 `DriveEntry::Resume`。应使用重建后的 session context 启动新的 `DriveEntry::Continue`。

## AgentRuntimeEvents And RuntimeSnapshot

压缩相关事件沿用 `AgentRuntimeProtocol`，生命周期顺序以 [AgentRuntimeEvents](agent-runtime-events.md) 为准：

```text
session_phase_changed { phase: compaction }
compaction_started { session_id, reason }
compaction_finished { session_id, result, aborted, will_retry }
usage_updated { context_usage }
persistence_save_point { had_pending_mutations }
session_phase_changed { phase: idle }
session_settled { session_id, next_turn_count }
```

立即执行时推荐顺序：`session_phase_changed(compaction)` → `compaction_started` → `compaction_finished` → `usage_updated` → `persistence_save_point` → `session_phase_changed(idle)` → `session_settled`。running 时提交的 manual compact 先发 `queue_updated(pending compact)`；当前 work chain 结束后发 `queue_updated(remove pending compact)`，再进入上述 compaction 顺序。如果 `will_retry = true`，压缩后可以直接启动后续 `run_started`，不先发 `session_settled`。

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

压缩相关 hook 是后期能力，完整边界见 [RuntimeHooks](runtime-hooks.md)。启用后，`SessionRuntime` 在压缩模型调用前触发 `SessionBeforeCompact`，在压缩条目写入后触发 `SessionCompact` observer；hook 不直接写 session entry，也不直接发布 `compaction_finished`。

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
- 如果 overflow error 作为普通 assistant message 保存并参与重试，模型会看到一次无意义失败；append-only 存储需要显式 `UiOnly` 或 diagnostic entry。
- 如果 summary generation 不限制输入，大型工具输出会让压缩本身 overflow；必须序列化并截断 tool results，后续支持 chunked summary。

## 不应承担

`Compaction` 模块不应：

- 调用 Rig `AgentRun`。
- 构造 `ModelCallRequest` 或选择模型调用策略。
- 执行工具或读取工作区文件。
- 持有 provider registry、API key 或 fallback policy。
- 追加 session entry 或移动 leaf。
- 发送 UI event。
- 修改 system prompt。
- 扫描 skills、resources 或 prompt templates。

这些职责分别属于 `Driver`、`SessionRuntime`、`Tools`、`ModelGateway`、`SessionHandle` / `SessionStorage`、`SessionRuntime`、`Prompt` 和 `ResourceManager`。
