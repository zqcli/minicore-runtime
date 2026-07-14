# 消息组装跨项目对照研究

日期：2026-07-14  
分支：`research/message-assembly-cross-project-study`  
状态：研究归档（非已接受 ADR）  
关联：`docs/review/background-session-runtime-progress.md` §BR-055  
场景锚点：长历史（已压缩）+ 项目 md + skill 调用带图 + 工具轮 + mid-run steering

## 0. 文档说明

本文归档 2026-07-14 对 MiniCore 消息组装流程的对照研究，包含：

1. **综合对照分析**（第 1–6 节）：在同一复杂场景下对比 pi、Codex、Claude Code 与 MiniCore 目标方案，提炼可借鉴点。
2. **场景流程图回顾**（第 2 节）：MiniCore Transcript-First 目标模型下的六阶段组装流。
3. **附录 A–C**：三份 subagent 原始调研报告全文（pi 本地源码、Codex main 源码、Claude Code 官方+逆向）。

**权威边界**：现行行为与架构仍以 `docs/modules/prompt.md`、`session-runtime.md`、`driver.md`、`session-manager.md`、`model-gateway.md`、`compaction.md` 及已接受 ADR 为准。本文是研究进度与推荐，不是已关闭 issue 或已落地实现。

---

## 1. 研究动机与问题

MiniCore 当前消息链路（UI → intent → parts/messages → commit → lanes → Driver/Rig → projection → ModelGateway → provider）经 BR-055 审计确认过重：

- user input 约 **10–11** 个表示层，多个中间表示都像 source of truth；
- 四个字段都叫 `messages`，owner/生命周期不同；
- **P0**：`TurnState.messages` 未跨过 `DriveRequest`，历史 seed 缺失；
- 长期 `durable_history/current_input` lane 贯穿整个 run，但保护语义只在 pre-run 有价值；
- `MessageRecord` 含 session 域 variant，直接进 `ModelGateway` 会迫使每个 adapter 猜映射。

目标：验证 **Transcript-First + ConversationSeed** 是否与成熟实现收敛；若是，哪些实现细节可直接吸收。

---

## 2. MiniCore 目标模型与场景流程（回顾）

### 2.1 三层输入 × 两个组装时机

```text
┌─ Profile 层（turn 级，慢路径，每 turn 一次）
│    system prompt + active tool schemas = PromptCallProfile
│    内容：base/custom、behavior、workspace md、environment、
│         tool guidelines、skill 摘要
│    来源：TurnResourceSnapshot + ToolProfileBaseline
│
├─ Transcript 层（唯一有序对话流，committed truth 投影）
│    compaction summary + 保留历史 + 当前 user message
│    + run 内 committed assistant/tool/steer delta
│    来源：SessionStorage → ConversationSeed → run-scoped live transcript
│
└─ Overlay 层（call 级瞬态，不落盘）
     RAG/memory/IDE 等；MVP 为空
```

组装只发生在：

1. **Turn boundary**：`capture_turn` → `assemble_turn` → `resolve_intent` → `commit(UserInput)`  
2. **Model-call boundary**：`project_model_call(profile, transcript, overlay)` → `ModelInputProjection<ModelMessage>`

### 2.2 复杂场景六阶段

场景：已有压缩历史 + `CLAUDE.md` + `/skill code-review …` + 截图 → pre-run compact → tool round → mid-run steer → 最终回答。

```text
① Turn Boundary
   admit PromptIntent
   → ResourceManager.capture_turn → TurnResourceSnapshot
   → Tools.capture_profile_baseline
   → prompt::assemble_turn → PromptCallProfile（Profile 层）
   → resolve_intent → 一条 canonical UserMessage（skill 正文+文字+图）
   → SessionWriter.commit(UserInput) → EntryId 标记 pre-run 保护

② Pre-run Gate（阈值触发）
   Compaction.prepare(protect EntryId)
   → ModelCallPurpose::CompactionSummary
   → commit(Compaction) 只改 Transcript 层

③ Seed & Start
   build_session_context
   → ConversationSeed（summary + retained + 当前输入恰好一次）
   → DriveRequest { conversation, turn: DriverTurnInput, … }
   → run_started

④ Model Call #1
   project_model_call → ModelMessage[] → ModelCallRequest { input: projection }
   → ModelGateway → provider

⑤ Tool Round
   invoke_tool_batch → commit(ToolRound) → exact committed delta
   → 喂回 Rig → live transcript 前进

⑥ Steer → 后续 call → Done
   before_next_model_call 消费 steer intent
   → resolve + commit(UserInput) → 同 RunId rollover
   → 再次 project_model_call（前缀稳定）
   → commit(AssistantFinal) → run_finished
```

