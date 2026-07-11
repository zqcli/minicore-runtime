# CommandSurface

`CommandSurface` 是 MiniCore 对“用户命令能力”的领域总称，不是一个有状态服务名。它覆盖 TUI 的 `/...` 输入、GUI command palette、菜单点击和快捷入口，但这些入口最终都必须回到 runtime core 做统一解析、校验和执行。

最新设计把实现拆成两层：

- `CommandManager`：`WorkspaceServices` 持有的共享、无状态命令管理器。它只持有只读的 command packs、candidate providers 和 handler registry，并在每次请求时临时 materialize 当前 session 的 command catalog。
- `Command`：`SessionRuntime` 持有的 session-scoped 命令入口。它不缓存 catalog，只负责从当前 session 组装 `CommandContext` / `SessionCommandHost`，再调用共享 `CommandManager`。

`slash command` 只是 command text 的一种输入语法，不再是核心领域名。协议层原来的 `agent_runtime_protocol::Command` 改名为 `agent_runtime_protocol::AgentCommand`，避免和 `command::Command` 子系统混淆。

## 参考经验

pi coding-agent 的 command/autocomplete 面表明 builtins、extension commands、prompt templates 和 skills 可以共享一个动态目录；可提炼的经验是命令定义与 handler 分离，技能/模板进入运行时命令入口，而不是由 UI 读文件。

Codex 的命令实现可以作为“集中定义可用性、解析和 dispatch”的参考，但 MiniCore 不能把 popup、picker、form 或 widget 模型放进 runtime core，因为 MiniCore 同时服务 TUI 和 GUI。

这些项目只作为设计参考；MiniCore 不兼容其注册 API、命令 enum、UI 组件或 dispatch 路径。

MiniCore 采用折中方案：runtime core 统一 command metadata、动态节点、解析、suggest、执行前 resolve 和 handler binding；UI 只渲染 catalog / suggestions / result，不能携带完整 runtime mutation command。

## 模块定位

```text
AgentRuntime
  └─ WorkspaceServices
      ├─ ResourceManager
      ├─ ModelGateway / ModelRegistry
      ├─ CommandManager                       // shared, stateless
      │   ├─ CommandPackStore                 // builtin nested JSON packs
      │   ├─ CandidateProviderRegistry        // dynamic candidate providers
      │   ├─ CommandHandlerRegistry           // trusted handler registry
      │   ├─ ManifestMaterializer             // manifest + context -> transient catalog
      │   ├─ Parser                           // command text -> tokens/path/args
      │   ├─ Suggester                        // command/path/arg suggestions
      │   └─ Resolver                         // resolve + validate + authorize
      └─ SessionManager
          ├─ SessionRuntime A
          │   ├─ command: Command             // session-scoped facade
          │   ├─ Tools
          │   └─ Driver
          └─ SessionRuntime B
              ├─ command: Command
              ├─ Tools
              └─ Driver
```

`CommandManager` 是无状态管理器：它不持有 current catalog、当前 resource snapshot、当前模型列表、UI 菜单状态或 pending action。`Command` 是 session-scoped facade：它属于 `SessionRuntime`，但不拥有 catalog cache；它每次调用时构造只读 `CommandContext` 和受控 `SessionCommandHost`。

## 核心职责

`CommandManager` 负责：

- 加载/持有只读 command packs。
- 将 nested JSON manifest 和动态候选 materialize 成临时 `MaterializedCommandCatalog`。
- 解析 command text，例如 `/model thinking high`。
- 为 UI 生成 command/path/argument suggestions。
- 在执行前 `resolve_for_execution`：重新解析、校验参数、校验 phase/trust/capability、绑定可信 handler。
- 调用 `CommandHandler`，但只通过 `SessionCommandHost` 访问 runtime 能力。

`CommandManager` 不负责：

- 不读文件、不读 skill body、不展开 prompt template body。
- 不调用模型、不执行工具、不写 session storage。
- 不缓存 UI selection 或 pending interaction。
- 不把 command result 渲染成 TUI/Vue 组件。
- 不把 UI-visible metadata 转换成完整 `AgentCommand` payload。

`SessionRuntime.command: Command` 负责：

