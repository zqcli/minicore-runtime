# CommandSurface

`CommandSurface` 是 MiniCore 运行时提供给下游 CLI、TUI 和 GUI 宿主的统一命令入口、解析、执行映射和呈现协调模块。它把用户输入的 `/...` 文本、command palette 选择、技能命令和提示模板命令，经过 `Parse → Plan → Execute → Present` 四个阶段，转换成已有 `agent_runtime_protocol::Command`、受控 query、模型可见用户输入，或 UI 可渲染的交互请求 / 命令输出。

它不是 Agent loop，不是工具调用，也不是 UI 本地快捷键系统。它的核心价值是让下游 CLI、TUI 和 GUI 共享同一套命令目录、可用性规则、资源命令投影、执行映射和用户可见结果表达，避免每个产品仓库各自实现一份 `/compact`、`/skill:name`、`/model`、`/usage` 解析和展示逻辑。

## 参考经验

pi coding-agent 的 slash command 面由几类来源合成：

- built-in commands：`/model`、`/compact`、`/reload`、`/resume`、`/tree`、`/login`、`/quit` 等，主要在 interactive mode 中处理。
- prompt templates：文件模板以 `/{template_name} args` 形式进入 autocomplete，并在 `AgentSession.prompt()` 中展开。
- skills：可选地注册为 `/skill:{name}`，由 `AgentSession._expandSkillCommand()` 读取技能正文并构造 `<skill>` 用户消息。
- extension commands：由 `pi.registerCommand()` 注册，先于技能和模板执行；扩展命令自己管理交互，不能像普通用户消息一样随意排队。

pi 的关键经验是：slash command 是用户输入入口，但真正执行仍落回 session/runtime 能力。技能和模板不是 UI 展开；UI 只提供 autocomplete 和提交入口。

Codex 的 TUI 把 slash command 明确建模为 `SlashCommand` enum，并为每个命令定义 description、inline args、是否可在任务运行中使用、是否可在 side conversation 中使用。Composer 负责识别 `/...`，dispatch 层再把命令转换成 app events、用户消息、compact 请求、model popup、status 输出或其他 runtime action。Codex 的关键经验是：slash command catalog、dispatch 规则和用户可见反馈必须集中，否则 popup、解析、队列和执行状态会漂移。

Claude Code 的公开行为也说明，slash command 不只用于聊天输入：模型、权限、配置、memory、compact、init、MCP 等都可以通过 `/...` 触发。对本项目的启发是：后续所有 runtime-owned UI 配置项都应有 slash command 版本，但这些命令仍必须回到 runtime/core 承接者执行，不能让 UI 直接修改配置文件、资源或会话状态。

## 推荐结论

本项目需要 slash command 能力，但它应该是 runtime-owned command surface，而不是 TUI-only 语法糖。

理由：

- MiniCore 有多个下游宿主。Ratatui 需要 `/...` 输入，Tauri/Vue 需要 command palette、picker 和配置搜索，CLI 也可以提供命令式入口；它们应该消费同一个 command catalog。
- 已有能力天然需要文本命令入口：`Compact`、`ReloadResources`、`InvokeSkill`、`InvokePromptTemplate`、`SetModel`、`SetThinkingLevel`、`FocusSession`、`NavigateSessionTree`。
- 后续所有 runtime-owned UI 配置项都应有 slash command 版本，例如 model、thinking level、stream options、auto compaction、active tools、permissions 和 provider/service tier。
- skills 和 prompt templates 来自 `ResourceManager`，UI 不应该读取或解析资源文件。
- command 可用性依赖 runtime 状态：session 是否 loaded、run 是否进行中、是否 focused、资源 revision、项目是否 trusted、工具/模型能力。
- 后续 extension/package/MCP command 需要统一名称冲突、source info、diagnostics 和权限边界。

