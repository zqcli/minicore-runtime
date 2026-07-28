# MiniCore Agent Runtime

本上下文描述 MiniCore：一个提供 Agent harness 能力的原生运行时核心。它把模型调用、会话、资源、工具、`CommandSurface`、事件和持久化编排收敛在 UI 无关的 runtime 中；后期 RuntimeHooks 作为内部扩展点接入；CLI、TUI 和 GUI 产品会在独立仓库中以 MiniCore 为核心接入。

> **权威顺序**：当前架构文档（`docs/architecture.md` 与 `docs/modules/`）→ 当前 ADR（`docs/adr/`，0100+）→ `docs/research/` → `docs/archive/v1/`（非权威，仅历史参考）。本术语表中标注为「目标架构」的条目即当前V2权威架构（设计已冻结，生产实现待启动）；标注为「pre-refactor term」的名称属于已归档的V1，仅用于说明术语演变，不得作为当前实现依据。

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

**Runtime Interface**：
界面适配器与`MiniCoreRuntime`之间的稳定通信interface，由`RuntimeCommand`/`CommandResponse`、`RuntimeQuery`/`QueryResponse`、`RuntimeSnapshot`/`SessionSnapshot`和`StateEvent`/`ProgressEvent`组成。Command表达mutation或work admission，Query表达只读typed request/response，Snapshot表达scope-local恢复读模型，StateEvent在当前subscription内按序交付，ProgressEvent为best-effort。
_避免_：运行时桥接、直接导入SDK、UI回调、内部Service interface

**Runtime Events**：
Runtime通知host的observer协议。subscribe第一帧是scope完整Snapshot，随后发送当前连接内的`StateEvent`和可合并/丢弃的`ProgressEvent`；断线、背压或restart后重新subscribe，不重放旧事件。它不是SessionStorage ledger或模型conversation。
_避免_：会话日志、UI store、provider stream原样透传、可恢复event log

**工作区（pre-refactor protocol term）**：
旧协议把Workspace建模为Runtime-global open/close entity并定义WorkspaceId。目标架构已删除该ownership；当前`Workspace`只表示`SessionDefinition.workspace`中的Session-owned definition value。
_避免_：OpenWorkspace、WorkspaceId、Runtime-global Workspace registry

**编程能力**：
助手通过 read、edit、write、search、list 或 shell execution 等工具检查或修改工作区的行为。
_避免_：通用聊天

**原生 Agent SDK**：
可以运行在可信运行宿主内的 Rust、Go、C++ 或类似原生框架；它拥有 Agent loop、工具调用、流式事件和状态推进能力，并且不要求 Node.js 运行时。
_避免_：模型客户端、API wrapper

**MiniCore Runtime（`MiniCoreRuntime`）**：
与UI无关的唯一顶层门面，供下游CLI、TUI和GUI可信宿主通过`dispatch / query / snapshot / subscribe`接入。它管理Agent/Session durable owner、loaded SessionExecutor路由和Runtime-owned共享模块，不保存UI selected Session。
_避免_：TUI后端、桌面后端、UI Service、GUI应用状态、全局current Session

**RuntimeSnapshot**：
Runtime scope的完整恢复读模型，包含Runtime信息、Agent summary、loaded Session membership和runtime diagnostics。它不包含全部loaded Session的完整message/current Turn/Pending Interaction，也不要求所有SessionExecutor同时park；shared resource reload后host通过新Snapshot或safe catalog query读取current values，不依赖额外catalog版本值。
_避免_：SessionSnapshot、UI store、Session index、JSONL、全局事件水位

**SessionSnapshot**：
一个loaded Session的live observer baseline，通过对应SessionExecutionHandle的immutable published view取得，包含lifecycle、definition summary、readiness、current Turn、按selected path entry顺序与assistant Reasoning/Text/ToolCall content顺序排列的active Items、Pending Interaction、queues和usage。它不是durable execution checkpoint；process restart先从JSONL replay并保守关闭unfinished Turn。长期Turn历史通过Query按需读取。
_避免_：RuntimeSnapshot、完整JSONL replay、durable storage snapshot

**Snapshot-first subscription**：
Runtime或单个Session的实时观察方式。owner原子完成subscriber注册和初始Snapshot capture，EventStream第一帧返回Snapshot，随后只发送实时事件；断线或背压后重新订阅并获取新Snapshot，不提供公开cursor或事件重放。process restart则先完成JSONL replay/conservative recovery，再建立新的snapshot-first stream。
_避免_：先snapshot再subscribe的非原子组合、durable observer log、跨restart offset

**运行时共享模块**：
`MiniCoreRuntime`拥有的PromptService、ToolService、SkillService和ModelGateway。四个Service各自build/validate immutable candidate，Runtime用private `SharedResourceRoots`原子持有四个current Arc；Service不单独publish current pointer。它们不随UI selected Session改变，也不保存current Session或current Turn。
_避免_：每Session复制服务、UI服务容器、全局current Session

**Workspace**：
`SessionDefinition.workspace`中的单实例definition value，描述root、访问规则和配置。它没有WorkspaceId、registry或open/close lifecycle；loaded Session通过WorkspaceResolver得到WorkspaceSnapshot。
_避免_：全局WorkspaceManager、cwd字符串、UI project object

**WorkspaceSnapshot**：
SessionExecutor在Turn admission期间取得的不可变Workspace解析结果，包含canonical roots、effective access和source grants。TurnExecutionContext持有同一个`Arc<WorkspaceSnapshot>`直到terminal；loaded Session的Workspace definition update只在Idle时接受，active Turn完全不读取future Workspace definition或重新拼接Workspace view。
_避免_：实时路径查询、mutable cwd state、可撤销authorization lease、跨Turn共享mutable Snapshot

