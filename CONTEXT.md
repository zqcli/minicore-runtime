# MiniCore Agent Runtime

本上下文描述 MiniCore：一个提供 Agent harness 能力的原生运行时核心。它把模型调用、会话、资源、工具、`CommandSurface`、事件和持久化编排收敛在 UI 无关的 runtime 中；后期 RuntimeHooks 作为内部扩展点接入；CLI、TUI 和 GUI 产品会在独立仓库中以 MiniCore 为核心接入。

## 语言

**MiniCore**：
面向桌面、终端和 CLI/GUI 宿主的轻量级原生 Agent harness runtime core。它承载模型调用、会话、资源、工具、`CommandSurface` 和运行时事件；具体 CLI、TUI、GUI 产品在下游仓库实现。
_避免_：旧项目代号、外部参考项目分支、WebView Agent SDK、下游产品仓库

**Harness 能力**：
围绕底层 Agent SDK 提供产品级编排的运行时能力，包括会话阶段、队列、资源加载、工具治理、审批、持久化、事件、上下文压缩、`CommandSurface` 和 UI 协议；后期包括 RuntimeHooks。它是 MiniCore runtime 的职责，不是一个独立的通用 `AgentHarness` 架构层。
_避免_：UI 应用、单纯模型客户端、Rig 高阶 runner、独立 AgentHarness 模块

**下游终端用户应用**：
基于 MiniCore 构建的 CLI、TUI 或 GUI 产品仓库，面向普通用户安装使用；用户不应被要求预装外部 Agent CLI、Node.js 或开发工具链。
_避免_：MiniCore core runtime、本地演示、测试 harness

**渲染器**：
运行在下游 GUI 宿主中的 Vue 与 TypeScript 用户界面，例如 Tauri 系统 WebView。它展示聊天状态，但不拥有 Agent 执行、本地项目访问或凭据。
_避免_：前端运行时、浏览器应用

**可信运行宿主**：
承载 MiniCore Agent 运行时的本地进程，拥有模型凭据、会话状态、本地项目访问和工具执行能力。它可以被下游 CLI/TUI 直接嵌入，也可以嵌入 Tauri 后端或作为原生 sidecar 打包。
_避免_：WebView SDK、前端 Agent

**AgentRuntimeProtocol**：
界面适配器与 Agent 运行时之间的稳定通信协议，由 `AgentCommand` / `CommandAck`、`RuntimeQuery` / `QueryResponse`、`Event` / `EventMsg` 和 `RuntimeSnapshot` 等协议类型组成。Command 表达 mutation 或异步工作，Query 表达只读 typed request/response，Event 表达运行变化，Snapshot 表达带事件水位的恢复读模型。
_避免_：运行时桥接、直接导入 SDK、UI 回调、command 子系统

**AgentRuntimeEvents**：
描述 `agent_runtime_protocol::Event` 的命名、顺序、所有权、重连和生命周期规则。它不是事件类型定义本身，也不是会话持久化日志。
_避免_：协议类型集合、会话日志、UI 状态管理

**工作区（Workspace）**：
一次 runtime 生命周期内打开的项目上下文容器，由 `OpenWorkspace { path }` 的 canonical root path 定义身份（`WorkspaceIdV1` 从该路径按版本化算法确定性派生，跨进程稳定）。它界定 session cwd 的合法域（root 及其之下）、充当持久化会话目录的分组维度，并投影 `WorkspaceSummary` 供 UI 展示。MVP 单实例、单根；`AgentRuntime` 是它的持有者，重复 `OpenWorkspace` 同根幂等、异根拒绝。它不承载 project trust（per-cwd）、项目资源 scope（per-cwd）、provider/auth/settings（user-global）或运行时服务生命周期。详见 [ADR 0022](docs/adr/0022-workspace-is-single-instance-thin-boundary.md)。
_避免_：VS Code 式多根容器、多 workspace 并存、trust 边界、资源 scope owner、UI 当前项目状态、隔离单元

**编程能力**：
助手通过 read、edit、write、search、list 或 shell execution 等工具检查或修改工作区的行为。
_避免_：通用聊天

**原生 Agent SDK**：
可以运行在可信运行宿主内的 Rust、Go、C++ 或类似原生框架；它拥有 Agent loop、工具调用、流式事件和状态推进能力，并且不要求 Node.js 运行时。
_避免_：模型客户端、API wrapper

**Agent 运行时（`AgentRuntime`）**：
与 UI 无关的 MiniCore 后端门面，供下游 CLI、TUI 和 GUI 宿主通过协议接入；它通过 `dispatch` 接收 mutation/异步工作，通过 `query` 返回只读业务数据，通过 `subscribe` 发布运行变化，通过 `snapshot` 生成恢复读模型，并管理工作区、会话目录和会话运行时。
_避免_：TUI 后端、桌面后端、UI 服务、GUI 应用状态

**RuntimeSnapshot**：
Agent 运行时在某个事件水位上的当前状态读模型，用于 adapter 初始化和同一 host 生命周期内的事件流重连/订阅重建。它在同一水位原子覆盖全部 loaded `SessionRuntime`，包括各自 current run 的 pending approvals；它不是 UI store、会话文件或持久化快照。打开工作区后默认 `loaded_sessions` 为空。MVP 不支持 adapter 失败但 runtime daemon 继续运行再被重连的独立生命周期模型。
_避免_：UI 状态、session index、JSONL、事件日志、持久化快照文件