但 slash command 不应该成为新的底层执行通道。它只解析、规划和呈现用户命令；真正执行仍由 `AgentRuntime`、`SessionManager`、`SessionRuntime`、`ResourceManager`、`Compaction`、`Tools`、`UsageStats` 等模块承接。

## 模块定位

```text
UI Adapter
  ├─ renders command palette / slash autocomplete
  ├─ renders command presentation: message / popup / picker / menu / form
  └─ submits ExecuteSlashCommand or runtime-provided interaction submit action

AgentRuntime
  ├─ owns CommandSurfaceService
  ├─ exposes slash command catalog in RuntimeSnapshot / events
  ├─ dispatches resolved command to SessionManager / SessionRuntime / ResourceManager
  └─ publishes command presentation events

CommandSurfaceService
  ├─ builds SlashCommandCatalog from builtins + resources + extensions
  ├─ parses raw "/..." text
  ├─ plans execution: protocol command / runtime query / prompt input / interaction
  ├─ normalizes backend outcome into CommandPresentation
  └─ never executes tools, calls model, writes session, or reads resource bodies directly

ResourceManager
  └─ owns SkillCatalog / PromptTemplateCatalog used by command projection

SessionRuntime
  ├─ executes session-scoped commands
  ├─ expands skill/template invocations into user messages
  ├─ applies queue/phase policy
  └─ owns run/compaction/config events for a session
```

`CommandSurfaceService` 可以作为 `AgentRuntime` 内部 service，而不是顶层架构层。文档上单独说明，是因为它定义了跨 UI 的 command surface、解析规则、执行映射和用户可见呈现语义。

## Command Sources

```rust
pub enum SlashCommandSource {
    Builtin,
    Skill,
    PromptTemplate,
    Extension,
    ModelServiceTier,
}
```

来源职责：

- `Builtin`：由 runtime 定义，例如 `/compact`、`/reload`、`/new`、`/status`、`/usage`、`/model`。
- `Skill`：由 `ResourceManager.skills` 投影，推荐稳定形式为 `/skill:{name}`。
- `PromptTemplate`：由 `ResourceManager.prompt_templates` 投影，推荐形式为 `/{template_name}`；冲突时可退回 `/prompt:{name}`。
- `Extension`：后续由 extension registry 提供 metadata 和 handler，不能绕过 runtime policy。
- `ModelServiceTier`：后续可像 Codex 一样在 `/model` 附近暴露动态 service tier shortcuts。

UI-local command 只属于 UI adapter，例如 TUI 的 copy scrollback、quit、theme 或 hotkeys。它可以参与本地 autocomplete overlay，但不属于 `agent_runtime_protocol::RuntimeSnapshot.command_surface.slash_commands`，不能进入 `AgentRuntime` 执行，也不能 shadow runtime command。

## Catalog

```rust
pub struct SlashCommandSummary {
    pub name: String,
    pub aliases: Vec<String>,
    pub description: Option<String>,
    pub source: SlashCommandSource,
    pub source_info: Option<ResourceSourceInfo>,
    pub argument_hint: Option<String>,
    pub presentation_hint: Option<CommandPresentationHint>,
    pub availability: SlashCommandAvailability,
    pub phase_policy: SlashCommandPhasePolicy,
}

pub enum CommandPresentationHint {
    MessagePanel,
    Popup,
    Picker,
    Menu,
    Form,
    DetailView,
}

pub enum SlashCommandAvailability {
    Available,
    Disabled { reason: String },
    Hidden,
}

pub enum SlashCommandPhasePolicy {
    IdleOnly,
    AllowedDuringRun,
    QueueAsSteer,
    QueueAsFollowUp,
    ImmediateRuntimeAction,
}
```

Catalog 是 UI 展示与 autocomplete / command palette 的输入，不是执行授权。真正执行仍要在 `dispatch()` 时重新校验 session、phase、trust、权限、资源 revision 和参数。本文中的 Rust 类型片段是设计草图；协议类型以 [AgentRuntimeProtocol](agent-runtime-protocol.md) 为权威。

