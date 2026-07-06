# ResourceLoader

`ResourceLoader` 是工作区绑定的内部运行时服务，负责把用户级、可信项目级、临时路径和后续扩展来源中的资源整理成一个带 revision 的资源快照。它统一解析资源来源、加载 resource metadata/content、维护当前资源快照、归集诊断，并向 `SessionRuntime` 提供构建 system prompt 所需的素材。

它不执行 Agent turn，不执行工具，不展开技能调用，也不直接构造最终 system prompt。最终 system prompt 由 [Prompt](prompt.md) 根据当前资源、活跃工具、工具提示片段、日期和 cwd 构建。

## 设计定位

pi coding-agent 的生产路径是：

```text
DefaultResourceLoader
  owns: resources lifecycle, source resolution, diagnostics, current loaded resources

AgentSession._rebuildSystemPrompt()
  reads: resourceLoader.getPrompt(), getAppendPrompt(), getSkills(), getAgentsFiles()
  adds: active tools, tool snippets, tool guidelines
  calls: buildSystemPrompt(...)

system-prompt.ts
  builds: final system prompt string
```

本项目应保持同样边界：

```text
AgentRuntime
  └─ owns workspace-bound RuntimeServices
       └─ ResourceLoader
            ├─ owns RuntimeResources lifecycle
            ├─ calls skills.rs / prompt_templates.rs / context loader
            └─ exposes PromptMaterials

SessionRuntime
  ├─ reads ResourceLoader current resources
  ├─ expands InvokeSkill / InvokePromptTemplate into user messages
  └─ calls prompt.rs when tools/resources change
```

`ResourceLoader` 是“资源快照”和“提示词素材”的 single source of truth；`SessionRuntime` 是“什么时候使用这些资源”的编排者；`prompt.rs` 是纯构建器。

## Boundary Decision

`ResourceLoader` 和 [AgentRuntimeProtocol](agent-runtime-protocol.md) 不在同一层。

```text
AgentRuntimeProtocol
  owns: UI command/event/snapshot schema, routing ids, summary/detail query surface
  does not: discover files, parse skills, evaluate project trust, cache resource content

ResourceLoader
  owns: source resolution, trust gate, resource snapshot, prompt materials, diagnostics, atomic reload
  does not: accept UI commands directly, publish UI events, expose protocol structs as its storage model
```

因此公开协议可以暴露 `ReloadResources`、`resources_changed` 资源摘要、snapshot 中的 resource summaries，以及后续受控 detail query；但不应允许 UI 直接提交完整 `RuntimeResources`，也不应让 UI 绕过 `ResourceLoader` 注入 prompt materials。测试、bootstrap 或 SDK 如果需要注入资源，也应该走 privileged/internal seam，并保留 source info、diagnostics 和 project trust 语义。

这个边界把协议当作 control plane，把资源加载器当作 runtime-owned data plane。协议告诉运行时“刷新资源”或“给我安全摘要/详情”；资源加载器决定资源从哪里来、是否可信、如何解析、何时原子替换。

## 为什么需要独立模块

如果没有 `ResourceLoader`，资源职责会被迫散落到多个地方：`AgentRuntime` 会变成文件扫描器，`SessionRuntime` 会在每次 turn 前读取项目文件，`Prompt` 会失去纯函数边界，UI adapter 也容易为了展示资源去读本地文件。独立 `ResourceLoader` 的价值是把下面这些能力收敛到一个运行时服务：

- resource source resolution：用户级、可信项目级、临时路径、后续 package/extension/MCP 来源的合并顺序。
- project trust gate：未信任项目不加载项目级 prompt、skill、context 或 extension 资源。
- atomic reload：reload 失败不能让 session runtime 看到半更新资源。
- source info 与 diagnostics：每个资源都能解释“来自哪里、为什么不可用、是否名称碰撞”。
- summary/detail split：UI 默认只看资源摘要，需要正文时通过受控 query 获取。
- prompt materials：为 `Prompt` 提供稳定输入，但不拼最终 system prompt。

