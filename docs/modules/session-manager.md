# SessionManager / SessionWriter / SessionStorage

这个文档描述工作区内会话生命周期、会话持久化与会话树模块，而不是 `SessionRuntime` 的运行编排本身。它之所以独立存在，是因为会话管理有自己的可测试 interface：创建/打开/列出/删除/fork 会话、加载/关闭会话运行时、提交稳定 batch、读取当前 leaf、从 root 到 leaf 重建上下文。

会话属于 Agent Runtime，不属于 UI。UI 通过运行时 command 执行 open/fork/navigation/delete 等 mutation，通过 `RuntimeQuery` 列出或读取会话；不能直接读写会话文件。

本项目采用广义 `SessionManager`：它同时协调持久化会话目录和已加载会话运行时；底层单会话存储仍叫 `SessionStorage`。pi coding-agent 和 Codex 的会话/线程管理可以作为参考对象，但不构成类型、文件格式或生命周期兼容承诺。

## 核心模型

- `SessionManager` 是工作区内会话生命周期 facade，负责 persistent session catalog / `SessionIndex`、loaded runtime map、create/open/list/delete/fork/close，并隐藏内存存储与 JSONL 文件存储的差异。
- `LoadedSessionRuntimes` 是 `SessionManager` 内部的 live runtime map，负责登记、查找和关闭已加载的 `SessionRuntime`；它不是独立架构模块，也不读写 session entries。多个 `SessionRuntime` 可以同时 loaded，并各自独立处于 idle、turn、compaction 或 retry phase。
- `SessionHandle` 是单会话领域对象，暴露稳定 batch commit、构建上下文、读取当前叶子、标签和会话名等行为。文档和实现中应优先使用 `SessionHandle`，避免把它简称为容易和完整会话概念混淆的 `Session`。
- `SessionWriter` 是所有会话 mutation 共用的唯一写入 seam。它接受 `SessionWriteBatch`，成功返回表示整个 batch 已按 adapter 契约写入，失败则该 batch 不得进入恢复投影。
- `SessionStorage` 是单会话底层存储 interface，也是 `SessionWriter` 的 adapter；它负责读取 committed batches、leaf 和 metadata，并隐藏内存存储与 JSONL 存储的差异。

`AgentRuntime` 对 adapter 暴露会话命令和快照，但内部只通过 `SessionManager` 打开会话、创建 `SessionHandle` 和加载 `SessionRuntime`。运行中产生的消息、配置变化和压缩条目最终由 `SessionRuntime` 通过 `SessionHandle` 写入 `SessionStorage`。`SessionManager` 不保存客户端 selected session，也不加载资源、不决定模型/provider/auth；`AgentRuntime` 提供的 factory 会读取 session metadata 中的 workspace/cwd，创建固定 cwd 的 `SessionRuntime`，由该 runtime 在每次 user turn 启动时通过 `ResourceManager.capture_turn(...)` 捕获当前 `TurnResourceSnapshot`。

session 的状态分层而不是压成一个 `active` / `running` boolean：persistent catalog membership 表示会话是否存在；`LoadedSessionRuntimes` 表示 runtime residency；loaded runtime 的工作状态由 `SessionPhase::{Idle, Turn, Compaction, RetryBackoff}`、optional `CurrentRunState` 和派生的 `session_settled` 表达。run failed/aborted 不会把 session 变成 terminal session，compaction/retry 也不能被一个 `is_running` 准确表示。

`SessionIndex` 是 `SessionManager` 的轻量会话目录，不是 `RuntimeSnapshot`。它用于 `/resume` 和 `SessionQuery::List`，可以由 JSONL header、session metadata、本地 index/cache 或数据库投影维护。它只包含 session id、workspace/cwd、名称、时间、预览、轻量模型/思考等级/usage 摘要和诊断，不包含完整 messages、当前 run、pending approval、队列或 UI 状态。打开窗口时不需要为了生成 `RuntimeSnapshot` 重建所有 session；只有用户执行 `OpenSession` 或创建新会话时才加载对应 `SessionRuntime` 并重建完整 session projection。

## Interface

