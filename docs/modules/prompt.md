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
- custom runtime hook 替换 system prompt。

重建结果进入下一次 `TurnState`。运行中的 turn 不应因为资源 reload 被中途改写，除非后续显式设计 restart/abort 行为。
