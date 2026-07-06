# Skills

`skills.rs` 是和 `resource_manager.rs`、`session_runtime.rs` 平级的技能文件能力模块。它不拥有资源生命周期，也不拥有会话运行生命周期；它提供技能元数据、技能目录数据结构，以及发现、解析、校验和格式化辅助函数。

技能不是工具，也不是插件代码。技能是 Markdown 指令包：它可以告诉模型“遇到某类任务时应采用什么流程”，但真正的本地副作用仍然只能通过 `SessionRuntime` 持有的工具策略、审批、工作区沙箱和 `ToolGateway` 发生。

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
  owns: 技能来源 roots、trust gate、runtime/cwd 分层、reload/recompose、overlay、current snapshot、diagnostics

skills.rs
  provides: SkillMetadata / SkillResource / SkillCatalog 数据结构，以及给定目录后的发现、metadata 解析、校验、去重、prompt 格式化 helper

SessionRuntime
  owns: /skill:name 或 InvokeSkill 的展开、从 captured TurnResourceSnapshot 取正文、<skill> 块构造、user message 构造、入队或启动 run
```

`skills.rs` 可以被 `ResourceManager` 和 `SessionRuntime` 同时调用，但它本身不是 runtime service。模型可见技能的生命周期和 cwd-over-runtime overlay 由 `ResourceManager` 负责。

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
| reload / ensure / recompose 生命周期 | 否 | 是 |
| 发布 current `RuntimeResourceSnapshot` / `CwdResourceSnapshot` | 否 | 是 |
| `resources_changed` 所需 skill summary / diagnostics | 提供 summary 数据/格式 helper | 提供 resolved selected resources，由 `AgentRuntime` 发布事件 |
| `/skill:name` raw input 解析 | 否 | 否，属于 `CommandSurface` |
| `InvokeSkill` 构造 user message | 否，提供格式化 helper | 否，属于 `SessionRuntime` |

一句话：`skills.rs` 负责“skill 长什么样、如何解析、如何格式化”；`ResourceManager` 负责“skill 从哪里来、何时加载、如何分层覆盖、哪一版对当前 turn 可见”。

`SkillCatalog` 作为数据结构可以定义在 `skills.rs`，但 current catalog 的生命周期 owner 是 snapshot：

```text
RuntimeResourceSnapshot.skills
CwdResourceSnapshot.local.skills
CwdResourceSnapshot.resolved.skills
TurnResourceSnapshot.cwd.resolved.skills
```

因此 `skills.rs` 可以创建 catalog，`ResourceManager` 决定 catalog 何时创建、如何 overlay、何时发布。

## 来自 pi coding-agent 的源码经验

pi coding-agent 的生产路径是：

```text
DefaultResourceLoader
  └─ 调用 skills.ts 加载技能 metadata，并持有当前 skills 状态

AgentSession._expandSkillCommand()
  └─ 解析 /skill:name
  └─ 从 resourceLoader.getSkills() 查 metadata
  └─ readFileSync(skill.filePath)
  └─ stripFrontmatter(content)
  └─ 构造 <skill>...</skill>
  └─ 作为普通 user message 进入 prompt / steer / followUp
