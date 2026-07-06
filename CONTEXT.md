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
界面适配器与 Agent 运行时之间的稳定通信协议，由 `agent_runtime_protocol::Command`、`Event`、`EventMsg`、`Snapshot` 和 `CommandAck` 等协议类型组成。
_避免_：运行时桥接、直接导入 SDK、UI 回调

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
与 UI 无关的 MiniCore 后端门面，供下游 CLI、TUI 和 GUI 宿主通过协议接入；它接收运行时命令、发布运行时事件、生成快照，并管理工作区、会话目录和会话运行时。
_避免_：TUI 后端、桌面后端、UI 服务、GUI 应用状态

**运行时服务**：
绑定到有效工作区的后端依赖集合，例如凭据、设置、模型注册表、模型调用网关、资源加载器和会话管理器。切换到不同工作区的会话时需要重新创建。
_避免_：全局单例、UI 服务

**会话替换**：
用一个新的会话运行时替换当前会话运行时的过程，包括关闭旧会话、释放订阅、重建运行时服务、打开新会话并通知界面适配器重新绑定。
_避免_：页面切换、聊天切换

**会话运行时（`SessionRuntime`）**：
单个会话的产品级 Agent 编排对象，参考 pi coding-agent 的 `AgentSession`；它管理会话阶段、当前运行、中止、等待空闲、队列、模型状态、资源、工具、工具策略、会话写入和 `Driver`。
_避免_：会话管理器、UI 会话状态

**产品级编排能力**：
从 pi-agent-core 的通用编排对象和 pi coding-agent `AgentSession` 中抽取出的职责，例如阶段、队列、资源、工具、会话写入、中止和事件。它是一组属于会话运行时的能力，不是独立模块。
_避免_：UI 后端、简单 wrapper

**运行时 Hook（`RuntimeHook`）**：
`RuntimeHooks` 模块管理的内部安全点干预能力，用于在资源发现、prompt/context、模型请求、provider payload、工具治理、压缩、保存点和命令呈现等流程中返回 typed decision / patch / replacement。Hook 影响最终会发生什么，但不直接发布 `agent_runtime_protocol::Event`，不读写 session storage，不执行工具，也不读取凭据。
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
会话管理器内部维护的运行中会话表，记录当前已加载的 `SessionRuntime` 和聚焦会话。它是运行时对象索引，不是持久化会话目录。
_避免_：会话存储、会话目录、独立会话运行时注册表

**聚焦会话（focused session）**：
当前 UI 或默认命令目标指向的会话。它可以是多个已加载会话中的一个，不等同于正在运行的会话。
_避免_：active session、running session、loaded session

**会话存储（`SessionStorage`）**：
单个会话的底层条目存储，负责保存追加式会话条目、当前叶子和会话元数据。它不决定 Agent 如何运行，也不直接服务 UI。
_避免_：会话管理器、会话运行时、聊天状态

**技能**：
可加载的 Markdown 指令包，包含稳定名称、简短描述、来源路径和可按需读取的正文。技能可以出现在模型可见摘要列表中，也可以由用户显式调用并作为普通用户消息进入一次 Agent 运行。
_避免_：提示词片段、插件、工具

**技能模块**：
与资源加载器、会话运行时平级的 `skills.rs` 模块，提供技能 metadata、技能目录数据结构、发现、校验、frontmatter 处理和格式化 helper。它不拥有资源刷新生命周期，也不构造会话消息。
_避免_：独立技能加载服务、UI 技能解析、工具注册、技能生命周期管理

**技能目录**：
当前运行时已经加载的技能元数据集合，包含技能摘要、来源、诊断和名称碰撞结果；默认不包含所有技能正文。
_避免_：技能全文缓存、命令列表

**技能调用**：
用户显式要求使用某个技能的行为。会话运行时读取技能正文，把它格式化为 `<skill>` 块，并将它作为普通用户消息交给 Agent 运行。
_避免_：系统提示词注入、工具调用

**运行时资源**：
由运行时统一管理、可影响未来 Agent 运行的模型资源，包括技能目录、提示模板、上下文文件、自定义系统提示词和追加系统提示词。
_避免_：UI 文件、会话消息、工具结果

**资源快照**：
资源加载器在一次成功刷新后持有的当前运行时资源集合，带有 revision、来源信息、诊断和必要正文。它是运行时内部状态，不是 UI 快照，也不是会话历史。
_避免_：界面快照、事件日志、会话条目

**资源摘要**：
资源快照投影给界面适配器的安全视图，只包含展示、来源、revision 和诊断等信息，不包含技能正文、上下文文件正文或完整系统提示词。
_避免_：资源正文、提示词素材、完整运行时资源

**提示词素材**：
资源加载器提供给系统提示词构建器的输入素材，例如自定义系统提示词、追加系统提示词、上下文文件和技能目录摘要。它不是最终系统提示词。
_避免_：完整 prompt、用户消息、工具描述

**资源加载器**：
Agent 运行时中的内部服务，负责运行时资源的来源聚合、信任校验、加载刷新、资源快照、提示词素材和诊断信息。它持有技能目录生命周期，但调用平级技能模块完成技能文件处理。
_避免_：文件扫描脚本、UI 资源管理器、系统提示词构建器、协议类型集合

**CommandSurface**：
Agent 运行时提供给 CLI、TUI 和 GUI 的跨界面用户命令面，负责命令目录、`/...` 文本解析、执行规划、运行时命令映射和命令呈现语义。它不是 UI 输入框，也不是 `agent_runtime_protocol::Command` enum 本身。
_避免_：UI adapter、快捷键系统、协议命令枚举、Agent loop