```rust
#[async_trait]
pub trait SessionManager {
    async fn create(&self, options: CreateSession) -> Result<SessionHandle, SessionError>;
    async fn open(&self, metadata: SessionMetadata) -> Result<SessionHandle, SessionError>;
    async fn list(&self, filter: SessionListFilter) -> Result<Vec<SessionMetadata>, SessionError>;
    async fn delete(&self, metadata: SessionMetadata) -> Result<(), SessionError>;
    async fn fork(&self, source: SessionMetadata, options: ForkSession) -> Result<SessionHandle, SessionError>;

    async fn load_runtime(&self, handle: SessionHandle, factory: &dyn SessionRuntimeFactory) -> Result<SessionRuntimeHandle, SessionError>;
    async fn open_and_load(&self, metadata: SessionMetadata, factory: &dyn SessionRuntimeFactory) -> Result<SessionRuntimeHandle, SessionError>;
    async fn close_runtime(&self, session_id: SessionId, policy: ShutdownPolicy) -> Result<(), SessionError>;
    async fn shutdown_all_runtimes(&self, policy: ShutdownPolicy) -> Result<ShutdownReport, SessionError>;
    fn get_runtime(&self, session_id: SessionId) -> Option<SessionRuntimeHandle>;
    fn list_loaded_runtimes(&self) -> Vec<SessionRuntimeHandle>;
}

#[async_trait]
pub trait SessionWriter: Send + Sync {
    async fn commit(&self, batch: SessionWriteBatch) -> Result<CommittedSessionBatch, SessionWriteError>;
}

#[async_trait]
pub trait SessionStorage: SessionWriter {
    async fn get_metadata(&self) -> Result<SessionMetadata, SessionStorageError>;
    async fn get_leaf_id(&self) -> Result<Option<EntryId>, SessionStorageError>;
    async fn get_entry(&self, id: EntryId) -> Result<Option<SessionEntry>, SessionStorageError>;
    async fn get_entries(&self) -> Result<Vec<SessionEntry>, SessionStorageError>;
    async fn get_path_to_root(&self, leaf_id: Option<EntryId>) -> Result<Vec<SessionEntry>, SessionStorageError>;
    async fn get_committed_batches_to_leaf(
        &self,
        leaf_id: Option<EntryId>,
    ) -> Result<Vec<StoredSessionBatch>, SessionStorageError>;
}
```

写入类型保持最小，不引入 `SessionRevision`：

```rust
pub struct SessionWriteBatch {
    purpose: SessionWritePurpose,
    entries: Vec<SessionEntryDraft>,
    leaf: BatchLeafUpdate,
}

pub enum SessionWritePurpose {
    UserInput,
    ToolRound,
    AssistantFinal,
    Compaction,
    SessionMutation,
    TreeMutation,
}

pub enum BatchLeafUpdate {
    AdvanceToLastEntry,
    MoveTo(EntryId),
}

pub struct CommittedSessionBatch {
    pub entry_ids: Vec<EntryId>,
    pub leaf_id: EntryId,
}

pub struct StoredSessionBatch {
    pub purpose: SessionWritePurpose,
    pub entries: Vec<SessionEntry>,
    pub leaf_id: EntryId,
}

pub enum SessionWriteError {
    InvalidBatch { reason: String },
    InvalidLeafTarget { target: EntryId, reason: LeafTargetErrorKind },
    CorruptStorage { reason: String },
    Io { message: String },
    StorageUnavailable { reason: String },
}

pub enum SessionStorageError {
    InvalidLeafTarget { target: EntryId, reason: LeafTargetErrorKind },
    CorruptStorage { reason: String },
    Io { message: String },
    StorageUnavailable { reason: String },
}

pub enum LeafTargetErrorKind {
    NotFound,
    NotStableBatchBoundary,
}
```

`get_path_to_root(...)` 与 `get_committed_batches_to_leaf(...)` 只接受 `None` 或 stable batch-boundary leaf；传入 interior entry 必须返回结构化 invalid-target error，不能截断 batch。