`presentation_hint` 是语义提示，不是 UI 样式指令。Runtime 可以表达 `/model` 更适合 picker、`/usage` 更适合 popup 或 message panel；下游 CLI、Ratatui 和 Tauri/Vue 各自决定具体渲染方式。

后续如果需要让 GUI 为配置项生成表单，可以在 catalog 中补充结构化 `argument_schema`；MVP 只保留 `argument_hint` 和少量内置 command 的 presentation plan。

## 四阶段模型

CommandSurface 采用四阶段模型：

```text
Parse
  → Plan
    → Execute
      → Present
```

### 1. Parse

Parse 只负责把 raw `/...` 文本解析成 invocation，不执行业务逻辑。

```rust
pub struct ParsedSlashCommand {
    pub raw: String,
    pub name: String,
    pub args_raw: String,
}

pub struct SlashCommandInvocation {
    pub command_id: CommandId,
    pub session_id: Option<SessionId>,
    pub delivery: DeliveryMode,
    pub parsed: ParsedSlashCommand,
}
```

`ParsedSlashCommand` 是纯文本解析结果；`SlashCommandInvocation` 是 `AgentRuntime.dispatch()` 附加 command id、目标 session 和 delivery 后形成的运行时输入。

示例：

```text
/model
  → name = "model", args_raw = ""

/model openai/gpt-5
  → name = "model", args_raw = "openai/gpt-5"

/skill:tdd add tests
  → name = "skill:tdd", args_raw = "add tests"
```

普通 `SubmitPrompt` 不应默认把所有 `/...` 当命令，否则用户无法发送以 `/` 开头的普通文本。推荐做法：UI adapter 发现输入首字符为 `/` 时发送 `ExecuteSlashCommand`；如果用户想发送普通文本，可使用转义，例如 `//literal` 或选择 “send as prompt”。

### 2. Plan

Plan 根据命令定义、参数和当前 runtime state 生成执行计划。

```rust
pub enum SlashCommandPlan {
    ProtocolCommand(agent_runtime_protocol::Command),
    RuntimeQuery(RuntimeQueryPlan),
    PromptInput(PromptInputPlan),
    Interaction(InteractionPlan),
    PresentOnly(CommandPresentation),
    Rejected(CommandPresentation),
}
```

同一个命令可以因参数不同走不同计划：

```text
/model
  → Interaction(ModelPicker)

/model openai/gpt-5
  → ProtocolCommand(SetModel)

/thinking
  → Interaction(ThinkingLevelPicker)

/thinking high
  → ProtocolCommand(SetThinkingLevel)

/status
  → RuntimeQuery(StatusSnapshot)
```

Plan 阶段可以把用户级错误转换成 `Rejected(CommandPresentation)`，例如 unknown command、参数非法、当前 phase 不允许执行。这样 UI 可以把错误以 message panel item 展示给用户，而不是只得到一个无法渲染的 ack reason。

### 3. Execute

Execute 把 plan 交给对应后端承接者。`CommandSurfaceService` 不直接拥有这些状态机。

| Plan 类型 | 承接者 | 示例 |
| --- | --- | --- |
| `ProtocolCommand` | `AgentRuntime` 路由到对应 module | `/reload`、`/new`、`/model openai/gpt-5` |
| `RuntimeQuery` | `AgentRuntime` / `SessionRuntime` snapshot 或 view provider | `/status`、`/usage`、`/help` |
| `PromptInput` | `SessionRuntime` | `/skill:tdd ...`、`/{template} ...` |
| `Interaction` | `AgentRuntime` 发布 interaction request，UI 承接展示 | `/model`、`/thinking`、`/sessions` |
| `PresentOnly` | `AgentRuntime` 发布 command output | `/help` 或 parse warning |