这不是为了复刻 pi 的类名，而是因为本项目有 pi-like 的资源面：skills、prompt templates、context files、custom system prompt、append system prompt，后续还会有 extension/package/MCP resource discovery。

## Codex 对照

Codex 的启发是“协议和核心运行时分开”，不是“所有能力都塞进协议”。`codex_protocol` 负责 `Submission` / `Op` / `Event` / `EventMsg` 这类通信形态；project instructions、工具环境、会话/线程状态和执行策略属于 core/runtime 侧。Codex 的产品资源面比本项目窄，所以它未必需要一个显式叫 `ResourceLoader` 的模块；但对应职责仍不应属于协议层或 UI 层。

本项目保留显式 `ResourceLoader`，是因为资源来源、信任、诊断、prompt materials 和 UI 摘要都已经成为产品能力。它吸收 Codex 的分层经验：协议只表达请求和事实，运行时服务持有真实状态。

## Boundary Scenarios

这些场景用来检验边界是否正确：

- 运行中触发 `ReloadResources`：`AgentRuntime` 发布 `resources_reload_started`，调用 `ResourceLoader.reload()`；当前 run 的 `TurnState` 不被中途改写，新 revision 只影响后续 turn。
- reload 部分失败：`ResourceLoader` 保留旧资源快照或提交可用的新快照并附带 diagnostics；不能暴露半更新状态。`AgentRuntime` 用 `resources_changed` 发布当前有效 revision 和诊断。
- 未信任项目打开：项目目录下的 skill、prompt template、context file、extension resource 不加载；用户级资源和明确传入的 trusted temporary resource 可以继续使用。
- UI 想预览技能正文：UI 不能直接读文件，也不能要求 snapshot 携带正文；必须通过受控 detail query，由运行时检查 source、trust 和权限后返回。
- extension 发现资源：trusted extension/package 通过 `RuntimeHooks::ResourcesDiscover` 声明资源路径或 manifest，`ResourceLoader` 负责合并 source info、读取内容、诊断和刷新 catalog；extension 不直接提交完整 `RuntimeResources`，也不直接把 prompt 正文塞进 session runtime。
- 资源 reload 后调用旧技能：已经展开并写入 session 的旧技能调用是历史 user message，不会被新资源版本改写；新的 `InvokeSkill` 使用当前 resource revision。

## pi 能力拆解

| pi `ResourceLoader` 能力 | 含义 | 本项目对应 |
| --- | --- | --- |
| `getExtensions()` | 当前已加载 extensions 与 errors | 后续 `ExtensionCatalog` / `RuntimeHooks` source |
| `getSkills()` | 当前技能 metadata 与 diagnostics | `RuntimeResources.skills: SkillCatalog` |
| `getPrompts()` | 当前 prompt templates 与 diagnostics | `RuntimeResources.prompt_templates` |
| `getThemes()` | 当前 TUI themes 与 diagnostics | 后续 UI theme 资源，MVP 可不实现 |
| `getAgentsFiles()` | 当前 `AGENTS.md` / `CLAUDE.md` 内容 | `RuntimeResources.context_files` |
| `getPrompt()` | 自定义 base system prompt | `PromptMaterials.custom_system_prompt` |
| `getAppendPrompt()` | 追加 system prompt 片段 | `PromptMaterials.append_system_prompts` |
| `extendResources(paths)` | extension 在 startup/reload 后追加资源路径 | `extend_resources(DiscoveredResourcePaths)` |
| `reload(options)` | 重新加载 settings、packages、extensions、skills、prompts、themes、context files 和 prompt files | `reload(ResourceReloadRequest)` |
| `loadProjectContextFiles()` | 加载全局与祖先目录上下文文件 | `load_context_files(workspace, agent_dir)` |

关键点：pi 的 `DefaultResourceLoader` 不拼最终 system prompt。它只保存 prompt construction 的输入。最终拼装发生在 `system-prompt.ts`，调用点在 `AgentSession._rebuildSystemPrompt()`。

## 资源类型

`RuntimeResources` 应覆盖两类资源：