Source-of-truth 一句话：

```text
SessionStorage 拥有「发生过什么」
Prompt 拥有「模型看见什么」
Driver/Rig 只拥有当前 run 的协议投影
ModelGateway 只拥有 provider 编码
```

---

## 3. 三家同场景组装流程摘要

### 3.1 pi（coding-agent 生产路径）

| 环节 | 行为 |
| --- | --- |
| 提交 | `AgentSession.prompt` 内展开 skill/template → 普通 user message |
| live history | `agent.state.messages` |
| 持久化 | JSONL 会话树（`id/parentId`） |
| 每 call | `transformContext` → `convertToLlm` → `transformMessages` → 厂商 DTO |
| system prompt | base + `<project_context>` AGENTS.md + skills 摘要；**session 级缓存** |
| 压缩 | run 结束后；`CompactionEntry`；`buildSessionContext` 重建 live messages |
| steering | 内存队列；工具轮后、下次调用前注入；**注入后才落盘**；abort 丢弃未注入 |

**核心选择**：厂商无关 `AgentMessage` 贯穿全程；非标准消息作一等 message 存会话树；只在模型调用边界经 `convertToLlm` 降解为 provider 格式。

**注意**：`AgentHarness` 与 coding-agent 生产路径逻辑同构但**代码不复用**——反面教材。

### 3.2 Codex（codex-rs main，2026-07）

| 环节 | 行为 |
| --- | --- |
| 请求 | `Prompt { input, tools, base_instructions, … }` |
| base instructions | Responses API 顶层 `instructions` |
| AGENTS.md / 环境 | **user role**；WorldState 分节快照；变化发 **diff** |
| skill | turn 开始 `build_skill_injections` 展开进历史 |
| 历史 owner | `ContextManager`；`for_prompt` + `normalize_history` |
| wire | 仅 Responses API；HTTP 全量；WS 严格前缀校验才增量 |
| 压缩 | 约 90% 阈值；摘要为 user message；`replacement_history` 替换 |
| steering | **一等 mid-turn 能力**；`TurnInputQueue` 每轮排干 |

**核心选择**：`ContextManager` 是唯一 live effective history；每次 sampling 全量（或等价增量）投影；normalize 保证 call/output 配对与稳定 ID（护缓存）。

### 3.3 Claude Code（官方 + 逆向）

| 环节 | 行为 |
| --- | --- |
| 架构 | 无状态 Messages API；**每次完整重发**；靠 prompt cache 前缀命中 |
| 分层 | system（静态分段 + cache_control）→ 首条 user（CLAUDE.md system-reminder）→ conversation |
| CLAUDE.md | **不进 system prompt**（缓存经济学：system 跨用户共享） |
| skills | 三级渐进披露：listing → 调用时正文 user message → 支持文件按需 Read |
| 压缩 | Snip → Microcompact → Auto-Compact；**摘要请求复用会话前缀** |
| compact 后 | 重挂已用 skill 正文预算；重读最近文件 |
| steering | 工具完成后、下次 API 前注入 |
| 缓存 | 尾部断点前滑；hit rate 按 uptime 级监控 |

**核心选择**：append-only、从不回头编辑前缀；一切动态信息追加；system 静态字节稳定换全局缓存。

---

## 4. 异同矩阵

| 维度 | pi | Codex | Claude Code | MiniCore 目标 |
| --- | --- | --- | --- | --- |
| 项目 md 落点 | system | user（WorldState diff） | user（system-reminder） | system（Profile 层） |
| skill 正文展开 | 提交时客户端 | turn 开始 core | 调用时渐进披露 | `resolve_intent` 提交时 |
| 历史 owner | live messages + 树 | ContextManager + rollout | 消息链 + JSONL 树 | SessionStorage → ConversationSeed |
| session→provider seam | `convertToLlm` | `for_prompt`/normalize | （内部单点） | `project_model_call` |
| 压缩摘要形态 | user 消息 | user 消息 | user 消息 | user 消息（Compaction batch） |
| steering 注入点 | 工具轮后 | mid-turn 队列 | 工具轮后 | `before_next_model_call` |
| steering 落盘 | 注入后 | 记历史时 | 注入请求时 | **commit 成功才 rollover（最严）** |
| 全量 vs 增量 | 全量 | HTTP 全量 / WS 条件增量 | 全量 + 缓存断点 | 全量；adapter 可选 delta |
| 表示层约数 | 8 | 7 | 未公开 | 审计 10–11 → 目标 ~7 |