`StoredSessionBatch` 是 storage 的只读 grouped projection：`get_committed_batches_to_leaf(...)` 返回构成所选 root-to-leaf path 的完整 append batches，不拆分多-entry `ToolRound`，也不把历史 `TreeMutation` records 当作 transcript batch 重放。fork 和 corruption validation 必须使用该 grouped interface，不能从 flat `get_entries()` 猜 batch 边界。

`SessionEntryDraft` 只携带 message/config/compaction 等领域内容，不携带 `EntryBase`、entry id、parent id 或 timestamp；这些存储身份由 writer 在一次 commit 内统一生成。`BatchLeafUpdate::MoveTo(...)` 的目标必须是某个 committed append batch 的最后一个 entry（stable batch boundary）；writer 必须拒绝 `ToolRound` 的 assistant/intermediate result 等 interior entry。batch-boundary 校验和 leaf metadata 由 writer 在同一 commit 内处理。`SessionRuntime` 不能预分配 entry ids 或自行连接 session tree。

稳定单元只能通过 validated constructors 构造；batch fields 不公开，调用方不能手填 purpose/entries/leaf 组合：

```rust
SessionWriteBatch::user_input(message)
SessionWriteBatch::tool_round(assistant, ordered_results)
SessionWriteBatch::assistant_final(message)
SessionWriteBatch::compaction(result)
SessionWriteBatch::session_mutation(drafts)
SessionWriteBatch::tree_move(target_entry_id)
```

`commit()` 的结果是可信写入结果，不是 best effort notification：

- `Ok(CommittedSessionBatch)`：整个 batch 和 leaf update 已按 adapter 的进程崩溃恢复契约提交，调用方可以发布对应领域事件或继续下一模型调用。
- `Err(SessionWriteError)`：调用方不得依赖 batch 中的 entry；adapter 必须回滚/排除失败 payload。若无法确认或恢复写入尾部，则该 storage 必须进入不可读写的 fatal state，不能静默继续投影或再次 append。
- 该契约不默认承诺断电级 durability；具体保证由 JSONL、memory 或未来 database adapter 文档说明。
- `SessionRuntime`、`Driver`、`Tools`、command handler 和 hook 都不能绕过 `SessionHandle.commit(...)` / `SessionWriter.commit(...)` 直接追加 entry；model/thinking/tools/name 等 runtime-visible state 也只能在 commit 成功后替换。
- 同一 session 的 `commit()` 必须由 writer 内部串行化；entry ids、parent links 和 leaf 都基于上一个成功 committed batch 生成。调用方不负责加锁，也不能并发猜测 parent/leaf。
- 同一 session 的 actor ordering 决定 abort 与 commit 谁先发生：abort 在 `commit()` 调用前被观察到时可以丢弃尚未提交 unit；`commit()` 一旦开始就不接收 run cancellation token，`AbortRun`、session close 和 graceful shutdown 不能把 writer future 半途取消。owner 必须等待本次 commit 得到 `Ok` / `Err` 后再完成 terminal handling；强制退出或 deadline 到期属于 crash path，由 adapter 的尾行恢复规则处理。

`SessionListFilter` 必须支持按 workspace scope 查询。TUI 的 `/resume` 默认列出当前 workspace 的会话；GUI sidebar 通过 `RuntimeQuery::Session(SessionQuery::List { scope: CurrentWorkspace, ... })` 或显式 workspace scope 读取清单。`/resume --all`、全局 recent view 或跨 workspace 搜索可以作为后续增强，但默认不应把所有工作区的 session 混在一起。

运行时加载使用 factory，factory 由 `AgentRuntime` 提供并持有 `WorkspaceServices`。`SessionManager` 只把 `SessionHandle` 交给 factory；factory 读取 handle metadata 的 workspace/cwd，确保 `ResourceManager` 已有该 cwd 的 current `CwdResourceSnapshot`，并把 fixed workspace/cwd 与共享 runtime services 注入新 `SessionRuntime`。这样 `SessionManager` 不直接依赖 Rig、工具、资源、凭据或 `SessionRuntime` 构造细节，也不会把某个 adapter 选中 session 的 cwd 或资源快照误传给其他 session：