**运行时服务**：
`AgentRuntime` 内部的后端依赖集合总称，不随任何客户端 selected session 改变。MVP 将服务保留在 `WorkspaceServices` 中；模型可见资源由 `ResourceManager` 管理级联不可变快照；会话和 run 捕获快照引用，而不是 pin 一整套 cwd 绑定服务。
_避免_：客户端当前选择的可变大包、每 session 服务容器、UI 服务

**WorkspaceServices**：
绑定到打开的 workspace / host 生命周期的运行时服务，例如 event bus、`SessionManager` / `SessionIndex`、共享无状态 `CommandManager`、`ResourceManager`、user-global settings/provider/auth、`ModelGateway` 和 runtime diagnostics 聚合。后期启用 hook system 时，`RuntimeHookRegistry` 也作为 workspace/runtime service 加入。它不随客户端 session selection 改变而重建。
_避免_：单会话运行态、selected-session 服务、UI 服务、有状态 CommandSurfaceService

**ResourceManager**：
运行时内部资源子系统，负责资源来源解析、project trust gate、source info、diagnostics、级联 snapshot、cwd-over-runtime overlay policy、cwd reload 和 turn resource capture。runtime 与 UI/`AgentRuntime` 生命周期绑定，`OpenWorkspace` 初始化一次 `RuntimeResourceSnapshot`；MVP 没有 runtime reload。`OpenSession/NewSession` 保证目标 cwd snapshot 存在；`start_user_turn -> capture_turn_resources` 在稳态只读取 current cwd pointer 并冻结 typed owner projections，产出 `TurnResourceSnapshot`。它不构造最终 system prompt，不执行技能调用，也不是公开协议的数据存储。
_避免_：UI 资源 store、系统提示词构建器、会话运行时、实时读文件入口、runtime reload 管理器

**ResourceSnapshotStore**：
`ResourceManager` 内部的 current-pointer 存储，保存 runtime 生命周期内初始化一次的 `RuntimeResourceSnapshot`，以及每个 `(workspace_id, cwd)` 的当前 `CwdResourceSnapshot`。MVP 中 `replace_cwd` 是唯一资源 current-pointer 替换线性化点：它只原子替换 cwd current pointer，不修改旧 snapshot；已经 running 的 run 继续使用启动时捕获的旧 snapshot，后续新显式 user turn / work chain 通过 `capture_turn_resources` 读取新 current cwd pointer。
_避免_：CwdServiceRegistry、CwdScopedServices、服务 generation registry、客户端 selected-session resources、runtime current-pointer 替换

**运行时资源快照（`RuntimeResourceSnapshot`）**：
一次 product/user-global defaults 解析结果，在 `OpenWorkspace` 初始化一次并与 `AgentRuntime` / UI host 生命周期绑定。它包含 builtin/user-global/runtime 级技能、提示模板、上下文默认值、自定义/追加系统提示词、source info 和 diagnostics。它不包含 cwd-local 项目资源，也不包含 provider/auth/model gateway 状态；MVP 不使用递增 runtime 级 revision。
_避免_：provider settings、凭据、cwd 项目资源、UI state、runtime reload revision

**cwd 资源快照（`CwdResourceSnapshot`）**：
某个 `(workspace_id, cwd)` 在一次成功资源加载/合成后的不可变资源集合。它持有构建时使用的 `Arc<RuntimeResourceSnapshot>`，并包含 cwd-local layer 与 overlay 后的 `resolved` effective view。多个 session 可以共享同一 cwd 的当前快照；不同 cwd 有各自快照。
_避免_：单纯 cwd 增量、全局当前资源、session 私有资源服务、UI 快照

**turn 资源快照（`TurnResourceSnapshot`）**：
一次 user turn / work chain 的稳定资源输入，由 `ResourceManager.capture_turn_resources(...)` 生成，包含 `session_id`、`user_turn_id`（不是 `RunId`）、`Arc<CwdResourceSnapshot>`、behavior、model、environment、policy 和 fingerprint。它进入 `TurnState` 后保证 automatic retry、overflow recovery、active Steer 和同 `RunId` segment rollover 复用同一 captured 输入；FollowUp、NextTurn 和新 idle prompt 会重新捕获。
_避免_：实时资源查询、mutable prompt context、RunId 等同 user turn、同时 pin runtime/cwd 的双来源快照

**资源覆盖策略（`ResourceOverlayPolicy`）**：
由 `ResourceManager` 集中实现的 deterministic overlay 规则。MVP 中 policy 可编码在代码内：runtime/global 资源提供候选，cwd/project 资源可按稳定 `ResourceKey { kind, namespace?, name }` 覆盖同 key 候选；selected 与 shadowed 都保留给 diagnostics。
_避免_：调用点临时合并、按文件路径碰撞、用户可随意声明 provider 覆盖

**资源更新边界**：
资源内容进入模型可见 current snapshot 的边界。MVP 中 runtime snapshot 只在 `OpenWorkspace` 初始化一次；cwd snapshot 由 `OpenSession/NewSession` ensure 或显式 `ReloadResources` 构建，并只通过 `replace_cwd` 发布。`resources_changed` 只是通知事件；`capture_turn_resources` 只是读取 current cwd pointer 并冻结传入 typed projections，不扫描文件、不自动 reload。
_避免_：文件保存即生效、hook 回调刷新、UI event 修改资源、turn 中途换资源、runtime reload

**会话运行时（`SessionRuntime`）**：
单个会话的产品级 Agent 编排对象；每个 loaded runtime 以 per-session actor 模式持有权威状态并持续处理 mailbox，管理会话阶段、当前运行、队列、模型状态、资源、工具、会话写入和 post-run arbitration。
_避免_：会话管理器、UI 会话状态

