# SessionManager / SessionStorage

这个文档描述工作区内会话生命周期、会话持久化与会话树模块，而不是 `SessionRuntime` 的运行编排本身。它之所以独立存在，是因为会话管理有自己的可测试 interface：创建/打开/列出/删除/fork 会话、加载/关闭运行中会话、追加条目、读取当前 leaf、从 root 到 leaf 重建上下文。

会话属于 Agent Runtime，不属于 UI。UI 只能通过运行时命令打开、列出、fork、导航或删除会话，不能直接读写会话文件。

设计参考 pi coding-agent `SessionManager`、`AgentSessionRuntime` 的会话替换经验，以及 Codex `ThreadManager` + `ThreadStore` + `LiveThread` 的分工。本项目采用广义 `SessionManager`：它同时协调持久化会话目录和已加载会话运行时；底层单会话存储仍叫 `SessionStorage`。

## 核心模型

- `SessionManager` 是工作区内会话生命周期 facade，负责 persistent session catalog / `SessionIndex`、loaded runtime map、focused session、create/open/list/delete/fork/focus/close，并隐藏内存存储与 JSONL 文件存储的差异。
- `LoadedSessionRuntimes` 是 `SessionManager` 内部的 live runtime map，负责登记、查找、聚焦、关闭已加载的 `SessionRuntime`；它不是独立架构模块，也不读写 session entries。多个 `SessionRuntime` 可以同时 loaded/running，失去 focus 的 runtime 不会被隐式关闭。
- `SessionHandle` 是单会话领域对象，暴露追加条目、构建上下文、移动当前叶子、读取标签和会话名等行为。文档和实现中应优先使用 `SessionHandle`，避免把它简称为容易和完整会话概念混淆的 `Session`。
- `SessionStorage` 是单会话底层存储 interface，负责 entry、leaf 和 metadata 的读写。

`AgentRuntime` 对 UI 暴露会话命令和快照，但内部通过 `SessionManager` 打开会话、创建 `SessionHandle`、加载 `SessionRuntime`、切换 focused session。运行中产生的消息、配置变化和压缩条目最终由 `SessionRuntime` 通过 `SessionHandle` 写入 `SessionStorage`。`SessionManager` 不加载资源、不决定模型/provider/auth；`AgentRuntime` 提供的 factory 会读取 session metadata 中的 workspace/cwd，创建固定 cwd 的 `SessionRuntime`，由该 runtime 在每次 user turn 启动时通过 `ResourceManager.capture_turn(...)` 捕获当前 `TurnResourceSnapshot`。

`SessionIndex` 是 `SessionManager` 的轻量会话目录，不是 `RuntimeSnapshot`。它用于 `/resume`、GUI sidebar 和 `ListSessions`，可以由 JSONL header、session metadata、本地 index/cache 或数据库投影维护。它只包含 session id、workspace/cwd、名称、时间、预览、轻量模型/思考等级/usage 摘要和诊断，不包含完整 messages、当前 run、pending approval、队列或 UI 状态。打开窗口时不需要为了生成 `RuntimeSnapshot` 重建所有 session；只有用户执行 `OpenSession` 或创建新会话时才加载对应 `SessionRuntime` 并重建完整 session projection。

## Interface

