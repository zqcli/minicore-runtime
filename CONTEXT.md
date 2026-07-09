# MiniCore Agent Runtime

本上下文描述 MiniCore：一个提供 Agent harness 能力的原生运行时核心。它借鉴 pi coding-agent 的 `AgentSessionRuntime` / `AgentSession` 生产路径，把模型调用、会话、资源、工具、`CommandSurface`、事件、RuntimeHooks 和持久化编排收敛在 UI 无关的 runtime 中；CLI、TUI 和 GUI 产品会在独立仓库中以 MiniCore 为核心接入。

## 语言

**MiniCore**：
面向桌面、终端和 CLI/GUI 宿主的轻量级原生 Agent harness runtime core。它承载模型调用、会话、资源、工具、`CommandSurface` 和运行时事件；具体 CLI、TUI、GUI 产品在下游仓库实现。
_避免_：旧项目代号、pi coding-agent 分支、WebView Agent SDK、下游产品仓库

**Harness 能力**：
围绕底层 Agent SDK 提供产品级编排的运行时能力，包括会话阶段、队列、资源加载、工具治理、审批、持久化、事件、上下文压缩、`CommandSurface`、RuntimeHooks 和 UI 协议。它是 MiniCore runtime 的职责，不是 pi-agent-core `AgentHarness` 这个具体历史类。
_避免_：UI 应用、单纯模型客户端、Rig 高阶 runner

**下游终端用户应用**：
基于 MiniCore 构建的 CLI、TUI 或 GUI 产品仓库，面向普通用户安装使用；用户不应被要求预装 pi、Node.js 或开发工具链。
_避免_：MiniCore core runtime、本地演示、测试 harness

**渲染器**：
运行在下游 GUI 宿主中的 Vue 与 TypeScript 用户界面，例如 Tauri 系统 WebView。它展示聊天状态，但不拥有 Agent 执行、本地项目访问或凭据。
_避免_：前端运行时、浏览器应用

**可信运行宿主**：
承载 MiniCore Agent 运行时的本地进程，拥有模型凭据、会话状态、本地项目访问和工具执行能力。它可以被下游 CLI/TUI 直接嵌入，也可以嵌入 Tauri 后端或作为原生 sidecar 打包。
_避免_：WebView SDK、前端 Agent

**AgentRuntimeProtocol**：
界面适配器与 Agent 运行时之间的稳定通信协议，由 `agent_runtime_protocol::AgentCommand`、`Event`、`EventMsg`、`RuntimeSnapshot` 和 `CommandAck` 等协议类型组成。`AgentCommand` 表示 UI/host 提交给 `AgentRuntime` 的协议级用户意图，不是 command 子系统本身。
_避免_：运行时桥接、直接导入 SDK、UI 回调、command 子系统

**AgentRuntimeEvents**：
描述 `agent_runtime_protocol::Event` 的命名、顺序、所有权、重连和生命周期规则。它不是事件类型定义本身，也不是会话持久化日志。
_避免_：协议类型集合、会话日志、UI 状态管理

**工作区**：
用户选定的本地项目目录，用来限定助手的文件与命令访问范围。
_避免_：当前文件夹、项目路径

**编程能力**：
助手通过 read、edit、write、search、list 或 shell execution 等工具检查或修改工作区的行为。
_避免_：通用聊天

**原生 Agent SDK**：
可以运行在可信运行宿主内的 Rust、Go、C++ 或类似原生框架；它拥有 Agent loop、工具调用、流式事件和状态推进能力，并且不要求 Node.js 运行时。
_避免_：模型客户端、API wrapper

**Agent 运行时（`AgentRuntime`）**：
与 UI 无关的 MiniCore 后端门面，供下游 CLI、TUI 和 GUI 宿主通过协议接入；它接收运行时命令、发布运行时事件、生成 `RuntimeSnapshot`，并管理工作区、会话目录和会话运行时。
_避免_：TUI 后端、桌面后端、UI 服务、GUI 应用状态

**RuntimeSnapshot**：
Agent 运行时在某个事件水位上的当前状态读模型，用于界面初始加载、窗口恢复和同一 host 生命周期内的事件流重连/订阅重建。它由运行时从内存状态、设置、资源摘要和会话目录投影生成，不是 UI store，不是会话文件，也不单独持久化。active session 当前 run 的待审批工具调用通过 `current_run.pending_tool_approvals` 投影给 UI。打开工作区后默认可以没有 active session。MVP 不支持 UI adapter 失败但 runtime daemon 继续运行再被重连的独立生命周期模型。
_避免_：UI 状态、session index、JSONL、事件日志、持久化快照文件