**SecurityRevoked**：
WorkspaceAuthority或host发布hard restriction后，通过Runtime current loaded map发送给对应`SessionExecutionHandle`的process-local sticky EmergencyControl signal；存在candidate/current Turn时绑定该handle内的current control epoch。它立即关闭admission和新的Model/Tool/source operation：Idle直接重新resolve，Starting取消candidate，active Turn执行truthful settlement和`TurnInterrupted(SecurityRevoked)`；old handle关闭后不重定向到new Executor，也不承诺动态撤销open OS handle。
_避免_：Workspace definition patch、durable Session字段、RevocableHandle registry、已发生副作用回滚

**会话执行器（`SessionExecutor`）**：
单个loaded Session的执行期编排对象，拥有`SessionExecutionState`、per-session `SessionIngress` semantic lanes/signals、SessionWriter、committed projections、CurrentTurnExecution和current `RunningOperation`。每个Session一个Executor；一个Runtime允许多个Executor同时Running。
_避免_：会话管理器、UI会话状态、全局current Session

**会话执行句柄（`SessionExecutionHandle`）**：
上层联系某个loaded `SessionExecutor`的显式可克隆句柄，用于提交typed request和请求一致snapshot；它不保存权威Session状态，也不直接执行AgentLoop、Model或Tool。
_避免_：`Arc<Mutex<SessionExecutor>>`、SessionExecutor本体、RunningOperation handle、客户端selected session

**产品级编排能力**：
围绕一次会话运行所需的阶段、队列、资源、工具、会话写入、中止和事件等职责。它是一组属于会话运行时的能力，不是独立模块。
_避免_：UI 后端、简单 wrapper

**运行时 Hook（`RuntimeHook`）**：
`RuntimeHooks` 模块规划的后期内部安全点干预能力，用于在 prompt/context、模型请求、provider payload、工具治理、压缩、session commit observer 和 UI-safe command result 等流程中返回 typed decision / patch / replacement。当前 MVP 不实现 hook system，也不定义资源 discovery/reload hook。Hook 影响最终会发生什么，但不直接发布Runtime StateEvent/ProgressEvent，不读写session storage，不执行工具，也不读取凭据。
_避免_：UI 回调、协议事件、插件系统、任意 runtime 后门、当前阶段必做模块

**Hook owner**：
拥有某个安全点业务不变量的模块。Hook owner 负责调用 hook、应用 typed result、重新校验并记录 diagnostics；`RuntimeHookRegistry` 只保存 handler，不拥有业务流程。`SessionExecutor`拥有Turn/context/request queue/compaction/session append observer处理位置，`ToolService`拥有工具治理处理位置，`ModelGateway` 拥有 model/provider 边界安全点，`CommandManager` / `Command` 拥有 command catalog/resolve/output 安全点。
_避免_：Hook 注册表、Driver、UI adapter、任意调用方

**运行时 Hook 注册表（`RuntimeHookRegistry`）**：
后期保存 hook handler、source、capability、timeout、cancellation 和 failure policy 的内部运行时服务。它不拥有业务状态机，也不拥有 event metadata；hook owner 调用它，并应用 typed result。
_避免_：事件总线、插件管理器、工具执行器、会话存储

**会话阶段（`SessionPhase`，pre-refactor protocol term）**：
旧Runtime snapshot中的粗粒度状态。当前公开SessionSnapshot投影`SessionExecutionView`和optional current Turn phase；内部权威状态仍是`SessionExecutionState { Idle, Starting, Running, Finishing }`加`TurnExecutionPhase`。不保留旧`SessionPhase::{Turn, Compaction, RetryBackoff}`公开enum。
_避免_：SessionExecutor mutable state、ModelGateway attempt、durable TurnStatus

**会话已稳定（`session_settled`）**：
会话处于`Idle`，没有Starting/Running/Finishing Turn，也没有马上启动的FollowUp。它是per-session StateEvent和SessionSnapshot中的状态事实，不是command、阻塞式wait interface或“所有process-local queue为空”的同义词。
_避免_：WaitForIdle、Turn terminal替代品、同步等待点

**会话**：
一次工作上下文的持久记录，包含消息、模型变化、思考等级变化、活跃工具变化、压缩摘要、标签、名称和当前叶子位置。
_避免_：聊天记录、日志文件

**SessionIndex / 会话目录**：
Runtime内部Session durable owner维护的轻量清单或可重建索引，用于CommandSurface `/resume`和`SessionQuery::List`。它包含SessionId、AgentRevisionRef、Workspace summary、名称、lifecycle、更新时间、预览和轻量统计；不包含完整消息、current Turn或UI状态。
_避免_：RuntimeSnapshot、完整会话上下文、UI sidebar store、第二durable truth

**会话条目（`StoredSessionEntry`）**：
会话中的一条不可变追加记录。条目通过`entry_id`和`parent_id`连接成树；顶层body固定为`TurnContext`、`Message`、`Event`或`Compaction`。Message使用`user | assistant | tool` role；usage和finalized provider response metadata随assistant entry保存。
_避免_：业务 batch、公开 RuntimeEvent 副本、数据库行

**会话树**：
由会话条目的父子关系形成的历史树。当前对话上下文由根到 current entry 的 selected path 投影，而不是由文件中所有条目线性构建。物理 entry 可独立 durable；assistant/tool sequence 只有在 `tool_round_completed` 后才作为完整 round 进入模型 conversation。
_避免_：消息数组、历史列表、batch-result leaf

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
一次finalized模型响应由provider返回的token分项，例如输入、缓存命中输入、输出和推理输出。它随assistant entry持久化；provider未返回的字段保持`None`。本地估算只属于可重建projection/diagnostics，不能伪装成provider usage fact。
_避免_：上下文占用、费用账单、消息长度、本地估算事实