```rust
pub trait SessionRuntimeFactory {
    async fn spawn(&self, handle: SessionHandle) -> Result<SessionRuntimeHandle, RuntimeError>;
}

pub struct LoadedSessionRuntimes {
    runtimes: HashMap<SessionId, SessionRuntimeHandle>,
}
```

`LoadedSessionRuntimes` 只允许做 live runtime lifecycle：`get`、`list`、`find_current_run(run_id)`、`insert`、`replace`、`close`、`shutdown_all` 和可选的 idle unload。`find_current_run` 只扫描/索引当前 host 中已经发布 `run_started` 且尚未 terminal 的 run，用于路由 `AbortRun { run_id }`；它不是 durable run registry。不要在这里追加 message、构建上下文、执行工具、调用模型、保存 adapter selection 或发布 UI event。

`SessionHandle` 是 `SessionRuntime` 的主要消费对象：

```rust
pub struct SessionHandle { /* wraps Arc<dyn SessionStorage> */ }

impl SessionHandle {
    pub async fn commit(&self, batch: SessionWriteBatch) -> Result<CommittedSessionBatch, SessionWriteError>;
    pub async fn build_session_context(&self) -> Result<SessionContext, SessionError>;
}
```

`SessionHandle` 可以把 `SessionStorageError` 包装进领域级 `SessionError::Storage(...)`，但不能把 `InvalidLeafTarget { reason: NotStableBatchBoundary }` 降级成字符串或 generic not-found；protocol command 需要据此返回结构化 invalid target。

这样 `SessionRuntime` 不需要知道当前会话来自 JSONL、内存还是未来的数据库实现，也不会把完整 tool round 拆成多个独立 append 调用。

## 稳定写入单元

只有协议完整、可以独立恢复的事实才能进入 `SessionWriteBatch`：

```text
UserInput
  → one user message

ToolRound
  → one complete assistant message containing tool calls
  → all matching tool result messages in call_index order

AssistantFinal
  → one complete final assistant message without unresolved tool calls

Compaction / SessionMutation / TreeMutation
  → the complete entries and leaf update for that mutation
```

assistant/tool/compaction drafts 中的 usage 必须先转换为 `PersistedModelCallUsage`；`SessionWriteBatch` constructors / writer 必须拒绝携带 `raw_provider_usage` 的 durable payload。

streaming delta、partial assistant、pending approval、执行中的 tool round、tool output delta、queue state 和其他 `CurrentRun` 状态只存在于内存，不得提前写入 session。`ToolRound` 必须整体提交；任何下一模型调用只有在对应 `commit()` 成功后才能开始。写入失败使当前 run 进入 failed terminal path，不能降级为继续使用仅存在内存的历史。

abort、failure、session close 或 host shutdown 时，已经成功提交的 batch 保留；当前尚未提交的 partial assistant、approval wait 或 incomplete tool round 直接丢弃。MVP 不为这些内存状态合成 tool result，也不在下一次 open 时恢复旧 run。若工具已经产生外部副作用但 batch 尚未提交，workspace 与 session history 可能不一致；MiniCore 明确不承诺 tool exactly-once 或文件系统与 session storage 的跨系统原子事务。

## 追加式条目

会话条目采用 append-only 结构，每条都有 `id`、`parent_id` 和 `timestamp`。MVP 至少需要：

```rust
pub enum SessionEntry {
    Message { base: EntryBase, message: MessageRecord },
    ModelChange { base: EntryBase, provider_id: String, model_id: String },
    ActiveToolsChange { base: EntryBase, tool_names: Vec<String> },
    SessionInfo { base: EntryBase, name: Option<String> },
}
```

后续条目：

```rust
ThinkingLevelChange { level: String }
Compaction { summary: String, first_kept_entry_id: EntryId, tokens_before: u64, estimated_tokens_after: Option<u64>, details: CompactionDetails, usage: Option<PersistedModelCallUsage>, from_hook: bool }
BranchSummary { from_id: EntryId, summary: String }
Custom { custom_type: String, data: serde_json::Value }
CustomMessage { custom_type: String, content: MessageContent, display: bool }
Usage { fact: PersistedModelCallUsage }
Label { target_id: EntryId, label: Option<String> }
```