- 选择目标 session、cwd、model、tools、run state、resource summary 等 session-scoped view。
- 构造 `CommandContext`。
- 构造窄接口 `SessionCommandHost`。
- 将 `ExecuteCommandText` / `ExecuteCatalogCommand` 交给 `CommandManager`。

## Command Tree

不要把命令建模成“一个 command 加一个 subcommand”。MiniCore 使用递归 command tree：

```text
CommandNode
  ├─ CommandSegment
  ├─ CommandPath
  ├─ children
  ├─ args
  └─ handler
```

`CommandPath` 可以任意深度：

```text
/model thinking high
/model provider openai gpt-5
/skill code-review 修复这个 bug
/tools enable bash
```

其中：

- `children` / dynamic children 表示“还在选择哪个命令节点”。
- `args` 表示“最终命令节点已经选定后的执行输入”。
- `bindings` 记录动态节点选择出的结构化值，例如 `skill_key`、`provider_id`、`model_id`、`thinking_level`。

这避免了二级菜单限制，也避免把有限集合的动态候选误塞成自由文本参数。

## Manifest Format

外部 command pack 使用多层 JSON，便于作者直观看到命令树；加载后由 `CommandManager` 临时 normalize 成 flat/indexed catalog。

推荐 schema 关键词：

- `nodeType`：节点语义，取值为 `group`、`action`、`groupAction`、`choice`。
- `children`：静态子节点或动态子节点声明。
- `dynamic`：说明这一批 children 由 provider 生成。
- `provider`：动态候选来源 id。
- `params`：传给 provider 的少量声明式参数。
- `bind`：candidate 字段到 invocation binding 的映射。
- `nodeTemplate`：把 provider candidate 投影成 `CommandNode` 的规则；不是 UI 模板。
- `args`：最终命令节点的执行参数。
- `handler`：可信 handler id 声明，执行时必须由 registry 重新校验。

避免使用模糊的 `kind`；也不要把 `subcommand` 作为协议/类型核心名。

示例：

```json
{
  "schema": "minicore.command.v1",
  "source": { "scope": "builtin", "name": "core" },
  "commands": [
    {
      "id": "builtin.model",
      "segment": "model",
      "title": "Model",
      "description": "Model operations",
      "nodeType": "group",
      "children": [
        {
          "id": "builtin.model.current",
          "segment": "current",
          "title": "Current model",
          "nodeType": "action",
          "handler": "builtin.model.current"
        },
        {
          "id": "builtin.model.thinking",
          "segment": "thinking",
          "title": "Thinking level",
          "nodeType": "group",
          "children": [
            {
              "dynamic": {
                "provider": "models.current.thinking_levels",
                "bind": { "thinking_level": "id" },
                "nodeTemplate": {
                  "id": "builtin.model.thinking.{id}",
                  "segment": "{id}",
                  "title": "{title}",
                  "description": "{description}",
                  "nodeType": "choice",
                  "handler": "builtin.model.set_thinking_level"
                }
              }
            }
          ]
        }
      ]
    },
    {
      "id": "builtin.skill",
      "segment": "skill",
      "title": "Skills",
      "description": "Invoke a skill",
      "nodeType": "group",
      "children": [
        {
          "dynamic": {
            "provider": "resources.skills",
            "bind": {
              "skill_key": "resource_key",
              "skill_name": "name"
            },
            "nodeTemplate": {
              "id": "resource.skill.{resource_key}",
              "segment": "{name}",
              "title": "{title}",
              "description": "{description}",
              "nodeType": "choice",
              "handler": "builtin.skill.invoke",
              "args": [
                { "name": "instruction", "argType": "tailText", "required": false }
              ]
            }
          }
        }
      ]
    }
  ]
}
```

上面的 `models.current.thinking_levels` 说明 builtin command 也可以是动态命令。思考等级来自当前 session 的模型定义，而不是固定写死在 manifest 中。

## Dynamic Candidate Providers

动态命令不由 JSON 直接读取文件或设置。JSON 只声明 provider id；真正数据来源由可信 provider 实现，并通过 `CommandContext` 读取 UI-safe summary。

```rust
pub trait CommandCandidateProvider {
    fn id(&self) -> CommandCandidateProviderId;

    fn list(
        &self,
        request: CandidateRequest,
        ctx: &CommandContext,
    ) -> CandidateResult;
}
```