- 命令型资源：技能、prompt templates、后续 extension commands。它们通常通过用户显式调用变成 user message。
- system prompt 素材：custom system prompt、append system prompt、context files、模型可见技能摘要。它们参与每次 turn 前的 system prompt 重建。

建议结构：

```rust
pub struct RuntimeResources {
    pub revision: ResourceRevision,
    pub sources: ResourceSourceIndex,
    pub skills: SkillCatalog,
    pub prompt_templates: PromptTemplateCatalog,
    pub context_files: Vec<ContextFile>,
    pub custom_system_prompt: Option<TextResource>,
    pub append_system_prompts: Vec<TextResource>,
    pub diagnostics: Vec<ResourceDiagnostic>,

    // 后续能力：extensions / themes / packages
    pub extensions: Option<ExtensionCatalog>,
    pub themes: Option<ThemeCatalog>,
}

pub struct PromptMaterials<'a> {
    pub custom_system_prompt: Option<&'a TextResource>,
    pub append_system_prompts: &'a [TextResource],
    pub context_files: &'a [ContextFile],
    pub skills: &'a SkillCatalog,
}
```

`PromptTemplates` 不默认进入每次 system prompt。它们是 slash-command-like 资源，只有 `InvokePromptTemplate` 时才展开为 user message。

## Interface

```rust
pub trait ResourceLoader {
    async fn reload(&mut self, request: ResourceReloadRequest) -> Result<ResourceReloadResult, ResourceError>;
    async fn extend_resources(&mut self, paths: DiscoveredResourcePaths) -> Result<ResourceReloadResult, ResourceError>;
    fn resources(&self) -> Arc<RuntimeResources>;
    fn prompt_materials(&self) -> PromptMaterials<'_>;
    fn diagnostics(&self) -> &[ResourceDiagnostic];
}

pub struct ResourceReloadResult {
    pub revision: ResourceRevision,
    pub changed: bool,
    pub summaries: ResourceSummaries,
    pub diagnostics: Vec<ResourceDiagnostic>,
}
```

`reload()` 和 `extend_resources()` 应以原子替换的方式更新 `RuntimeResources`：加载过程中的部分失败进入 diagnostics，不能让 UI 或 session runtime 看到半更新状态。`AgentRuntime` 使用 `ResourceReloadResult` 生成 `resources_changed`，但 `ResourceLoader` 本身不发布 UI event。

## ResourceReloadRequest

```rust
pub struct ResourceReloadRequest {
    pub workspace_root: PathBuf,
    pub agent_dir: PathBuf,
    pub project_trusted: bool,
    pub additional_skill_paths: Vec<PathBuf>,
    pub additional_prompt_template_paths: Vec<PathBuf>,
    pub additional_context_paths: Vec<PathBuf>,
    pub additional_extension_paths: Vec<PathBuf>,
    pub disabled_kinds: BTreeSet<ResourceKind>,
    pub custom_system_prompt: Option<PromptInput>,
    pub append_system_prompts: Vec<PromptInput>,
}
```

`PromptInput` 可以是文件路径或直接文本。pi 的 `resolvePromptInput()` 就支持“如果路径存在则读取文件，否则按文本处理”。这个行为对 CLI、设置项和测试很有用。

## 来源解析

pi 的 `DefaultResourceLoader.reload()` 不直接遍历一个目录了事，而是统一合并这些来源：

- 用户级配置目录。
- 已信任工作区目录。
- package manager 解析出的资源。
- CLI / options 传入的 temporary resource paths。
- extension factories 和 extension packages。
- extension `resources_discover` 事件返回的追加资源路径。

本项目 MVP 可以先不做 package/extension/theme，但 `ResourceLoader` 的模型应保留来源信息：

```rust
pub struct ResourceSourceInfo {
    pub path: PathBuf,
    pub source: String,
    pub scope: ResourceScope,      // user / project / temporary / builtin
    pub origin: ResourceOrigin,    // top_level / package / extension / builtin
    pub base_dir: Option<PathBuf>,
}
```