```

`skills.ts` 不是完整的技能管理器。它主要提供：

- `loadSkills()` / `loadSkillsFromDir()`：给定路径后的发现、读取 metadata、校验、去重和诊断。
- `formatSkillsForPrompt()`：生成模型可见的 `<available_skills>` 摘要。
- 技能 metadata 类型和相关解析能力。

`AgentSession` 才负责显式技能调用的读取正文和 message 构造。这一点本项目应保留。

## 技能发现和 metadata 加载

`ResourceManager.reload_cwd()` / `ensure_cwd_snapshot()` 负责决定来源，`skills.rs` 负责把给定路径转换成 skill candidate / catalog 数据。

pi coding-agent 的发现规则值得保留：

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

pi coding-agent 的 catalog 默认只保存 metadata，并在显式调用时按路径读取正文。MiniCore 的 `ResourceManager` snapshot 需要更强的原子性：进入 `CwdResourceSnapshot.resolved` 的 selected skill 应保存 stable body content，或保存 content hash + immutable loaded content reference。这样 running turn 不会在 reload 或文件修改后读到与 captured catalog 不一致的正文。

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

`SkillDocument` 可以作为显式调用或 `GetSkill` 的临时值，但不应放进 `agent_runtime_protocol::RuntimeSnapshot`：

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

显式调用时，`SessionRuntime` 应从 captured `TurnResourceSnapshot.cwd.resolved.skills` 取得 `SkillResource.body`，再调用 `format_skill_block()`。这样 message 构造保持在会话编排层，`skills.rs` 只提供纯辅助能力，同时不绕过 snapshot 原子性。

## 模型可见技能摘要

pi 的 `formatSkillsForPrompt()` 会生成：

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

本项目应保留这个模式，但要遵守两个约束：

- 只列出 `disable_model_invocation == false` 的技能。
- 只有 active tools 中包含 `read` 时，才把可见技能列表放进 system prompt。

原因是 pi 的模型可见技能摘要要求模型“用 read 工具加载技能文件”。如果当前会话没有 `read`，把技能位置暴露给模型会形成不可执行的承诺。用户显式 `InvokeSkill` 不受这个限制，因为 `SessionRuntime` 会直接展开正文。

## 显式技能调用

显式调用属于 `SessionRuntime`：

```text
InvokeSkill 或 /skill:name
  → SessionRuntime capture TurnResourceSnapshot
  → 从 turn.cwd.resolved.skills 查找 selected SkillResource
  → skills::format_skill_block(metadata, body)
  → 追加 additional instructions
  → MessageRecord::user(...)
  → 按 delivery 立即运行或入队
```

调用格式对齐 pi：

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
3. `SessionRuntime` 解析 `/skill:name` 或处理结构化 `InvokeSkill`。
4. `SessionRuntime` 从 `turn.cwd.resolved.skills` 读取 selected skill body，调用 `skills.rs` helper 格式化 `<skill>` 块。
5. 格式化后的内容作为新的 user message 进入本次 `DriveEntry::Prompt` 或进入队列。
6. `SessionRuntime` 基于 active tools、context files 和可见技能摘要重建 system prompt。
7. `BeforeAgentStart` Hook 可以追加 custom messages 或替换 system prompt。
8. `Driver` 使用 `TurnState` 创建 Rig run。
9. user message、assistant message 和 tool result 都通过 `SessionRuntime` 写入 session。

这意味着：

- 过去的对话历史来自 session storage。
- 技能摘要来自当前 `SkillCatalog` 和 system prompt。
- 被显式调用的技能全文只进入当前 user message。
- 同一技能后续 reload 不会改变已经持久化的历史消息。

## 生命周期

```text
App / workspace open
  → ResourceManager.ensure_runtime_snapshot()
  → ResourceManager.ensure_cwd_snapshot(CwdResourceRequest)
  → ResourceManager resolves skill sources
  → skills::load_skill_catalog(inputs)
  → ResourceManager overlays runtime/global and cwd/project skill candidates
  → ResourceManager publishes CwdResourceSnapshot { resolved.skills, diagnostics, ... }
  → AgentRuntime publishes `resources_changed`
  → SessionRuntime rebuilds system prompt on next turn

InvokeSkill
  → SessionRuntime captures TurnResourceSnapshot
  → SessionRuntime reads SkillResource from turn.cwd.resolved.skills
  → skills::format_skill_block()
  → SessionRuntime creates user message
  → Driver drive_run
  → SessionRuntime persists messages through SessionHandle

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
- 不要把模型可见技能列表和显式技能调用混为一谈。前者只是摘要，后者注入全文。
- 不要在没有 `read` 工具时把技能列表暴露给模型。模型无法按摘要中的指令加载技能文件。
- 不要让资源 reload 改写历史。已经持久化的 skill invocation 是一次历史 user message。