leaf move 不再使用 `SessionEntry::Leaf`。每个 committed `session_batch` 自带 `BatchLeafUpdate`，它是 memory/JSONL adapter 共用的唯一 current-leaf source of truth；`get_path_to_root()` 只遍历实际领域 entries，不需要跳过 marker entry。所有追加 entries 的 batch（包括 metadata-only `SessionMutation`）都使用 `AdvanceToLastEntry`，确保新事实位于 current root-to-leaf path 并能参与恢复；只有不追加 entry 的导航 batch 使用 `MoveTo(target)`；target 必须是 stable batch boundary，不能把 leaf 移到多-entry batch 内部。

`Usage` entry 是中期增强，用于精确恢复失败调用或不伴随 stable assistant message 的消耗事实。MVP 只把对应 `PersistedModelCallUsage` 跟随 committed tool-call assistant、final assistant 或 compaction fact 保存；run aggregate 只用于当前 host 的 `run_finished` / UI view，不能重复写入每条 message。实时 `SessionUsageStats` 仍由 `SessionRuntime` 归约，不由 `SessionStorage` 计算；crash 前未进入 stable batch 的 usage 不保证恢复。

## 上下文构建

上下文构建遵循以下权威规则：

- 只读取成功 committed 的 `SessionWriteBatch`；JSONL 末尾未完成 batch 不进入投影。
- 只使用当前叶子从根到叶子的路径构建上下文；current leaf 必须是 committed stable batch boundary，因此路径不会终止在 `ToolRound` 内部。
- `model_change` 和 assistant message 可恢复当前模型。
- `active_tools_change` 恢复活跃工具集合。
- `thinking_level_change` 恢复思考等级。
- 最新 `compaction` 条目会把较早历史替换为压缩摘要，并从 `first_kept_entry_id` 之后继续保留消息。
- `branch_summary` 和 `custom_message` 可以转换成模型可见消息或 UI 可见消息。
- 每个 committed `ToolRound` 必须包含完整 assistant tool calls 与全部 matching tool results；若已提交数据违反该 invariant，返回结构化 corruption error 并 fail closed，不做 synthetic repair。

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
system: PromptTurn.profile.system_prompt
user:   The conversation history before this point was compacted into the following summary:
        <summary>
        ...
        </summary>