没有 slash command 时，后端能力仍通过普通 UI 操作触发：点击 reload 按钮触发 `ReloadResources`，模型选择器触发 `SetModel`，skill picker 触发 `InvokeSkill`。通过 slash command 触发时，后端事实、事件和状态变化应保持一致；差别只在入口 metadata 和用户可见 command presentation。

### 4. Present

Present 把 parse/plan/execute/query 的结果转换成 UI 可消费的语义展示请求。

```rust
pub enum CommandPresentation {
    Message(CommandOutput),
    Interaction(UiInteractionRequest),
    Both { message: Option<CommandOutput>, interaction: Option<UiInteractionRequest> },
    None,
}
```

`CommandPresentation` 不描述像素、CSS、终端布局或 Vue 组件名。它只描述“应该展示什么语义”：一条 message panel 输出、一个可弹窗展示的输出、一个 picker、一个 menu、一个 form，或组合结果。具体展示由下游 CLI/TUI/GUI adapter 决定。`Popup` 是 `CommandOutput` 的展示目标提示；需要用户选择或提交时才使用 `UiInteractionRequest`。

## CommandAck 边界

`CommandAck` 是 UI 调用 `AgentRuntime.dispatch(...)` 后的协议层接收确认，不是 slash command 的执行结果。

```text
UI dispatch ExecuteSlashCommand("/status")
  ← CommandAck { accepted: true, command_id }
  → command_output_appended { command_id, output: Status }
```

推荐边界：

- transport/runtime 级无法接收，例如 runtime 已关闭、workspace 不存在、目标 session id 无效，可以返回 `CommandAck { accepted: false }` 或 `RuntimeError`。
- slash 输入层或业务语义错误，例如 unknown command、参数非法、当前 phase 不允许，优先返回 `CommandAck { accepted: true }`，再发 `command_output_appended { severity: Error }`。这样 TUI 和 GUI 都能在 message 面板中显示一致错误。
- 后端异步成功/失败仍通过原有业务事件表达，例如 `resources_changed`、`session_model_changed`、`run_finished`、`compaction_finished`。Command output 只是把这些结果解释成用户可读文本或交互请求。

不应把 `/status` 的结果、`/usage` 的统计或 `/model` 的候选列表塞进 `CommandAck`，否则 `CommandAck` 会混淆“已接收”和“已执行/已查询”。

## Presentation Model

### Command Output

`CommandOutput` 是可以显示到 message panel 的 UI-visible、non-model-visible 输出。它适合 `/help`、`/status`、`/usage`、`/reload` 结果、`/model` 设置完成提示、解析错误和长任务阶段提示。

```rust
pub struct CommandOutput {
    pub title: String,
    pub body: Option<String>,
    pub severity: CommandOutputSeverity,
    pub blocks: Vec<CommandOutputBlock>,
    pub actions: Vec<CommandOutputAction>,
}
```

示例：

```text
/status
  → CommandOutput(title = "Status", blocks = session/model/resources/usage summary)

/model openai/gpt-5
  → CommandOutput(title = "Model set", body = "Model set to openai/gpt-5. Takes effect on the next model call.")

/reload
  → CommandOutput(title = "Resources reloaded", body = "8 skills, 3 templates, 1 warning.")
```

`CommandOutput` 不是 assistant message，也不进入模型上下文。它可以作为 message panel item 展示；如果产品需要重连或重启后恢复这些 UI 输出，应优先通过后续 UI-only presentation log 或 runtime read model 设计恢复，不能进入 `TurnState.messages`。

### UI Interaction Request

`UiInteractionRequest` 表示 runtime 请求 UI 展示一个需要用户选择或提交的交互控件，例如 picker、multi-select、menu、form 或 detail view。

```rust
pub struct UiInteractionRequest {
    pub interaction_id: InteractionId,
    pub command_id: CommandId,
    pub session_id: Option<SessionId>,
    pub title: String,
    pub kind: UiInteractionKind,
    pub items: Vec<UiInteractionItem>,
    pub initial_selection: Option<String>,
    pub allow_search: bool,
    pub allow_multi_select: bool,
    pub submit: UiInteractionSubmit,
}
```

