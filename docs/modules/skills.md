# Skills

`skills.rs` 是和 `resource_manager.rs`、`session_runtime.rs` 平级的技能文件能力模块。它不拥有资源生命周期，也不拥有会话运行生命周期；它提供技能元数据、技能目录数据结构，以及发现、解析、校验和格式化辅助函数。

技能不是工具，也不是插件代码。技能是 Markdown 指令包：它可以告诉模型“遇到某类任务时应采用什么流程”，但真正的本地副作用仍然只能通过 `SessionRuntime` 持有的 session-scoped `Tools` 子系统发生，并受工具策略、审批、工作区沙箱和 executor 约束。

## 设计定位

建议代码布局：

```text
src/
  agent_runtime.rs
  session_runtime.rs
  resource_manager.rs
  skills.rs
  tools.rs
  driver.rs
```

职责边界：

```text
ResourceManager
  owns: 技能来源 roots、trust gate、runtime/cwd 分层、cwd reload、overlay、current snapshot、diagnostics

skills.rs
  provides: SkillMetadata / SkillResource / SkillCatalog 数据结构，以及给定目录后的发现、metadata 解析、校验、去重、prompt 格式化 helper

PromptTurn
  owns: 结构化 SkillPromptIntent 展开、从 captured PromptResourceView 取正文、<skill> 块构造和 ResolvedPromptInput 生成

SessionRuntime
  owns: /skill intent admission、PromptDelivery、目标 turn capture、入队或启动 run
```

`skills.rs` 可以被 `ResourceManager` 和 `Prompt` 同时调用，但它本身不是 runtime service。模型可见技能的生命周期和 cwd-over-runtime overlay 由 `ResourceManager` 负责。

## 与 ResourceManager 的边界

`skills.rs` 和 `ResourceManager` 会共享 skill 数据结构，但不能共同拥有同一段生命周期。推荐边界是：

| 能力 | `skills.rs` | `ResourceManager` |
| --- | --- | --- |
| 定义 `SkillMetadata` / `SkillResource` / `SkillCatalog` | 是 | 使用这些类型 |
| 在给定目录中发现 `SKILL.md` | 是，作为纯 helper | 决定给哪些目录调用 helper |
| 解析 frontmatter、校验 name/description | 是 | 不做格式细节 |
| 剥离 frontmatter、格式化 `<available_skills>` / `<skill>` | 是 | 不拼消息、不拼最终 prompt |
| 决定 builtin / user-global / cwd/project skill roots | 否 | 是 |
| project trust gate | 否 | 是 |
| runtime/global 与 cwd/project 分层 | 否 | 是 |
| cwd 覆盖 runtime 的 overlay | 否 | 是 |
| runtime-once / cwd ensure / cwd reload 生命周期 | 否 | 是 |
| 发布 current `RuntimeResourceSnapshot` / `CwdResourceSnapshot` | 否 | 是 |
| `resources_changed` 所需 skill summary / diagnostics | 提供 summary 数据/格式 helper | 提供 resolved selected resources，由 `AgentRuntime` 发布事件 |
| `/skill <name>` / `/skill:name` command text 解析 | 否 | 否，属于 `CommandManager.resolve_for_execution` |
| `InvokeSkill` 构造 user message | 否，提供格式化 helper | 否，属于 `PromptTurn.resolve_intent()` |

一句话：`skills.rs` 负责“skill 长什么样、如何解析、如何格式化”；`ResourceManager` 负责“skill 从哪里来、何时加载、如何分层覆盖、哪一版对当前 turn 可见”。

`SkillCatalog` 作为数据结构可以定义在 `skills.rs`，但 current catalog 的生命周期 owner 是 snapshot：

```text
RuntimeResourceSnapshot.skills
CwdResourceSnapshot.local.skills
CwdResourceSnapshot.resolved.skills
TurnResourceSnapshot.cwd.resolved.skills
```