...     kept messages from first_kept_entry_id onward
...     messages after compaction
```

`CompactionSummaryMessage` 是模型可见历史消息，不是 system prompt。UI 可以把它渲染成折叠的 `[compaction]` 消息；模型转换时应把它转成 user-role text message。

如果 `first_kept_entry_id` 在当前 path 上找不到，`build_session_context` 应返回结构化诊断并退化为 `CompactionSummaryMessage + entries after compaction`，不要静默把整段旧历史重新放回上下文。

普通 assistant overflow error 和 partial assistant 只通过当前 host diagnostics / streaming lifecycle 展示，不进入 `SessionWriter`。overflow recovery 从最后 committed context 重建，不能把运行中临时 messages 当成 durable history。

## JSONL 持久化

JSONL 存储格式：

- 第一行是 session header，包含 `type: "session"`、版本号、session id、创建时间、cwd 和可选父会话路径。header 属于 storage 初始化：`SessionManager.create(...)` 通过 adapter factory 以临时文件 + atomic rename 创建完整 header，成功后才返回 `SessionHandle` / 发布 `session_created`；handle 存在后的所有领域 mutation 都必须走 `SessionWriter.commit(...)`。
- 后续每行是一个完整 `session_batch`，包含 purpose、entries 和 leaf update；一个 `ToolRound` 不得跨多行。`SessionIndex` 若单独落盘只能作为可重建 catalog/cache，不得成为绕过 writer 的第二份 transcript source of truth。
- writer 接收 `SessionEntryDraft`s，先在内存中生成全部 entry ids、timestamps、parent links 和最终 leaf，再把整个 batch 序列化为一次 append payload；记录 append 前文件长度，OS 接受包含结尾换行的完整 record payload 后才返回 `CommittedSessionBatch`。写入返回错误时先 truncate 回原长度；truncate 失败则 storage 进入不可读写的 fatal state，后续 open 必须 fail closed 直到显式修复。JSONL MVP 不用 `fsync` 声称断电级 durability。
- loader 只接受以换行终止且可完整解析/校验的 records；即使尾部 JSON 本身可解析，只要没有结尾换行也视为未完成 tail。进程在 batch 写入中崩溃时，只允许忽略或截断这个末尾 tail；再次 append 前必须先把文件物理截断到最后一个完整换行，无法截断时 storage 只读/报错。中间行损坏、重复 entry id、非法 parentId 或不完整 committed tool round 都是结构化 corruption error。
- JSONL adapter 不需要 `batch_begin` / `batch_commit` 双标记，也不引入数字 session revision；最后一个完整 batch 的 leaf 是恢复位置。
- session 文件按工作区目录分组，列表按创建时间倒序。

MVP 可以同时提供两种持久化实现：

- `InMemorySessionManager` + `InMemorySessionStorage`：用于测试、原型和无持久化运行。
- `JsonlSessionManager` + `JsonlSessionStorage`：用于桌面应用真实持久化。

两种 `SessionManager` 都应复用同一个 `LoadedSessionRuntimes` 行为；差异只在 persistent catalog 和 `SessionStorage` adapter。

## 会话命令行为

- `NewSession` 创建新会话并发出 `session_created` / `session_opened`。
- `OpenSession` 打开已有会话，必要时加载 `SessionRuntime` 并发出 `session_opened`；目标已 loaded 时是幂等 no-op，不改变任何客户端选择。
- `SessionQuery::List` 通过 `AgentRuntime.query()` 返回工作区下的会话元数据；底层复用 `SessionManager.list_sessions(...)`。
- `DeleteSession` 删除未被当前运行占用的会话。
- `SetSessionName` 通过 `SessionMutation` batch 提交 `session_info` draft，而不是修改 header。
- `ForkSession` 通过 `get_committed_batches_to_leaf(...)` 读取源会话目标路径上的完整 `StoredSessionBatch` records，先验证 `Compaction.first_kept_entry_id`、`BranchSummary.from_id`、`Label.target_id` 和其他 entry references 都位于 selection 中且只向后引用，再按源 batch 顺序通过 staging target writer replay。每次 commit 返回 `entry_ids` 后，fork 按源 batch entry 顺序增量扩展 `old_entry_id -> new_entry_id` 映射，下一批 draft 只使用已建立的映射重写 references。任何 reference 若超出可复制 closure 或形成 forward/dangling reference，必须返回结构化 `ForkReferenceOutsideSelection` / corruption error，不能保留 source id。全部 replay 成功后才 atomic publish target file、更新 `SessionIndex` 并发布 `session_created`；失败或 crash 留下的 staging target 不得出现在 session list。
- `NavigateSessionTree` 通过 `TreeMutation` batch 提交 leaf move，而不是删除历史。协议仍携带 `EntryId`，但 query/tree view 只应暴露可导航 batch-boundary ids；writer 对 interior/nonexistent target 返回 `InvalidLeafTarget`。

运行中产生的稳定消息和配置变化必须由 `SessionRuntime` 组装为 `SessionWriteBatch`，再通过 `SessionHandle.commit(...)` 写入。`Driver` 只产出持久化前事件和 message records；对应领域事件只能在 commit 成功后发布。

## 已加载会话运行时

`LoadedSessionRuntimes` 是 `SessionManager` 的内部组件，用来表达“哪些 session runtime 正活着”。它替代独立的 `SessionRuntimeRegistry` 架构层。

典型生命周期：

```text
OpenSession
  → SessionManager.open(metadata) -> SessionHandle
  → SessionManager.load_runtime(handle, factory) -> SessionRuntimeHandle
  → LoadedSessionRuntimes.insert(session_id, runtime)
  → AgentRuntime publishes session_opened if newly loaded