**上下文占用**：
当前会话投影到下一次模型请求时预计占用的上下文窗口大小。它用于上下文进度条、pre-run threshold check和压缩触发；每次模型调用的权威大小判断仍由最终call projection validation完成。它不等同于历史累计消耗。
_避免_：会话总 token、账单消耗、消息数量

**会话消耗统计**：
一个会话从创建以来累计的模型调用消耗视图。压缩不会降低会话累计消耗，只会降低后续上下文占用。
_避免_：当前上下文窗口、压缩摘要大小、UI 计数器

**会话管理器（`SessionManager`，pre-refactor module term）**：
旧架构中协调会话目录和loaded runtime的模块名。目标领域不建立SessionManager对象；MiniCoreRuntime内部以Session durable owner、SessionIndex和LoadedSessionExecutors实现同类职责。
_避免_：公开SessionManager、领域Manager entity、Agent loop owner

**已加载会话执行器（`LoadedSessionExecutors`）**：
Runtime内部维护的live map，记录当前已加载Session对应的`SessionExecutionHandle`。它不是持久化会话目录；每个`SessionExecutor`独立持有state、request queues、current Turn、Pending Interaction和SessionWriter。它不保存客户端selected/current Session，多个Executor可以同时推进work。
_避免_：会话存储、会话目录、独立会话运行时注册表、UI selection store

**会话存储（`SessionStorage`）**：
单个会话的底层 by-entry ledger，也是 `SessionWriter` 的 adapter；负责读取 header/entries、重建 parent tree、校验entry body与cross-entry references、生成 committed projections，并隐藏 memory/JSONL实现。MVP cold load完整replay全部complete entries到physical current entry（最后成功append的EntryId），不恢复process-local执行对象，保守关闭unfinished Turn后进入Idle；已loaded Session切换不触发storage replay。MVP不实现ProjectionSnapshot或checkpoint index。它不决定Agent如何运行，也不直接服务UI。
_避免_：会话管理器、会话运行时、聊天状态、第二 durable event log

**技能**：
通过SkillService发现和加载的Markdown指令包。Turn admission从captured shared resource root与WorkspaceSkillContext构建并捕获SkillView；显式调用时TurnExecutionContext按captured entry加载正文，经SkillInjector产生PromptContribution，再由PromptSet规范化为UserMessage。
_避免_：提示词片段、插件、工具、独立版本实体

**技能模块（`SkillService`）**：
MiniCoreRuntime-owned深模块，负责Skill source discovery、metadata validation、shared SkillResourceView candidate build、per-Turn SkillView构建、lazy load、content cache和diagnostics；完整SharedResourceRoots publication由MiniCoreRuntime负责。它不构造最终UserMessage、不拥有Workspace lifecycle，也不调用模型。
_避免_：skills.rs helper集合、UI解析器、Prompt manager、Tool registry

**技能视图（`SkillView`）**：
SkillService从Turn admission捕获的`Arc<SkillResourceView>`和`WorkspaceSkillContext`构建的immutable metadata view；它不是global current view。TurnExecutionContext持有同一个`Arc<SkillView>`，future shared或Workspace reload不改变active Turn。Skill可以lazy parse/load，但只能基于对应publication捕获的immutable bytes/content，不能在Turn内按path重新读取current file。SecurityRevoked通过Turn control取消active source operation。
_避免_：current磁盘目录、命令列表、mutable global catalog、durable Skill revision

**技能调用**：
用户显式Skill intent先解析为PromptIntent；TurnExecutionContext使用captured SkillEntry调用SkillService::load，经SkillInjector形成typed PromptContribution，再由PromptSet.compose_user_message生成CanonicalUserMessage并append。模型触发的Skill能力走ToolResult路径。
_避免_：系统提示词旁路、按metadata path重读、未committed current-call contribution

**运行时资源（pre-refactor aggregate term）**：
旧ResourceManager设计对Prompt、Skill和Workspace source的统称。目标架构由PromptService、SkillService、ToolService和Workspace各自拥有定义、加载与snapshot语义，不建立通用RuntimeResource entity。旧设计派生的“资源快照/资源摘要/提示词素材”随ResourceManager一并废除：快照语义由WorkspaceSnapshot、PromptSet、ToolSet、SkillView等Turn-pinned immutable `Arc`对象承接；UI安全投影由各子系统UI-safe view承接；Prompt输入由PromptIntent与PromptContribution承接。
_避免_：统一ResourceManager、跨模块resource snapshot、UI文件

**提示词意图（`PromptIntent`）**：
尚未展开成模型消息的结构化用户输入，可以引用普通文本、技能、提示模板或它们的组合。队列保存 intent 的稳定资源 key、参数和附件引用，不保存 raw slash command text 或提前展开的资源正文。
_避免_：用户消息、命令文本、已展开 prompt、PendingSessionAction

**Prompt子系统**：
MiniCoreRuntime-owned `PromptService`和Turn-pinned `PromptSet`组成的模型上下文组装模块。`PromptSet.compose_user_message(PromptIntent)`是UserMessage规范化入口；`PromptSet.assemble(PromptAssemblyInput::AgentRun | CompactionSummary)`是唯一provider-neutral模型上下文组装interface，只接受trusted full/prefix conversation view和typed policy。它不拥有Session history、request queue、Tool execution、storage或provider调用。
_避免_：PromptManager、ContextManager、ModelGateway内拼接messages

**Turn提示词集合（`PromptSet`）**：
SessionExecutor在Turn admission期间通过PromptService构造的不可变提示词集合，绑定captured PromptResourceView、Agent/Session PromptId selection、Workspace User context、SkillPromptView和同一ToolSet的ToolPromptView。active Turn内的Submit composition、Steer和每次Model context assembly都复用该PromptSet。
_避免_：长期PromptManager、current Session prompt、模型request、Tool executor