示例：

```text
/model
  → command_interaction_requested(ModelPicker)
  → TUI 用 ↑/↓ + Enter 选择
  → GUI 用 modal/list/search 选择
  → selection dispatches ExecuteSlashCommand("/model provider/model")

/thinking
  → command_interaction_requested(ThinkingLevelPicker)
  → selection dispatches ExecuteSlashCommand("/thinking high")

/sessions
  → command_interaction_requested(SessionPicker)
  → selection dispatches OpenSession / FocusSession
```

MVP 可以让 interaction 的选择结果重新生成 slash command，例如 `/model anthropic/claude-sonnet-4`。后续复杂表单可以增加 `SubmitCommandInteraction { interaction_id, values }`，避免 UI 拼复杂字符串。

## 第一阶段推荐命令

第一阶段目标是验证 catalog、parse、plan、execute、present、后端承接者和 message panel / picker 的完整闭环，不需要复刻 pi 或 Codex 的完整命令集。

| Slash | 无参数行为 | 有参数行为 | 承接者 | Presentation |
| --- | --- | --- | --- | --- |
| `/help [command]` | 展示命令目录 | 展示单个命令帮助 | `SlashCommandCatalog` query | message panel |
| `/status` | 展示 session/runtime/model/resource/usage 摘要 | 后续支持 `detail` | `AgentRuntime.snapshot` / session view | message panel |
| `/usage` | 展示 usage 摘要 | 后续支持 `detail` | `UsageStats` / snapshot views | message panel，GUI 可 popup |
| `/reload` | 刷新资源 | 无 | `AgentRuntime` → `ResourceManager` | lifecycle events + reload summary message |
| `/compact [instructions]` | 手动压缩 | 压缩时附加说明 | `SessionRuntime` → `Compaction` | started/final command output + compaction events |
| `/skill:{name} [args]` | 调用技能 | 附加说明 | `SessionRuntime` + `Skills` + `ResourceManager` metadata | 可选 invoked message + run events |
| `/{template} [args]` | 展开 prompt template | 模板参数 | `SessionRuntime` + prompt template catalog | 可选 expanded message + run events |
| `/model` | 打开 model picker | `SetModel` | `SessionRuntime` model state | picker；设置后 message |
| `/thinking` | 打开 thinking picker | `SetThinkingLevel` | `SessionRuntime` thinking state | picker；设置后 message |
| `/new` | 新建并聚焦会话 | 后续支持 name | `AgentRuntime` → `SessionManager` | session events + summary message |
| `/sessions` 或 `/resume` | 打开当前 workspace 的 session picker | 后续支持 `--all` / filter | `SessionManager.list_sessions(CurrentWorkspace)` + UI picker | picker；选择后 session events |
| `/tools` | 展示工具摘要 | mutation 后置 | `SessionRuntime` tool state | message 或 readonly multi-select |

第一阶段暂缓：`/tools +read -bash`、`/permissions`、`/login`、`/logout`、`/export`、`/tree`、`/fork`、extension commands、MCP resource commands 和 UI-local theme/keymap。它们涉及工具风险、凭据流程、复杂 session tree 或 extension policy，适合在核心闭环稳定后加入。

`/compact` 的 protocol command 可以随第一阶段定义，但在 `Compaction` 后端尚未实现前，catalog 必须把它标记为 `Disabled { reason }`，不能接受后静默失败。等 [Compaction](compaction.md) 模块落地后，它再成为可执行 command。

## Resolution Priority

名称冲突必须确定性处理：