```rust
pub trait SessionManager {
    async fn create(&self, options: CreateSession) -> Result<SessionHandle, SessionError>;
    async fn open(&self, metadata: SessionMetadata) -> Result<SessionHandle, SessionError>;
    async fn list(&self, filter: SessionListFilter) -> Result<Vec<SessionMetadata>, SessionError>;
    async fn delete(&self, metadata: SessionMetadata) -> Result<(), SessionError>;
    async fn fork(&self, source: SessionMetadata, options: ForkSession) -> Result<SessionHandle, SessionError>;

    async fn load_runtime(&self, handle: SessionHandle, factory: &dyn SessionRuntimeFactory) -> Result<SessionRuntimeHandle, SessionError>;
    async fn open_and_load(&self, metadata: SessionMetadata, factory: &dyn SessionRuntimeFactory) -> Result<SessionRuntimeHandle, SessionError>;
    async fn focus_session(&self, session_id: SessionId, policy: FocusSessionPolicy, factory: &dyn SessionRuntimeFactory) -> Result<SessionRuntimeHandle, SessionError>;
    async fn close_runtime(&self, session_id: SessionId, policy: ShutdownPolicy) -> Result<(), SessionError>;
    async fn shutdown_all_runtimes(&self, policy: ShutdownPolicy) -> Result<ShutdownReport, SessionError>;
    fn get_runtime(&self, session_id: SessionId) -> Option<SessionRuntimeHandle>;
    fn focused_runtime(&self) -> Option<SessionRuntimeHandle>;
}

pub trait SessionStorage {
    async fn get_metadata(&self) -> Result<SessionMetadata, SessionError>;
    async fn get_leaf_id(&self) -> Result<Option<EntryId>, SessionError>;
    async fn set_leaf_id(&self, leaf_id: Option<EntryId>) -> Result<(), SessionError>;
    async fn append_entry(&self, entry: SessionEntry) -> Result<(), SessionError>;
    async fn get_entry(&self, id: EntryId) -> Result<Option<SessionEntry>, SessionError>;
    async fn get_entries(&self) -> Result<Vec<SessionEntry>, SessionError>;
    async fn get_path_to_root(&self, leaf_id: Option<EntryId>) -> Result<Vec<SessionEntry>, SessionError>;
}
```

`SessionListFilter` 必须支持按 workspace scope 查询。TUI 的 `/resume` 默认列出当前 workspace 的会话；GUI sidebar 也应通过 `ListSessions(CurrentWorkspace)` 或显式 workspace scope 读取清单。`/resume --all`、全局 recent view 或跨 workspace 搜索可以作为后续增强，但默认不应把所有工作区的 session 混在一起。

运行时加载使用 factory，factory 由 `AgentRuntime` 提供并持有 `WorkspaceServices`。`SessionManager` 只把 `SessionHandle` 交给 factory；factory 读取 handle metadata 的 workspace/cwd，确保 `ResourceManager` 已有该 cwd 的 current `CwdResourceSnapshot`，并把 fixed workspace/cwd 与共享 runtime services 注入新 `SessionRuntime`。这样 `SessionManager` 不直接依赖 Rig、工具、资源、凭据或 `SessionRuntime` 构造细节，也不会把 focused session 的当前 cwd 或资源快照误传给后台 session：

```rust
pub trait SessionRuntimeFactory {
    async fn spawn(&self, handle: SessionHandle) -> Result<SessionRuntimeHandle, RuntimeError>;
}

pub struct LoadedSessionRuntimes {
    focused_session_id: Option<SessionId>,
    runtimes: HashMap<SessionId, SessionRuntimeHandle>,
}
```

`LoadedSessionRuntimes` 只允许做 live runtime lifecycle：`get`、`insert`、`replace`、`set_focused`、`close`、`shutdown_all` 和可选的 idle unload。不要在这里追加 message、构建上下文、执行工具、调用模型或发布 UI event，也不要根据 focus 切换改写已加载 runtime 的 workspace/cwd、phase、queue 或 current run。

`SessionHandle` 是 `SessionRuntime` 的主要消费对象：

```rust
pub struct SessionHandle { /* wraps Arc<dyn SessionStorage> */ }

impl SessionHandle {
    pub async fn append_message(&self, message: MessageRecord) -> Result<EntryId, SessionError>;
    pub async fn append_model_change(&self, provider_id: String, model_id: String) -> Result<EntryId, SessionError>;
    pub async fn append_active_tools_change(&self, tool_names: Vec<String>) -> Result<EntryId, SessionError>;
    pub async fn append_compaction(&self, result: CompactionResult) -> Result<EntryId, SessionError>;
    pub async fn move_to(&self, entry_id: Option<EntryId>) -> Result<(), SessionError>;
    pub async fn build_session_context(&self) -> Result<SessionContext, SessionError>;
}
```

这样 `SessionRuntime` 不需要知道当前会话来自 JSONL、内存还是未来的数据库实现。

## 追加式条目