**运行时服务**：
`AgentRuntime` 内部的后端依赖集合总称，不是一套会随 focused session 改变而整体替换的全局单例。MVP 将服务保留在 `WorkspaceServices` 中；模型可见资源由 `ResourceManager` 管理级联不可变快照；会话和 run 捕获快照引用，而不是 pin 一整套 cwd 绑定服务。
_避免_：当前 focused session 的可变大包、每 session 服务容器、UI 服务

**WorkspaceServices**：
绑定到打开的 workspace / host 生命周期的运行时服务，例如 event bus、`SessionManager` / `SessionIndex`、共享无状态 `CommandManager`、`RuntimeHookRegistry`、`ResourceManager`、user-global settings/provider/auth、`ModelGateway` 和 runtime diagnostics 聚合。它不随 session focus 切换而重建。
_避免_：单会话运行态、focused session 服务、UI 服务、有状态 CommandSurfaceService

**ResourceManager**：
运行时内部资源子系统，负责资源来源解析、project trust gate、source info、diagnostics、级联 snapshot、cwd-over-runtime overlay policy、ensure/reload/recompose 和 turn capture。初始化由 `OpenWorkspace -> ensure_runtime_snapshot`、`OpenSession/NewSession -> ensure_cwd_snapshot`、`start_user_turn -> capture_turn` 三道调用保证；它不构造最终 system prompt，不执行技能调用，也不是公开协议的数据存储。
_避免_：UI 资源 store、系统提示词构建器、会话运行时、实时读文件入口

**ResourceSnapshotStore**：
`ResourceManager` 内部的 current-pointer 存储，保存当前 `RuntimeResourceSnapshot` 以及每个 `(workspace_id, cwd)` 的当前 `CwdResourceSnapshot`。`replace_runtime` / `replace_cwd` 是资源更新对后续 turn 生效的线性化点：它们只原子替换 current pointer，不修改旧 snapshot；已经 running 的 run 继续使用启动时捕获的旧 snapshot，后续 user turn 通过 `capture_turn` 读取新 current pointer。
_避免_：CwdServiceRegistry、CwdScopedServices、服务 generation registry、focused session resources

**运行时资源快照（`RuntimeResourceSnapshot`）**：
一次 user-global/runtime-global 资源解析结果，包含 builtin/user-global/runtime 级技能、提示模板、上下文默认值、自定义/追加系统提示词、source info、diagnostics 和 revision。它不包含 cwd-local 项目资源，也不包含 provider/auth/model gateway 状态。
_避免_：provider settings、凭据、cwd 项目资源、UI state

**cwd 资源快照（`CwdResourceSnapshot`）**：
某个 `(workspace_id, cwd)` 在一次成功资源加载/合成后的不可变资源集合。它持有构建时使用的 `Arc<RuntimeResourceSnapshot>`，并包含 cwd-local layer 与 overlay 后的 `resolved` effective view。多个 session 可以共享同一 cwd 的当前快照；不同 cwd 有各自快照。
_避免_：单纯 cwd 增量、全局当前资源、session 私有资源服务、UI 快照

**turn 资源快照（`TurnResourceSnapshot`）**：
一次 user turn/run 的资源输入，只持有 `Arc<CwdResourceSnapshot>`，不额外 pin runtime snapshot。它进入 `TurnState` 后保证 running turn 不受资源 reload 或 focused session 切换影响。
_避免_：实时资源查询、mutable prompt context、同时 pin runtime/cwd 的双来源快照

**资源覆盖策略（`ResourceOverlayPolicy`）**：
由 `ResourceManager` 集中实现的 deterministic overlay 规则。MVP 中 policy 可编码在代码内：runtime/global 资源提供候选，cwd/project 资源可按稳定 `ResourceKey { kind, namespace?, name }` 覆盖同 key 候选；selected 与 shadowed 都保留给 diagnostics。
_避免_：调用点临时合并、按文件路径碰撞、用户可随意声明 provider 覆盖