**会话运行时句柄（`SessionRuntimeHandle`）**：
上层联系某个 loaded `SessionRuntime` 的显式可克隆句柄，用于提交 session command、请求一致 snapshot/read projection 和发起 graceful shutdown；它不保存权威 session 状态，也不直接等待或执行 Agent run。
_避免_：`Arc<Mutex<SessionRuntime>>`、SessionRuntime 本体、RunTask handle、客户端 selected session

**产品级编排能力**：
围绕一次会话运行所需的阶段、队列、资源、工具、会话写入、中止和事件等职责。它是一组属于会话运行时的能力，不是独立模块。
_避免_：UI 后端、简单 wrapper

**运行时 Hook（`RuntimeHook`）**：
`RuntimeHooks` 模块规划的后期内部安全点干预能力，用于在 prompt/context、模型请求、provider payload、工具治理、压缩、session commit observer 和 UI-safe command result 等流程中返回 typed decision / patch / replacement。当前 MVP 不实现 hook system，也不定义资源 discovery/reload hook。Hook 影响最终会发生什么，但不直接发布 `agent_runtime_protocol::Event`，不读写 session storage，不执行工具，也不读取凭据。
_避免_：UI 回调、协议事件、插件系统、任意 runtime 后门、当前阶段必做模块

**Hook owner**：
拥有某个安全点业务不变量的模块。Hook owner 负责调用 hook、应用 typed result、重新校验并记录 diagnostics；`RuntimeHookRegistry` 只保存 handler，不拥有业务流程。`SessionRuntime` 拥有 run/prompt/context/queue/compaction/session commit observer 安全点，`Tools` 拥有工具治理安全点，`ModelGateway` 拥有 model/provider 边界安全点，`CommandManager` / `Command` 拥有 command catalog/resolve/output 安全点。
_避免_：Hook 注册表、Driver、UI adapter、任意调用方

**运行时 Hook 注册表（`RuntimeHookRegistry`）**：
后期保存 hook handler、source、capability、timeout、cancellation 和 failure policy 的内部运行时服务。它不拥有业务状态机，也不拥有 event metadata；hook owner 调用它，并应用 typed result。
_避免_：事件总线、插件管理器、工具执行器、会话存储

**会话阶段（`SessionPhase`）**：
会话运行时当前互斥工作状态，封闭为 `Idle`、`Turn`、`Compaction` 和 `RetryBackoff`。`Turn` 可以覆盖 prompt preflight、一个或多个连续 Agent runs、审批等待、可恢复暂停、持久化和 post-run arbitration；它不等于单个 `RunId`。`RetryBackoff` 只表示 Agent run 自动重试前的调度等待，不表示 provider retry 或 compaction 内部重试。
_避免_：CurrentRunState、branch summary、模型调用状态、loading 状态、按钮状态

**会话已稳定（`session_settled`）**：
会话处于 `Idle`，且没有 active run、compaction、retry、pending session action 或马上启动的 continuation；`NextTurn` queue 可以保留。它是 runtime 发布给 observer 的状态事实，不是 command、阻塞式 wait API 或“所有队列为空”的同义词。
_避免_：WaitForIdle、run finished、queue empty、同步屏障

**会话**：
一次工作上下文的持久记录，包含消息、模型变化、思考等级变化、活跃工具变化、压缩摘要、标签、名称和当前叶子位置。
_避免_：聊天记录、日志文件

**SessionIndex / 会话目录**：
`SessionManager` 维护的会话轻量清单或索引，用于 `/resume` 和 `SessionQuery::List`。它可以从 session 文件 header、metadata 或本地 index/cache 重建，包含 session id、workspace、名称、更新时间、预览和轻量统计；它不是 `RuntimeSnapshot`，也不包含完整消息、运行中状态或 UI 状态。
_避免_：RuntimeSnapshot、完整会话上下文、UI sidebar store、事件日志

**会话条目**：
会话中的一条不可变追加记录。条目通过 `id` 和 `parentId` 连接成树；当前类型包括 message、model_change、active_tools_change、thinking_level_change、compaction、branch_summary、label、session_info、custom、custom_message 和 usage，完整集合以 SessionManager 文档为准。current leaf 由 committed batch 的 `BatchLeafUpdate` 维护，不是独立 entry 类型。
_避免_：消息、数据库行

**会话树**：
由会话条目的父子关系形成的历史树。当前对话上下文由根到当前叶子的一条路径构建，而不是由文件中所有条目线性构建；可导航 leaf 必须是 committed stable batch boundary，不能落在多-entry `ToolRound` 内部。
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
当前会话投影到下一次模型请求时预计占用的上下文窗口大小。它用于上下文进度条、pre-run threshold gate 和压缩触发；每次模型调用的权威大小判断仍由最终 call projection validation 完成。它不等同于历史累计消耗。
_避免_：会话总 token、账单消耗、消息数量

**会话消耗统计**：
一个会话从创建以来累计的模型调用消耗视图。压缩不会降低会话累计消耗，只会降低后续上下文占用。
_避免_：当前上下文窗口、压缩摘要大小、UI 计数器

**会话管理器（`SessionManager`）**：
负责工作区内会话生命周期的运行时模块。它协调持久化会话目录、单会话句柄和已加载会话运行时，但不执行单个 Agent run。
_避免_：文件工具、会话管理 UI、Agent loop

