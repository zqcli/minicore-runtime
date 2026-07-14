# Prompt Templates

`prompt_templates.rs` 是与 `skills.rs` 平级的纯提示模板能力模块。它定义 prompt template 的 metadata/resource/catalog/invocation 类型，并提供 Markdown/frontmatter 解析、参数解析和单次展开 helper；它不是长期 runtime service，不拥有 roots、trust、overlay、reload、queue 或 session history。

## Owner 分层

```text
ResourceManager
  owns: template roots、trust gate、runtime/cwd 分层、overlay、snapshot、reload、diagnostics

prompt_templates.rs
  provides: 类型、frontmatter 解析、参数解析、单次替换和校验 helper

CommandManager
  consumes: metadata only，生成 /template <name> 和安全 alias

PreparedMessageTurn
  consumes: captured PromptTemplateResource，按 PromptDelivery 对应的目标 snapshot 展开 intent
```

`ResourceManager` 和 PromptTemplates 不直接构造 user message；`PreparedMessageTurn.compose_user_message()` 是模板正文进入模型输入的唯一组装入口。

## 数据结构

```rust
pub struct PromptTemplateMetadata {
    pub name: PromptTemplateName,
    pub description: String,
    pub argument_hint: Option<String>,
    pub required_skills: Vec<SkillName>,
    pub file_path: PathBuf,
    pub source: ResourceSourceInfo,
}

pub struct PromptTemplateResource {
    pub metadata: PromptTemplateMetadata,
    pub body: Arc<str>,
    pub content_hash: ContentHash,
}

pub struct PromptTemplateCatalog {
    pub templates: Vec<PromptTemplateResource>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

pub struct PromptTemplateIntent {
    pub template_key: ResourceKey,
    pub args: Vec<String>,
    pub additional_instructions: Option<String>,
}
```

selected template 必须把正文或不可变正文引用放入 `CwdResourceSnapshot.resolved`。执行时禁止通过 `metadata.file_path` 重新读取磁盘。

## 文件格式

模板是 Markdown 文件，文件名默认成为 template name：

```markdown
---
description: Review a release
argument-hint: "<version> [focus]"
skills:
  - code-review
  - security-audit
---
Review release $1.
Focus on ${2:-regressions, security, and compatibility}.
```

frontmatter：

- `description` 可选；缺省时使用正文第一条非空行的受限预览。
- `argument-hint` 可选，只用于 command presentation。
- `skills` 可选，声明结构化 skill dependencies；这些 skill 必须从同一个 captured `PromptResourceView` 解析，该 view pin 住目标 `TurnResourceSnapshot`。
- name 必须满足稳定 command/resource name 规则。

模板正文不是 system prompt，也不能声明 system-level override capability。

## 参数语法

MVP 采用受限单次替换；占位符语法参考成熟 prompt-template 实现，但由 MiniCore 独立定义，不承诺外部兼容：

```text
$1、$2、...
$@ / $ARGUMENTS
${1:-default}
${@:N}
${@:N:L}
```

command text 参数支持单双引号分组；`ExecuteCatalogCommand` / 结构化 `InvokePromptTemplate` 直接携带 `Vec<String>`，不再次 tokenise。

替换规则：

- 只扫描模板正文一次。
- 参数值和 default 中的 `$1` / `$@` 不递归替换。
- 缺失普通 positional placeholder 展开为空字符串。
- `${N:-default}` 在参数缺失或为空时使用 default。
- slice 使用 1-based index；结果以单个空格连接。
- 展开后不重新解析 slash command、skill mention、shell 或模板指令。

禁止：

- shell/environment interpolation。
- 文件读取或 URL fetch。
- template include template。
- 任意代码执行。
- 展开结果递归执行 `/skill` 或 `/template`。

未来若需要 template include，只能另立 ADR 设计显式 DAG、cycle detection、depth/size limit；不能通过文本递归解析实现。

## 解析与展开接口

```rust
pub fn load_prompt_template_catalog(
    inputs: PromptTemplateLoadInputs,
) -> Result<PromptTemplateCatalog, ResourceError>;

pub fn parse_prompt_template(
    path: &Path,
    markdown: &str,
    source: ResourceSourceInfo,
) -> Result<PromptTemplateResource, PromptTemplateError>;

pub fn parse_template_args(text: &str) -> Result<Vec<String>, PromptTemplateError>;

pub fn expand_template(
    template: &PromptTemplateResource,
    args: &[String],
) -> Result<String, PromptTemplateError>;
```

这些函数保持同步、纯计算；目录选择和 I/O 由 ResourceManager 的 loader pipeline 编排。

## Snapshot 与 PromptDelivery

队列保存：

```text
resource_key
args
attachments / immutable attachment references
```

additional instructions 作为 args/metadata 的一部分保存；不保存 raw slash text，也不保存提前展开的 body。

展开边界：

```text
Steer
  → active PreparedMessageTurn
  → active TurnResourceSnapshot 中的 template + required skills

FollowUp / NextTurn / idle submission
  → target turn capture_turn_resources(...) + capture_turn_tools(...)
  → new PreparedMessageTurn
  → future snapshot 中的 template + required skills
```

如果 active snapshot 不包含 template 或某个 required skill，返回 `PromptTemplateUnavailableInTurnSnapshot` / `SkillUnavailableInTurnSnapshot`。不能重新读取 current snapshot、使用新 revision 或静默降级为 FollowUp。

## 与 Skills 的组合

模板声明的 skills 和调用方显式提供的 skills 先按 `ResourceKey` 去重，再使用稳定顺序格式化：

```text
resolved skill blocks
  → expanded template body
  → additional instructions
  → attachments
```

skill body 和 template body 必须来自同一个 captured `TurnResourceSnapshot`。这保证 reload 时不会出现 rev-1 template 搭配 rev-2 skill。

## CommandSurface

canonical command 始终是：

```text
/template <name> [args...]
```

短 alias：

```text
/{name}
```

只有在不与 builtin/root command、skill alias 或其他 trusted command node 冲突时才 materialize。冲突产生 diagnostic，不能依赖加载顺序。

CommandManager 只读取 `PromptTemplateMetadata`，不读取或展开正文。执行时重新 materialize catalog、resolve selection，再生成结构化 `PromptIntent::Template(PromptTemplateIntent)`。

## Limits 与安全

- 单个 template body 有 resource load size limit。
- 展开后有 prompt composition byte/token budget。
- template name/description/argument hint 有长度限制。
- required skill 数量有限制并去重。
- project template 必须通过 ResourceManager trust gate。
- template 无权读取凭据、环境变量、session storage 或 tool executor。
- template 无权把普通用户输入提升为 system message。

## 测试重点

- quoted args、positionals、defaults、slice 和非递归替换。
- runtime/global 与 cwd/project 同名模板 overlay。
- canonical `/template` 在 alias 冲突时仍可调用。
- queued invocation 在 future snapshot 展开新 revision。
- active Steer 在 reload 后仍展开 active revision。
- template 与 required skills 始终来自同一个 snapshot。
- template/skill 不存在时明确失败且不读磁盘。
- 展开结果中的 slash/shell placeholder 不被执行。
- size limit、invalid frontmatter 和 duplicate name 产生结构化 diagnostics。