所有 loaded resource 都应带 `ResourceSourceInfo`，用于 UI 展示、诊断、碰撞报告和后续项目可信策略。

## 项目信任

pi reload 的一个重要职责是 project trust bootstrap：

1. 先用未信任项目状态加载 trust-related extensions。
2. 让外层决定项目是否可信。
3. 用最终 trust state reload settings。
4. 只在可信时加载项目级资源。

本项目不一定在 MVP 实现 extension-based trust，但 `ResourceLoader` 必须接收最终 `project_trusted`，并保证未信任工作区不会加载项目级 prompt、skill、context、extension 资源。

用户级资源和显式 temporary paths 可以独立于项目可信状态；项目目录下资源必须受 trust gate 保护。

## Context Files

pi 的 `loadProjectContextFiles()` 加载顺序：

1. 用户级 `agentDir` 下的 `AGENTS.md` / `CLAUDE.md`。
2. 从 filesystem root 到当前 cwd 的祖先目录上下文文件。
3. 去重后按稳定顺序进入 `<project_context>` section。

本项目建议保留相同语义，但名称可以配置：

```rust
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
    pub source: ResourceSourceInfo,
}
```

Context files 是 system prompt 素材，不是 session entry。reload 只影响未来 turn，不改写已经持久化的消息。

## System Prompt Inputs

pi 支持两种外部 system prompt 输入：

- `SYSTEM.md` 或 `systemPrompt` option：替换默认 base prompt。
- `APPEND_SYSTEM.md` 或 `appendPrompt` option：追加到 base prompt。

发现规则：

- 项目级文件只有在 project trusted 时可用。
- 用户级文件作为 fallback。
- option 可以传路径或直接文本。

本项目应把它们建模为 `TextResource`：

```rust
pub struct TextResource {
    pub content: String,
    pub source: ResourceSourceInfo,
}
```

`ResourceLoader` 只负责读取和保存这些素材；最终如何与工具说明、context files、skills、date、cwd 组合，属于 [Prompt](prompt.md)。

## Prompt Templates

Prompt templates 是可显式调用资源，不默认进入 system prompt。

Prompt templates 也会被 [CommandSurface](command-surface.md) 投影成 `/{template}` 命令摘要；名称冲突和可用性由 `CommandSurface` 决定。

`ResourceLoader` 负责：

- 从用户级、可信工作区、temporary paths 和后续 extension paths 加载模板。
- 记录 name、description、argument hint、content、source。
- 处理名称碰撞并产生 diagnostics。
- 在 reload/extend 后替换当前 catalog。

`SessionRuntime` 负责 `InvokePromptTemplate`：查 catalog、替换参数、构造 user message、入队或运行。

## Skills

技能文件处理由平级 [Skills](skills.md) 模块提供；`ResourceLoader` 只负责生命周期：

```text
ResourceLoader.reload()
  → resolve ordered skill paths
  → skills::load_skill_catalog(...)
  → attach source info
  → store SkillCatalog
  → return reload result; AgentRuntime publishes `resources_changed`
```

显式 `InvokeSkill` 的正文读取和 `<skill>` message 构造属于 `SessionRuntime`，对齐 pi `AgentSession._expandSkillCommand()`。

`ResourceLoader` 只提供技能 metadata 给 [CommandSurface](command-surface.md) 生成 `/skill:{name}` 命令摘要；它不解析用户输入，也不展开技能正文。

## Extension / Package Resource Discovery

pi 的 extension 不是直接把资源内容塞进 ResourceLoader，而是在 `resources_discover` 事件中返回路径：

```ts
interface ResourcesDiscoverResult {
  skillPaths?: string[];
  promptPaths?: string[];
  themePaths?: string[];
}
```

然后 `DefaultResourceLoader.extendResources()`：

- 规范化路径。
- 补充 extension source info。
- 与上一次 paths 合并去重。
- 只刷新受影响的 catalog。

本项目后续接入 extension/package 时，也应优先让扩展声明资源路径，而不是让扩展直接注入 prompt 内容。这样 diagnostics、source info、reload 和 UI 展示都能保持一致。