1. Builtin commands 最高优先级。
2. Extension commands 不能 shadow builtins；冲突进入 diagnostics。
3. Prompt templates 不能 shadow builtins 或 extension commands；冲突时在 catalog 中标记 disabled，或只允许 `/prompt:{name}`。
4. Skills 默认只通过 `/skill:{name}` 暴露，避免和 builtins/templates 冲突。
5. UI-local commands 不进入 runtime catalog，且不能 shadow runtime commands；UI adapter 合并本地 overlay catalog 时必须做本地冲突检查。

这样可以避免 `/model` 同时是内置模型选择和项目 prompt template 时出现不可预测行为。

## 与现有流程结合

### ResourceManager

`ResourceManager` 仍然负责加载技能和提示模板候选、执行 cwd-over-runtime overlay，并在 `CwdResourceSnapshot.resolved` 中提供 effective metadata。资源 reload 后：

```text
ResourceManager.reload_cwd(CwdResourceRequest { cwd, ... })
  → build CwdResourceSnapshot { runtime, local, resolved }
  → ResourceSnapshotStore.replace_cwd(cwd, snapshot)
  → cwd resource revision changed
  → AgentRuntime rebuilds SlashCommandCatalog projection from resolved summaries
  → runtime snapshot state updated
  → resources_changed
  → slash_commands_changed?   // 当 catalog 内容或可用性变化时
```

`CommandSurface` 只读取资源摘要和 metadata，不读取技能正文。技能正文读取仍发生在 `SessionRuntime::invoke_skill`，但必须来自 captured `TurnResourceSnapshot`，不能绕过 snapshot 重新读文件。

### AgentRuntimeProtocol

协议入口：

```rust
ExecuteSlashCommand { session_id: Option<SessionId>, raw: String, delivery: DeliveryMode }
```

RuntimeSnapshot 包含：

```rust
command_surface: CommandSurfaceSnapshot { slash_commands: Vec<SlashCommandSummary> }
```

事件包含：

```rust
SlashCommandEvent::CatalogChanged { ... }       // wire: slash_commands_changed
CommandPresentationEvent::OutputAppended { ... } // wire: command_output_appended
CommandPresentationEvent::InteractionRequested { ... } // wire: command_interaction_requested
```

`GetSlashCommandCatalog`、`CompleteSlashCommandArgs`、`SubmitCommandInteraction` 和 `command_interaction_resolved` 可以后置。MVP 可以只把 `slash_commands` 放进 `RuntimeSnapshot.command_surface`，让 UI 本地 fuzzy filter；interaction 选择结果可以重新提交 `ExecuteSlashCommand`，例如 `/model {item.id}`。参数 completion、复杂 form 和 runtime-tracked pending interaction 等待基础 interaction 模型稳定后再实现。

不推荐给每次 slash command 都发 `slash_command_started/finished`。真正业务事件仍用已有事件表达，例如 `compaction_started`、`resources_changed`、`session_model_changed`、`skill_invoked`、`prompt_template_invoked`、`usage_updated`。用户可见说明由 `CommandPresentationEvent` 表达。

### AgentRuntime

`AgentRuntime` 是 `ExecuteSlashCommand` 的入口路由者：

```text
ExecuteSlashCommand
  → CommandSurfaceService.parse
  → CommandSurfaceService.plan
  → execute plan through backend owner
  → CommandSurfaceService.present backend outcome
  → AgentRuntime publishes CommandPresentationEvent
```

它负责把 `command_id` 贯穿到业务事件和 command presentation events 中，让 UI 可以把用户输入、后端事实和展示结果关联起来。

### SessionRuntime

`SessionRuntime` 负责 session-scoped slash command 的最终执行：

- `/skill:name args`：读取当前 `SkillCatalog` metadata，读取正文，构造 `<skill>` user message，然后按 `delivery` 启动 run 或入队。
- `/{template} args`：查当前 `PromptTemplateCatalog`，替换参数，构造 user message。
- `/compact`：进入 compaction 流程，不走 Driver 的普通 run。
- `/model`：更新 model state，发 `session_model_changed`，必要时重建 next `TurnState`。
- `/thinking`：更新 thinking state，发 `session_thinking_level_changed`，影响下一次 turn 或安全点后的模型调用。
- 运行中命令：根据 `SlashCommandPhasePolicy` 决定立即执行、拒绝并输出错误 message、排队为 steer/follow-up，或等待 safe point。