**已加载会话运行时（`LoadedSessionRuntimes`）**：
会话管理器内部维护的 live runtime 表，记录当前已加载的 `SessionRuntime`。它是运行时对象索引，不是持久化会话目录；其中每个 `SessionRuntime` 独立持有 phase、queue、current run、pending approval 和固定 workspace cwd。它不保存客户端 selected/current session，多个 runtime 可以同时推进 work。
_避免_：会话存储、会话目录、独立会话运行时注册表、UI selection store

**会话存储（`SessionStorage`）**：
单个会话的底层 committed batch 存储，也是 `SessionWriter` 的 adapter；负责按 batch grouping 读取稳定会话条目、当前叶子和会话元数据，并隐藏 memory/JSONL 实现。它不决定 Agent 如何运行，也不直接服务 UI。
_避免_：会话管理器、会话运行时、聊天状态

**技能**：
可加载的 Markdown 指令包，包含稳定名称、简短描述、来源路径和正文。模型可见 selected skill 必须以 `SkillResource` 形式进入 resource snapshot，或被 snapshot pin 到不可变正文引用；显式调用时由目标 `PreparedMessageTurn` 从 captured `PromptResourceView` 读取正文并作为普通用户消息进入一次 Agent 运行。
_避免_：提示词片段、插件、工具、运行时按文件路径实时读取正文

**技能模块**：
与 `ResourceManager`、会话运行时平级的 `skills.rs` 模块，提供 `SkillMetadata` / `SkillResource` / `SkillCatalog` 数据结构，以及给定目录后的发现、校验、frontmatter 处理和格式化 helper。它不决定 skill roots，不拥有 reload/ensure/recompose 生命周期，不执行 overlay，也不构造会话消息。
_避免_：独立技能加载服务、UI 技能解析、工具注册、技能生命周期管理、资源 scope owner

**技能目录**：
技能资源集合的数据结构，可由 `skills.rs` 创建，但 current catalog 的生命周期由 `ResourceManager` 的 `RuntimeResourceSnapshot`、`CwdResourceSnapshot.local`、`CwdResourceSnapshot.resolved` 和 `TurnResourceSnapshot` 持有。对 selected skills，catalog 必须能提供稳定正文或不可变正文引用。
_避免_：独立生命周期 owner、命令列表、直接读当前磁盘文件

**技能调用**：
用户显式要求使用某个技能的结构化提示词意图。slash skill 先解析为 `PromptIntent`；目标 turn 的 `PreparedMessageTurn` 从 captured `PromptResourceView` 中读取 selected `SkillResource.body`，调用 `skills.rs` helper 格式化 `<skill>` 块，并通过 `compose_user_message(PromptIntent)` 展开为一条 `CanonicalUserMessage`。
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
`ResourceManager` 从 captured `TurnResourceSnapshot.cwd.resolved` 投影给 Prompt 的结构化资源输入，例如自定义系统提示词、追加系统提示词、上下文文件和技能目录摘要。它不是最终 system prompt，也不包含工具、agent profile、动态上下文或模型调用契约。
_避免_：完整 prompt、用户消息、工具描述、实时资源查询

**提示词意图（`PromptIntent`）**：
尚未展开成模型消息的结构化用户输入，可以引用普通文本、技能、提示模板或它们的组合。队列保存 intent 的稳定资源 key、参数和附件引用，不保存 raw slash command text 或提前展开的资源正文。
_避免_：用户消息、命令文本、已展开 prompt、PendingSessionAction

**Prompt 子系统**：
无状态的提示词组装深模块，是唯一模型可见上下文组装 seam。它负责从已捕获和已授权的 typed views 执行 `prepare_message_turn(...) -> PreparedMessageTurn`，通过 `PreparedMessageTurn.compose_user_message(PromptIntent) -> CanonicalUserMessage` 展开用户输入，并通过 `assemble_model_context(ModelContextProfile + ordered committed conversation + transient context + purpose) -> AssembledModelContext` 生成 provider-neutral 模型上下文。它不拥有资源生命周期、会话历史、队列、动态 context provider、工具执行、storage/provider handle 或模型调用。
_避免_：系统提示词构建器、PromptManager、ContextManager、ResourceManager

**已准备消息 turn（`PreparedMessageTurn`）**：
一次 user turn / work chain 使用的不可变提示词准备值，由 `Prompt.prepare_message_turn(...)` 基于 captured `TurnResourceSnapshot` 投影出的 `PromptResourceView`，以及同一 `TurnToolProfile` 中的 `ToolPromptView` 创建。它用于展开 resource-backed `PromptIntent` 为 `CanonicalUserMessage`，并暴露窄 `ModelContextProfile` 供后续模型上下文组装使用；active Steer 使用当前 `PreparedMessageTurn`，FollowUp、NextTurn 和 idle submission 在目标 future turn 创建新的 `PreparedMessageTurn`。Prompt 不接收 `TurnToolProfile` 中的 executor。
_避免_：TurnState、长期 PromptManager、资源快照 owner、会话上下文、模型调用请求

**turn 工具 profile（`TurnToolProfile`）**：
session-scoped `Tools` 在新的 user turn / work chain 边界通过 `capture_turn_tools()` 原子捕获的不可变组合，包含同一 `ToolProfileFingerprint` 下的模型可见 `prompt_view`、run-only `executor` 和 fingerprint。Prompt 只消费 `prompt_view`，执行路径只消费 `executor`；automatic retry、overflow recovery 和同一 work chain 复用同一 profile，不能分别调用 getter 拼装。
_避免_：工具运行快照、独立 prompt/invoker getter、ResourceManager-owned tool view