## 与 Prompt 的边界

`ResourceLoader` 不知道当前 active tools，也不应该知道工具 prompt snippets。原因是 active tools 是会话状态，而资源目录是工作区服务。

system prompt 重建应由 `SessionRuntime` 触发：

```text
resources changed OR active tools changed OR tool prompt snippets changed
  → SessionRuntime reads PromptMaterials
  → SessionRuntime reads active tool summaries / snippets / guidelines
  → prompt::build_system_prompt(request)
  → update current TurnState / Rig request
```

这保持了 pi 的模式：`ResourceLoader` 提供素材，`AgentSession` 提供会话态，`system-prompt.ts` 做纯拼装。

## AgentRuntimeEvents And RuntimeSnapshot

资源重载事件遵循 `resources_reload_started` → `resources_changed`，生命周期顺序以 [AgentRuntimeEvents](agent-runtime-events.md) 为准。`resources_changed` 表示新 revision 已经原子替换；失败 reload 应保留旧 revision，并通过 diagnostics 告知 UI。

`resources_changed` 应传递摘要，不传递大文本正文：

```text
resources_changed {
    workspace_id: WorkspaceId,
    revision: ResourceRevision,
    skills: Vec<SkillSummary>,
    prompt_templates: Vec<PromptTemplateSummary>,
    context_files: Vec<ContextFileSummary>,
    system_prompt: Option<TextResourceSummary>,
    append_system_prompts: Vec<TextResourceSummary>,
    diagnostics: Vec<ResourceDiagnostic>,
}
```

`agent_runtime_protocol::RuntimeSnapshot.resources` 也只放 summary 和 diagnostics。UI 需要预览详情时应走命令：

- `GetSkill`。
- `GetPromptTemplate`。
- 后续 `GetContextFile` / `GetEffectivePrompt`，如果产品需要。

默认不要把技能正文、context file 正文或完整 system prompt 放入快照。

## MVP Scope

MVP 建议实现：

- 用户级和可信工作区资源目录。
- `skills.rs` catalog 加载。
- prompt template catalog 加载。
- `AGENTS.md` / `CLAUDE.md` context files。
- `SYSTEM.md` / `APPEND_SYSTEM.md` prompt inputs。
- diagnostics、source info、atomic reload、`resources_changed`。

后续再加：

- package manager resource sources。
- extension `resources_discover`。
- theme catalog。
- resource watch / list changed notification。
- remote MCP resource/prompt bridge。

## 外部项目对照

- Codex 把 protocol crate 与 core runtime 分开；通信协议负责 submission/event schema，执行环境、工具、项目上下文和会话状态留在 core。`ResourceLoader` 对应的是 core/runtime 内部服务，不是 `agent_runtime_protocol` 类型。
- Rig 的 `AgentBuilder` 接收 `preamble()`、`context()`、tools 和 dynamic context；`loaders` 只是文件读取工具。因此本项目需要在 Rig 之上保留产品级 `ResourceLoader`，把本地资源整理成 Rig prompt/context 输入。
- MCP 把 prompts 和 resources 建模成可 list/get/read 的 server-managed catalog，并支持 `listChanged` 通知。这支持我们用 `resources_changed` 和 summary/detail 分离的设计。
- OpenAI Agents SDK 区分 local context 与 LLM-visible context。`ResourceLoader` 管的是 LLM-visible prompt materials，不应混入工具执行依赖或凭据。
- LangChain 把 agent harness 描述为 model loop 周边的 prompt、tools 和 middleware。`ResourceLoader` 属于本项目 harness/product layer，而不是模型 SDK 或 UI 层。

## 不应承担

`ResourceLoader` 不应：

- 构造最终 system prompt。
- 展开 `/skill:name` 或 prompt template 为 user message。
- 执行工具、审批工具或读取模型凭据。
- 拥有 session history 或 session persistence。
- 让 UI 直接读取本地资源文件。
- 接受 UI 提交的完整 `RuntimeResources` 作为公开协议输入。
- 在 reload 中把部分成功结果暴露成半更新状态。