因此 `skills.rs` 可以创建 catalog，`ResourceManager` 决定 catalog 何时创建、如何 overlay、何时发布。

## 设计原则

`skills.rs` 只提供给定输入后的发现、metadata/frontmatter 解析、校验、去重、诊断和格式化 helper。资源 roots、trust、overlay、snapshot 和 reload 归 `ResourceManager`；显式调用的 delivery 归 `SessionRuntime`；skill body 到普通 user message 的组装归 `PromptTurn.resolve_intent()`。

pi coding-agent 的技能加载和显式注入路径可以作为参考对象，但不构成 MiniCore 的类型、文件布局或行为兼容承诺。MiniCore 的权威 interface 和不变量只由本文件及 ResourceManager/Prompt 文档定义。

## 技能发现和 metadata 加载

`ResourceManager.reload_cwd()` / `ensure_cwd_snapshot()` 负责决定来源，`skills.rs` 负责把给定路径转换成 skill candidate / catalog 数据。

MVP 技能发现规则：

- 若目录包含 `SKILL.md`，该目录就是一个技能根，并且不再向下递归。
- 若目录不包含 `SKILL.md`，递归子目录寻找 `SKILL.md`。
- 根目录直接 `.md` 文件也可以作为轻量技能加载。
- 遵守 `.gitignore`、`.ignore`、`.fdignore`。
- 跳过隐藏目录和 `node_modules`。
- 跟随可解析的 symlink，并用 canonical path 去重。
- 缺失目录静默跳过；不可读、解析失败或元数据无效产生诊断。

加载规则：

- frontmatter 中 `description` 必填。
- `name` 可来自 frontmatter；缺省时使用父目录名。
- `name` 限制为小写字母、数字和连字符，长度上限 64。
- `description` 长度上限 1024。
- `disable-model-invocation: true` 表示不让模型主动发现该技能。
- 同名技能按确定顺序 first wins，loser 产生 collision diagnostic。
- 同一真实文件路径通过 symlink 重复出现时静默去重。

## 数据结构

MiniCore 的 `ResourceManager` snapshot 要求 selected skill 保存 stable body content，或保存 content hash + immutable loaded content reference。这样 running turn 不会在 reload 或文件修改后读到与 captured catalog 不一致的正文。

```rust
pub struct SkillMetadata {
    pub name: SkillName,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub source: ResourceSourceInfo,
    pub disable_model_invocation: bool,
}

pub struct SkillResource {
    pub metadata: SkillMetadata,
    pub body: Arc<str>,
    pub content_hash: ContentHash,
}

pub struct SkillSummary {
    pub name: SkillName,
    pub description: String,
    pub file_path: PathBuf,
    pub source: ResourceSourceInfo,
    pub disable_model_invocation: bool,
}

pub struct SkillCatalog {
    pub skills: Vec<SkillResource>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}
```

`SkillDocument` 可以作为显式调用或 `ResourceQuery::GetSkill` 的临时值，但不应放进 `agent_runtime_protocol::RuntimeSnapshot`：

```rust
pub struct SkillDocument {
    pub metadata: SkillMetadata,
    pub body: String,
}
```

## 函数能力

`skills.rs` 建议提供函数，而不是提供一个长期持有状态的 loader service：

```rust
pub fn load_skill_catalog(inputs: SkillLoadInputs) -> Result<SkillCatalog, ResourceError>;
pub fn load_skills_from_dir(dir: &Path, source: ResourceSourceInfo) -> SkillLoadReport;
pub fn parse_skill_metadata(path: &Path, markdown: &str, source: ResourceSourceInfo) -> Result<SkillMetadata, SkillError>;
pub fn strip_skill_frontmatter(markdown: &str) -> String;
pub fn format_available_skills(catalog: &SkillCatalog, active_tools: &[ToolName]) -> String;
pub fn format_skill_block(metadata: &SkillMetadata, body: &str) -> String;
```