内置 provider 示例：

- `resources.skills`：从当前 session cwd 的 resolved resource summary 生成 skill command nodes；只读 skill metadata，不读 skill body。
- `resources.prompt_templates`：从 prompt template metadata 生成 template command nodes；不展开模板正文。
- `models.current.thinking_levels`：从当前 `ModelSelection` 和 model definition 读取可用 thinking levels。
- `models.providers`：生成 provider 节点。
- `models.models_by_provider`：根据已有 `bindings.provider_id` 生成对应 model 节点。
- `tools.available` / `tools.active`：从 session `Tools` summary 生成工具相关命令节点。
- `sessions.recent`：从 `SessionIndex` summary 生成 session 选择节点。

Provider 输出必须经过 UI-safe validation/redaction。Catalog、suggestion 和 command result 中禁止包含：skill body、prompt template body、context file content、凭据、auth ref 细节、raw provider payload、handler internal key、完整 `AgentCommand` payload 或敏感绝对路径。

## Materialization

每次 `catalog`、`children`、`suggest` 或 `execute` 都基于当前 `CommandContext` 临时 materialize：

```text
CommandContext
  + nested command packs
  + dynamic candidate providers
  → MaterializedCommandCatalog
```

临时 catalog 可使用 flat/indexed 结构：

```rust
pub struct MaterializedCommandCatalog {
    pub revision: CommandCatalogRevision,
    pub roots: Vec<CommandNodeId>,
    pub nodes: HashMap<CommandNodeId, CommandNode>,
    pub by_path: HashMap<CommandPath, CommandNodeId>,
    pub children_by_parent: HashMap<Option<CommandNodeId>, Vec<CommandNodeId>>,
    pub diagnostics: Vec<CommandDiagnostic>,
}
```

`CommandCatalogRevision` 不要求由 `CommandManager` 存储。它可以由输入 revision 计算：command pack revision、cwd resource revision、runtime resource revision、model registry revision、current model revision、tools revision、settings revision、feature flags revision 和 run state revision。

UI 选择旧 catalog item 后执行时，runtime 必须基于当前 context 重新 materialize 并 resolve；如果节点消失或 binding 不再有效，返回 `CommandExpired` / `CatalogStale` 类错误，而不是执行旧 UI selection 携带的动作。

`CommandRunPolicy` 只控制 command handler 相对 active work 的执行时机：

- `Immediate`：立即执行，例如 `/status`、`/usage`、`/help`；结果通过 command output 异步追加到 message panel，不进入模型上下文。
- `IdleOnly`：active work 中拒绝，适用于无法安全并发且不应隐式排队的命令。
- `QueueAfterRun`：idle 时立即执行；active work 中 resolve 为 typed `PendingSessionAction`，由 `SessionRuntime` 在 post-run safe point 执行。

`CommandManager` 不保存 pending action，也不把 command 转换成 follow-up message。使用 `QueueAfterRun` 的 command 必须提供受控的 typed action 映射，不能保存 raw slash text 或延后重放 handler。`/compact` 使用该 policy，因此 running 时 catalog 使用 `availability = Available` + `run_policy = QueueAfterRun`，而不是 disabled 或隐式 abort。

`PromptDelivery` 是另一条正交轴，只适用于 command 产生的模型可见 prompt intent。`/skill`、prompt template 等 command 先立即完成 resolve，生成结构化 prompt intent，再由 `SessionRuntime` 按调用方选择的 `Steer` / `FollowUp` / `NextTurn` 调度。`/status` 和 `/compact` 忽略 `prompt_delivery`。

## Parse / Suggest / Resolve

`parse` 负责 command text 的 tokenization 和 path/args 切分，不执行业务逻辑：

```text
/model provider openai gpt-5
  → tokens = ["model", "provider", "openai", "gpt-5"]
```

随后 resolver 基于 materialized catalog 做递归 path match：优先 static child，必要时匹配 dynamic child，并记录 bindings。无法继续匹配时，剩余 token 交给最终 node 的 args parser。

`suggest` 根据 input/cursor 返回候选，不是 UI renderer：