**斜杠命令**：
用户以 `/` 开头输入的运行时命令入口，例如 `/compact`、`/reload`、`/skill:name` 或 `/{template}`。它是命令入口语法，不是 Agent 消息类型，也不是工具调用。
_避免_：聊天消息、工具命令、UI 快捷键

**斜杠命令目录**：
运行时提供给界面适配器的命令摘要集合，包含命令名、来源、说明、参数提示和当前可用性。它用于 autocomplete 和 command palette，不等同于执行授权。
_避免_：命令执行器、工具注册表、资源目录

**斜杠命令解析器**：
把 `/...` 文本解析成已有运行时命令、技能调用、提示模板调用或受控查询的运行时能力。解析器必须遵守会话阶段、资源 revision、信任和权限边界。
_避免_：UI 输入框逻辑、Agent loop、shell parser

**斜杠命令执行计划**：
斜杠命令解析后形成的意图归类，说明这次命令应进入运行时命令、受控查询、模型可见输入、界面交互请求还是用户可见错误。
_避免_：执行结果、UI 组件、Agent run

**命令呈现**：
运行时把用户命令的结果或下一步交互需要表达成界面可渲染语义的过程。它可以产生命令输出、选择器、弹窗、菜单或表单，但不等同于后端业务事实本身。
_避免_：CommandAck、业务事件、前端组件实现

**命令输出**：
由用户命令产生的界面可见说明，例如 `/status` 摘要、`/usage` 统计、`/model` 设置完成提示或解析错误。它可以显示在消息面板中，但不是模型可见消息。
_避免_：助手消息、工具结果、会话条目

**命令交互请求**：
运行时要求界面展示一个交互控件的语义请求，例如模型选择器、思考等级选择器、会话选择器、弹窗、菜单或表单。具体如何渲染由界面适配器决定。
_避免_：前端回调、UI 组件实例、运行时状态修改

**消息面板项**：
界面消息面板中展示的一项内容，可以是用户消息、助手消息、工具活动、运行时通知或命令输出。并非所有消息面板项都会进入模型上下文。
_避免_：模型消息、会话条目、事件消息

**系统提示词构建器**：
纯构建能力，消费提示词素材、活跃工具集合、工具提示片段、当前日期和工作区路径，生成一次 Agent 运行使用的最终系统提示词。
_避免_：资源加载器、会话历史、工具执行

**界面适配器**：
很薄的集成层，将 Ratatui、Tauri/Vue 或 CLI 这类具体界面技术翻译成 Agent 运行时命令与事件。它通常属于下游应用仓库；MiniCore 只定义协议和可复用 runtime 行为。
_避免_：重复后端、UI 专属 Agent

**Agent 运行**：
由 prompt、continuation、排队的 steering message、排队的 follow-up 或 retry 触发的一次 Agent loop 执行。一次运行可以包含多个模型回合和多个工具执行。
_避免_：响应、请求、chat completion

**Driver（Rig 适配器）**：
会话运行时中的 Rig 适配器，负责推进 Rig `AgentRun`，适配 `CallModel` / `CallTools` / `Done`，将底层流式项映射为运行时事件，并在 `CallTools` 时调用会话运行时内部的工具网关。
_避免_：自定义 Agent loop、工具注册表、UI loop、Rig 高阶工具执行

**工具模块**：
与技能模块、资源加载器、系统提示词构建器平级的 `tools.rs` / `tools/` 模块，提供工具定义、内置工具、外部工具适配、schema、prompt metadata 和 executor helper。它不拥有会话工具状态。
_避免_：工具运行时、UI 工具层、Rig ToolServer 替代品

**工具定义**：
描述一个可被模型调用的工具能力，包括稳定名称、模型可见描述、参数结构、风险等级和展示元数据。
_避免_：工具函数、按钮动作

**工具注册表**：
会话运行时维护的工具目录，记录内置工具和自定义工具的定义、来源、风险和执行入口。
_避免_：Rig tools、UI 工具列表

**活跃工具集合**：
当前会话实际暴露给模型的工具子集。它影响模型请求中的工具 schema 和系统提示词中的工具说明。
_避免_：所有工具、工具开关 UI

**工具策略（`ToolPolicy`）**：
工具模块中的策略判断器，由会话运行时持有实例。它根据工具定义、参数、工作区信任、沙箱结果、用户设置和 hook 结果决定允许、拒绝、要求审批、改写参数或中止运行。
_避免_：审批弹窗、工具执行器、UI 权限系统

**工具审批代理（`ToolApprovalBroker`）**：
工具模块中的 pending approval 状态机，由会话运行时持有实例。它保存等待用户确认的工具调用，触发 `tool_call_approval_requested`，并等待 `agent_runtime_protocol::Command::DecideToolApproval`。
_避免_：UI 回调、策略判断器、工具执行器

**工具审批决定（`agent_runtime_protocol::ToolApprovalDecision`）**：
下游 UI 对某个 pending tool approval 的协议回答，只能批准或拒绝，不能替换工具参数，也不能直接执行工具。
_避免_：工具策略决定、工具参数、用户命令结果

**工具网关**：
会话运行时内部的工具执行门面，由 `Driver` 在 Rig `CallTools` step 中调用。它消费工具注册表、活跃工具集合、工具策略、审批、Hook 和工具执行器，并把结果归一化后交回 `Driver`。
_避免_：工具实现、Rig tool call、独立运行时

**工具执行器**：
执行某个具体工具副作用的组件，例如读取文件、搜索文本、修改文件或运行命令。
_避免_：工具策略、工具注册

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