显式调用时，目标 `PromptTurn` 应从 captured `PromptResourceView` 取得 `SkillResource.body`，再调用 `format_skill_block()`。这样 message 构造集中在 Prompt 组装 seam，`skills.rs` 只提供纯辅助能力，同时不绕过 snapshot 原子性。

## 模型可见技能摘要

模型可见技能摘要采用以下稳定结构：

```text
The following skills provide specialized instructions for specific tasks.
Use the read tool to load a skill's file when the task matches its description.
When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.

<available_skills>
  <skill>
    <name>...</name>
    <description>...</description>
    <location>...</location>
  </skill>
</available_skills>
```

该结构必须遵守两个约束：

- 只列出 `disable_model_invocation == false` 的技能。
- 只有 active tools 中包含 `read` 时，才把可见技能列表放进 system prompt。

摘要会指示模型通过 `read` 加载技能文件；如果当前会话没有 `read`，把技能位置暴露给模型会形成不可执行的承诺。用户显式 `InvokeSkill` 不受这个限制，因为目标 `PromptTurn` 会直接展开 captured 正文。

## 显式技能调用

显式调用的 delivery 属于 `SessionRuntime`，组装属于 `PromptTurn`：

```text
InvokeSkill、/skill <name> 或兼容 /skill:name
  → CommandManager resolves SkillPromptIntent
  → SessionRuntime admits intent by PromptDelivery
  → target turn captures TurnResourceSnapshot and creates PromptTurn
  → PromptTurn.resolve_intent(...) finds selected SkillResource
  → skills::format_skill_block(metadata, body)
  → ResolvedPromptInput / MessageRecord::user(...)
```

显式 skill block 使用以下稳定格式：

```text
<skill name="skill-name" location="/abs/path/SKILL.md">
References are relative to /abs/path.

...skill body without frontmatter...
</skill>

...additional instructions...
```

这段文本成为一次普通 user message。它不是 system prompt，也不是隐藏上下文。

## 和已有 messages 如何组成一次运行

一次显式技能调用进入 Agent 运行时后，应按这个顺序处理：

1. `SessionRuntime` 从当前 session leaf 重建已有上下文消息。
2. `SessionRuntime` capture `TurnResourceSnapshot`。
3. `CommandManager` 已将 `/skill <name>` 或兼容 `/skill:name` 解析为结构化 `SkillPromptIntent`；`SessionRuntime` 只决定 delivery。
4. delivery 到达目标边界时选择 PromptTurn：active Steer 使用 `CurrentRun.prompt_turn`；idle submission、FollowUp 和 NextTurn 在 future turn capture resources 并创建新 `PromptTurn`。所有 steering/follow-up/next-turn queues 都只保存未展开的结构化 `SkillPromptIntent`。
5. 目标 `PromptTurn.resolve_intent()` 从 captured `PromptResourceView` 读取 selected skill body，调用 `skills.rs` helper 格式化 `<skill>` 块，得到目标边界的 `ResolvedPromptInput`；已展开技能正文绝不放回队列。
6. new-run 路径中，`prompt::assemble_turn(...)` 已基于 captured `PromptResourceView` 与 `ToolPromptView` 构建同版 `PromptCallProfile`；后期 bounded `BeforeAgentStart` / `PromptBuilt` / `RunBeforeStart` 可以在 commit 前变换并重新校验 input/profile。
7. new-run 路径先提交 `UserInput` batch；成功后发布外层 `Event.run_id = None` 的 `skill_invoked`、`message_user_appended`，再分配 `RunId`、建立 `CurrentRun`、发布 `run_started` 并调用 Driver。
8. active Steer 路径在 current run safe point 提交 `UserInput` batch；成功后发布外层 `Event.run_id = Some(current_run_id)` 的 `skill_invoked`、`message_user_appended`，再把同一个 committed message 放入 `NextModelCallPlan.persistent_messages`，不创建新 RunId。
9. 后续完整 tool rounds 和 final assistant 继续由 `SessionRuntime` 通过 session writer 提交。

这意味着：