**资源更新边界**：
资源内容进入模型可见 current snapshot 的边界。MVP 中只有 startup/session/turn 的 `ensure_*` 兜底、显式 `ReloadResources` / `reload_runtime`、以及 runtime revision 变化时的 `recompose_cwd` 可以发布新 snapshot；`resources_changed` 只是通知事件，`capture_turn` 只是读取边界，不扫描文件、不自动 reload。
_避免_：文件保存即生效、hook 回调刷新、UI event 修改资源、turn 中途换资源

**会话切换 / 聚焦**：
改变 UI 或默认命令目标指向的会话。它不表示旧会话被关闭，也不表示旧会话的后台 run 被中止；只有显式 `CloseSession`、idle unload、workspace teardown 或 shutdown policy 才卸载 `SessionRuntime`。
_避免_：页面切换、替换运行时服务、关闭旧会话

**会话运行时（`SessionRuntime`）**：
单个会话的产品级 Agent 编排对象，参考 pi coding-agent 的 `AgentSession`；它管理会话阶段、当前运行、中止、等待空闲、队列、模型状态、资源、工具、工具策略、会话写入和 `Driver`。
_避免_：会话管理器、UI 会话状态

**产品级编排能力**：
从 pi-agent-core 的通用编排对象和 pi coding-agent `AgentSession` 中抽取出的职责，例如阶段、队列、资源、工具、会话写入、中止和事件。它是一组属于会话运行时的能力，不是独立模块。
_避免_：UI 后端、简单 wrapper

**运行时 Hook（`RuntimeHook`）**：
`RuntimeHooks` 模块管理的内部安全点干预能力，用于在 prompt/context、模型请求、provider payload、工具治理、压缩、保存点和 UI-safe command result 等流程中返回 typed decision / patch / replacement。当前设计不定义资源 discovery/reload hook。Hook 影响最终会发生什么，但不直接发布 `agent_runtime_protocol::Event`，不读写 session storage，不执行工具，也不读取凭据。
_避免_：UI 回调、协议事件、插件系统、任意 runtime 后门

**运行时 Hook 注册表（`RuntimeHookRegistry`）**：
保存 hook handler、source、capability、timeout、cancellation 和 failure policy 的内部运行时服务。它不拥有业务状态机，也不拥有 event metadata；`AgentRuntime` / `SessionRuntime` 调用它，并应用 typed result。
_避免_：事件总线、插件管理器、工具执行器、会话存储

**会话阶段（`SessionPhase`）**：
会话运行时当前互斥状态，例如 `idle`、`turn`、`compaction`、`branch_summary` 和 `retry`。阶段用于拒绝不合法命令和保护会话写入顺序。
_避免_：loading 状态、按钮状态

**会话**：
一次工作上下文的持久记录，包含消息、模型变化、思考等级变化、活跃工具变化、压缩摘要、标签、名称和当前叶子位置。
_避免_：聊天记录、日志文件

**SessionIndex / 会话目录**：
`SessionManager` 维护的会话轻量清单或索引，用于 `/resume`、GUI sidebar 和 `ListSessions`。它可以从 session 文件 header、metadata 或本地 index/cache 重建，包含 session id、workspace、名称、更新时间、预览和轻量统计；它不是 `RuntimeSnapshot`，也不包含完整消息、运行中状态或 UI 状态。
_避免_：RuntimeSnapshot、完整会话上下文、UI sidebar store、事件日志

**会话条目**：
会话中的一条不可变追加记录。条目通过 `id` 和 `parentId` 连接成树，类型包括 message、model_change、active_tools_change、compaction、branch_summary、label、session_info 和 leaf。
_避免_：消息、数据库行

**会话树**：
由会话条目的父子关系形成的历史树。当前对话上下文由根到当前叶子的一条路径构建，而不是由文件中所有条目线性构建。
_避免_：消息数组、历史列表

**上下文压缩**：
把会话较早历史替换为摘要，同时保留近期上下文的会话级能力。它用于控制模型上下文大小，并保持后续 Agent 运行能够继续理解已有目标、约束、进展和关键决定。
_避免_：删除历史、模型摘要工具、system prompt 改写

**压缩摘要消息**：
上下文压缩后代表较早会话历史的模型可见消息。它是历史上下文的替代物，不是新的用户请求，也不是长期行为规则。
_避免_：系统提示词、助手回复、UI 折叠文本

**压缩边界**：
上下文压缩时旧历史与保留近期上下文之间的分界。边界必须避免破坏一次 Agent 运行内部的模型消息、工具调用和工具结果关系。
_避免_：任意 token 截断、显示分页、文件切片