**Turn工具集合（`ToolSet`）**：
`ToolService::for_turn(...)`返回的不可变Turn工具集合，原子绑定ToolSpec、ToolPromptView和execution route。ToolPromptView只能由parent ToolSet私有投影并随PromptSet捕获；PromptSet只使用该view，SessionExecutor通过同一ToolSet执行ToolCall；active Turn内不能替换或由caller伪造替代view。
_避免_：session-scoped Tools副本、独立prompt/executor getter、UI工具列表

**组装后的模型上下文**：
`AssembledModelContext`绑定PromptSet、CommittedConversationView、ModelCallPurpose、OutputContract和最终provider-neutral content。完整logical call由同一个immutable `Arc<ModelCallRequest>`承载；Session logical retry必须复用该request，不重新assemble，也不以派生摘要作一致性证明。
_避免_：provider payload hash、Session revision、Tool executor identity替代品、模型上下文摘要身份

**上下文素材（`ContextMaterial`）**：
由RAG、memory、IDE、issue lookup或后期hook等动态来源成功提供的typed模型上下文，带稳定来源、生命周期和required/optional要求。需要durable的内容必须先由其owner转换并提交为canonical session message；项目文件、技能和提示模板不能绕过PromptSet与append/apply伪装成动态素材。
_避免_：资源快照、无来源字符串、会话历史、系统提示词

**上下文素材贡献（`ContextMaterialContribution`）**：
一次动态 context 获取的显式结果：`Available(ContextMaterial)` 或带 key/source/persistence/requirement/diagnostic 的 `Unavailable`。required source 失败必须保留为 `Unavailable` 并阻止模型调用；optional 失败进入 projection diagnostics。禁止通过 vector 缺项表达获取失败。
_避免_：Option<ContextMaterial>、静默跳过、provider future、原始 I/O error

**组装后的模型上下文（`AssembledModelContext`）**：
PromptSet在一次模型调用前生成的最终provider-neutral模型可见上下文，包含ordered System sections、前置User context与协议安全conversation messages、ToolSpec、OutputContract、贡献来源和diagnostics。它由`PromptSet.assemble(...)`从committed conversation和purpose组装，是ModelCallRequest唯一的model-visible input；correctness由typed messages、exact checkpoint和同一个immutable request保证，不公开额外模型上下文身份值。
_避免_：provider payload、Turn state、session message vector、Gateway内重新组装

**输出契约（`OutputContract`）**：
一次模型调用对输出结构的provider-neutral要求，例如NoToolCalls或JSON schema。它与prompt text一起进入`AssembledModelContext`，但不能靠普通文本替代；真实provider映射和response validation由`ModelGateway`完成。MVP Structured调用不携带ToolSpec。
_避免_：system prompt、provider payload、显示格式、max output tokens

**CommandSurface**：
MiniCoreRuntime提供给CLI、TUI和GUI的跨界面用户命令领域面。它不是一个有状态Service名；实现上由Runtime-owned共享无状态`CommandManager`和每次调用构造的explicit optional Session command context组成。Slash text和catalog selection必须走相同的materialize、resolve、validate和handler binding。
_避免_：UI adapter、快捷键系统、协议命令枚举、Agent loop、有状态 CommandSurfaceService

**RuntimeCommand**：
下游adapter通过`MiniCoreRuntime.dispatch(...)`提交的公开协议命令，按Agent、Session、Turn、Interaction和CommandSurface分组。它表达领域mutation或work admission，不代表command tree节点，也不包含直接模型调用、raw storage mutation或高权限内部handle。
_避免_：command::Command、UI action payload、内部调试 API

**运行时查询（`RuntimeQuery`）**：
下游adapter通过`MiniCoreRuntime.query(...)`提交的只读typed查询总线，按runtime、agent、session、command surface、model、prompt、skill、tool、usage和diagnostics领域分组。它不创建Turn、不启动work、不消费queue、不改变revision，也不通过事件流广播结果。
_避免_：RuntimeCommand、CommandOutput、Snapshot、UI本地selector、后台job

**查询响应（`QueryResponse`）**：
`RuntimeQuery`的直接request/response结果，包含可选领域revision和typed `QueryResult`。它不是业务事件，也不保证多个Query调用形成原子Snapshot；transport request id不进入领域模型。
_避免_：CommandResponse、StateEvent、Snapshot、JSON-RPC request object

**CommandManager**：
`MiniCoreRuntime`持有的共享、无状态命令管理器。它持有只读command packs、candidate provider registry和handler registry；每次调用时基于显式构造的`CommandContext`临时materialize command catalog，并执行parse、suggest和`resolve_for_execution`。
_避免_：SessionExecutor、catalog cache、UI菜单状态、执行Tool或Model的module

**Command（session-scoped，pre-refactor facade term）**：
旧设计中由SessionRuntime长期持有的命令facade。目标架构不保留该对象；MiniCoreRuntime按每次请求构造CommandContext，并把resolved action路由到公开Command或Query owner。
_避免_：SessionExecutor持有Command facade、current Session command object

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

**Command Prompt Delivery（`CommandPromptDelivery`）**：
Prompt-producing slash/catalog command解析为PromptIntent后选择的公开交付方式：`Submit`创建新Turn，`Steer { expected_turn_id }`作用于当前Running Turn，`FollowUp`在当前Turn terminal后admit下一Turn。它不决定非prompt handler何时运行。
_避免_：DeliveryMode、NextTurn、通用InputSchedule、raw slash text queue