- 过去的对话历史来自 session storage。
- 技能摘要来自当前 `SkillCatalog` 和 system prompt。
- 被显式调用的技能全文只进入当前 user message。
- 同一技能后续 reload 不会改变已经持久化的历史消息。

## 生命周期

```text
App / workspace open
  → ResourceManager.ensure_runtime_snapshot_once()
  → ResourceManager.ensure_cwd_snapshot(CwdResourceRequest)
  → ResourceManager resolves skill sources
  → skills::load_skill_catalog(inputs)
  → ResourceManager overlays runtime/global and cwd/project skill candidates
  → ResourceManager publishes CwdResourceSnapshot { resolved.skills, diagnostics, ... }
  → AgentRuntime publishes `resources_changed`
  → next new user turn / work chain captures the new cwd snapshot
  → SessionRuntime creates a new `PromptTurn` / `PromptCallProfile`

InvokeSkill (new-run delivery)
  → SessionRuntime captures TurnResourceSnapshot
  → SessionRuntime creates PromptTurn from captured PromptResourceView
  → PromptTurn.resolve_intent reads SkillResource
  → skills::format_skill_block()
  → bounded pre-run hooks + revalidation
  → SessionRuntime commits UserInput through SessionHandle
  → publish skill_invoked with outer Event.run_id = None
  → publish message_user_appended
  → allocate RunId + establish CurrentRun + publish run_started
  → Driver drive_run
  → SessionRuntime commits ToolRound / AssistantFinal through SessionHandle

InvokeSkill (active Steer)
  → resolve with CurrentRun.prompt_turn
  → commit UserInput through SessionHandle
  → publish skill_invoked with outer Event.run_id = Some(current_run_id)
  → publish message_user_appended
  → add committed message to NextModelCallPlan.persistent_messages

ReloadResources
  → ResourceManager builds a new CwdResourceSnapshot for target cwd
  → ResourceManager atomically replaces ResourceSnapshotStore current cwd pointer
  → AgentRuntime publishes diagnostics and `resources_changed`
  → future turns use new catalog
  → existing persisted messages remain unchanged
```

## 管理能力

`SkillCatalog` 是数据结构，不是生命周期 owner。它至少需要支持：

- `list()`：返回 `SkillSummary`。
- `get(name)`：返回 selected `SkillResource` 或结构化 not found。
- `visible(active_tools)`：返回可进入 system prompt 的技能摘要。
- `diagnostics()`：返回加载、校验、碰撞和路径错误。

名称碰撞处理应确定且可诊断。不要依赖文件系统遍历顺序。产品必须定义来源优先级；MVP 统一使用 [ResourceManager](resource-manager.md) 的 `ResourceOverlayPolicy`。runtime/global 候选先进入 `RuntimeResourceSnapshot`，cwd/project 候选在 `CwdResourceSnapshot` 构建时覆盖 same-key runtime/global 候选；被覆盖技能保留在 `shadowed` 中供 diagnostics/UI 展示。无论谁获胜，都不能绕过工具策略。

## 设计约束

- 不要把技能当工具。技能只是 prompt resource；工具才有副作用。
- 不要把技能全文塞进 `RuntimeSnapshot` 或资源摘要事件。`CwdResourceSnapshot` 可以为原子性保存 selected skill body，但 UI 默认只能看到 summary/detail query 的受控结果。
- 不要让 UI 拼 `<skill>` 块。否则相对路径规则、frontmatter 剥离、队列语义和 session persistence 会分叉。
- 不要让 `SessionRuntime`、CommandManager 或 Driver 各自实现一套 skill message 拼装；统一调用 `PromptTurn.resolve_intent()`。
- 不要把模型可见技能列表和显式技能调用混为一谈。前者只是摘要，后者注入全文。
- 不要在没有 `read` 工具时把技能列表暴露给模型。模型无法按摘要中的指令加载技能文件。
- 不要让资源 reload 改写历史。已经持久化的 skill invocation 是一次历史 user message。