**模型调用消耗**：
一次模型调用真实消耗的 token 分项，例如输入、缓存命中输入、输出和推理输出。它来自模型提供方或运行时估算，用于 run 与会话累计展示。
_避免_：上下文占用、费用账单、消息长度

**上下文占用**：
当前会话投影到下一次模型请求时预计占用的上下文窗口大小。它用于上下文进度条、压缩触发和 overflow 预检，不等同于历史累计消耗。
_避免_：会话总 token、账单消耗、消息数量

**会话消耗统计**：
一个会话从创建以来累计的模型调用消耗视图。压缩不会降低会话累计消耗，只会降低后续上下文占用。
_避免_：当前上下文窗口、压缩摘要大小、UI 计数器

**会话管理器（`SessionManager`）**：
负责工作区内会话生命周期的运行时模块。它协调持久化会话目录、单会话句柄和已加载会话运行时，但不执行单个 Agent run。
_避免_：文件工具、会话管理 UI、Agent loop

**已加载会话运行时（`LoadedSessionRuntimes`）**：
会话管理器内部维护的运行中会话表，记录当前已加载的 `SessionRuntime` 和聚焦会话。它是运行时对象索引，不是持久化会话目录；其中每个 `SessionRuntime` 独立持有 phase、queue、current run、pending approval 和固定 workspace cwd。
_避免_：会话存储、会话目录、独立会话运行时注册表

**聚焦会话（focused session）**：
当前 UI 或默认命令目标指向的会话。它可以是多个已加载会话中的一个，不等同于正在运行的会话，也不是运行时服务的 scope 锚点。
_避免_：active session、running session、loaded session、service scope

**会话存储（`SessionStorage`）**：
单个会话的底层条目存储，负责保存追加式会话条目、当前叶子和会话元数据。它不决定 Agent 如何运行，也不直接服务 UI。
_避免_：会话管理器、会话运行时、聊天状态

**技能**：
可加载的 Markdown 指令包，包含稳定名称、简短描述、来源路径和正文。模型可见 selected skill 必须以 `SkillResource` 形式进入 resource snapshot，或被 snapshot pin 到不可变正文引用；显式调用时从 captured `TurnResourceSnapshot` 读取正文并作为普通用户消息进入一次 Agent 运行。
_避免_：提示词片段、插件、工具、运行时按文件路径实时读取正文

**技能模块**：
与 `ResourceManager`、会话运行时平级的 `skills.rs` 模块，提供 `SkillMetadata` / `SkillResource` / `SkillCatalog` 数据结构，以及给定目录后的发现、校验、frontmatter 处理和格式化 helper。它不决定 skill roots，不拥有 reload/ensure/recompose 生命周期，不执行 overlay，也不构造会话消息。
_避免_：独立技能加载服务、UI 技能解析、工具注册、技能生命周期管理、资源 scope owner

**技能目录**：
技能资源集合的数据结构，可由 `skills.rs` 创建，但 current catalog 的生命周期由 `ResourceManager` 的 `RuntimeResourceSnapshot`、`CwdResourceSnapshot.local`、`CwdResourceSnapshot.resolved` 和 `TurnResourceSnapshot` 持有。对 selected skills，catalog 必须能提供稳定正文或不可变正文引用。
_避免_：独立生命周期 owner、命令列表、直接读当前磁盘文件

**技能调用**：
用户显式要求使用某个技能的行为。`SessionRuntime` 从 captured `TurnResourceSnapshot.cwd.resolved.skills` 读取 selected `SkillResource.body`，调用 `skills.rs` helper 格式化 `<skill>` 块，并将它作为普通用户消息交给 Agent 运行。
_避免_：系统提示词注入、工具调用、按 `metadata.file_path` 重新读文件

**运行时资源**：
由运行时统一管理、可影响未来 Agent 运行的模型资源，包括技能目录、提示模板、上下文文件、自定义系统提示词和追加系统提示词。
_避免_：UI 文件、会话消息、工具结果

**资源快照**：
`ResourceManager` 在一次成功加载/合成后发布的不可变模型资源视图，具体分为 `RuntimeResourceSnapshot`、`CwdResourceSnapshot`、`TurnResourceSnapshot` 和预留的 `StepResourceSnapshot`。旧 snapshot 永不原地修改；reload 只替换 current pointer，running turn 继续使用已捕获引用。
_避免_：界面快照、事件日志、会话条目、实时文件视图