**命令运行策略（`CommandRunPolicy`）**：
Command manifest/handler相对active work的执行约束。首版普通读取和不改变Workspace的future-only definition mutation可以Immediate执行；Workspace definition patch明确要求Session Idle，非Idle返回SessionBusy且不排队；Prompt-producing command使用Submit/Steer/FollowUp。首版不建立generic QueueAfterRun或PendingSessionAction。
_避免_：通用任务调度器、manual compact queue、Prompt delivery替代品

**命令目录**：
运行时基于当前 session context 临时 materialize 给界面适配器的命令摘要集合，包含 command key、path、来源、说明、参数提示和当前可用性。它用于 autocomplete、command palette 和嵌套菜单，不等同于执行授权。
_避免_：命令执行器、工具注册表、资源目录、持久化 catalog

**命令解析 / resolve**：
`CommandManager` 将命令文本或 catalog selection 解析成 command node、dynamic bindings 和 args，并在执行前重新校验 command 是否仍存在、参数是否合法、phase/trust/capability 是否允许、handler binding 是否可信。
_避免_：UI 输入框逻辑、Agent loop、shell parser、一次性 planner

**命令输出**：
由用户命令产生的display-neutral界面说明，例如`/status`摘要、`/usage`统计、`/model`设置完成提示或解析错误。它可以显示在消息面板中，但不是模型可见消息，也不能携带完整`RuntimeCommand`。
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

**排队消息（`QueuedMessage`）**：
Steer/FollowUp FIFO中的process-local数据值，包含原CommandId和PromptIntent。SessionExecutor直接持有`VecDeque<QueuedMessage>`；不建立PendingMessageQueue service/wrapper。Steer在完整assistant/tool step后、下一次Model前pop一条；FollowUp在Turn terminal后pop一条。仍在队列中的消息可以remove，撤销后不重新入队；append后的UserMessage已是durable fact。
_避免_：durable entry、Queue entity、priority queue、batch drain mode、PromptSet临时输入

**异步操作（`RunningOperation`）**：
SessionExecutor启动的Context构造、Model调用、Tool执行或Compaction操作。每个Session最多一个current operation；主循环同时poll它和request queue，旧operation terminal/remove或安全drop前不启动下一operation。它只接收不可变输入并返回`OperationResult`，不能拥有SessionWriter、projections、request queues或Turn terminal state。
_避免_：SessionExecutor、长期后台Session、领域Turn、第二状态owner

**流式Item（`StreamingItem`）**：
SessionExecutor在当前AgentRun logical Model operation中保存的process-local AgentMessage或Reasoning累积buffer。首次content progress时分配稳定ItemId，started/delta使用该ID；它不是Item projection、JSONL entry或Snapshot authoritative content，terminal error/Cancel时直接丢弃。
_避免_：ProvisionalItem、durable Started Item、provider attempt entity、UI store draft

**最终Item候选（`FinalItemCandidate`）**：
provider完成并通过validation后，由finalized response和可选StreamingItem映射生成的未提交值。有progress时沿用StreamingItem的ItemId；没有progress时在terminal normalization分配ItemId。只有append/apply成功后projection才产生Completed Item并发布item_completed。
_避免_：Committed Item、StoredSessionEntry、提前发布的StateEvent

**Submit命令标识（Submit `CommandId`）**：
Submit envelope的随机、不可复用CommandId同时作为initiating UserMessage append前的process-local admission/cancel identity。公开Cancel target使用`Submit(CommandId) | Turn(TurnId)`；同一Runtime内duplicate in-flight Submit加入原completion，Turn创建后长期execution identity切换为TurnId。CommandId不写入SessionStorage，restart后不可恢复或重放。
_避免_：独立SubmissionId、领域TurnId、crash-safe queue identity

**运行标识（`RunId`，pre-refactor protocol term）**：
旧Runtime protocol用于标识一次公开Agent run。目标Runtime protocol不定义RunId；公开执行identity是SessionId+TurnId，Starting admission使用Submit的CommandId，内部operation使用execution_version和OperationType。
_避免_：公开RunId、host-global Run索引、用RunId替代TurnId

**待执行会话动作（`PendingSessionAction`）**：
旧Runtime protocol中用于QueueAfterRun的结构化Session操作。目标首版不建立PendingSessionAction，也不公开manual CompactSession；future maintenance需要独立设计admission、queue、cancel和snapshot语义。
_避免_：RuntimeCommand payload、QueuedMessage、writer batch、CommandManager pending state

**当前运行状态（`CurrentRunState`，pre-refactor protocol term）**：
旧RuntimeSnapshot中的run read model。目标SessionSnapshot使用`SessionExecutionView + optional CurrentTurnView`，不保留CurrentRunState或Suspended run模型。
_避免_：SessionExecutor mutable state、durable TurnStatus替代品、公开Run lifecycle

**可恢复暂停（`Suspended`，pre-refactor proposal）**：
阶段6 baseline不定义Suspended execution state。WaitingApproval保持Running；Cancel/host restart使用Interrupted。若未来需要同进程pause/resume，必须另行定义operation state、identity和public protocol。
_避免_：WaitingApproval、UI selection、跨进程恢复旧Model/Tool operation

**Turn执行上下文（`TurnExecutionContext`）**：
SessionExecutor在Turn admission期间构造的不可变execution binding，固定exact AgentRevisionRef、SessionDefinitionRevision、WorkspaceSnapshot、PromptSet、ToolSet、captured SkillView和TurnModelSnapshot。active Turn内的retry、Steer和model→tool→model循环复用该Context；FollowUp创建新Turn并重新capture。
_避免_：模型request、ResourceManager current view、公开协议snapshot、mutable Turn state