会话条目采用 append-only 结构，每条都有 `id`、`parent_id` 和 `timestamp`。MVP 至少需要：

```rust
pub enum SessionEntry {
    Message { base: EntryBase, message: MessageRecord },
    ModelChange { base: EntryBase, provider_id: String, model_id: String },
    ActiveToolsChange { base: EntryBase, tool_names: Vec<String> },
    SessionInfo { base: EntryBase, name: Option<String> },
    Leaf { base: EntryBase, target_id: Option<EntryId> },
}
```

后续条目：

```rust
ThinkingLevelChange { level: String }
Compaction { summary: String, first_kept_entry_id: EntryId, tokens_before: u64, estimated_tokens_after: Option<u64>, details: CompactionDetails, from_hook: bool }
BranchSummary { from_id: EntryId, summary: String }
Custom { custom_type: String, data: serde_json::Value }
CustomMessage { custom_type: String, content: MessageContent, display: bool }
Usage { run_id: Option<RunId>, model_call_id: ModelCallId, purpose: UsagePurpose, provider_id: String, model_id: String, usage: TokenUsage, source: UsageSource }
Label { target_id: EntryId, label: Option<String> }
```

`Usage` entry 是中期增强，用于精确恢复每次模型调用的消耗事实。MVP 可以先把 run aggregate usage 存在 assistant message 或 run result 附近；无论哪种方式，实时 `SessionUsageStats` 仍由 `SessionRuntime` 归约，不由 `SessionStorage` 计算。

## 上下文构建

上下文构建参考 pi 的 `buildSessionContext(pathEntries)`：

- 只使用当前叶子从根到叶子的路径构建上下文。
- `model_change` 和 assistant message 可恢复当前模型。
- `active_tools_change` 恢复活跃工具集合。
- `thinking_level_change` 恢复思考等级。
- 最新 `compaction` 条目会把较早历史替换为压缩摘要，并从 `first_kept_entry_id` 之后继续保留消息。
- `branch_summary` 和 `custom_message` 可以转换成模型可见消息或 UI 可见消息。

`ModelChange` 保存的是 MiniCore `provider_id` / `model_id`。它不得保存 Rig provider type、provider API `api_model_name`、base URL、auth ref 或 credentials；这些执行细节由 [ModelGateway](model-gateway.md) 在每次调用前解析。

### 压缩后的消息组装

压缩是 append-only projection，不删除旧 entry。`build_session_context(path)` 只看当前 leaf 的 root-to-leaf path，并使用 path 上最新的 `Compaction` entry：

```text
if no compaction on path:
  messages = all context-visible messages on path

if latest compaction exists:
  messages = [CompactionSummaryMessage]
  messages += context-visible entries before compaction, starting at first_kept_entry_id
  messages += context-visible entries after compaction
```

例如：

```text
entry 1..79     summarized into compaction summary
entry 80..99    kept recent suffix before the compaction entry
entry 100       compaction entry itself
entry 101..end  normal messages after compaction
```

模型实际上下文为：

```text
system: prompt::build_system_prompt(...)
user:   The conversation history before this point was compacted into the following summary:
        <summary>
        ...
        </summary>
...     kept messages from first_kept_entry_id onward
...     messages after compaction
```

`CompactionSummaryMessage` 是模型可见历史消息，不是 system prompt。UI 可以把它渲染成折叠的 `[compaction]` 消息；模型转换时应把它转成 user-role text message。

如果 `first_kept_entry_id` 在当前 path 上找不到，`build_session_context` 应返回结构化诊断并退化为 `CompactionSummaryMessage + entries after compaction`，不要静默把整段旧历史重新放回上下文。

普通 assistant overflow error 如果只是为了 UI 展示而持久化，必须标记为非模型可见，或写入 diagnostic/custom entry。append-only storage 不能依赖运行时临时删除内存 messages 来避免它进入 retry prompt。

## JSONL 持久化

JSONL 存储参考 pi `JsonlSessionStorage`：