**资源摘要**：
资源快照投影给界面适配器的安全视图，只包含展示、来源、revision、覆盖关系和诊断等信息，不包含技能正文、上下文文件正文或完整系统提示词。
_避免_：资源正文、提示词素材、完整运行时资源

**提示词素材**：
`ResourceManager` 从 captured `TurnResourceSnapshot.cwd.resolved` 提供给系统提示词构建器的结构化输入，例如自定义系统提示词、追加系统提示词、上下文文件和技能目录摘要。它不是最终 system prompt；`Prompt` 不能主动读取 `ResourceManager` 或 `ResourceSnapshotStore`，只能消费 `SessionRuntime` 传入的素材。
_避免_：完整 prompt、用户消息、工具描述、实时资源查询

**CommandSurface**：
Agent 运行时提供给 CLI、TUI 和 GUI 的跨界面用户命令领域面。它不是一个有状态 service 名；实现上由共享无状态 `CommandManager` 和 `SessionRuntime` 持有的 session-scoped `Command` 共同组成。
_避免_：UI adapter、快捷键系统、协议命令枚举、Agent loop、有状态 CommandSurfaceService

**AgentCommand**：
`agent_runtime_protocol::AgentCommand`，下游 adapter 提交给 `AgentRuntime` 的公开协议命令枚举，例如 `SubmitPrompt`、`ExecuteCommandText`、`ExecuteCatalogCommand`、`DecideToolApproval`、`ReloadResources`。它表达用户意图，不代表 command tree 节点，也不应包含高权限内部 mutation。
_避免_：command::Command、UI action payload、内部调试 API

**CommandManager**：
`WorkspaceServices` 持有的共享、无状态命令管理器。它持有只读 command packs、candidate provider registry 和 handler registry；每次调用时基于传入 `CommandContext` 临时 materialize command catalog，并执行 parse、suggest、`resolve_for_execution`。
_避免_：SessionRuntime 子系统、catalog cache、UI 菜单状态、执行工具或模型的 service

**Command（session-scoped）**：
`SessionRuntime` 持有的命令入口 facade。它不缓存 catalog，只负责从当前 session 组装 `CommandContext` 和 `SessionCommandHost`，再调用共享 `CommandManager`。
_避免_：agent_runtime_protocol::AgentCommand、全局命令服务、UI command palette

**CommandNode / CommandPath**：
命令树中的节点与路径。`CommandPath` 可以任意深度，例如 `/model thinking high`、`/model provider openai gpt-5`、`/skill code-review ...`。有限集合和动态候选应作为 command nodes，而不是被塞进普通 args。
_避免_：固定二级 subcommand、自由文本参数、UI 菜单项本身

**Command Manifest**：
多层 JSON command pack，使用 `nodeType`、`children`、`dynamic`、`provider`、`bind`、`nodeTemplate`、`args` 和 `handler` 描述命令树骨架与动态节点投影规则。它是命令定义格式，不是 UI schema，也不能读文件或执行代码。
_避免_：平铺 parent 配置、前端菜单配置、插件脚本

**Command Candidate Provider**：
把当前 session 的安全摘要投影成动态 command candidates 的可信 provider，例如 `resources.skills`、`resources.prompt_templates`、`models.current.thinking_levels`、`tools.available`。Provider 只能读取 `CommandContext` 中的 UI-safe view，不读取技能正文、模板正文、凭据或 raw provider payload。
_避免_：资源 loader、文件扫描器、UI autocomplete 函数、任意查询执行器

**命令文本**：
用户或 UI 提交给 `ExecuteCommandText` 的文本形式命令。`/...` slash command 是命令文本的一种常见语法；同一 command 也可以来自 `ExecuteCatalogCommand` 的结构化 catalog selection。
_避免_：普通用户 prompt、shell 命令、Agent 消息类型

**命令目录**：
运行时基于当前 session context 临时 materialize 给界面适配器的命令摘要集合，包含 command key、path、来源、说明、参数提示和当前可用性。它用于 autocomplete、command palette 和嵌套菜单，不等同于执行授权。
_避免_：命令执行器、工具注册表、资源目录、持久化 catalog

**命令解析 / resolve**：
`CommandManager` 将命令文本或 catalog selection 解析成 command node、dynamic bindings 和 args，并在执行前重新校验 command 是否仍存在、参数是否合法、phase/trust/capability 是否允许、handler binding 是否可信。
_避免_：UI 输入框逻辑、Agent loop、shell parser、一次性 planner