**模型上下文 profile（`ModelContextProfile`）**：
`PreparedMessageTurn` 暴露给模型上下文组装的窄 profile，把 system prompt、active tool schemas、`ToolProfileFingerprint`、贡献来源和 fingerprint 绑定在一起。`Prompt.assemble_model_context(...)` 使用它与有序 committed conversation、transient context 和 purpose 构造 `AssembledModelContext`；切换模型可见工具或相关 profile 时必须整体替换，并与 replacement executor fingerprint 相等，不能分别 patch system prompt、schemas 与执行 executor。
_避免_：DriverTurnInput、ModelCallRequest、ToolPromptCatalog、provider payload

**上下文素材（`ContextMaterial`）**：
由 RAG、memory、IDE、issue lookup 或后期 hook 等动态来源成功提供的 typed 模型上下文，带稳定来源、content hash、`CurrentRun | CurrentCall` 生命周期和 required/optional 要求。需要 durable 的内容必须先由其 owner 转换并提交为 canonical session message；项目文件、技能和提示模板不能绕过 `ResourceManager` 伪装成动态素材。
_避免_：资源快照、无来源字符串、会话历史、系统提示词

**上下文素材贡献（`ContextMaterialContribution`）**：
一次动态 context 获取的显式结果：`Available(ContextMaterial)` 或带 key/source/persistence/requirement/diagnostic 的 `Unavailable`。required source 失败必须保留为 `Unavailable` 并阻止模型调用；optional 失败进入 projection diagnostics。禁止通过 vector 缺项表达获取失败。
_避免_：Option<ContextMaterial>、静默跳过、provider future、原始 I/O error

**组装后的模型上下文（`AssembledModelContext`）**：
Prompt 在一次模型调用前生成的最终 provider-neutral 模型可见上下文现称 `AssembledModelContext`，包含同一 `ModelContextProfile` 的 system prompt 与 tools、协议安全 messages、output contract、贡献来源和 fingerprint。它由 `Prompt.assemble_model_context(...)` 从 ordered committed conversation、transient context 和 purpose 组装，是构造 `ModelCallRequest` 前的唯一组装结果。
_避免_：ModelCallRequest、provider payload、TurnState、session messages

**输出契约（`OutputContract`）**：
一次模型调用对输出结构的 provider-neutral 要求，例如 JSON schema、response format 或 required tool choice。它与 prompt text 一起进入 `AssembledModelContext`，但不能靠普通文本替代；真实 provider 映射由 `ModelGateway` 完成。
_避免_：system prompt、provider payload、显示格式、max output tokens

**CommandSurface**：
Agent 运行时提供给 CLI、TUI 和 GUI 的跨界面用户命令领域面。它不是一个有状态 service 名；实现上由共享无状态 `CommandManager` 和 `SessionRuntime` 持有的 session-scoped `Command` 共同组成。
_避免_：UI adapter、快捷键系统、协议命令枚举、Agent loop、有状态 CommandSurfaceService

**AgentCommand**：
`agent_runtime_protocol::AgentCommand`，下游 adapter 提交给 `AgentRuntime` 的公开协议命令枚举，例如 `SubmitPrompt`、`ExecuteCommandText`、`ExecuteCatalogCommand`、`DecideToolApproval`、`ReloadResources`。它表达用户意图，不代表 command tree 节点，也不应包含高权限内部 mutation。
_避免_：command::Command、UI action payload、内部调试 API

**运行时查询（`RuntimeQuery`）**：
下游 adapter 通过 `AgentRuntime.query(...)` 提交的只读 typed 查询总线，按 runtime、session、settings、resources、command surface、models、usage 和 diagnostics 领域分组。它不创建 turn、不启动 run、不消费 queue、不改变 revision，也不通过事件流广播结果。
_避免_：AgentCommand、CommandOutput、RuntimeSnapshot、UI 本地 selector、后台 job

**查询响应（`QueryResponse`）**：
`RuntimeQuery` 的直接 request/response 结果，包含 `as_of_sequence`、可选领域 revision 和 typed `QueryResult`。它不是业务事件，不分配 `CommandId`，transport request id 也不进入领域模型。
_避免_：CommandAck、EventMsg、RuntimeSnapshot、JSON-RPC envelope

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

**提示词交付方式（`PromptDelivery`）**：
模型可见输入相对当前 Agent 运行的交付位置：`Steer` 在最早可用的下一次模型调用前注入，`FollowUp` 等当前 work 完成后启动后续运行，`NextTurn` 等下一次显式用户 turn 时合并。它不决定 slash command handler 何时运行。
_避免_：DeliveryMode、InputSchedule、CommandRunPolicy、PendingSessionAction

**命令运行策略（`CommandRunPolicy`）**：
slash/catalog command 相对 active work 的执行策略：`Immediate` 立即执行，`IdleOnly` 在 active work 时拒绝，`QueueAfterRun` 在 idle 时立即执行、在 active work 时保存为结构化待执行会话动作。它不表示 steer、follow-up 或模型消息队列。
_避免_：CommandPhasePolicy、DeferUntilPostRun、PromptDelivery、通用任务调度器

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

**界面适配器**：
很薄的集成层，将 Ratatui、Tauri/Vue 或 CLI 这类具体界面技术翻译成 Agent 运行时命令与事件。它通常属于下游应用仓库；MiniCore 只定义协议和可复用 runtime 行为。
_避免_：重复后端、UI 专属 Agent