---

## 5. 共同收敛模式（验证 MiniCore 方向）

以下三家一致，可放心写入未来 ADR：

1. **一条 live effective transcript**，每次调用全量（或等价）投影——**没有**贯穿整个 run 的 `durable_history/current_input` 双 lane。
2. **skill/模板正文 = 展开定型的普通 user message**，事后改文件不回溯历史。
3. **压缩摘要都是 user role 消息**，不是改写 system prompt。
4. **steering 在工具轮边界、下一次采样前注入**，无人做流式生成中途插入。
5. **session 域 → provider 格式的转换收敛在一个 seam**。
6. **append-only + 前缀稳定**是 prompt cache 的根基。

---

## 6. 可借鉴点与取舍

### 6.1 建议 MVP 或近期采纳

| # | 来源 | 建议 | 影响文档 |
| --- | --- | --- | --- |
| 1 | Claude Code | **压缩摘要请求复用会话前缀**：与会话相同 system/tools/history，只追加摘要指令；避免最大上下文时刻 0 缓存命中 | `compaction.md`、`CompactionSummaryMaterial` |
| 2 | Claude Code / 全员 | **system prompt 确定性纪律**：time 不进 Profile 或仅日期粒度；tools/skills 稳定排序；fingerprint 未变则 **Arc 复用** 上一 turn 的 `PromptCallProfile` | `prompt.md`、environment projection |
| 3 | Codex | **合成 tool output / 稳定 ID** 明确服务 cache；continuation 仅当「新请求 = 旧 + 严格前缀延伸」 | `ModelGateway` provider continuation |

### 6.2 后期路线图

| # | 来源 | 建议 |
| --- | --- | --- |
| 4 | Claude Code | 分级整形：full compact 前先 snip 旧 tool_result、大输出落盘引用 |
| 5 | Claude Code | compact 后恢复预算：重挂 skill 正文、重读最近访问文件 |
| 6 | Claude Code / pi | skill 第三级：支持文件不进 snapshot，模型按需 Read |
| 7 | Codex | WorldState 式 mid-thread 资源 diff（仅当有真实热更新需求） |

### 6.3 维持现有 MiniCore 取舍

| 议题 | 结论 |
| --- | --- |
| 项目 md 进 system 还是 user | 本地单用户，无跨用户共享 system 缓存需求；进 **Profile 层** 与资源生命周期一致；须保证未变时逐字节复用 |
| steering 落盘 | commit-before-rollover 比 pi/CC 更严，**保持** |
| 单生产路径 | 避免 pi 的 Agent vs Harness 双实现；SDK 只能是同一实现的薄包装 |
| 表示层数量 | 目标 ~7 层即可；关键是 **唯一 owner**，不是绝对层数 |

### 6.4 阻塞定稿项（未变）

1. **Rig 0.40.0 spike**：seed 完整历史、`{history,prompt}` 是否只藏 adapter、Steer rollover。  
2. **BR-055**：`ResolvedPromptInput` canonical lowering / cardinality。  
3. **NextTurn** 多 intent：Composite vs 顺序多条 user message。  
4. spike 通过后：**ADR 0023** + 同步 prompt/driver/session-runtime 等权威文档。

---

## 7. 推荐结论

跨项目对照 **全面验证 Transcript-First + ConversationSeed**：

- 单一 live transcript  
- 单一 model 转换 seam（`ModelMessage`）  
- 压缩摘要为 user 消息  
- steering 在工具轮边界、commit 后注入  

真正的新增量（相对进度文件 BR-055 已有推荐）：

1. 压缩请求复用会话前缀（改 material 设计，建议 MVP）  
2. 分级整形与 compact 后恢复预算（后期）  
3. Profile 确定性与跨 turn Arc 复用（MVP 实现约束）  
4. Codex 式增量前缀严格判据（ModelGateway）  