- 第一行是 session header，包含 `type: "session"`、版本号、session id、创建时间、cwd 和可选父会话路径。
- 后续每行是一条 session entry。
- `leaf` 条目用于记录当前活动叶子；追加普通条目后，当前叶子默认变为该条目。
- 加载时要校验 header、entry id、parentId 和 timestamp。
- session 文件按工作区目录分组，列表按创建时间倒序。

MVP 可以同时提供两种持久化实现：

- `InMemorySessionManager` + `InMemorySessionStorage`：用于测试、原型和无持久化运行。
- `JsonlSessionManager` + `JsonlSessionStorage`：用于桌面应用真实持久化。

两种 `SessionManager` 都应复用同一个 `LoadedSessionRuntimes` 行为；差异只在 persistent catalog 和 `SessionStorage` adapter。

## 会话命令行为

- `NewSession` 创建新会话并发出 `session_created` / `session_opened`。
- `OpenSession` 打开已有会话，必要时加载 `SessionRuntime`，并通常发出 `session_focus_changed`。
- `FocusSession` 切换 runtime-visible focused session；如果目标未 loaded，可以拒绝，也可以按产品策略先执行 open-and-load。
- `ListSessions` 返回工作区下的会话元数据。
- `DeleteSession` 删除未被当前运行占用的会话。
- `SetSessionName` 追加 `session_info` 条目，而不是修改 header。
- `ForkSession` 复制源会话中目标路径上的条目到新会话。
- `NavigateSessionTree` 通过追加 `leaf` 条目移动当前叶子，而不是删除历史。

运行中产生的消息和配置变化必须由 `SessionRuntime` 写入会话。`Driver` 只产出持久化前事件和 message records，最终由 `SessionRuntime` 追加条目并发出 `persistence_save_point`。

## 已加载会话运行时

`LoadedSessionRuntimes` 是 `SessionManager` 的内部组件，用来表达“哪些 session runtime 正活着”。它替代独立的 `SessionRuntimeRegistry` 架构层。

典型生命周期：

```text
OpenSession
  → SessionManager.open(metadata) -> SessionHandle
  → SessionManager.load_runtime(handle, factory) -> SessionRuntimeHandle
  → LoadedSessionRuntimes.set_focused(session_id)
  → AgentRuntime publishes session_opened? and session_focus_changed

SubmitPrompt
  → AgentRuntime asks SessionManager.get_runtime(session_id)
  → SessionRuntimeHandle.dispatch(...)

CloseSession
  → SessionManager.close_runtime(session_id, policy)
  → LoadedSessionRuntimes.remove(session_id)

AppShutdown
  → SessionManager.shutdown_all_runtimes(policy)
```

`SessionManager` 可以协调 live runtime 生命周期，但不能拥有 `SessionRuntime` 内部状态机。`SessionRuntime` 仍然拥有 phase、queue、current run、tool state、usage stats、compaction/retry state 和 persistence save point。

Focused session 是 runtime-visible state。它只表示 UI 或默认命令目标，不表示唯一 loaded session，也不表示唯一 running session。多 session 同时运行时，失去 focus 的 session 可以继续执行后台 run。

## 与 pi 的对应

```text
pi coding-agent SessionManager
  ≈ 本项目 SessionManager + SessionHandle + SessionStorage

pi coding-agent AgentSessionRuntime session replacement
  ≈ 本项目 AgentRuntime + SessionManager loaded runtime lifecycle

Codex ThreadManager
  ≈ 本项目 SessionManager 的 persistent catalog + LoadedSessionRuntimes 协调

Codex ThreadStore
  ≈ 本项目 SessionStorage / 持久化 adapter

Codex LiveThread
  ≈ 本项目 SessionHandle 的 active persistence handle 部分

pi-agent-core SessionRepo
  ≈ 本项目 SessionManager

pi-agent-core Session
  ≈ 本项目 SessionHandle

pi-agent-core SessionStorage
  ≈ 本项目 SessionStorage
```

本项目保留 pi 的 `SessionManager` 命名，并吸收 Codex `ThreadManager` 协调 live threads 与 store 的经验；但底层 storage seam 和单会话运行编排仍拆开，便于测试、替换 JSONL / memory 实现，并避免 `SessionManager` 变成 Agent loop。