**编辑器草稿**：
界面适配器持有的尚未提交文本、光标、selection、输入历史和 undo state。runtime 只拥有已经受理的结构化 prompt intent 与 queue 状态，不保存原始 slash text，也不负责在 abort 后把 queue item 还原到编辑器。具体 UI 可以基于本地 submission history 提供 best-effort restore，但它不是 core protocol、snapshot 或跨重连保证。
_避免_：QueuedMessage、PromptIntent、committed user message、runtime event payload

**Agent 运行**：
由 prompt、continuation、排队的 steering message、排队的 follow-up 或 retry 触发的一次 Agent loop 执行。一次运行可以包含多个模型回合和多个工具执行。
_避免_：响应、请求、chat completion

**RunTask**：
`SessionRuntime` 为一次已经公开启动的 Agent 运行创建的短期执行任务，拥有 `Driver`、run-local usage/limits 和 cancellation；Driver 通常推进一个 Rig `AgentRun`，active Steer 时可在同一 `RunId` 下顺序推进多个 Rig segment。RunTask 不拥有 session phase、queues、writer、公共事件或 terminal arbitration。
_避免_：SessionRuntime actor、长期后台 session、RunId、DriverHost

**运行标识（`RunId`）**：
一次已经公开启动的 `Driver::drive_conversation()` / Agent loop 执行实例的 runtime-host-global opaque id。它在 runtime 准备创建 `CurrentRun`、调用 Driver 并发布 `run_started` 时分配，同一 run 内的模型调用、工具轮次、审批、suspend/resume、Steer 触发的 Rig segment rollover 和 terminal event 共享该 id；新的 retry、FollowUp 或其他 post-run continuation `drive_conversation()` 使用新 id。`RunId` 不绑定单个 Rig `AgentRun` 对象，也不是 prompt admission、`CommandId`、user turn、session revision 或跨进程 resume token，不能用于取消尚未公开启动的 work。
_避免_：命令 id、模型调用 id、工具调用 id、会话 entry id、持久化 revision

**待执行会话动作（`PendingSessionAction`）**：
`SessionRuntime` 接受 `CommandRunPolicy::QueueAfterRun` 后保存、并在当前 work 结束后的安全点执行的结构化会话操作，例如 running 时提交的 manual compact。它不是用户消息，不进入 steering/follow-up/next-turn message queue，也不进入模型上下文；UI-safe 投影通过 `QueueSnapshot.pending_actions` 暴露。
_避免_：AgentCommand payload、QueuedMessage、writer 内部 batch draft、CommandManager pending action

**当前运行状态（`CurrentRunState`）**：
`RuntimeSnapshot.loaded_sessions[*].current_run` 中描述对应 session 当前 run 是否正在执行、等待审批或处于可恢复暂停的状态。它不是 run 终态；终态只通过 `run_finished { status: Completed | Failed | Aborted }` 表达。
_避免_：RunTerminalStatus、SessionPhase、工具调用状态

**可恢复暂停（`Suspended`）**：
当前 run 在协议安全 checkpoint 停住，并持有 `ResumeId` / resume state，后续可以继续同一个未完成 AgentRun / tool-result continuation。典型 checkpoint 包括 tool result 已产生但尚未回填 Rig、等待用户交互、external job pending 或 safe point 用户暂停。MVP resume state 只在同一 host 生命周期内存活；host shutdown 后不恢复 running run。它不能表达为 `run_finished { status: paused }`。
_避免_：客户端 session selection、terminal finished、普通 waiting approval、模型 streaming 中途暂停

**TurnState**：
`SessionRuntime` 在一次新的显式 user turn / work chain 启动时构建的内部稳定快照，pin 住 `TurnResourceSnapshot`、`TurnToolProfile`、`PreparedMessageTurn`、模型状态和 context usage。它不保存 transcript；automatic retry、overflow recovery、active Steer 和同 `RunId` segment rollover 复用该状态中的 captured resources/tools/profile。它不跨过 `Driver` seam；给 `Driver` 的输入必须先投影成 `DriverTurnInput`，conversation 则独立来自 `ConversationSeed`。
_避免_：Driver 输入、ResourceManager current view、公开协议快照

**DriverTurnInput**：
`TurnState` 投影给 `Driver` 的窄输入，只包含 model selection、原子 `ModelContextProfile`、thinking level 和 stream options。它不能包含 `TurnResourceSnapshot`、resource revision、context usage、queue/storage 或工具治理状态；system prompt 与 active tool schemas 不能作为两个独立可 patch 字段跨过 seam。
_避免_：TurnState、ToolRunContext、ModelCallRequest、SessionRuntime 状态

**Driver（Rig 适配器）**：
会话运行时中的 Rig 适配器，负责推进一个或多个顺序 Rig `AgentRun` segment，适配 `CallModel` / `CallTools` / `Done`，将底层流式项映射为运行时事件，并在 `CallTools` 时通过 `DriverHost::execute_and_commit_tool_round(...)` 回到 `SessionRuntime`，由 session-scoped `Tools` 子系统执行工具治理与 stable commit。Steer rollover 是 Driver 私有实现，不改变公开 `RunId`。
_避免_：自定义 Agent loop、工具注册表、UI loop、Rig 高阶工具执行、TurnState owner

**DriverHost**：
`Driver` 访问外部世界的 trait seam，定义 `generate_model_turn`、`execute_and_commit_tool_round`、`commit_pending_messages`、`before_run_finish` 等回调。它不是长期运行时对象，也不拥有 session 状态；它只是让无状态/浅状态的 `Driver` 回到 `SessionRuntime` 所拥有的模型、工具、队列和事件能力。
_避免_：Driver 实例、工具执行器、SessionRuntime 本体、全局服务