**下一步**：Rig history-seed / rollover spike → 若可行，ADR 0023 → 同步权威模块文档与 BR-055 关闭条件。

---

## 附录 A：pi「用户输入 → provider 请求」消息组装全链路调研报告

调研时间：2026-07-14  
方法：本地仓库 `D:/OneDrive - FORVIA/code/git/pi` 源码级阅读  
证据：代码确认（除非另行标注）

### A.0 架构总览：两套 API，一条 loop

`packages/agent` 同时导出两套上层封装，共享底层循环 `agent-loop.ts`：

- **`Agent`**（`packages/agent/src/agent.ts`）：有状态低层封装，持有 transcript（`state.messages`），提供 `prompt/continue/steer/followUp`。
- **`AgentHarness`**（`packages/agent/src/harness/agent-harness.ts`）：高层封装，transcript 归属 `Session`，自带 compaction/skills/prompt-template/session 持久化。

**关键事实：实际的 pi coding-agent（CLI/TUI）走的是 `Agent` 路径，不是 `AgentHarness`。**  
证据：`packages/coding-agent/src/core/sdk.ts` 中 `new Agent({...})`；`AgentSession`（`core/agent-session.ts`）包装 `Agent` + 自有 `SessionManager`、`core/compaction/*`、`core/messages.ts`、`core/system-prompt.ts`。`AgentHarness` 及 `harness/*` 是 SDK/库使用者的平行实现（逻辑几乎同构，coding-agent 不用它）。

以下以 **coding-agent 实际路径** 为主，并标注 harness 平行实现。

### A.1 System prompt 组装

**构成**（`coding-agent/src/core/system-prompt.ts` 的 `buildSystemPrompt`）：

1. **base prompt**：硬编码 `"You are an expert coding assistant operating inside pi..."`，含 Available tools（one-line snippet）、Guidelines、pi 文档路径。`customPrompt` 可整体替换 base。
2. **appendSystemPrompt**：`--append-system-prompt` 追加。
3. **项目 md（AGENTS.md/CLAUDE.md）**：包进 `<project_context><project_instructions path="...">…</project_instructions></project_context>`。
4. **skills 摘要**：`formatSkillsForPrompt`（`core/skills.ts`），仅当有 `read` 工具时加入。`<available_skills>` 含 name/description/location，**正文不进 system prompt**。
5. **结尾**：Current date + Current working directory。

**AGENTS.md 进 system prompt（不是 user message）。**  
加载：`core/resource-loader.ts` `loadProjectContextFiles`；候选 `AGENTS.md` / `CLAUDE.md`；agentDir + cwd 向上收集去重。

**拼装位置**：`core/agent-session.ts` `_rebuildSystemPrompt` → `buildSystemPrompt`。

**缓存粒度：session 级，非每次调用重建。** 存 `this._baseSystemPrompt`；仅工具集变化 / 资源 reload 时重建。每次模型调用在 `prepareNextTurnWithContext` 塞回 system prompt。

> harness：`createTurnState()` 每 turn 可调用 `systemPrompt` 回调。

### A.2 Slash / prompt template / skill 正文展开

**时机：提交时**（`AgentSession.prompt()` 内，发给 agent 之前）：

1. `/xxx` 先试 extension command，命中则不进 LLM。
2. input extension hook 可 transform。
3. skill：`_expandSkillCommand`，`/skill:name`，`readFileSync` → 去 frontmatter → `<skill name location>…body…</skill>` + 参数。
4. prompt template：`expandPromptTemplate`，`$1/$@/$ARGUMENTS` 等替换。

**展开成：普通 `user` message 文本**（可带 images）。无专门 message type。提交即定型；事后改模板不回溯历史。

### A.3 会话历史 owner 与持久化

- **Live**：`agent.state.messages`（`AgentMessage[]`）；`message_end` 时 push。
- **持久化**：append-only 会话树 JSONL（`{ts}_{sessionId}.jsonl`）；`SessionEntry` 带 `id/parentId`；`leafId` 指向当前分支。
- **Entry 类型**：Message / Custom / Compaction / BranchSummary / ModelChange / ThinkingLevelChange / ActiveToolsChange / Label / SessionInfo 等。
- **恢复**：`buildSessionContext` 从 leaf 沿 parentId 到 root 展平。