```text
/                         -> root commands
/model                    -> current / thinking / provider
/model thinking           -> 当前模型支持的 thinking levels
/model provider openai    -> openai 下的 models
/skill                    -> 当前 cwd 的 skills
```

`resolve_for_execution` 是所有入口执行前的强制步骤。它必须检查：

- command node 仍存在且 catalog revision 可兼容。
- dynamic bindings 仍能在当前 context 下重解析。
- args 合法。
- 当前 session、cwd、run phase、trust、feature flag 和 capability 允许执行。
- handler binding 来自可信 registry，且 source 允许绑定该 handler。

不要把这一步叫复杂 `planner`。这里的本质是 resolve + validate + authorize + bind handler。

## Execution Interface

协议层使用 `agent_runtime_protocol::AgentCommand` 作为外部运行时命令枚举；command 子系统使用 `command::Command` 作为 session-scoped facade。

推荐外部入口：

```rust
pub enum AgentCommand {
    SubmitPrompt { session_id: SessionId, input: UserInput, delivery: PromptDelivery },
    ExecuteCommandText { session_id: Option<SessionId>, raw: String, prompt_delivery: PromptDelivery },
    ExecuteCatalogCommand { session_id: SessionId, selection: CommandSelection, args: CommandArgs, prompt_delivery: PromptDelivery },
    DecideToolApproval { approval_id: ApprovalRequestId, session_id: SessionId, run_id: RunId, call_id: ToolCallId, decision: ToolApprovalDecision },
    AbortRun { run_id: RunId },
    ResumeRun { session_id: SessionId, resume_id: ResumeId },
    ReloadResources { workspace_id: WorkspaceId, cwd: PathBuf },
    SetModel { session_id: SessionId, provider_id: String, model_id: String },
    SetThinkingLevel { session_id: SessionId, level: ThinkingLevel },
    SetActiveTools { session_id: SessionId, tool_names: Vec<String> },
}
```

`ExecuteCommandText` 覆盖 TUI slash input 和 GUI command palette 的文本入口。`ExecuteCatalogCommand` 覆盖 GUI/TUI 从 catalog 选择某个节点后的结构化执行入口。它只能携带 command selection、catalog revision、bindings、args 和 prompt-producing command 使用的 `prompt_delivery`，不能携带完整 runtime mutation command。

高权限 mutation 不属于公开 `AgentCommand`：

```rust
pub(crate) enum InternalAgentCommand {
    AppendMessage { session_id: SessionId, message: MessageRecord, trigger_turn: bool },
    SetToolDefinitions { session_id: SessionId, tools: Vec<ToolConfig> },
    MutateSessionHistory(...),
}
```

这类能力只能由内部受控代码路径或测试 harness 使用，不能出现在 UI-visible catalog、command output action、event metadata 或快照中。

## Command Handlers

handler 逻辑放在 `src/command/handlers/`。`src/command/handler.rs` 只放 trait 和 registry。

推荐代码结构：

```text
src/
  command.rs
  command/
    mod.rs
    manager.rs          // CommandManager，无状态
    session.rs          // pub struct Command，SessionRuntime 持有的 facade
    manifest.rs         // nested JSON schema / load
    definition.rs       // CommandNodeSpec / nodeType / args / dynamic
    materialize.rs      // manifest + CommandContext -> transient catalog
    provider.rs         // CommandCandidateProvider trait + registry
    parse.rs            // command text parser
    suggest.rs          // command/path/arg suggestions
    resolve.rs          // resolve_for_execution
    handler.rs          // CommandHandler trait + registry
    host.rs             // SessionCommandHost trait
    result.rs           // CommandResult / CommandError
    handlers/
      mod.rs
      help.rs
      status.rs
      session.rs
      model.rs
      thinking.rs
      resources.rs
      skills.rs
      prompt_templates.rs
      tools.rs
```

`CommandHandler` 通过窄 host 操作 runtime：

```rust
#[async_trait]
pub trait CommandHandler {
    async fn execute(
        &self,
        invocation: ResolvedCommandInvocation,
        host: &mut dyn SessionCommandHost,
    ) -> CommandResult;
}
```