SubmitPrompt
  → AgentRuntime asks SessionManager.get_runtime(session_id)
  → SessionRuntimeHandle.dispatch(...)

CloseSession
  → SessionManager.close_runtime(session_id, policy)
  → runtime stops new command/work admission and clears deferred queues
  → runtime cancels provider/tool/approval work, but not an in-flight SessionWriter.commit(...)
  → await terminal handling and the in-flight commit result according to ShutdownPolicy
  → LoadedSessionRuntimes.remove(session_id)

AppShutdown
  → SessionManager.shutdown_all_runtimes(policy)
  → apply the same close protocol to all loaded runtimes
  → forced deadline expiry exits through the documented crash-recovery path
```

`SessionManager` 可以协调 live runtime 生命周期，但不能拥有 `SessionRuntime` 内部状态机。`SessionRuntime` 仍然拥有 phase、queue、current run、tool state、usage stats、compaction/retry state 和稳定 batch 的提交时机。

根据 [ADR 0020](../adr/0020-agent-runtime-has-no-current-session.md)，客户端选择哪个 session 属于 adapter-local state，不进入 `SessionManager`。所有 session-scoped runtime command 必须显式携带 `SessionId`；同一 host 中多个 loaded session 可以同时推进各自 work，打开或关闭一个 session 不改变其他 runtime 的 phase、queue、current run 或 cwd，也不替客户端选择 fallback session；adapter 收到 matching `session_closed` 后自行更新本地 selection。

`SessionManager`、`SessionHandle`、`SessionWriter`、`SessionStorage` 和 `LoadedSessionRuntimes` 的含义只由本文件定义。底层 storage seam 与单会话运行编排保持分离，以便测试和替换 JSONL / memory adapter，并避免 `SessionManager` 变成 Agent loop owner。

## 必测项

- 同时 load 两个 session 时 `LoadedSessionRuntimes.list()` 返回二者，任一 session 的 phase/current run 变化不影响另一个；不存在 focused/current pointer。
- 重复 `OpenSession` 是幂等 no-op；关闭一个 loaded runtime 只移除目标 session，客户端 selection 不参与 lifecycle。
- `commit(SessionWriteBatch::user_input(...))` 成功后，message、entry ids、parent links 和 leaf 一起可恢复。
- `commit(SessionWriteBatch::tool_round(...))` 只接受完整 assistant tool calls 与全部 matching results，并保持 `call_index` 顺序。
- writer 返回错误时，失败 batch 不出现在 `get_entries()`、path-to-root 或 `build_session_context()` 中。
- JSONL 在最后一个 batch 写到一半或只缺结尾换行时，重新加载忽略或截断尾 record，并恢复到上一个换行终止的完整 batch；再次 commit 前必须物理截断坏尾行，不能把新 JSON 接在残缺 payload 后。
- JSONL 中间行损坏、重复 entry id、非法 parent link 或 committed incomplete tool round 返回结构化 corruption error，不静默修复。
- InMemory adapter 对 batch entries 与 leaf update 原子可见。
- tree move 只接受 committed append batch 的最后一个 entry；指向 `ToolRound` assistant/intermediate result 的 interior target 返回 `InvalidLeafTarget`。
- fork 只通过 `get_committed_batches_to_leaf(...)` 复制 committed source path，保持原 batch grouping，并通过目标 writer 重新生成 entry ids / parent links。
- fork 每 replay 一个 grouped batch，就按 source entries 与 `CommittedSessionBatch.entry_ids` 的同序对应增量扩展 id map；必须重写 compaction、branch summary、label 和其他 backward entry-id references，目标 session 不得保留 source entry id；任一 replay commit 失败时 staging target 不进入 session list，也不发布 `session_created`。
- abort/failure/shutdown 不提交 partial assistant、pending approval 或 incomplete tool round。
- abort/close/shutdown 与 in-flight stable commit 竞态：writer future 不被 run cancellation 半途取消；先得到 commit 结果，再完成 terminal handling，且不得启动后续模型调用。
- tool executor 已产生副作用但 batch 未 commit 时，不自动重放工具；session 只恢复最后 committed context。