`SessionRuntime` 不解析 raw `/...` 字符串；它只处理结构化 command、prompt input 或 session action。

### UI Adapter

UI adapter 只负责 presentation 渲染：

- 根据 `RuntimeSnapshot.command_surface.slash_commands` 或 `slash_commands_changed` 渲染 autocomplete / command palette。
- TUI slash input 和 GUI command palette 默认提交 `ExecuteSlashCommand`；runtime-provided picker/menu/form 选择结果默认使用 `UiInteractionSubmit`。专用设置控件如果直接提交结构化 command，必须复用同一后端校验、事件和 command presentation 规则。
- 消费 `command_output_appended`，在 message panel 追加 `CommandOutput`。
- 消费 `command_interaction_requested`，用本端能力渲染 picker、menu、form 或 detail view；`/usage` 这类只展示信息的 popup 可由 `CommandOutput` 的 presentation hint 渲染。
- 对 interaction selection，MVP 可以重新提交 `ExecuteSlashCommand`，例如从 model picker 选择后提交 `/model provider/model`。
- UI-local command 可以在 adapter 内处理，但不能 shadow runtime command，也不能读取资源正文、执行工具或修改 runtime-owned settings。

### Persistence

slash invocation 本身不是模型可见 session entry。只有产生模型可见输入或会话状态变化的结果才持久化：

- `/skill:name args` 产生一条 user message，可在 metadata 中记录 `PromptInputSource::SlashSkill { raw, resource_revision }`。
- `/{template} args` 产生一条 user message，可记录 template name 和 resource revision。
- `/compact` 产生 compaction session entry 和 save point。
- `/model`、`/tools` 等配置变化按 session metadata 或 settings persistence 规则处理。
- `/status`、`/usage`、`/help` 产生 `CommandOutput`，默认不进入模型上下文。若要跨重启恢复 message panel，应通过 UI-only presentation log 或 snapshot projection 设计，不能进入 `TurnState.messages`。

## 安全边界

slash command 不能成为绕过策略的后门：

- 不允许 UI 通过 slash command 直接读本地文件；资源正文读取必须经过 runtime。
- 不允许 slash command 直接执行工具；工具仍走 `ToolGateway`、approval 和 sandbox。
- 不允许 extension slash command 绕过 `RuntimeHookRegistry` / extension policy。
- 运行中命令必须遵守 phase policy 和 queue semantics。
- catalog 中显示 command 不代表执行时一定成功；dispatch 必须重新校验。
- `CommandPresentation` 是语义展示请求，不是 UI 私有回调；UI 不能通过它修改 runtime state。

## 测试重点

- builtin/template/skill/extension 名称冲突的优先级。
- resource reload 后 catalog revision、snapshot 更新和 `slash_commands_changed`。
- `/help`、`/status`、`/usage` 产生 `command_output_appended`，而不是把结果塞进 `CommandAck`。
- `/model`、`/thinking` 无参数产生 `command_interaction_requested`，选择后重新进入同一 `ExecuteSlashCommand` 路线。
- `/model provider/model` 和 UI model picker 选择后的后端事件一致。
- `/skill:name args` 展开为 user message，旧 message 不受后续 resource reload 改写。
- `/{template} args` 参数替换和缺参行为。
- 运行中 `/compact`、`/skill`、普通 prompt 的 phase policy 和用户可见错误输出。
- UI-local command 不能通过 `AgentRuntime` 执行，也不能 shadow runtime command。
- `SubmitPrompt` 发送 `/literal` 与 `ExecuteSlashCommand` 解析命令的边界。
- `CommandPresentation` 不进入模型上下文，不被 `Driver` 或 tools 消费。