**DriverTurnInput（pre-refactor adapter term）**：
旧Driver设计使用的窄输入。当前SessionExecutor直接从TurnExecutionContext和CommittedConversationView构造PromptAssemblyInput；自研AgentLoop只接收其逻辑所需值（ADR 0115），不存在SDK adapter value。
_避免_：公开Runtime request、TurnExecutionContext替代品、ModelCallRequest

**AgentLoop（自研协议状态机）**：
Turn execution内部的crate-private sans-I/O同步状态机（ADR 0115），在`NeedModel | NeedTools | Finished`之间推进并把稳定模型输出交回Session execution；不由Rig或其他SDK驱动。它不拥有SessionWriter、conversation projection、Prompt assembly、Tool execution或terminal arbitration；Steer由`accept_committed_steer`原地推进或等价重建segment，Compaction Replace后必须重建segment。旧称「Driver（AgentLoop适配器）」随ADR 0115废弃。
_避免_：SDK loop适配层、Driver、会话状态owner、工具注册表、UI loop、第二conversation

**异步操作结果（`OperationResult`）**：
current Context、Model、Tool或Compaction operation返回给SessionExecutor的typed result，携带`SessionId`、`TurnId`、`execution_version`和`OperationType`。SessionExecutor不detach可迟到的本地future；logical retry只在旧operation terminal/remove后启动。execution_version验证conversation/control basis，不充当retry attempt编号。
_避免_：durable entry、Runtime event、直接Session mutation

**工具执行控制（`ToolExecutionControl`）**：
Tool执行期间请求SessionExecutor完成approval和ToolExecutionStarted记录的crate-private interface。它通过typed request/response工作，不借用SessionExecutor mutable state，也不拥有第二writer。
_避免_：InteractionService、ToolLedgerService、公开Runtime interface

**工具服务（`ToolService`）**：
`MiniCoreRuntime`拥有的独立深模块，封装工具定义、registry、prompt view、policy、approval需要、sandbox和executor implementations。candidate Turn通过`ToolService::for_turn(...)`得到不可变`ToolSet`；Session execution协调AgentLoop与ToolSet。每个loaded Session的SessionExecutor另行拥有独立file mutation queue（ADR 0116），Agent/Session/Turn领域对象不持有工具属性。
_避免_：session-scoped工具配置副本、UI工具层、Rig ToolServer替代品、平级helper-only模块

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
`Tools` 子系统内部的纯策略判断器。它根据工具定义、prepared invocation、工作区信任、沙箱结果和用户设置决定允许、拒绝、要求审批、改写参数、强制串行或中止运行；后期启用 hook system 时可以叠加 hook 结果。它不等待 UI、不执行工具、不构造需要 I/O 的 preview。
_避免_：审批弹窗、工具执行器、UI 权限系统、preview builder

**工具审批代理（`ToolApprovalBroker`）**：
Tool执行期等待外部批准的private waiter/broker实现。Durable truth是SessionStorage中的Item-owned Interaction request/resolution；broker不能成为第二pending approval owner，也不能直接通知UI。
_避免_：UI回调、durable Interaction替代品、长期授权存储、公开Runtime object

**待审批工具调用（`PendingToolApproval`）**：
旧approval-specific projection term。目标公开投影是SessionSnapshot中的`InteractionView`，identity为SessionId+TurnId+ItemId+RequestId；prepared args和executor handle保持private。
_避免_：独立approval truth、RuntimeSnapshot全局pending列表、可由UI修改的工具参数

**工具审批决定（`ToolApprovalDecision`）**：
`InteractionCommand::Resolve`中的typed ToolApproval resolution，可以ApproveOnce、ApproveWithRestrictions或Reject；它不能替换工具参数，也不能直接执行工具。approval不跳过schema validation、hard deny、sandbox、mutation ordering或audit，也不保存跨调用授权。
_避免_：工具策略决定、工具参数、用户命令结果

**工具执行协调器（`ToolExecutionCoordinator`）**：
`Tools` 子系统内部的 batch 执行协调器。它执行已经声明的 parallel/sequential、approval wait、并发限制和 mutation lock 约束，并按 LLM source `call_index` 稳定回填结果；它不根据工具名自行发明执行策略。
_避免_：独立 scheduler、LLM 策略解释器、工具执行器

**工具沙箱视图（`ToolSandboxView`）**：
`Tools` 子系统在 executor 前使用的路径授权与执行约束 source of truth，描述 cwd、read/write/denied roots、shell/network/env policy 和 effective enforcement capability。MVP 只承诺 MiniCore 内置工具的进程内路径边界，并保持通用 shell / `bash` disabled；通用子进程请求的 filesystem/network 限制只有在 OS-native/external backend capabilities 满足时才能标记为 enforced，否则 fail closed。UI approval 不能替代 enforcement；`FullAccessWithApproval` 明确没有 sandbox guarantee。后期 hook 改写参数后必须重新 schema validate、canonicalize、sandbox check 和 policy evaluate。
_避免_：审批弹窗、路径字符串前缀检查、把 best-effort/full-access 称为 sandbox、executor 自行放宽权限

**工具执行器**：
`Tools` 子系统内部执行某个具体工具副作用的组件，例如读取文件、搜索文本、修改文件或运行命令。executor 不能绕过 registry、active tool check、policy、approval、sandbox 和 mutation lock。
_避免_：工具策略、工具注册、UI 执行器

**模型调用网关（`ModelGateway`）**：
MiniCoreRuntime-owned深模块，通过`resolve_for_turn(...)`固定exact TurnModelSnapshot，并通过一个`generate_model_turn(...)`异步operation处理provider encoding、credential、单次request/stream、usage、error、cache和continuation。MVP每个operation最多执行一个provider attempt，不做transparent retry或transport fallback；它不重新组装Prompt，不保存current Session/Turn，也不在active Turn内替换model identity。
_避免_：provider client透传、系统提示词构建器、cross-model fallback、第二conversation