**SessionDriverHost**：
一次 `drive_conversation()` 期间由 `RunTask` 持有的 owned `DriverHost` wrapper，保存 run identity、turn resources、`ModelGateway` / `Tools` handle、cancellation 和回到 owner actor 的窄联系 seam；它不借用 `SessionRuntime` 的 mutable state。
_避免_：长期子系统、session 状态 owner、独立 runtime、DriverTurnInput

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
`Tools` 子系统内部的纯策略判断器。它根据工具定义、prepared invocation、工作区信任、沙箱结果、用户设置和 grant 决定允许、拒绝、要求审批、改写参数、强制串行或中止运行；后期启用 hook system 时可以叠加 hook 结果。它不等待 UI、不执行工具、不构造需要 I/O 的 preview。
_避免_：审批弹窗、工具执行器、UI 权限系统、preview builder

**工具审批代理（`ToolApprovalBroker`）**：
`Tools` 子系统内部的 pending approval 状态机。它保存等待用户确认的工具调用，冻结 prepared args，触发 `tool_call_approval_requested`，并等待 `agent_runtime_protocol::AgentCommand::DecideToolApproval`。
_避免_：UI 回调、策略判断器、长期授权存储、工具执行器

**待审批工具调用（`PendingToolApproval`）**：
`ToolApprovalBroker` 内部保存的当前等待用户批准或拒绝的工具调用。它包含 `ApprovalRequestId`、冻结的 prepared args 和 UI-safe 审批请求；只有 UI-safe 投影 `PendingToolApprovalView` 可以进入对应 `RuntimeSnapshot.loaded_sessions[*].current_run.pending_tool_approvals`，用于同一 host 生命周期内恢复审批 adapter 状态和构造 `DecideToolApproval`。
_避免_：审批弹窗本身、工具执行结果、会话条目、可由 UI 修改的工具参数

**工具审批决定（`agent_runtime_protocol::ToolApprovalDecision`）**：
下游 UI 对某个 pending tool approval 的协议回答，可以 `ApproveOnce`、`ApproveGrant` 或 `Reject`；它不能替换工具参数，也不能直接执行工具。grant 只记录免批规则，不跳过 schema validation、hard deny、sandbox、mutation queue 或 audit。
_避免_：工具策略决定、工具参数、用户命令结果

**工具执行协调器（`ToolExecutionCoordinator`）**：
`Tools` 子系统内部的 batch 执行协调器。它执行已经声明的 parallel/sequential、approval wait、grant、并发限制和 mutation lock 约束，并按 LLM source `call_index` 稳定回填结果；它不根据工具名自行发明执行策略。
_避免_：独立 scheduler、LLM 策略解释器、工具执行器

**工具沙箱视图（`ToolSandboxView`）**：
`Tools` 子系统在 executor 前使用的路径授权与执行约束 source of truth，描述 cwd、read/write/denied roots、shell/network/env policy 和 effective enforcement capability。MVP 只承诺 MiniCore 内置工具的进程内路径边界，并保持通用 shell / `bash` disabled；通用子进程请求的 filesystem/network 限制只有在 OS-native/external backend capabilities 满足时才能标记为 enforced，否则 fail closed。UI approval 不能替代 enforcement；`FullAccessWithApproval` 明确没有 sandbox guarantee。后期 hook 改写参数后必须重新 schema validate、canonicalize、sandbox check 和 policy evaluate。
_避免_：审批弹窗、路径字符串前缀检查、把 best-effort/full-access 称为 sandbox、executor 自行放宽权限

**工具执行器**：
`Tools` 子系统内部执行某个具体工具副作用的组件，例如读取文件、搜索文本、修改文件或运行命令。executor 不能绕过 registry、active tool check、policy、approval、sandbox 和 mutation lock。
_避免_：工具策略、工具注册、UI 执行器

**模型调用网关**：
运行时服务中负责真实模型调用的边界，处理模型选择解析、凭据注入、provider 请求、fallback、流式结果、usage 归一化和错误分类。后期 provider Hook 的 owner 也在该边界内。`Driver` 只通过 `generate_model_turn` seam 请求它；真实 driver 集成前必须先有最小稳定 spine。
_避免_：Driver、模型客户端、系统提示词构建器、provider registry、临时 provider 路径

**ModelGateway spine**：
真实 driver 集成前必须稳定的最小模型调用骨架，包括 `ModelCallPurpose`、`ModelCallRequest` / `ModelCallResult` / `ModelCallErrorKind` / `ModelCallUsage`、`ModelGateway.generate_model_turn(...)`、最小 `ProviderRegistry.resolve(...)` 和 `AuthStore.resolve(...)`。它不是完整 custom provider、fallback 或 usage/context usage 实现。
_避免_：临时 gateway、Driver 直接调用 provider

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

**模型调用目的（`ModelCallPurpose`）**：
一次模型调用的稳定业务意图，例如 `AgentRun` 或 `CompactionSummary`。它从 `ModelCallRequest` 原样传播到 `ModelCallUsage` 和 future `SessionEntry::Usage`；retry/fallback、客户端是否选中该 session、调用是否在后台执行都不是 purpose。
_避免_：`UsagePurpose`、Retry、Background、provider attempt status、调度状态

