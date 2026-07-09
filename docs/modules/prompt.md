# Prompt

`prompt.rs` 是纯系统提示词构建模块，对齐 pi coding-agent 的 `system-prompt.ts`。它不加载文件、不扫描资源、不执行工具，只把会话态和资源素材拼成一次 Agent turn 使用的最终 system prompt。

## 输入边界

```text
TurnResourceSnapshot.cwd.resolved
  → PromptMaterials: custom system prompt, append system prompts, context files, SkillCatalog

SessionRuntime tool state
  → active tools, tool prompt snippets, prompt guidelines

SessionRuntime
  → cwd, current date, product defaults, model/tool state

prompt.rs
  → final system prompt string
```

`ResourceManager` 管素材生命周期和 snapshot capture，`Prompt` 管拼装规则，`SessionRuntime` 决定何时重建。

`ResourceManager` 和 `Prompt` 不直接互相调用。二者之间的唯一编排者是 `SessionRuntime`：

```text
ResourceManager.capture_turn(...)
  → TurnResourceSnapshot

SessionRuntime
  → 从 TurnResourceSnapshot.cwd.resolved 取 PromptMaterials
  → 合并 active tools / tool snippets / cwd / date 等会话态
  → 调用 prompt::build_system_prompt(...)

Prompt
  → 返回最终 system prompt 字符串
```

因此 `Prompt` 不能调用 `ResourceManager.current_cwd(...)`、不能读 `ResourceSnapshotStore`、不能读文件，也不能触发 reload。`ResourceManager` 也不能调用 `prompt::build_system_prompt(...)`；它只发布结构化资源和 prompt materials。

## Interface

```rust
pub struct PromptRequest<'a> {
    pub cwd: &'a Path,
    pub current_date: Date,
    pub prompt_materials: PromptMaterials<'a>,
    pub active_tool_names: &'a [ToolName],
    pub tool_snippets: BTreeMap<ToolName, String>,
    pub tool_guidelines: Vec<String>,
    pub product_docs: Option<ProductDocsPrompt>,
}

pub fn build_system_prompt(request: PromptRequest<'_>) -> String;
```

`build_system_prompt()` 应该是确定性的纯函数。测试可以直接传入 fake resources 和 active tools 验证输出。

## PromptMaterials 示例

一次 turn 捕获到的资源可能投影为：

```rust
PromptMaterials {
    custom_system_prompt: None,
    append_system_prompts: [
        TextResource {
            content: "回答要简洁，优先给可执行步骤。",
            source: "~/.minicore/prompts/append.md",
        },
    ],
    context_files: [
        ContextFileResource {
            path: "/repo/AGENTS.md",
            content: "本项目使用 Rust。提交前运行 cargo test。",
        },
    ],
    skills: SkillCatalog {
        skills: [
            SkillResource {
                metadata: SkillMetadata {
                    name: "code-review",
                    description: "审查代码变更并指出风险。",
                    file_path: "/repo/.minicore/skills/code-review/SKILL.md",
                    disable_model_invocation: false,
                },
                body: "...完整 skill 正文...",
            },
        ],
    },
}
```

`SessionRuntime` 再补上会话态和工具态：

```rust
PromptRequest {
    cwd: "/repo",
    current_date: "2026-07-06",
    prompt_materials,
    active_tool_names: ["read", "bash"],
    tool_snippets: {
        "read": "Read file contents.",
        "bash": "Execute shell commands.",
    },
    tool_guidelines: ["修改文件前先读取相关上下文。"],
}
```

`prompt::build_system_prompt(...)` 可能生成：

```text
You are MiniCore, a coding agent runtime.

Guidelines:
- 修改文件前先读取相关上下文。
- 回答要简洁，优先给可执行步骤。

Project context:

<project_instructions path="/repo/AGENTS.md">
本项目使用 Rust。提交前运行 cargo test。
</project_instructions>

Available tools:
- read: Read file contents.
- bash: Execute shell commands.

The following skills provide specialized instructions for specific tasks.
Use the read tool to load a skill's file when the task matches its description.

<available_skills>
  <skill>
    <name>code-review</name>
    <description>审查代码变更并指出风险。</description>
    <location>/repo/.minicore/skills/code-review/SKILL.md</location>
  </skill>
</available_skills>

Current date: 2026-07-06
Current working directory: /repo
```

注意：skill body 不默认进入 system prompt；这里只展示 skill 摘要。显式 `InvokeSkill` 时，`SessionRuntime` 才从 captured `TurnResourceSnapshot` 读取 `SkillResource.body` 并作为 user message 注入。

## pi 对齐规则

pi `buildSystemPrompt()` 的重要行为：

- 如果存在 custom system prompt，就使用它作为 base，但仍追加 append system prompt、context files、skills、date 和 cwd。
- 如果没有 custom system prompt，就使用产品默认 coding-agent prompt。
- tool list 来自 active tools；只有存在 prompt snippet 的工具才显示在 `Available tools` 列表。
- tool guidelines 去重后追加到 `Guidelines`。
- context files 被包进 `<project_context>` 和 `<project_instructions path="...">`。
- skills 只有在 `read` 工具可用时才加入 `<available_skills>`。
- 当前日期和当前工作目录放在最后。

## Skills Section

技能摘要不是无条件进入 system prompt：

```text
if active_tool_names contains "read" and skill_catalog has visible skills:
  append skills::format_available_skills(...)
```

原因是技能摘要会指示模型使用 `read` 工具加载技能文件。没有 `read` 时暴露技能位置会形成不可执行承诺。用户显式 `InvokeSkill` 不受这个限制，因为会话运行时会直接把技能正文构造成 user message。

## Context Files

context files 是 prompt materials，不是 session entries。构建器应保留来源路径：

```text
<project_context>

Project-specific instructions and guidelines:

<project_instructions path="/abs/path/AGENTS.md">
...
</project_instructions>

</project_context>
```

这些内容只影响未来 turn，不改写历史。

## 与 SessionRuntime 的关系

`SessionRuntime` 在以下时机重建 system prompt：

- 新会话启动。
- 下一次 user turn 捕获到新的 current `TurnResourceSnapshot` / `CwdResourceSnapshot` revision。
- active tools 改变。
- 工具 prompt snippets / guidelines 改变。
- 后期 custom runtime hook 替换 system prompt。

重建结果进入下一次 `TurnState`。运行中的 turn 不应因为资源 reload 被中途改写，除非后续显式设计 restart/abort 行为。