**ModelGateway spine**：
真实provider integration前必须稳定的MiniCore-owned类型和interface：TurnModelSnapshot、ModelCallPurpose、ModelCallRequest/Result/Error、ModelUsage、ModelProgressEvent、`resolve_for_turn(...)`和`generate_model_turn(...)`。ProviderCatalog、AuthStore和private ProviderAdapter属于其implementation；首个production adapter使用Rig。
_避免_：临时provider路径、SessionExecutor直接调用Rig

**Provider适配器（`ProviderAdapter` / `RigProviderAdapter`）**：
ModelGateway内部执行单次provider attempt的private seam。它接收Gateway已经规划并解析model/endpoint/credential的`ProviderAttemptRequest`，完成具体provider协议编码、request/stream调用、cancellation桥接和attempt result/error映射。`RigProviderAdapter`是首个production implementation；底层SDK automatic retry固定为0。它不选择provider/model，不拥有Session logical retry、auth policy、cache/continuation policy或最终`ModelCallResult`。
_避免_：ModelGateway替代品、AgentLoop adapter、provider选择器、重试scheduler、公开SDK client

**模型选择（`ModelSelection`）**：
一次会话或模型调用使用的稳定模型身份，由 `provider_id` 和 `model_id` 组成。它不等同于 provider API 中的真实模型名，也不包含凭据或 base URL。
_避免_：模型显示名、API model name、Rig 模型类型

**Turn模型快照（`TurnModelSnapshot`）**：
Turn admission期间由ModelGateway根据ModelSelection解析的immutable execution value，pin exact ModelDefinitionRef、ModelDefinitionVersion、capabilities、effective limits和generation policy。它可以携带crate-private opaque execution reference，但不暴露endpoint、auth reference、credential或provider client。
_避免_：ModelSummary、current catalog lookup、cross-model fallback plan

**模型状态（`ModelState`，pre-refactor term）**：
旧SessionRuntime中的mutable model execution state。目标架构由SessionDefinition保存ModelSelection和用户偏好，TurnExecutionContext保存TurnModelSnapshot；provider connection与stream state留在ModelGateway，logical retry delay/count由SessionExecutor的RunningOperation持有。
_避免_：active Turn mutable model、provider client、credential

**模型目录（`ProviderCatalog`）**：
ModelGateway内部的versioned provider/model definition catalog，记录explicit protocol、API model name、capabilities、limits和adapter encoding policy。Runtime公开查询以后只取得safe catalog view，不直接持有该对象。
_避免_：credential store、provider client pool、project-defined endpoint

**凭据存储（`AuthStore`）**：
ModelGateway private implementation中的secret解析模块，负责API key、OAuth token和runtime override，并支持request前singleflight refresh。只向private provider attempt提供typed secret material；provider返回401后本次Gateway operation不refresh-and-resend。
_避免_：TurnModelSnapshot、Runtime event、环境变量直读调用点

**模型调用目的（`ModelCallPurpose`）**：
一次模型调用的稳定业务意图，例如`AgentRun`或`CompactionSummary`。它从`ModelCallRequest`原样传播到response metadata；provider usage直接附着finalized assistant entry，不建立独立`SessionEntry::Usage`。logical retry、客户端是否选中该session、调用是否在后台执行都不是purpose。
_避免_：`UsagePurpose`、Retry、Background、provider attempt status、调度状态

**模型调用请求（`ModelCallRequest`）**：
SessionExecutor交给ModelGateway的唯一provider-neutral request，包含TurnModelSnapshot、ModelCallPurpose、唯一`input: AssembledModelContext`和可选输出token上限。generation defaults和reasoning已在Snapshot固定；stream/cache是Gateway策略；role、messages、ToolSpec和OutputContract只存在于assembled input中。
_避免_：provider HTTP request、Rig CompletionRequest、SessionId/TurnId、credential、raw params

**模型调用结果（`ModelCallResult`）**：
ModelGateway返回的一次完整terminal success，包含ordered FinalizedAssistantResponse、normalized finish reason、provider-reported ModelUsage和allowlisted response metadata。只有SessionExecutor验证OperationResult identity/version和authorization后才能把它转换为assistant entry。
_避免_：partial stream、durable Item、provider raw response、Turn terminal

**模型调用错误（`ModelCallError`）**：
ModelGateway返回的redacted typed terminal error。`ModelCallErrorReason`区分Cancelled、Auth、RateLimited、Quota、ContextOverflow、Capability、Safety、Timeout、Transport、RequestOutcomeUnknown、StreamInterrupted，以及UnexpectedToolCall、InvalidStructuredOutput、InvalidProviderResponse和IncompleteResponse；caller不得解析provider字符串决定retry。后四者由Response Validation产生且不自动retry。

**失败处理归属**：
raw external failure只存在于具体adapter implementation；发生事实的module负责typed error或truthful outcome，掌握Turn、control和durable state的owner负责恢复。不建立全局Error module、ErrorService或共同retryable分类。该规则由ADR 0120记录，首版只应用于ModelGateway response validation。
_避免_：generic RuntimeError、raw HTTP body、assistant message、ToolResult

**模型进度（`ModelProgressEvent`）**：
ModelGateway发布的process-local content delta。AgentRun GenerateModelResponse的scoped adapter先无损更新StreamingItem/ItemId映射，再把observer update送入bounded、可合并/丢弃的Host ProgressEvent queue；CompactionSummary adapter不创建ItemId。Session logical retry由SessionExecutor另行发布`model_retry_scheduled`。两者都不进入SessionStorage，finalized result或typed error负责最终校正。
_避免_：durable Message、第二event log、cancellation token