**模型调用请求（`ModelCallRequest`）**：
`Driver` 或 `SessionRuntime` 交给模型调用网关的唯一 provider-neutral 请求，包含模型选择、唯一 `input: AssembledModelContext`、thinking level、输出限制、调用目的和流式选项。system prompt、`ModelMessage[]`、工具 schema 和 output contract 只存在于 assembled input 中，调用方不能再拆成平行字段；请求不包含凭据、auth header、Rig provider 类型或 raw provider payload。
_避免_：provider HTTP request、Rig provider request、会话消息、SummaryModelRequest

**压缩摘要指令（`CompactionSummaryDirective`）**：
`Compaction` 根据压缩准备结果生成的摘要目标消息、typed instruction 和最大输出 token 预算。它不是 system prompt、模型调用请求或最终模型上下文，不包含模型选择、thinking/stream policy、call/run id 或工具 schema；`SessionRuntime` 把它连同 active/standalone `ModelContextProfile` 交给 `Prompt.assemble_model_context(..., purpose = CompactionSummary)`。
_避免_：SummaryModelRequest、ModelCallRequest、AssembledModelContext、系统提示词状态

**压缩方法（`CompactionMethod`）**：
一次压缩的 provider-neutral 执行方式。MVP 使用 `SummaryModel`；后期可由模型 capability 提供 `ProviderNative` 专用端点，或使用有界 deterministic reduction fallback。用户配置表达 preference，`SessionRuntime` 按当前模型 capability 和 trigger 解析计划，不按模型名称硬编码。
_避免_：GPT 压缩、Claude 压缩、provider endpoint 名、UI handler

**上下文限制错误（`DriverError::ContextLimitExceeded`）**：
当前 Agent run 的下一次模型调用无法进入有效上下文窗口的 typed recovery class。`PromptAssembly` 表示本地最终上下文组装拒绝、未调用 provider；`Provider` 表示 provider 返回 context overflow。两者由 `SessionRuntime` 在当前 run failed terminal 后共享一次 work-chain compaction recovery，但保留 diagnostics 和 usage 来源差异。
_避免_：普通 retry error、Prompt admission rejection、模型可见 assistant message

**驱动安全点**：
`Driver` 在 Rig 状态机推进到某些边界时交还控制权给会话运行时的点。`commit_pending_messages` 位于当前 assistant response 及其完整工具批次结束后、下一次 LLM 调用前；`before_run_finish` 位于 Rig segment 原本将结束时。会话运行时可在此提交 Steer、替换完整 profile、暂停、重试或结束；FollowUp 在公开 run 结束后的 post-run arbitration 中启动新 run。
_避免_：prepare next turn、UI 回调、工具 Hook

**运行时命令**：
由 UI 发往 Agent 运行时的指令，例如提交 prompt、调用技能、中止、批准工具调用、切换模型或打开会话。
_避免_：UI 动作、后端 API 调用

**运行时事件**：
由 Agent 运行时发出、供界面适配器消费的完整事件记录。它包含顺序、路由、关联和事件消息，用于渲染消息、工具活动、队列状态、会话状态、错误和生命周期变化。
_避免_：回调、前端状态修改、会话日志

**事件消息（`agent_runtime_protocol::EventMsg`）**：
运行时事件中的业务事实部分，例如 run started、tool call output delta 或 message tool result appended。外层 `agent_runtime_protocol::Event` 是 workspace/session/run/command 坐标以及顺序、关联和重连水位的唯一权威位置；`EventMsg` 不重复这些通用坐标，只保留 message/call/interaction 等局部对象 identity、transition operands 和业务数据。裸 `EventMsg` 不是可独立路由的完整事件。
_避免_：chat message、UI 显示文案、前端状态对象

**事件族**：
按公开生命周期对象划分的一组事件，例如 session、run、message、tool call、resources、compaction、retry 和 diagnostics。事件族用于命名和 reducer 分发，不等同于内部 Rust module。
_避免_：文件名、模块名、UI 分组

**事件类型名**：
UI/wire 层识别事件种类的稳定名称，使用 flat `snake_case`，例如 `run_started`、`message_assistant_text_delta` 和 `tool_call_output_delta`。
_避免_：Rust enum variant、内部模块路径、显示文案

**事件生命周期**：
一个运行时动作从接收命令到进入终态期间应产生的事件顺序，例如开始、增量、完成、提交后的领域事实和空闲通知。
_避免_：UI 动画流程、内部函数调用顺序

**事件状态机**：
描述某类运行时对象可处于哪些状态、哪些事件能推动状态转换，以及哪些状态是终态。它用于约束 UI reducer 和运行时测试。
_避免_：前端 store、流程图、内部实现步骤

**会话写入器（`SessionWriter`）**：
所有会话 mutation 共用的可信写入 seam。它只接受协议完整、可以独立恢复的 `SessionWriteBatch`；`commit()` 成功返回表示整个 batch 已按 storage adapter 契约写入，失败则该 batch 不得进入恢复投影。公共协议不暴露单独的 save-point event。
_避免_：事件发布器、逐条 append helper、UI 保存状态、通用数据库事务管理器

**会话写入批次（`SessionWriteBatch`）**：
一次原子提交的稳定 session entries，例如 user input、完整 tool-call/result round、最终 assistant message、compaction、独立 session mutation 或 tree mutation。streaming delta、partial assistant、pending approval 和执行中的 tool round 不属于 write batch。
_避免_：CurrentRun snapshot、事件批次、模型请求 batch、SessionRevision

**模型提供方客户端**：
用于调用一个或多个模型提供方 API 的底层库。它可能支持流式输出和 function-call payload，但它本身不提供完整 Agent loop、本地编程工具、会话模型或权限边界。
_避免_：Agent SDK