### A.4 每次模型调用前转换链

`agent-loop.ts` `streamAssistantResponse`：

1. `transformContext`（可选，extension/hook）
2. `convertToLlm`（`core/messages.ts`）：  
   - bashExecution → user（或排除）  
   - custom → user  
   - branchSummary / compactionSummary → user（带 prefix 包裹）  
   - user/assistant/toolResult 透传  
3. ai 包 `transformMessages`：跨 provider 归一（图片降级、thinking、tool id、孤儿 result）  
4. provider adapter → 厂商 SDK DTO → HTTP JSON

### A.5 压缩

`coding-agent/src/core/compaction/compaction.ts`：

- **触发**：run 结束后 overflow 或 threshold（`contextTokens > window - reserveTokens`，默认 reserve 16384）。
- **cut point**：从最新回推 `keepRecentTokens`（默认 20000）；合法点不切 toolResult；可 split turn 前缀单独摘要；增量更新用 previous-summary。
- **摘要**：结构化模板 Goal/Progress/Next Steps；独立 LLM 调用。
- **写回**：`appendCompaction` + `agent.state.messages = buildSessionContext().messages`。
- **当前输入**：压缩在 agent_end 后、下次 prompt 前；keepRecent 与 cut 规则保护近期与配对。

### A.6 Steering

- streaming 时按 `streamingBehavior`：`steer` / `followUp` 队列。
- **注入边界**：每个 turn（assistant + tools）结束后 drain steering；follow-up 仅在本会停下时 drain。
- **模式**：`all` / `one-at-a-time`（默认 one-at-a-time）。
- **落盘**：注入触发 message 事件后才 append；abort 清空未注入队列。

### A.7 工具轮

执行完批 tool → emit toolResult → push context → 下一轮 `convertToLlm` 透传。parallel 完成可乱序，回填按源顺序。

### A.8 表示层（约 8 层）

1. 原始输入字符串  
2. 展开后文本  
3. `AgentMessage`  
4. `SessionEntry`  
5. `AgentContext`  
6. pi-ai `Message[]` / `Context`  
7. `transformMessages` 输出  
8. 厂商 SDK DTO  

### A.9 总结

**一句话**：厂商无关 `AgentMessage` + append-only 会话树；只在调用边界经 `convertToLlm` 等降为 provider DTO；system prompt session 级缓存。

**复杂度集中**：会话树 cut-point/重放；loop 队列边界；跨 provider 归一。

**警示**：coding-agent 与 harness 双实现勿照搬。

---

## 附录 B：OpenAI Codex CLI 消息组装全链路调研报告

调研时间：2026-07-14  
方法：GitHub `openai/codex` main 分支网页/raw + 浅克隆 grep  
证据：代码确认（除标注推断/历史文档）

### B.0 与旧认知差异最大的三点

1. **Chat Completions 已删除**：`WireApi` 仅 `Responses`；`wire_api = "chat"` 报错。
2. **custom prompts（`~/.codex/prompts`）已从代码移除**；文档标 deprecated，由 **skills** 取代。
3. **AGENTS.md / 环境上下文重构为 WorldState 分节-快照-diff**，非简单“首条 user 注一次”。

### B.1 请求组装：Prompt 结构

`codex-rs/core/src/client_common.rs` **`Prompt`**：

- `input: Vec<ResponseItem>`
- `tools`、`parallel_tool_calls`
- `base_instructions: BaseInstructions`
- `output_schema` 等
- `get_formatted_input_for_request(...)`

位置：

- **base instructions** → Responses 请求顶层 **`instructions`**（`Session::get_base_instructions`；默认模板 `prompts/base_instructions/default.md`）。
- **user_instructions（AGENTS.md）** → **user role** `ResponseItem::Message`（markers `# AGENTS.md instructions` / `</INSTRUCTIONS>`）。
- **environment_context** → user role fragment（`<environment_context>` XML：cwd/shell/date/timezone/network/filesystem 等）。
- **developer role**：permissions、collaboration、personality、available skills、token-budget 等经 `build_developer_update_item`。

**重建**：`Prompt.input` 每次从 `ContextManager` **全量重建**（`clone_history().for_prompt(...)`）。初始上下文仅首个真实 turn 全量，之后 **diff**。缓存靠 `prompt_cache_key`（thread id）与稳定 item ID。