**压缩摘要指令（`CompactionSummaryDirective`）**：
`Compaction`根据strict stable-unit plan生成的摘要格式、typed instruction和最大输出token预算。它不是system prompt、模型调用请求或最终模型上下文，不包含模型选择、thinking/stream policy、call/run id或工具schema；`SessionExecutor`把它和trusted `CommittedConversationPrefixView`交给active PromptSet的`CompactionSummary` assembly variant，固定`OutputContract::NoToolCalls`。完整规则见`docs/modules/compaction.md`。
_避免_：SummaryModelRequest、ModelCallRequest、AssembledModelContext、系统提示词状态

**压缩计划（`CompactionPlan`）**：
从committed conversation、active Turn initiating UserMessage保护、stable-unit boundaries和context budget确定的crate-private immutable plan。首版固定使用portable rolling SummaryModel，不根据模型名称选择ProviderNative，也不使用deterministic conversation truncation。operation持有同一个`Arc<CompactionPlan>`及其immutable settings、budget、directive、source和request；durable entry只保存exact source checkpoint、typed boundaries/provenance与模型调用结果，不保存派生plan摘要。
_避免_：CompactionMethod、GPT压缩、Claude压缩、provider endpoint、UI handler

**上下文限制错误（`ContextOverflow`）**：
当前Turn的下一次模型调用无法进入有效上下文窗口的typed recovery class。`PromptAssembly`表示PromptSet本地最终组装拒绝、未调用provider；`Provider`表示ModelGateway归约provider context overflow。两者由SessionExecutor共享一次bounded compaction recovery，但保留diagnostics和usage来源差异。
_避免_：普通 retry error、Prompt admission rejection、模型可见 assistant message

**驱动安全点**：
AgentLoop在稳定模型输出、`tool_round_completed` conversation projection update和candidate final之间把控制权交回Session execution的边界。Session execution只在对应entry成功`append → apply`后消费Steer、开始下一次模型调用或发布terminal；helper名称是private implementation detail。FollowUp在当前Turn terminal后的admission中启动新Turn。
_避免_：prepare next turn、UI回调、工具Hook、physical batch boundary

**运行时命令**：
由可信host通过MiniCoreRuntime发出的typed指令，例如创建或更新Agent/Session、Submit/Steer/FollowUp/Cancel Turn、Resolve Interaction或执行CommandSurface text/catalog selection。
_避免_：UI widget action、直接后端函数调用、raw storage/model mutation

**StateEvent**：
Runtime向host发布的scope-local、非durable observer record。它可以描述committed-derived状态或process-local readiness/phase/queue状态，两者都只在当前snapshot-first subscription lifetime内按发送顺序交付。stream断开、背压或restart时不重放；host以新Snapshot为准，durable-derived状态从projection重建，未append queue和旧phase可以消失。Durable conversation fact必须从append/apply后的receipt派生。
_避免_：ProgressEvent、SessionStorage entry副本、runtime-global sequence

**ProgressEvent**：
模型AgentMessage/Reasoning started与delta、Tool output和Session logical retry等高频observer update。它可以合并或丢弃；message/reasoning started、delta与final completed共享稳定ItemId，append/apply后的StateEvent或重新订阅后的Snapshot携带完整view进行校正。
_避免_：durable message、terminal event、恢复水位

**事件消息（`StateEventMsg` / `ProgressEventKind`）**：
事件中的typed payload。外层event拥有route、timestamp和optional CommandId；payload不重复SessionId/TurnId/ItemId/RequestId等route坐标。Progress payload不能作为独立权威状态。
_避免_：chat message、UI显示文案、前端store对象

**事件族**：
按公开生命周期对象划分的一组StateEvent或ProgressEvent，例如agent、session、turn、item、interaction、queue、usage和diagnostics。事件族用于wire命名和reducer分发，不等同于内部Rust module。
_避免_：文件名、模块名、UI分组

**事件类型名**：
UI/wire层识别事件种类的稳定名称，使用flat `snake_case`，例如`turn_started`、`item_tool_invocation_completed`和`agent_message_delta`。
_避免_：Rust enum variant、内部模块路径、显示文案

**事件生命周期**：
一个运行时动作从接收命令到进入终态期间应产生的事件顺序，例如开始、增量、完成、提交后的领域事实和空闲通知。
_避免_：UI 动画流程、内部函数调用顺序

**事件状态机**：
描述某类运行时对象可处于哪些状态、哪些事件能推动状态转换，以及哪些状态是终态。它用于约束 UI reducer 和运行时测试。
_避免_：前端 store、流程图、内部实现步骤

**会话写入器（`SessionWriter`）**：
所有会话ledger mutation共用的可信写入seam。它接受一个`SessionEntryDraft`，校验expected current parent、body contract和cross-entry references，分配storage-owned identity，append一条JSONL entry并返回`CommittedSessionEntry`。只有receipt被全部required projections应用后，领域事实才可通知host或进入模型conversation。
_避免_：事件发布器、业务调度器、UI保存状态、通用数据库事务管理器

**会话条目草稿（`SessionEntryDraft`）**：
一次明确的entry append intent，包含expected current entry、parent和typed body。多个entries之间的业务完整性由Session execution的sequential `append → apply → dependent action`编排和cross-entry validation保证，不伪装成物理batch。
_避免_：CurrentRun snapshot、事件批次、模型请求batch、`SessionWriteBatch`

**模型提供方客户端**：
用于调用一个或多个模型提供方 API 的底层库。它可能支持流式输出和 function-call payload，但它本身不提供完整 Agent loop、本地编程工具、会话模型或权限边界。
_避免_：Agent SDK