**命令输出**：
由用户命令产生的界面可见说明，例如 `/status` 摘要、`/usage` 统计、`/model` 设置完成提示或解析错误。它可以显示在消息面板中，但不是模型可见消息，也不能携带完整 `AgentCommand`。
_避免_：助手消息、工具结果、会话条目、UI action command

**命令交互请求**：
运行时要求界面展示候选或收集输入的 display-neutral 请求。具体如何渲染成 picker、菜单、modal 或表单由界面适配器决定；提交时必须回到 runtime 的 `ExecuteCatalogCommand`、`ExecuteCommandText` 或 runtime-tracked interaction，不得携带完整 mutation command。
_避免_：前端回调、UI 组件实例、运行时状态修改、Command payload

**消息面板项**：
界面消息面板中展示的一项内容，可以是用户消息、助手消息、工具活动、运行时通知或命令输出。并非所有消息面板项都会进入模型上下文。
_避免_：模型消息、会话条目、事件消息

**系统提示词构建器**：
纯构建能力，消费提示词素材、活跃工具集合、工具提示片段、当前日期和工作区路径，生成一次 Agent 运行使用的最终 system prompt。它由 `SessionRuntime` 调用，不调用 `ResourceManager`，也不读取文件或触发 reload。
_避免_：ResourceManager、会话历史、工具执行、资源生命周期 owner

**界面适配器**：
很薄的集成层，将 Ratatui、Tauri/Vue 或 CLI 这类具体界面技术翻译成 Agent 运行时命令与事件。它通常属于下游应用仓库；MiniCore 只定义协议和可复用 runtime 行为。
_避免_：重复后端、UI 专属 Agent

**Agent 运行**：
由 prompt、continuation、排队的 steering message、排队的 follow-up 或 retry 触发的一次 Agent loop 执行。一次运行可以包含多个模型回合和多个工具执行。
_避免_：响应、请求、chat completion

**当前运行状态（`CurrentRunState`）**：
`RuntimeSnapshot.active_session.current_run` 中描述当前 run 是否正在执行、等待审批或处于可恢复暂停的状态。它不是 run 终态；终态只通过 `run_finished { status: Completed | Failed | Aborted }` 表达。
_避免_：RunTerminalStatus、SessionPhase、工具调用状态

**可恢复暂停（`Suspended`）**：
当前 run 在协议安全 checkpoint 停住，并持有 `ResumeId` / resume state，后续可以继续同一个未完成 AgentRun / tool-result continuation。典型 checkpoint 包括 tool result 已产生但尚未回填 Rig、等待用户交互、external job pending、safe point 用户暂停或 host shutdown checkpoint。它不能表达为 `run_finished { status: paused }`。
_避免_：focus 切换、terminal finished、普通 waiting approval、模型 streaming 中途暂停

**Driver（Rig 适配器）**：
会话运行时中的 Rig 适配器，负责推进 Rig `AgentRun`，适配 `CallModel` / `CallTools` / `Done`，将底层流式项映射为运行时事件，并在 `CallTools` 时通过 `DriverHost::invoke_tool_batch(...)` 回到 `SessionRuntime`，由 session-scoped `Tools` 子系统执行工具治理。
_避免_：自定义 Agent loop、工具注册表、UI loop、Rig 高阶工具执行

**DriverHost**：
`Driver` 访问外部世界的 trait seam，定义 `call_model`、`invoke_tool_batch`、`before_next_model_call`、`before_run_finish` 等回调。它不是长期运行时对象，也不拥有 session 状态；它只是让无状态/浅状态的 `Driver` 回到 `SessionRuntime` 所拥有的模型、工具、队列和事件能力。
_避免_：Driver 实例、工具执行器、SessionRuntime 本体、全局服务

**SessionDriverHost**：
一次 `drive_run()` 期间临时创建的 `DriverHost` wrapper，借用 `SessionRuntime` 中本次 run 需要的一小片能力，例如 `Tools`、`ModelGateway`、event sink、queue state 和 `CurrentRun`，并携带从 `TurnState` clone 出来的 turn resources。直接 `impl DriverHost for SessionRuntime` 是合法简化版，但 wrapper 更能收窄访问面、隔离 run-scoped context，并避免 Rust 自借用压力。
_避免_：长期子系统、session 状态 owner、独立 runtime