### B.2 AGENTS.md

- `agents_md.rs`：`AGENTS.override.md` 优先于 `AGENTS.md` + fallback 文件名；project root→cwd 正序；字节预算截断。
- 注入：`AgentsMdState` WorldState 分节；首 turn `render_full`；之后 diff + “替换此前指令”文案。
- 持久化：渲染出的 Message 进历史；快照 `RolloutItem::WorldState` 供 resume。

### B.3 custom prompts / slash

- main 已无 `~/.codex/prompts` 实现。
- **skills**：`UserInput::Skill`；turn 开始 `build_skill_injections` 展开记入历史。
- 内置 slash（/model、/compact 等）客户端本地，不进模型消息。

### B.4 历史 owner、rollout、resume

**`ContextManager`**（`context_manager/history.rs`）：`items`、`history_version`、`token_info`、`reference_context_item`、`world_state_baseline`。

- `record_items`、`replace`、`drop_last_n_user_turns`、`update_world_state`、`for_prompt`。
- **normalize_history**：补合成 aborted output（UUIDv5 稳定 ID）、删孤儿 output、图片 modality 降级。

**rollout**：`~/.codex/sessions/rollout-*.jsonl`；`RolloutItem`：SessionMeta / ResponseItem / Compacted / TurnContext / WorldState / EventMsg 等。

**resume/fork**：倒序找 compaction checkpoint 与 TurnContext；正序回放；`Compacted.replacement_history` 替换；rollback 事件支持 fork。

### B.5 转换链（Responses only）

`ModelClient` → `stream()` 仅 Responses；优先 WebSocket，426 回退 HTTP。

`ResponsesApiRequest`：model、instructions、input、tools、reasoning、prompt_cache_key 等。

- HTTP：**全量 input**（无 previous_response_id）。
- WS：严格前缀校验通过才 `previous_response_id + incremental_items`，否则全量。

### B.6 auto-compact

- 阈值约 `min(config, context_window * 9/10)`（90%）。
- 摘要 prompt 模板；可被 `config.compact_prompt` 覆盖；合成 user 消息触发摘要。
- 写回：SUMMARY_PREFIX + 文本为 user Message；`build_compacted_history` 保留用户消息预算（约 20K tokens，最新优先）；`replace_compacted_history` + `CompactedItem`。
- 当前输入：最新用户消息优先预算；超限截断不丢弃。

### B.7 mid-turn steering

`user_input_or_turn_inner`：先 `steer_input`；无 active turn 才新 turn。  
有活跃 Regular task 时推进 `pending_input`；Review/Compact 不可 steer。  
turn 循环每轮 drain；`has_pending_input` 强制 follow-up 再采样。

### B.8 工具轮

流中 FunctionCall 等先记 history；`FuturesOrdered` 保序 await；output 再 record；`for_prompt` 全量重发 + normalize 配对。

### B.9 表示层（约 7 层）

1. `UserInput`  
2. `Op::UserInput`  
3. `TurnInput`  
4. `ResponseItem` / `ResponseInputItem`（含 ContextualUserFragment）  
5. `ContextManager`  
6. `Prompt`  
7. `ResponsesApiRequest` / `ResponseCreateWsRequest`  

### B.10 总结

Codex：**ContextManager 为 live 历史唯一 owner**；AGENTS/环境 user 消息 + WorldState diff；Responses 全量/条件增量；steering 为一等 mid-turn 能力。

---

## 附录 C：Claude Code 消息组装机制调研报告

调研时间：2026-07  
证据等级：**【官方】** / **【逆向】** / **【推断】**

### C.0 总体模型

**【官方】** 每轮全新无状态 Messages API 请求；重发 system + project context + 全部 prior + 新消息。  
架构不变式：**请求体 append-only，靠 prompt caching 前缀匹配摊薄成本**。

| 层 | 内容 | 何时变化 |
| --- | --- | --- |
| System prompt | 核心指令、工具定义、output style | 工具集变化、升级 |
| Project context | CLAUDE.md、auto memory、rules | 启动 /clear /compact |
| Conversation | user/assistant/tool results | 每轮 |

### C.1 System prompt 构成与分段