`SessionCommandHost` 包含 `submit_prompt_intent(intent, delivery)`、`queue_session_action(action)`、`reload_resources`、`set_model`、`set_thinking_level`、`set_active_tools`、`abort_run`、`resume_run`、`new_session`、`switch_session` 等受控方法。Handler 不拿完整 `SessionRuntime`，避免形成超宽接口。prompt-producing handler 只能提交结构化 intent，`QueueAfterRun` handler 只能提交该节点声明允许的 typed session action。

## Skill And Prompt Template Commands

Skill 必须注册进 command catalog。它不是普通 arg completion。

```text
/skill code-review 修复这个 bug
```

解析结果：

```text
path = ["skill", "code-review"]
bindings.skill_key = ResourceKey(...)
bindings.skill_name = "code-review"
args.instruction = "修复这个 bug"
handler = "builtin.skill.invoke"
```

Catalog 只使用 skill metadata：name、title、description、`ResourceKey`、source info、resource revision/content hash 摘要。真正执行时：

```text
builtin.skill.invoke
  -> SessionCommandHost.admit_prompt_intent(SkillPromptIntent, delivery)
  -> SessionRuntime chooses active/future delivery boundary
  -> target PromptTurn.resolve_intent(...)
  -> 从 captured PromptResourceView 读取 selected SkillResource.body
```

Prompt template 也可以通过 dynamic provider 进入 command tree。canonical path 是 `/template <name>`；仅在不与 builtin/root command 或其他 alias 冲突时 materialize `/{template}` alias。Template body、required skills 和参数替换只在目标 `PromptTurn.resolve_intent(...)` 发生，不在 catalog/materialization 阶段发生；完整规则见 [PromptTemplates](prompt-templates.md)。

可选 colon alias：支持 `/skill:name`，但 canonical path 是 `/skill name`。Alias 解析后必须归一化为相同 `CommandPath` 和 binding；这不是对任何外部命令系统的兼容承诺。

## UI Boundary

UI adapter 负责渲染，不负责授权或执行：

- 根据 `CommandCatalogView` / `CommandSuggestion` 渲染 slash autocomplete、command palette、嵌套菜单或 GUI picker。
- 提交 `AgentCommand::ExecuteCommandText` 或 `AgentCommand::ExecuteCatalogCommand`。
- 普通 Enter 默认携带 `PromptDelivery::Steer`：idle 时立即开始，running 时尝试注入当前 run；显式 follow-up 快捷键或 UI mode 使用 `PromptDelivery::FollowUp`。该 delivery 对 prompt-producing command 生效，但不能覆盖节点的 `CommandRunPolicy`。
- 消费 display-neutral `CommandResultView` / command output event，自行显示到 message panel、toast、modal 或页面。
- UI-local command 可以作为 adapter 本地 overlay，但不能 shadow runtime command，也不能进入 `AgentRuntime` 执行。

Runtime core 不输出 TUI/Vue widget，不输出 popup layout，不输出 button callback，不输出完整 `AgentCommand` action。若未来需要 runtime-owned command action，也必须是 opaque action id，并由 runtime 重新校验，不允许 UI-visible object 携带完整 mutation command。

## Protocol Events And Snapshot

协议快照只暴露 command catalog summary，不代表执行授权：

```rust
pub struct CommandSnapshot {
    pub revision: CommandCatalogRevision,
    pub commands: Vec<CommandNodeSummary>,
    pub diagnostics: Vec<CommandDiagnostic>,
}
```

Catalog change 可以发：

```rust
CommandCatalogEvent::Changed {
    workspace_id: WorkspaceId,
    session_id: SessionId,
    revision: CommandCatalogRevision,
    commands: Vec<CommandNodeSummary>,
}
```

`CommandAck` 只表示 `AgentRuntime.dispatch(AgentCommand)` 是否接收了协议命令。Unknown command、参数非法、phase 不允许等用户级错误，优先返回 accepted，然后通过 command result/output 事件展示；transport/runtime 级无法接收才 rejected。

程序化 UI 读取不经过 CommandSurface：GUI sidebar、settings page、资源详情、command catalog/suggestions 和 usage detail 使用 `AgentRuntime.query(RuntimeQuery)` 直接取得 typed result。面向用户的 `/status`、`/usage`、`/help` 仍是 command text，结果通过 command output 展示。`/resume` handler 可以调用同一个 `SessionManager.list_sessions(...)` 并生成 picker interaction，但 GUI sidebar 使用 `SessionQuery::List`；两者不能因此合并 Command 和 Query seam。