**Tools 子系统 / 工具模块**：
`SessionRuntime` 内部的 session-scoped `tools.rs` / `tools/` 子系统，封装工具定义、注册表、活跃工具、工具提示素材、策略、审批、授权记忆、执行协调、沙箱、mutation lock 和 executor implementations。`SessionRuntime` 负责协调 `Driver` 与 `Tools`，`Driver` 不直接依赖 `Tools`。
_避免_：工具运行时、ToolRuntime、全局工具服务、UI 工具层、Rig ToolServer 替代品、平级 helper-only 模块

**工具定义**：
描述一个可被模型调用的工具能力，包括稳定名称、模型可见描述、参数结构、风险等级和展示元数据。
_避免_：工具函数、按钮动作


**工具注册表**：
`Tools` 子系统内部的工具目录，记录内置工具和自定义工具的定义、来源、风险和执行入口。
_避免_：Rig tools、UI 工具列表

**活跃工具集合**：
`Tools` 子系统内部维护、当前会话实际暴露给模型的工具子集。它影响模型请求中的工具 schema 和系统提示词中的工具说明。
_避免_：所有工具、工具开关 UI

**工具策略（`ToolPolicy`）**：
`Tools` 子系统内部的纯策略判断器。它根据工具定义、prepared invocation、工作区信任、沙箱结果、用户设置、grant 和 hook 结果决定允许、拒绝、要求审批、改写参数、强制串行或中止运行；它不等待 UI、不执行工具、不构造需要 I/O 的 preview。
_避免_：审批弹窗、工具执行器、UI 权限系统、preview builder

**工具审批代理（`ToolApprovalBroker`）**：
`Tools` 子系统内部的 pending approval 状态机。它保存等待用户确认的工具调用，冻结 prepared args，触发 `tool_call_approval_requested`，并等待 `agent_runtime_protocol::AgentCommand::DecideToolApproval`。
_避免_：UI 回调、策略判断器、长期授权存储、工具执行器

**待审批工具调用（`PendingToolApproval`）**：
`ToolApprovalBroker` 内部保存的当前等待用户批准或拒绝的工具调用。它包含 `ApprovalRequestId`、冻结的 prepared args 和 UI-safe 审批请求；只有 UI-safe 投影 `PendingToolApprovalView` 可以进入 `RuntimeSnapshot.active_session.current_run.pending_tool_approvals`，用于同一 host 生命周期内恢复审批界面和构造 `DecideToolApproval`。
_避免_：审批弹窗本身、工具执行结果、会话条目、可由 UI 修改的工具参数

**工具审批决定（`agent_runtime_protocol::ToolApprovalDecision`）**：
下游 UI 对某个 pending tool approval 的协议回答，可以 `ApproveOnce`、`ApproveGrant` 或 `Reject`；它不能替换工具参数，也不能直接执行工具。grant 只记录免批规则，不跳过 schema validation、hard deny、sandbox、mutation queue 或 audit。
_避免_：工具策略决定、工具参数、用户命令结果

**工具执行协调器（`ToolExecutionCoordinator`）**：
`Tools` 子系统内部的 batch 执行协调器。它执行已经声明的 parallel/sequential、approval wait、grant、并发限制和 mutation lock 约束，并按 LLM source `call_index` 稳定回填结果；它不根据工具名自行发明执行策略。
_避免_：独立 scheduler、LLM 策略解释器、工具执行器

**工具沙箱视图（`ToolSandboxView`）**：
`Tools` 子系统在 executor 前使用的安全边界 source of truth，描述 cwd、read roots、write roots、denied roots、process/network/env policy 和 sandbox verdict。UI approval 不能替代 sandbox；hook 改写参数后必须重新 schema validate、canonicalize、sandbox check 和 policy evaluate。
_避免_：审批弹窗、路径字符串前缀检查、executor 自行放宽权限

**工具执行器**：
`Tools` 子系统内部执行某个具体工具副作用的组件，例如读取文件、搜索文本、修改文件或运行命令。executor 不能绕过 registry、active tool check、policy、approval、sandbox 和 mutation lock。
_避免_：工具策略、工具注册、UI 执行器

**模型调用网关**：
运行时服务中负责真实模型调用的边界，处理模型选择解析、凭据注入、provider 请求、provider Hook、fallback、流式结果、usage 归一化和错误分类。`Driver` 只通过 `call_model` seam 请求它。
_避免_：Driver、模型客户端、系统提示词构建器、provider registry