**【官方】** system 含核心指令、**工具定义**、output style、环境信息、末尾 git 快照。CLAUDE.md **不在** system 内。

**【逆向】** 条件化拼装多段；`__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__` 分隔全局可缓存与会话段；wire 上 system 为多 text block 数组，静态块带 `cache_control: ephemeral`。工具定义体量可达 14K–20K+ tokens。

### C.2 CLAUDE.md 注入

**【官方原文】** “CLAUDE.md content is delivered as **a user message after the system prompt**, not as part of the system prompt itself.”

- 会话启动读盘注入首条 user 上下文；中途编辑不生效也不破缓存直至 /clear、/compact 或重启。
- 嵌套/path rules 惰性加载。
- 层级 managed → user → project → local，拼接非覆盖。

**【逆向】** 包在 `<system-reminder>` 中；动机：system 跨用户相同才能共享全局缓存前缀。

### C.3 Skills（Agent Skills）

**【官方】** 三级渐进披露：

1. **元数据**：启动进 skill listing；description+when_to_use 截断约 1536 字符。  
2. **正文**：调用时作为 **单条消息进入会话**；追加式、不动前缀。用户 `/skill` 占位符展开；重复相同渲染可只追加短注。  
3. **支持文件**：按需 Read/Bash。

compact 后 skill listing 不重载；已调用 skill 按预算重挂（约 5K/skill、25K 总）。

### C.4 会话历史与 JSONL

**【官方】** `~/.claude/projects/<project>/<session-id>.jsonl`；格式内部、会变；默认约 30 天。resume 同 ID 追加；fork 复制新 ID。

**【逆向】** type/uuid/parentUuid 树；tool_result 在后续 user 行；transcript 可先于 API 落盘；compact 磁盘非破坏性（boundary + 读取打补丁）。

### C.5 Compaction / Microcompact

**【官方】** 先清旧 tool 输出，再 summarize。摘要请求 **复用与会话相同的 system/tools/history**，摘要指令作最后一条 user message 追加。compact 后 system/CLAUDE.md 等从盘重载；skill listing 不重载，已调 skill 按预算重挂。

**【逆向】** 阈值约有效窗口减 buffer（200K 模型约 167K 触发量级）；整形链 Budget → Snip → Microcompact → … → Auto-Compact；Microcompact 清旧 tool_result 保最近若干条；摘要以 user message 形态进后续历史。

### C.6 Steering

**【官方】** Esc 打断；直接输入在当前动作完成后、下次决策前读取。  
**【推断/社区】** 与 tool_result 同边界注入 user 内容；不同 surface/版本粒度有争议；stream-json 无非中断 steer 原语。

### C.7 Tool round

**【官方 API】** assistant tool_use；下一条 user 含全部 tool_result（并行合并一条）。  
**【逆向】** 只读工具可并发；file-changed 等 system-reminder 附着；按 round 分组。

### C.8 Prompt caching

**【官方】** 静态 system/tools 全局缓存 → CLAUDE.md 项目内 → session → 对话；动态走 system-reminder 追加。失效：换模型/effort/fast mode、MCP 工具加载进前缀、整工具 deny、compact、升级等。不失效：改仓库文件、会话中改 CLAUDE.md（不生效）、调 skill、rewind 等到已缓存前缀等。

**【逆向】** cache_control 在 system 静态块与消息尾部；尾部断点前滑；实测高前缀复用率与预热调用。

### C.9 完整重发 vs 增量

**【官方】** 每次完整 messages 重发；“增量”= 服务端缓存前缀匹配 + 客户端 compaction/microcompact 缩短历史。

### C.10 来源（摘要）

官方：code.claude.com docs（prompt-caching、memory、skills、context-window、how-claude-code-works、sessions）；claude.com 工程博客 *Prompt caching is everything*。

逆向：minusx.ai、dbreunig system prompt、claudecodecamp、karanprasad、sathwick、LMCache、liranyoffe、JSONL 解析工具、GitHub issues（steering）等。

### C.11 不确定点

steering 精确 wire 形态；compact 摘要 JSONL 字段名；反编译阈值数字；JSONL 格式随版本变化。

---

## 变更记录

| 日期 | 说明 |
| --- | --- |
| 2026-07-14 | 初版：综合对照 + 附录 A/B/C 全文归档；progress 引用 |