不推荐给每次 command 发 `command_started/finished`。真正业务事实仍使用对应事件：`resources_changed`、`session_model_changed`、`skill_invoked`、`prompt_template_invoked`、`run_finished`、`tool_call_*`。Command output 只是用户可见解释，不替代业务事件。

## 安全边界

- Command catalog 是 UI-safe metadata，不是授权。
- UI selection 不是授权；执行时必须重新 materialize 和 resolve。
- UI-visible metadata 不得携带完整 `AgentCommand` 或 internal handler key。
- `nodeTemplate` 是 candidate 到 `CommandNode` 的投影，不是 UI 模板，也不能读文件或执行代码。
- Dynamic provider 只能从 `CommandContext` 的安全 view 读取摘要。
- Project-local manifest MVP 不允许注册可执行 handler；未来若允许，必须经过 project trust、package/source identity、capability 和 handler registry 校验。
- Builtin handler binding 只能由 builtin pack 使用；manifest 不能自由引用任意高权限 handler。
- Skill body、prompt template body、context file content 和完整 system prompt 不进入 command catalog。
- 工具 mutation、会话历史 mutation 和 tool definition mutation 不属于公开 command catalog。

## MVP 命令范围

第一阶段建议只覆盖：

- `/help`：展示 command catalog / 单个 command help。
- `/status`：展示当前 session/runtime 摘要。
- `/reload`：调用 `ReloadResources`。
- `/skill <name> [instruction]`，兼容 `/skill:name [instruction]`：执行 skill invocation。
- `/template <name> ...` 或 `/{template}` alias：执行 prompt template invocation。
- `/model current`、`/model provider <provider> <model>` 或动态 path：读取/设置模型。
- `/model thinking <level>`：设置当前模型支持的 thinking level。
- `/tools list`：只读展示工具摘要；mutation 后置。
- `/compact [instructions]`：Compaction 后端启用后使用 `QueueAfterRun`；idle 时立即压缩，running 时排为唯一 pending manual compact。

暂缓 extension executable commands、project-local command handlers、复杂 form schema、generic runtime interaction submit、runtime-owned action ids、tool/permission mutation command 和 catalog hooks。

## 测试重点

- nested JSON materialize 成 flat catalog 的 parent/children/path index。
- 动态 provider 生成 builtin 动态节点，例如 thinking levels。
- Skill 作为 dynamic command node 注册进 catalog，不读取 skill body。
- `/skill name args` 与 `/skill:name args` 归一到同一 invocation。
- `ExecuteCommandText` 与 `ExecuteCatalogCommand` 最终都走 `resolve_for_execution`。
- stale catalog selection 被拒绝或重解析，不执行旧 UI payload。
- `PromptDelivery` 是 prompt-producing 输入的唯一交付入口；不再存在独立 `Steer` / `FollowUp` / `NextTurn` protocol command。
- `/status` 在 active work 中按 `Immediate` 执行，command output 可立即显示且不进入模型上下文。
- prompt-producing slash command 在 active work 中按 `prompt_delivery` 进入 steer/follow-up/next-turn，不把 raw slash text 入队。
- `/compact` 在 idle 时立即进入 handler，在 active work 时 resolve 为 `QueueAfterRun`，不能隐式调用 `AbortRun` 或转换成 follow-up message。
- `/skill review` + `Steer` 在下一模型调用前注入结构化 skill prompt intent；相同命令 + `FollowUp` 等当前 work 后再启动，不重复 parse raw slash text。
- runtime 尚不支持运行中注入时，`Steer` 返回明确 capability/phase error，不能静默变成 `FollowUp`。
- pending compact 已存在或 compaction 已开始时，重复执行分别返回 `CompactAlreadyQueued` / `CompactionAlreadyRunning`。
- UI-visible catalog/result 不包含完整 `AgentCommand`、handler key、resource body 或 secret。
- 高权限 `InternalAgentCommand` 不出现在公开协议、快照、事件或 command output 中。