**模型选择（`ModelSelection`）**：
一次会话或模型调用使用的稳定模型身份，由 `provider_id` 和 `model_id` 组成。它不等同于 provider API 中的真实模型名，也不包含凭据或 base URL。
_避免_：模型显示名、API model name、Rig 模型类型

**活跃模型（`ActiveModel`）**：
一次 turn 使用的模型选择及其安全摘要和能力快照。它可以进入 `TurnState`，但不能携带 provider client、raw payload 或凭据。
_避免_：ModelSummary、provider client、模型配置文件

**模型状态（`ModelState`）**：
会话运行时持有的当前模型选择、思考等级、流式选项和 fallback 偏好。它不持有 provider client、API key、OAuth token 或 Rig provider 类型。
_避免_：ProviderRegistry、ModelGateway、模型客户端

**模型提供方注册表（`ProviderRegistry`）**：
运行时服务中的 provider/model catalog，记录内置和自定义 provider、模型能力、API model name 映射和默认模型候选。它不读取凭据，也不执行模型调用。
_避免_：AuthStore、ModelGateway、provider client pool

**凭据存储（`AuthStore`）**：
运行时服务中的凭据解析边界，负责根据受控引用解析 API key、OAuth token 或运行时 override。它只向模型调用网关内部提供 secret material。
_避免_：ProviderRegistry、UI 设置对象、环境变量直读器

**模型调用请求（`ModelCallRequest`）**：
`Driver`、压缩流程或后台任务交给模型调用网关的 provider-neutral 请求，包含模型选择、消息、系统提示词、工具 schema、调用目的和流式选项。它不包含凭据、auth header、Rig provider 类型或 raw provider payload。
_避免_：provider HTTP request、Rig provider request、会话消息

**驱动安全点**：
`Driver` 在 Rig 状态机推进到某些边界时交还控制权给会话运行时的点，例如 `before_next_model_call` 和 `before_run_finish`。会话运行时可在此处理队列、patch turn state、暂停、重试或结束。
_避免_：prepare next turn、UI 回调、工具 Hook

**运行时命令**：
由 UI 发往 Agent 运行时的指令，例如提交 prompt、调用技能、中止、批准工具调用、切换模型或打开会话。
_避免_：UI 动作、后端 API 调用

**运行时事件**：
由 Agent 运行时发出、供界面适配器消费的完整事件记录。它包含顺序、路由、关联和事件消息，用于渲染消息、工具活动、队列状态、会话状态、错误和生命周期变化。
_避免_：回调、前端状态修改、会话日志

**事件消息（`agent_runtime_protocol::EventMsg`）**：
运行时事件中的业务事实部分，例如 run started、tool call output delta 或 persistence save point。外层 `agent_runtime_protocol::Event` 负责顺序、路由、关联和重连水位。
_避免_：chat message、UI 显示文案、前端状态对象

**事件族**：
按公开生命周期对象划分的一组事件，例如 session、run、message、tool call、resources、compaction、retry、persistence 和 diagnostics。事件族用于命名和 reducer 分发，不等同于内部 Rust module。
_避免_：文件名、模块名、UI 分组

**事件类型名**：
UI/wire 层识别事件种类的稳定名称，使用 flat `snake_case`，例如 `run_started`、`message_assistant_text_delta` 和 `tool_call_output_delta`。
_避免_：Rust enum variant、内部模块路径、显示文案

**事件生命周期**：
一个运行时动作从接收命令到进入终态期间应产生的事件顺序，例如开始、增量、完成、保存点和空闲通知。
_避免_：UI 动画流程、内部函数调用顺序

**事件状态机**：
描述某类运行时对象可处于哪些状态、哪些事件能推动状态转换，以及哪些状态是终态。它用于约束 UI reducer 和运行时测试。
_避免_：前端 store、流程图、内部实现步骤

**保存点（`SavePoint` / `persistence_save_point`）**：
运行时确认会话相关写入已经形成可恢复边界的事件。Rust 内部概念可叫 `SavePoint`，UI/wire 事件名使用 `persistence_save_point`。它说明界面此前看到的相关消息或配置变化已经可以从会话快照中恢复。
_避免_：自动保存按钮、文件系统 flush 细节

**模型提供方客户端**：
用于调用一个或多个模型提供方 API 的底层库。它可能支持流式输出和 function-call payload，但它本身不提供完整 Agent loop、本地编程工具、会话模型或权限边界。
_避免_：Agent SDK
