# Skills

`skills.rs` 是和 `resource_loader.rs`、`session_runtime.rs` 平级的技能文件能力模块。它不拥有资源生命周期，也不拥有会话运行生命周期；它提供技能元数据、技能目录数据结构，以及发现、解析、校验和格式化辅助函数。

技能不是工具，也不是插件代码。技能是 Markdown 指令包：它可以告诉模型“遇到某类任务时应采用什么流程”，但真正的本地副作用仍然只能通过 `SessionRuntime` 持有的工具策略、审批、工作区沙箱和 `ToolGateway` 发生。

## 设计定位

建议代码布局：

```text
src/
  agent_runtime.rs
  session_runtime.rs
  resource_loader.rs
  skills.rs
  tools.rs
  driver.rs
```

职责边界：

```text
ResourceLoader
  owns: 技能来源聚合、reload、extend resources、当前 SkillCatalog、diagnostics、`resources_changed`

skills.rs
  provides: SkillMetadata / SkillCatalog，以及技能发现、metadata 解析、校验、去重、prompt 格式化 helper

SessionRuntime
  owns: /skill:name 或 InvokeSkill 的展开、技能正文读取、<skill> 块构造、user message 构造、入队或启动 run
```

`skills.rs` 可以被 `ResourceLoader` 和 `SessionRuntime` 同时调用，但它本身不是 runtime service。

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

`ResourceLoader.reload()` 负责决定来源，`skills.rs` 负责把给定路径转换成 `SkillCatalog`。

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

生产路径应跟随 pi coding-agent：catalog 默认只保存 metadata，不保存所有技能正文。

```rust
pub struct SkillMetadata {
    pub name: SkillName,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub source: ResourceSourceInfo,
    pub disable_model_invocation: bool,
}

pub struct SkillSummary {
    pub name: SkillName,
    pub description: String,
    pub file_path: PathBuf,
    pub source: ResourceSourceInfo,
    pub disable_model_invocation: bool,
}

pub struct SkillCatalog {
    pub skills: Vec<SkillMetadata>,
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

显式调用时，`SessionRuntime` 可以直接读取 `metadata.file_path`，再调用 `strip_skill_frontmatter()` 和 `format_skill_block()`。这样 message 构造保持在会话编排层，`skills.rs` 只提供纯辅助能力。

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
  → SessionRuntime 从 ResourceState 取得当前 SkillCatalog
  → 查找 SkillMetadata
  → SessionRuntime 读取 metadata.file_path
  → skills::strip_skill_frontmatter(markdown)
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
2. `SessionRuntime` 从 `ResourceState` 读取当前 `SkillCatalog`。
3. `SessionRuntime` 解析 `/skill:name` 或处理结构化 `InvokeSkill`。
4. `SessionRuntime` 读取技能正文，调用 `skills.rs` helper 剥离 frontmatter 和格式化 `<skill>` 块。
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
  → ResourceLoader.reload()
  → ResourceLoader resolves skill sources
  → skills::load_skill_catalog(inputs)
  → ResourceLoader stores SkillCatalog + diagnostics
  → AgentRuntime publishes `resources_changed` and updates snapshot state
  → SessionRuntime rebuilds system prompt on next turn

InvokeSkill
  → SessionRuntime reads current SkillCatalog
  → SessionRuntime reads skill file
  → skills::strip_skill_frontmatter()
  → skills::format_skill_block()
  → SessionRuntime creates user message
  → Driver drive_run
  → SessionRuntime persists messages through SessionHandle

ReloadResources
  → ResourceLoader replaces SkillCatalog
  → AgentRuntime publishes diagnostics and `resources_changed`
  → future turns use new catalog
  → existing persisted messages remain unchanged
```

## 管理能力

`SkillCatalog` 是数据结构，不是生命周期 owner。它至少需要支持：

- `list()`：返回 `SkillSummary`。
- `get(name)`：返回 metadata 或结构化 not found。
- `visible(active_tools)`：返回可进入 system prompt 的技能摘要。
- `diagnostics()`：返回加载、校验、碰撞和路径错误。

名称碰撞处理应确定且可诊断。不要依赖文件系统遍历顺序。产品必须定义来源优先级；MVP 建议：

1. 用户显式配置路径。
2. 用户级技能。
3. 应用内置技能。
4. 已信任工作区技能。
5. 后续扩展或包资源按声明顺序追加。

这个默认顺序让项目内技能不能静默覆盖用户级或内置技能。若未来允许 workspace skill 覆盖 builtin skill，必须在项目可信后才允许，并产生可见诊断。无论谁获胜，都不能绕过工具策略。

## 设计约束

- 不要把技能当工具。技能只是 prompt resource；工具才有副作用。
- 不要把技能全文塞进 snapshot。否则大文件、私有路径和指令内容会泄漏给 UI 状态层。
- 不要让 UI 拼 `<skill>` 块。否则相对路径规则、frontmatter 剥离、队列语义和 session persistence 会分叉。
- 不要把模型可见技能列表和显式技能调用混为一谈。前者只是摘要，后者注入全文。
- 不要在没有 `read` 工具时把技能列表暴露给模型。模型无法按摘要中的指令加载技能文件。
- 不要让资源 reload 改写历史。已经持久化的 skill invocation 是一次历史 user message。
