# MiniCore Runtime v0.2 精简重构实施规格

> 适用仓库：`zqcli/minicore-runtime`  
> 审查基线：`dev@5088bc254548b3e80e87179898ebb7abbea52c7d`  
> 基线提交：`fix: bound session progress scheduling`  
> 文档目标：可直接交给代码模型，按任务逐步完成破坏性重构  
> 方案定位：**v0.2 Core Reset，不以兼容 v0.1 Wire/API 为首要目标**

---

## 0. 使用说明与边界

本方案不是一次“整理目录”或“把大文件拆成小文件”的表面重构，而是先收缩产品边界，再重建职责清晰、逻辑闭合的 coding-agent runtime core。

本报告基于上述固定提交进行源码静态审查。仓库 README 宣称现有测试门禁全部通过，但本次审查没有在完整仓库副本上独立复跑所有测试；实施任务 `P0` 必须先在可构建环境中复现基线结果，并将结果保存到重构分支。

### 0.1 本文中的关键词

- **删除**：最终目标代码中不得继续存在；迁移期可临时保留，但切换 `lib.rs` 后必须移除。
- **重写**：不得在原巨型实现上继续叠加条件分支；应建立小模块和新数据模型，再迁移必要行为。
- **保留语义**：保留用户可观察能力，不要求保留内部类型、错误码、Wire 表示或旧方法签名。
- **可选能力**：不得进入默认核心路径；应放入 feature、独立 crate、示例或由宿主实现。
- **完成**：代码、测试、文档、依赖清理全部满足对应验收条件，而不是“可以编译”即结束。

### 0.2 总体结论

推荐做一次允许破坏性变更的 **v0.2 Core Reset**：

1. 保留一个 coding agent 真正需要的闭环：会话、模型流式调用、模型—工具循环、文件工具、命令工具、用户交互、取消、事件、持久化、恢复和压缩。
2. 删除或外移平台化管理面：Agent durable entity、Agent revision/status/metadata、Session 定义 CAS、Fork/Archive、Wire V1、分页游标、共享资源热重载、多层 readiness fan-out、workspace authority 动态失效恢复。
3. 将三层 actor/owner-proof/gate 收敛为：`Runtime + SessionManager + 每 Session 一个 actor + 每次一个 ActiveTurn`。
4. 将当前 closed ToolSet 改造成小型通用 `ToolRegistry`，并新增当前项目明确缺失、但 coding agent 必需的 `run_command`。
5. 将 Durable Store V1 的 generation/lease/reservation/publication 状态机改为：单进程可选根锁、原子 metadata 文件、每 Session append-only JSONL。
6. 把 Wire 从核心 crate 移走；Rust library 对宿主暴露 typed API。需要 HTTP/JSON 的宿主自行序列化，或使用独立 `minicore-wire-legacy` crate。

---

## 1. 当前项目诊断

### 1.1 当前产品范围已经超过“轻量 runtime core”

当前 README 同时覆盖：Runtime 协议、Session control actor、Turn loop、资源加载、技能、工具、事件、录制、压缩、ModelGateway、usage、Agent/Session 管理、Fork、Archive、CAS、共享资源重载、安全 authority invalidation、复杂 readiness 恢复等。对单一 coding-agent core 来说，这些能力不是同一优先级，却被放进一个 crate、一个运行时和一组强耦合状态机中。

公开接入面目前是 transport 风格的：

```rust
MiniCoreRuntime::dispatch(CommandRequest)
MiniCoreRuntime::query(RuntimeQuery)
MiniCoreRuntime::snapshot(SnapshotRequest)
MiniCoreRuntime::subscribe(SubscriptionRequest)
MiniCoreRuntime::session_transcript(SessionId, PageRequest)
```

这使核心库承担了“领域模型 + 应用服务 + 协议路由 + Wire codec + 服务端幂等/分页”的多重职责。

同时，README 明确把 `process adapter`、generic Tool registry/policy/Sandbox 和 public Tool DTO 放在 Post-MVP。也就是说，当前实现为大量管理和恢复边界付出了复杂度，却还不能独立完成 coding agent 最关键的“运行编译、测试、格式化、搜索等命令”能力。

### 1.2 复杂度集中点

以下行数取固定基线的 GitHub raw 文件元数据；行数包含文件内测试：

| 文件 | 约行数 | 主要问题 |
|---|---:|---|
| `src/durable_state.rs` | 31,234 | generation、lease、reservation、staging、COMMITTED/PUBLISHED、recovery、Agent/Session 双领域全部集中 |
| `src/session_execution.rs` | 25,073 | 会话 actor、admission、publication、security、progress、tool round、compaction、retry、队列、测试 hook 全部集中 |
| `src/runtime.rs` | 13,445 | 配置、公开 facade、通用 dispatch/query、分页、幂等、事件发布、错误映射、关闭协调集中 |
| `src/wire/public_protocol.rs` | 9,667 | 大量 Input/Output 镜像 DTO、双向转换、shape/limit 校验 |
| `src/conversation_storage.rs` | 8,701 | storage、replay、fork、scanner、recorder、诊断、测试混合 |
| `src/session_residency.rs` | 6,823 | registry actor 与每 Session gate，重复 executor/runtime 路由和错误层 |
| `src/tools.rs` | 6,388 | closed ToolSet、start proof、exact identity、mutation queue、approval/question 多层协议 |
| `src/workspace.rs` | 6,100 | 多 root、authority、trust、source capture、snapshot、tool capability、写入目标混合 |
| `src/model_gateway.rs` | 5,225 | 模型类型、catalog、source、gateway、structured output、script fixture、测试混合 |
| `src/prompt.rs` | 3,752 | prompt source/catalog、selection、assembly proof、模型消息、工具结果、压缩拼装混合 |

静态扫描还显示：

- `session_execution.rs`、`session_residency.rs`、`session_ingress.rs` 使用了生产级 `#![allow(dead_code)]`，说明当前模块保留了尚未闭合或已失去调用者的设计面。
- `agent_session_lifecycle`、`live_conversation`、`model_gateway`、`prompt` 之间形成循环依赖，领域数据类型没有稳定的向下依赖层。
- `wire` 被大量领域模块反向依赖，协议 DTO/校验工具事实上成为基础层；这与“Wire 应依赖领域、领域不依赖 Wire”的正确方向相反。
- 巨型源文件内嵌了大量竞态测试和 failure-injection hook，使生产控制流与测试控制流互相污染。

### 1.3 复杂度的根因

#### 根因 A：产品边界没有冻结

每个里程碑继续向同一个 runtime 叠加能力，导致 “为了未来通用平台” 的功能与 “当前 coding agent 必需能力” 同时进入主状态机。

#### 根因 B：同一事实有多个 owner

例如一次 Session 操作可能依次经过：

```text
Runtime command dedup/publication gate
  -> SessionResidency registry actor/per-session gate
    -> SessionExecutor work lane/emergency lane/admission gate
      -> ActiveTurn/ToolStartGate/MutationQueue
        -> DurableState actor/publication generation
```

为防止各层 stale completion，又叠加 owner identity、epoch、permit、first-wins 和 exact `Arc::ptr_eq` proof。每增加一层 owner，就增加一套关闭、错误映射、竞态测试和恢复路径。

#### 根因 C：协议镜像了领域，而不是序列化领域

`runtime_interface.rs` 已经定义公开语义类型，`wire/public_protocol.rs` 又定义一套 Input/Output carrier 并手写双向转换和大量 shape 校验。核心 crate 因此维护两套几乎平行的数据模型。

#### 根因 D：测试保证了过度设计，而不是阻止过度设计

现有测试数量很大，但很多测试验证的是 owner proof、gate、epoch、publication/fence 的内部合同。直接保留所有测试会迫使重构继续保留原有复杂度。重构必须先定义新的用户可观察验收矩阵，再删除仅绑定旧实现机制的测试。

### 1.4 禁止采用的“伪重构”

执行模型不得做以下事情：

1. 只把 `session_execution.rs` 拆成多个文件，但保留全部状态、permit、gate 和 completion 类型。
2. 为兼容旧 API 再增加一层 adapter，同时永久保留 `dispatch/query/wire`。
3. 把 `DurableState` 拆成 AgentStore、SessionStore、PublicationStore，但仍保留 generation/COMMITTED/PUBLISHED 全套协议。
4. 新增更多 trait 来包裹每个旧 owner；本方案只允许在真正需要宿主扩展的边界使用 trait：`ModelProvider`、`Tool`、可选 `ToolPolicy`。
5. 用 `#[allow(dead_code)]`、`#[allow(clippy::too_many_arguments)]`、`#[allow(clippy::large_enum_variant)]` 掩盖迁移未完成。
6. 在默认核心中继续保留 Fork、Archive、Agent revision、共享资源热重载、Wire codec，只因为“以后可能需要”。

---

## 2. v0.2 产品边界

### 2.1 必须完整支持的 coding-agent 闭环

v0.2 完成后，宿主必须能够完成以下流程：

```text
打开 Runtime
  -> 创建或加载 Session
    -> 提交用户任务
      -> 构造 coding prompt
        -> 流式调用模型
          -> 模型返回文本：结束 Turn
          -> 模型返回工具调用：顺序执行工具并写回结果
          -> ask_user：暂停并等待宿主回答，然后继续
          -> run_command：执行编译/测试/格式化/搜索命令并返回输出
        -> 必要时继续下一轮模型调用
      -> 持久化对话和结果
    -> 订阅事件 / 读取 snapshot / 取消 Turn
  -> 关闭并可在重启后恢复
```

核心能力清单：

| 能力 | v0.2 要求 |
|---|---|
| Workspace | 每 Session 一个 root；所有文件工具使用 root-relative path |
| Session | 支持多个 Session，但每 Session 同时只允许一个 active Turn |
| Model | OpenAI Responses、Anthropic Messages 适配器继续可用 |
| Streaming | 文本/推理增量为 best-effort；状态和 terminal 可通过 snapshot 恢复 |
| Tool loop | 模型工具调用 → 工具结果 → 再次模型调用，直到文本结束或达到上限 |
| 文件工具 | `read_file`、`list_directory`、`write_file` |
| 命令工具 | 新增 `run_command`，结构化 program/args，不提供默认 shell 字符串接口 |
| 交互 | `ask_user`，一个 Turn 同时最多一个 pending interaction |
| 取消 | 可取消模型调用、工具调用和命令；命令至少终止直接子进程 |
| Persistence | `session.json` + `conversation.jsonl`；可重启加载 |
| Compaction | 触发摘要并在后续 prompt 使用；不要求重写历史文件 |
| Observation | typed snapshot + event stream；事件滞后时允许 resync |
| Errors | 领域错误直接返回，不经过三层错误码映射 |

### 2.2 明确从默认核心删除的能力

| 当前能力 | 处理 | 原因 |
|---|---|---|
| Durable Agent entity | 删除 | Session 直接保存模型、prompt、tool 配置；共享模板由宿主管理 |
| Agent status/revision/metadata CRUD | 删除 | 引入多层可用性 fan-out 和 revision pinning，非单 agent 必需 |
| Session definition CAS | 删除 | 运行时内嵌配置无需通用乐观并发协议 |
| Agent revision upgrade/rollback | 删除 | 改为下一 Turn 使用更新后的 `SessionConfig`，或关闭后重开 |
| Session Fork/provenance/anchor | 删除 | 非 coding-agent 必需；可由宿主复制目录实现 |
| Archive/Unarchive | 删除 | 生命周期管理面，不影响执行闭环 |
| Steer/FollowUp/queued cancellation | 删除 | v0.2 只允许 Idle 时 submit；运行中使用 cancel 后重提 |
| Runtime-wide `CommandId` dedup | 删除 | 核心是进程内 typed API，不是网络幂等服务 |
| 通用 dispatch/query router | 删除 | 用 typed 方法替代 |
| Cursor registry | 删除 | transcript 使用 stateless `after_seq + limit` |
| Shared resource hot reload | 删除 | Runtime 重开或 Session 更新配置即可 |
| Workspace security invalidation/recovery | 删除 | root/config 在 Session 生命周期内冻结；失效时关闭 Session |
| 多维 readiness fan-out | 删除 | open/load/submit 直接返回错误；状态只保留少量运行态 |
| Wire V1 | 从核心删除 | 协议是宿主/独立 crate 的职责 |
| Structured output foundation | 暂时删除或 feature-gate | 当前公开激活未闭合，非 coding agent 基础路径 |
| Prompt/Skill source ecosystem | 删除 | v0.2 使用直接文本配置；Skill 可由宿主拼入 system prompt |
| `fetch_url` 默认启用 | 改为可选 feature | coding agent 核心可通过命令或宿主工具扩展；网络策略独立 |

### 2.3 仍可保留但必须简化的能力

- 多 Session：保留一个简单 `HashMap<SessionId, SessionHandle>`，不得保留 registry actor。
- 模型 retry：保留最小 retry policy 和 provider delivery-state 安全判断，不保留 retry basis/steer arbitration。
- Workspace 安全：保留 capability-relative 文件访问和 symlink 防护，不保留多 root/source authority/fan-out。
- 进程锁：可选保留一个 data root lock，避免两个 Runtime 同时写同一目录；不得恢复 generation publication 协议。
- usage：保留模型返回的 token/费用累计即可，不建立复杂 projection 层。

---

## 3. 目标组织架构

### 3.1 最终目录

```text
src/
├── lib.rs
├── config.rs
├── error.rs
├── event.rs
├── ids.rs
├── runtime/
│   ├── mod.rs
│   ├── runtime.rs
│   └── session_manager.rs
├── session/
│   ├── mod.rs
│   ├── actor.rs
│   ├── command.rs
│   ├── state.rs
│   ├── snapshot.rs
│   ├── conversation.rs
│   └── store.rs
├── agent/
│   ├── mod.rs
│   ├── runner.rs
│   └── turn.rs
├── model/
│   ├── mod.rs
│   ├── types.rs
│   ├── provider.rs
│   ├── gateway.rs
│   ├── transport.rs
│   └── providers/
│       ├── mod.rs
│       ├── openai.rs
│       └── anthropic.rs
├── prompt/
│   ├── mod.rs
│   ├── builder.rs
│   └── compaction.rs
├── tools/
│   ├── mod.rs
│   ├── types.rs
│   ├── registry.rs
│   ├── policy.rs
│   ├── context.rs
│   └── builtins/
│       ├── mod.rs
│       ├── ask_user.rs
│       ├── path_args.rs
│       ├── read_file.rs
│       ├── list_directory.rs
│       ├── write_file.rs
│       └── run_command.rs
└── workspace/
    ├── mod.rs
    ├── root.rs
    └── path.rs

tests/
├── common/
│   ├── mod.rs
│   ├── scripted_model.rs
│   └── temp_workspace.rs
├── agent_loop.rs
├── cancellation.rs
├── persistence.rs
├── providers.rs
├── tool_security.rs
└── runtime_api.rs
```

可选能力不得污染上述默认核心：

```text
crates/minicore-wire-legacy/   # 只有确有兼容需求时创建
src/tools/builtins/fetch_url.rs # feature = "fetch-url"
examples/migrate_store_v1.rs    # 仅一次性迁移，不进入 Runtime 热路径
```

### 3.2 依赖方向

必须满足以下单向依赖：

```text
runtime
  -> session
     -> agent
        -> prompt
        -> model
        -> tools
     -> store/conversation
     -> workspace

model/providers -> model/types + model/transport
builtins        -> tools/types + tools/context + workspace
prompt          -> model/types + session/conversation(read-only DTO)
```

禁止：

- `model` 导入 `session`、`runtime`、`prompt` 的具体 owner 类型。
- `prompt` 导入 `agent_session_lifecycle` 或 provider adapter。
- `workspace` 导入 `prompt`、`skills`、`runtime`。
- 任何领域模块导入 `wire`。
- `event`/`ids`/`error` 导入上层执行模块。

### 3.3 所有权模型

| 对象 | 唯一 owner | 说明 |
|---|---|---|
| Runtime 关闭状态 | `Runtime` | 一个 cancellation token；不再有 shutdown leadership proof |
| Session registry | `SessionManager` | 一个锁保护 `HashMap`，锁内不 await |
| Session 状态 | `SessionActor` | 只有 actor 修改 `Idle/Running/Waiting/Closing` |
| Active Turn | `SessionActor::active` | 一个 `TurnId + CancellationToken + JoinHandle` |
| Conversation | `ConversationLog` | 一次 append 同步更新存储与内存 projection |
| Tool registry | `RuntimeConfig` 创建的 immutable `Arc<ToolRegistry>` | 不热重载 |
| Model provider registry | Runtime 初始化时冻结 | 不热重载 |
| Workspace root | Session 创建/加载时捕获 | Session 生命周期内不更换 |
| pending interaction | 当前 `ActiveTurn` | 最多一个，使用 oneshot answer channel |

不再允许为同一事实创建额外 owner identity、epoch、permit 或 publication proof。

### 3.4 精简状态机

```rust
pub enum SessionStatus {
    Idle,
    Running { turn_id: TurnId },
    WaitingForInput {
        turn_id: TurnId,
        interaction_id: InteractionId,
    },
    Closing,
}
```

状态转换只有：

```text
Idle --submit--> Running
Running --ask_user--> WaitingForInput
WaitingForInput --answer--> Running
Running/WaitingForInput --completed/failed/cancelled--> Idle
任意非终止状态 --close--> Closing
```

以下状态和独立布尔事实全部删除：

- `Starting`、`Finishing`
- `workspace_preparing`
- `agent_available`
- `model_available`
- `prompt_available`
- `runtime_dependency_unavailable`
- `PendingAvailability`
- `ActivePublication`
- `ActiveAdmission`
- `SecurityInvalidationState`
- `RuntimeDependencyProbeState`

资源不可用不是常驻 Session 状态：

- 创建/加载失败：直接返回 `SessionOpenError`。
- submit 前配置失效：直接返回 `SessionError::Unavailable(reason)`。
- active Turn 内 provider/tool/store 失败：Turn 结束为 `Failed`，Session 回到 `Idle`。

---

## 4. 目标公开 API

### 4.1 `lib.rs` 只导出稳定语义

```rust
pub mod config;
pub mod error;
pub mod event;
pub mod ids;
pub mod model;
pub mod runtime;
pub mod session;
pub mod tools;
pub mod workspace;

pub use config::{RuntimeConfig, SessionConfig};
pub use error::{RuntimeError, SessionError};
pub use ids::{InteractionId, SessionId, TurnId};
pub use runtime::Runtime;
pub use session::{SessionEventStream, SessionSnapshot, TranscriptPage};
```

不得公开内部 actor request、store generation、provider attempt、tool start proof 或 Wire carrier。

### 4.2 Runtime API

```rust
pub struct Runtime {
    inner: Arc<RuntimeInner>,
}

impl Runtime {
    pub async fn open(config: RuntimeConfig) -> Result<Self, RuntimeError>;

    pub async fn create_session(
        &self,
        config: SessionConfig,
    ) -> Result<SessionId, SessionError>;

    pub async fn load_session(
        &self,
        session_id: SessionId,
    ) -> Result<(), SessionError>;

    pub async fn close_session(
        &self,
        session_id: SessionId,
    ) -> Result<(), SessionError>;

    pub async fn delete_session(
        &self,
        session_id: SessionId,
    ) -> Result<(), SessionError>;

    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, SessionError>;

    pub async fn submit(
        &self,
        session_id: SessionId,
        input: impl Into<String>,
    ) -> Result<TurnId, SessionError>;

    pub async fn answer(
        &self,
        session_id: SessionId,
        interaction_id: InteractionId,
        answer: UserAnswer,
    ) -> Result<(), SessionError>;

    pub async fn cancel(
        &self,
        session_id: SessionId,
    ) -> Result<(), SessionError>;

    pub fn snapshot(
        &self,
        session_id: SessionId,
    ) -> Result<SessionSnapshot, SessionError>;

    pub fn subscribe(
        &self,
        session_id: SessionId,
    ) -> Result<SessionEventStream, SessionError>;

    pub async fn transcript(
        &self,
        session_id: SessionId,
        after_seq: Option<u64>,
        limit: usize,
    ) -> Result<TranscriptPage, SessionError>;

    pub async fn shutdown(&self) -> Result<(), RuntimeError>;
}
```

说明：

- `snapshot()` 直接读取 `watch` 最新值，不通过 actor request/oneshot。
- `subscribe()` 返回 snapshot-first stream；broadcast lag 时发 `ResyncRequired`，宿主重新读 snapshot。
- `submit()` 仅在 `Idle` 成功；运行中返回 `SessionError::Busy`，不排 FollowUp/Steer 队列。
- `close_session()` 取消 active Turn、等待有界时间，然后移出 registry；不使用 PrepareUnload 状态机。
- `delete_session()` 仅允许未加载 Session；已加载时返回 `SessionError::Busy`，避免删除正在写入的目录。
- `transcript()` 使用 `after_seq`，不保存 server-side cursor。

### 4.3 配置

```rust
pub struct RuntimeConfig {
    pub data_dir: PathBuf,
    pub providers: ProviderRegistry,
    pub tools: ToolRegistry,
    pub shutdown_timeout: Duration,
    pub event_capacity: usize,
    pub process_policy: ProcessPolicy,
}

pub struct SessionConfig {
    pub workspace_root: PathBuf,
    pub model: ModelSelection,
    pub system_prompt: String,
    pub enabled_tools: BTreeSet<ToolName>,
    pub compaction: CompactionConfig,
    pub max_tool_rounds: u8,
}
```

配置在 Session 创建时校验并持久化；provider credential、HTTP client、动态闭包和宿主秘密不得持久化。

---

## 5. 按当前文件执行的修改方案

本节是实施模型的主要操作清单。所有“删除”动作只能在新路径已有验收测试覆盖后执行。

## 5.1 `Cargo.toml`

### 修改

1. Tokio 增加命令工具需要的 feature：

```toml
tokio = { version = "1.53.1", default-features = false,
          features = ["macros", "rt", "sync", "time", "process", "io-util"] }
```

2. 添加可选 feature：

```toml
[features]
default = []
fetch-url = []
heavy-tests = []
```

3. 重构结束后逐项删除无调用依赖：

- `regex-syntax`：Wire schema/lexical 删除后应移除。
- `same-file`、`file-id`：多 root identity/generation lease 删除后应移除。
- `serde_json/arbitrary_precision`：自定义 canonical number/Wire 删除后改为普通 `serde_json`。
- `fs4`：仅在保留单一 data-root lock 时继续使用；否则删除。
- `cap-primitives`：只有新的 no-follow 写实现仍直接调用时保留。
- `time`、`base64`：由 provider adapter 实际使用情况决定，不得凭猜测删除。

4. 保持：

- Rust edition 2024、MSRV 1.85。
- `unsafe_code = "forbid"`。
- `await_holding_lock = "deny"`。

### 验收

```bash
cargo +1.85.0 check --all-targets
cargo +stable check --all-targets
cargo tree -e normal
```

最终 `Cargo.toml` 中不得存在只由已删除 Wire/Store 模块使用的依赖。

---

## 5.2 `src/lib.rs`

### 当前问题

当前文件直接暴露 `agent_session_lifecycle`、`runtime_interface`、`skills`、`tools`、`turn_item_interaction`、`wire`、`workspace`，同时通过 `#[allow(dead_code)]` 暴露迁移基础。

### 修改

1. 先添加新模块，不立即删除旧模块：

```rust
mod agent;
mod config;
mod error;
mod event;
mod ids;
mod model;
mod runtime_v2; // 迁移期临时名称
mod session;
mod tools_v2;
mod workspace_v2;
```

2. 新闭环端到端测试通过后：

- 将 `runtime_v2` 重命名为 `runtime`。
- 将 `tools_v2` 重命名为 `tools`。
- 将 `workspace_v2` 重命名为 `workspace`。
- 删除所有旧 `mod` 声明和 re-export。

3. 最终只导出第 4 节列出的稳定语义类型。

### 验收

- `rg 'allow\(.*dead_code|allow\(\s*dead_code' src/lib.rs src -g '*.rs'` 无生产结果。
- `cargo doc --no-deps` 的 public item 数量显著下降；内部 owner 类型不出现在文档中。

---

## 5.3 `src/runtime.rs` — 重写并拆分

### 当前必须移除的方法/结构

#### 通用路由

- `MiniCoreRuntime::dispatch`
- `MiniCoreRuntime::query`
- `RuntimeInner::dispatch`
- `RuntimeInner::dispatch_once`
- `RuntimeInner::query`

#### 服务端游标

- `first_agents` / `next_agents` / `agent_page`
- `first_sessions` / `next_sessions` / `session_page`
- `first_transcript` / `next_transcript` / `transcript_page`
- `insert_cursor` / `remove_expired`

#### Agent/Session 管理面分发

- `dispatch_agent_status`
- `dispatch_agent_definition`
- `dispatch_agent_metadata`
- `dispatch_session_lifecycle`
- `dispatch_session_metadata`
- `dispatch_session_definition`
- `dispatch_session_agent_upgrade`
- `dispatch_session_workspace_reload`
- `dispatch_shared_resources_reload`

#### 多层错误映射

删除全部 `map_agent_*`、`map_session_*`、`map_fork_error`、`map_follow_up_error`、`map_steer_error`、`map_cancel_queued_message_error` 等方法。新 API 直接返回领域错误。

#### 其他平台机制

- `RuntimeCommandInFlight` / `RuntimeCommandOwner` / `RuntimeCommandOwnerGuard`
- runtime-wide `CommandId` in-flight map
- `SharedResourceRoots` 和 runtime publication semaphore
- `invalidate_session_workspace_authority`
- `implemented_runtime_capabilities`
- Agent/Session event projection 大量辅助方法

### 保留并迁移的语义

- `open` → `runtime/runtime.rs::Runtime::open`
- `shutdown` → `Runtime::shutdown`
- `snapshot` → typed `Runtime::snapshot(session_id)`
- `subscribe` → typed `Runtime::subscribe(session_id)`
- `session_transcript` → `Runtime::transcript(after_seq, limit)`
- provider、tool、store 初始化

### 新文件拆分

- `src/config.rs`：从 `MiniCoreRuntimeConfig` 迁移纯配置和校验。
- `src/runtime/runtime.rs`：公开 facade 和关闭。
- `src/runtime/session_manager.rs`：简单 registry。

### `SessionManager` 要求

```rust
pub(crate) struct SessionManager {
    sessions: Mutex<HashMap<SessionId, SessionHandle>>,
    store: Arc<SessionStore>,
    deps: Arc<SessionDependencies>,
}
```

方法只允许：

```rust
create
load
get
remove
list
shutdown_all
```

锁内只进行 map 操作，严禁 `.await`。

### 验收

- 不再存在 `RuntimeCommand`、`CommandRequest`、`RuntimeQuery` 入口。
- `runtime/runtime.rs` 生产代码不超过 1,200 行。
- `session_manager.rs` 生产代码不超过 500 行。
- Runtime 层不匹配具体 tool call、model response、conversation entry。

---

## 5.4 `src/runtime_interface.rs` — 删除

### 当前问题

该文件定义了大量 command/query/outcome/error/page/snapshot/event DTO，随后又被 Wire 再镜像一次。

### 处理

将必要类型迁移为三个小文件：

- `src/error.rs`
- `src/event.rs`
- `src/session/snapshot.rs`

保留的公开类型控制在约 15–20 个：

```text
RuntimeError
SessionError
TurnError
SessionSnapshot
SessionStatus
SessionEvent
SessionEventKind
TranscriptItem
TranscriptPage
SessionSummary
UserQuestion
UserAnswer
Usage
FinishReason
```

删除：

- `CommandSurface` / capability catalog
- `RuntimeCommand`、`AgentCommand`、`SessionCommand`、`TurnCommand`、`InteractionCommand`
- `CommandResponse`、`CommandOutcome`、`CommandErrorCode`、`RetryAdvice`
- `RuntimeQuery`、各种 PageRequest/PageCursor
- 为 Wire shape 合同服务的 constructor/validate 方法
- Agent snapshot/event 类型

### 验收

- 核心公开 API 不再出现 `Command*`、`Query*`、`Wire*`、`PageCursor`。
- 新 DTO 全部直接 `serde::{Serialize, Deserialize}`；不再为每个类型写 Input/Output 镜像。

---

## 5.5 `src/session_residency.rs` — 删除

### 当前问题

该模块拥有 registry actor、per-session gate、load/unload/fork/lifecycle/definition/metadata/reload/security/submit 等完整转发面，并为 executor 和 runtime 再定义一层错误类型。

### 替代

使用 `runtime/session_manager.rs` 的锁保护 map：

```rust
fn get(&self, id: SessionId) -> Result<SessionHandle, SessionError>;
async fn load(&self, id: SessionId) -> Result<SessionHandle, SessionError>;
async fn create(&self, cfg: SessionConfig) -> Result<SessionHandle, SessionError>;
async fn remove(&self, id: SessionId) -> Result<Option<SessionHandle>, SessionError>;
```

每个 `SessionHandle` 自身包含 actor sender、snapshot watch receiver 和 event broadcaster；无需 registry actor 串行化。

### 删除

- 全部 `SessionResidency*Error/Outcome`。
- per-Session publication gate。
- Fork、lifecycle、metadata、agent upgrade、workspace reload、shared resource update、安全失效路由。
- 重复的 submit/cancel/interaction error mapping。

### 验收

- `src/session_residency.rs` 不存在。
- Runtime 到 Session 只经过一次 `SessionManager::get`，然后直接调用 `SessionHandle`。

---

## 5.6 `src/session_ingress.rs` — 删除

### 当前问题

该模块为普通 work lane 和 emergency lane 建立 owner/epoch/permit/first-wins 合同，随后 SessionExecutor 和 ActiveTurn 又维护自己的状态。

### 替代

```rust
pub(crate) struct ActiveTurn {
    turn_id: TurnId,
    cancel: CancellationToken,
    task: JoinHandle<()>,
    pending_interaction: Option<PendingInteraction>,
}
```

actor 通过一个 bounded `mpsc::Receiver<SessionCommand>` 接收所有命令。取消不需要 emergency lane：

- sender 容量保持小而可控，例如 32。
- `cancel()` 使用 `CancellationToken` 的同步 `cancel()`；不依赖向满 mailbox 发送消息才能生效。
- `SessionHandle` 可持有当前 turn cancellation token 的只读共享槽，或 actor 在收到 cancel command 后立即 cancel。推荐前者用于保证取消不被 mailbox 背压阻塞：

```rust
struct CancelSlot(Mutex<Option<(TurnId, CancellationToken)>>);
```

该锁内只 clone token，不 await。

### 删除

- `EmergencyControlOwner`
- `EmergencyControlObservation`
- `EmergencyControlSignal`
- generation/epoch
- 所有 permit/proof 类型
- FollowUpQueue/SteerQueue
- terminal claim first-wins 层

### 验收

- 取消路径最多经过一次 token clone + `cancel()`。
- 不存在 bounded work lane + unbounded emergency lane 双通道。

---

## 5.7 `src/session_execution.rs` — 完全重写

这是重构的核心。禁止在原文件继续删除分支后勉强保留；应创建新的 `session/actor.rs`、`agent/runner.rs`、`agent/turn.rs`，完成后删除原文件。

### 当前公开方法中仅保留的语义

| 当前方法 | v0.2 对应 |
|---|---|
| `snapshot` / `published_snapshot` | `SessionHandle::snapshot`，直接读 watch |
| `transcript_capture` | `ConversationLog::page` |
| `submit` | `SessionHandle::submit` |
| `resolve_interaction` | `SessionHandle::answer` |
| `cancel` | `SessionHandle::cancel` |
| `subscribe*` | `SessionHandle::subscribe` |
| `close` | `SessionHandle::close` |

### 必须删除的方法

- 所有 `start_loaded_*` 变体，改为一个 `SessionActor::spawn`。
- `begin_prepare_for_unload` / `prepare_for_unload`。
- `update_workspace_definition*`。
- `update_session_definition_with_cancellation`。
- `upgrade_session_agent_with_cancellation`。
- `reload_workspace_with_cancellation`。
- `set_agent_availability_with_cancellation`。
- `update_shared_resources_with_cancellation`。
- `publish_metadata`。
- `security_revoke` / `begin_security_invalidation`。
- `follow_up` / `steer` / `cancel_queued_message`。
- 所有生产 test hook accessor。

### 必须删除的内部状态/机制

- `TurnAdmissionGate` / `TurnAdmissionPermit`
- `SessionDefinitionPublicationPermit` / `PublicationPermitIdentity`
- `ActivePublication`
- `ActiveAdmission`
- `PendingAvailability`
- `PrepareUnloadState`
- `SecurityInvalidationState`
- `RuntimeDependencyProbeState`
- `ExpectedPublication`
- publication/admission/security/probe completion enum
- reliable progress fence、`ProgressDemand`、`TurnProgressMailbox` 高水位机制
- `SteerArbitration`
- Tool operation slot/start proof/mutation preparation 编排
- 大量 `Drop` guard 用于给 waiter 填充内部错误的模式

### 新 `SessionActor` 的职责

```rust
struct SessionActor {
    id: SessionId,
    config: SessionConfig,
    status: SessionStatus,
    active: Option<ActiveTurn>,
    commands: mpsc::Receiver<SessionCommand>,
    turn_events: mpsc::Receiver<TurnActorEvent>,
    snapshot_tx: watch::Sender<Arc<SessionSnapshot>>,
    event_tx: broadcast::Sender<SessionEvent>,
    conversation: Arc<ConversationLog>,
    deps: Arc<SessionDependencies>,
}
```

命令：

```rust
enum SessionCommand {
    Submit { input: String, reply: oneshot::Sender<Result<TurnId, SessionError>> },
    Answer { interaction_id: InteractionId, answer: UserAnswer,
             reply: oneshot::Sender<Result<(), SessionError>> },
    Cancel { reply: oneshot::Sender<Result<(), SessionError>> },
    Close { reply: oneshot::Sender<Result<(), SessionError>> },
}
```

内部事件：

```rust
enum TurnActorEvent {
    InteractionRequested {
        turn_id: TurnId,
        question: UserQuestion,
        answer_tx: oneshot::Sender<UserAnswer>,
    },
    Finished {
        turn_id: TurnId,
        outcome: TurnOutcome,
    },
}
```

### actor 规则

1. `Submit` 仅在 `Idle` 接受；先把用户消息写入 `ConversationLog`，再 spawn turn。
2. active task 的每个 completion 都带 `TurnId`；若不等于当前 active id，记录 debug 日志并忽略，不创建 proof 类型。
3. `InteractionRequested` 仅允许当前 Turn 且当前无 pending interaction；否则 active Turn 失败为 internal invariant。
4. `Answer` 必须匹配当前 interaction id；发送 oneshot 后状态回 `Running`。
5. `Cancel` 仅调用 token；状态保持 Running/Waiting，直到 `Finished(Cancelled)`。
6. `Close` 先标记 `Closing`，取消 active task，在 `shutdown_timeout` 内等待；超时 abort task，然后关闭事件流。
7. 每次状态变化调用一个 `publish_snapshot()`；不得出现几十个 `with_*` clone 方法。

### `SessionSnapshot`

使用一个普通构造函数从 actor state 投影：

```rust
impl SessionSnapshot {
    fn from_actor(actor: &SessionActor) -> Self;
}
```

字段控制在：

```rust
pub struct SessionSnapshot {
    pub session_id: SessionId,
    pub status: SessionStatus,
    pub active_turn: Option<TurnSummary>,
    pub pending_question: Option<UserQuestion>,
    pub usage: Usage,
    pub last_error: Option<PublicErrorSummary>,
    pub conversation_seq: u64,
}
```

### active turn 拆分

将原来的 `run_active_turn`、`run_active_turn_inner`、`run_admission`、`execute_gated_tool_round` 等替换为：

```rust
pub(crate) async fn run_turn(ctx: TurnContext) -> TurnOutcome;
async fn call_model(ctx: &TurnContext, conversation: &PromptInput) -> Result<ModelResponse, TurnError>;
async fn execute_tool_calls(ctx: &TurnContext, calls: Vec<ToolCall>) -> Result<Vec<ToolResult>, TurnError>;
async fn execute_one_tool(ctx: &TurnContext, call: ToolCall) -> ToolResult;
async fn request_user_input(ctx: &TurnContext, question: UserQuestion) -> Result<UserAnswer, TurnError>;
```

主循环示意：

```rust
for round in 0..=config.max_tool_rounds {
    cancellation.check_cancelled()?;
    let request = prompt_builder.build(conversation.snapshot().await?, tool_specs)?;
    let response = model.generate(request, event_sink, cancellation.clone()).await?;
    conversation.append_assistant(&response).await?;

    if response.tool_calls.is_empty() {
        return TurnOutcome::Completed(response.final_text());
    }

    if round == config.max_tool_rounds {
        return TurnOutcome::Failed(TurnError::ToolRoundLimit);
    }

    let results = execute_tool_calls(...).await?;
    conversation.append_tool_results(results).await?;
}
```

默认顺序执行工具调用。并行执行不是 v0.2 目标。

### 验收

- `session/actor.rs` 生产代码不超过 1,200 行。
- `agent/runner.rs` 生产代码不超过 1,000 行。
- 单个函数原则上不超过 120 行，绝对上限 200 行。
- 生产代码中没有 `SessionExecutorTestHooks`、failure gate、owner proof。
- 状态 enum 只有四种。

---

## 5.8 `src/durable_state.rs` — 删除并重建 Store

### 当前必须删除

- `DurableState` actor/request loop。
- Agent head/definition/status/metadata 存储。
- Session generation directories。
- root lease identity 与永久 reservation。
- staging、COMMITTED、PUBLISHED 标记。
- publication certainty/reconcile/recover/cleanup 状态机。
- Fork source/child publication。
- 每个 operation 的 enqueue/settle/wait wrapper。
- `run_loop` 和所有 `publish_*_generation_blocking` 方法。

### 新磁盘布局

```text
<data_dir>/
├── runtime.lock              # 可选，单一进程锁
└── sessions/
    └── <session-id>/
        ├── session.json
        └── conversation.jsonl
```

### `session.json`

```json
{
  "format_version": 2,
  "session_id": "...",
  "created_at": "...",
  "updated_at": "...",
  "workspace_root": "/host/path",
  "model": { "provider": "openai", "model": "..." },
  "system_prompt": "...",
  "enabled_tools": ["read_file", "write_file", "run_command"],
  "compaction": { "trigger_tokens": 80000, "target_tokens": 30000 },
  "max_tool_rounds": 16
}
```

说明：如果不希望持久化绝对 workspace path，可由宿主提供 `workspace_key -> PathBuf` resolver；v0.2 核心不同时实现两套模式。实施前选择一种并冻结。

### 新 `SessionStore`

```rust
pub(crate) struct SessionStore {
    root: PathBuf,
    _lock: Option<RootLock>, // 可选单进程 data-root 锁；由本 crate 自己封装文件句柄
}

impl SessionStore {
    pub async fn open(root: PathBuf) -> Result<Self, StoreError>;
    pub async fn create(&self, config: &StoredSessionConfig) -> Result<(), StoreError>;
    pub async fn load_config(&self, id: SessionId) -> Result<StoredSessionConfig, StoreError>;
    pub async fn list(&self) -> Result<Vec<SessionId>, StoreError>;
    pub async fn delete(&self, id: SessionId) -> Result<(), StoreError>;
    pub async fn open_conversation(&self, id: SessionId) -> Result<ConversationLog, StoreError>;
}
```

### metadata 原子写

实现一个共享函数：

```rust
async fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<(), StoreError>;
```

步骤：

1. 在同目录创建随机临时文件。
2. 写完整 JSON + `\n`。
3. `flush`，必要时 `sync_data`。
4. rename 覆盖目标。
5. Unix 上可 best-effort sync parent dir；跨平台失败策略写入文档。

不要创建 generation 目录或第二套 commit marker。

### 验收

- `src/durable_state.rs` 不存在。
- 创建 Session 只产生一个目录和两个文件。
- 同一 Session 的写入只有 `ConversationLog` 一个 writer。
- 模拟最后一行半写后重载可恢复到最后一个完整 JSONL 记录。

---

## 5.9 `src/agent_session_lifecycle.rs` — 删除

### 当前职责

Agent/Session definition、revision、metadata patch、status、canonical comparison、create/fork/upgrade 等。

### 替代

- `SessionConfig`：`src/config.rs`。
- `StoredSessionConfig`：`src/session/store.rs`。
- `SessionStatus`：`src/session/state.rs`。
- `ModelSelection`：`src/model/types.rs`。
- metadata 如确有需要，仅保留 `title: Option<String>`，直接更新 `session.json`；不要恢复 patch/CAS/revision。

### 删除的概念

- `AgentRevisionRef`
- `AgentDefinitionRevision`
- `SessionDefinitionRevision`
- Agent status and admission decision
- metadata patch/normalization owner
- sealed attempt/candidate/materialize
- fork provenance
- lifecycle CAS

### 验收

- 代码库不存在 durable `AgentId`；如宿主需要“Agent 模板”，它是宿主配置，不是 Runtime 存储实体。
- Session 配置更新只有一个明确策略：推荐关闭 Session 后 `update_config`，下一次 load 生效；不得在线 publication。

---

## 5.10 `conversation_storage.rs`、`live_conversation.rs`、`session_transcript.rs` — 合并

### 目标文件

- `session/conversation.rs`
- `session/store.rs`

### 保留语义

- user message、assistant text/reasoning、tool call、tool result、interaction、summary。
- append-only JSONL。
- sequence number。
- reload/replay。
- transcript stateless range 查询。
- 不完整最后一行截断或忽略。
- 工具调用必须最终有结果；重启发现尾部未完成调用时生成或投影 `Interrupted` 结果。

### 删除

- `CapturedForkConversation`
- fork anchor/provenance/child re-encode
- loaded/unloaded 双 source selection
- branch projection
- 为每种 corruption 建立大量 diagnostics carrier
- Recorder actor + owner-tracked job
- transcript server-side cursor
- 复杂 relation/revision identity

### 新 entry

```rust
#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConversationEntry {
    User { seq: u64, turn_id: TurnId, text: String },
    Assistant {
        seq: u64,
        turn_id: TurnId,
        text: Option<String>,
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
        usage: Option<Usage>,
    },
    ToolResult {
        seq: u64,
        turn_id: TurnId,
        call_id: ToolCallId,
        result: ToolOutput,
    },
    Interaction {
        seq: u64,
        turn_id: TurnId,
        interaction_id: InteractionId,
        question: UserQuestion,
        answer: UserAnswer,
    },
    Summary {
        seq: u64,
        through_seq: u64,
        text: String,
    },
    TurnTerminal {
        seq: u64,
        turn_id: TurnId,
        outcome: StoredTurnOutcome,
    },
}
```

### `ConversationLog`

```rust
pub(crate) struct ConversationLog {
    state: RwLock<ConversationState>,
    writer: Mutex<ConversationWriter>,
}

impl ConversationLog {
    pub async fn open(path: PathBuf) -> Result<Self, StoreError>;
    pub async fn append(&self, entry: NewConversationEntry) -> Result<u64, StoreError>;
    pub async fn snapshot(&self) -> ConversationSnapshot;
    pub async fn page(&self, after_seq: Option<u64>, limit: usize) -> TranscriptPage;
    pub async fn prompt_view(&self) -> PromptConversationView;
}
```

`append()` 内不得持有 `RwLock` 跨 await。建议：

1. 先在 writer mutex 中分配 seq、序列化并写文件。
2. 写入成功后释放 writer lock。
3. 再短暂写内存 projection。

为了避免 seq 并发错序，只有 active Turn 写；如后续允许 metadata/host 写 conversation，改为在同一个 writer mutex 内更新一个小内存 counter，不要引入 actor。

### 恢复规则

- 最后一行 JSON 不完整：截断到上一完整换行。
- 中间行 JSON 无法解析：返回 `StoreError::Corrupt { seq/offset }`，不要静默跳过几十种损坏。
- 尾部存在未完成 tool call：加载时追加或投影 synthetic `CancelledByRestart` tool result，再允许新 submit。
- 不重放 token delta；只保存完成的语义项。

### 验收

- 一个简单对话只产生可读 JSONL。
- restart 后 prompt 输入与 restart 前 terminal transcript 一致。
- `conversation.rs` 不导入 provider adapter、runtime 或 wire。

---

## 5.11 `src/compaction.rs` — 简化并迁移

### 目标

迁移为 `prompt/compaction.rs`。

### 保留

```rust
pub struct CompactionConfig {
    pub trigger_tokens: u64,
    pub target_tokens: u64,
}
```

- 估算上下文超过阈值时调用模型生成摘要。
- 写入 `ConversationEntry::Summary { through_seq, text }`。
- PromptBuilder 使用最新 summary + `through_seq` 之后的条目。

### 删除

- live Replace 状态变换。
- 专门的 compaction marker owner/proof/revision。
- 为 retry/steer/publication 建立的 compaction operation identity。
- 物理删除历史或 rewrite JSONL 的在线路径。

### 验收

- 压缩后完整 transcript 仍可查看原历史。
- 下一次模型请求只包含最新 summary 和之后消息。
- 摘要失败不破坏原 conversation；本 Turn 可返回明确失败或继续未压缩调用，策略写死并测试。

---

## 5.12 `src/runtime_task.rs` — 重写为 TaskGroup

### 删除

- task owner registry。
- shutdown leadership/settlement proof。
- 注入式 failure gate。
- 每种 tracked async/blocking job 的独立 wrapper。

### 替代

```rust
pub(crate) struct TaskGroup {
    cancel: CancellationToken,
    tasks: Mutex<JoinSet<()>>,
}

impl TaskGroup {
    pub fn spawn<F>(&self, future: F) -> Result<(), RuntimeError>
    where F: Future<Output = ()> + Send + 'static;

    pub async fn shutdown(&self, timeout: Duration);
}
```

对于阻塞文件操作，建立一个小函数：

```rust
pub async fn blocking<T, F>(cancel: CancellationToken, f: F) -> Result<T, TaskError>
where T: Send + 'static,
      F: FnOnce() -> Result<T, TaskError> + Send + 'static;
```

不得把 test barrier 放入生产 struct；测试用 scripted provider、临时文件和 Tokio paused time。

### 验收

- `runtime_task.rs` 或新 `runtime/task_group.rs` 不超过 350 行。
- shutdown 只有一个 cancellation token 和一个 join drain 路径。

---

## 5.13 `src/tools.rs` — 重写为 Registry/Policy/Context

### 当前必须删除的机制

- closed 32 种布尔组合 ToolSet。
- `ToolExecutionPlan::{Execute, FileMutation, Approval, UserQuestion, ...}` 多阶段计划。
- `ToolStartGate`、`ToolStartedExecution` 和 exact request proof。
- `Arc::ptr_eq` exact capture 校验。
- `SessionFileMutationQueue`、ticket、permit、per-target FIFO。
- preparation factory/start factory/move-only settlement。
- approval option 的完整私有映射体系；v0.2 只保留通用 allow/deny/ask。
- `cancelled_before_start_outcome`、`failed_before_start_outcome`、`run_started_execution` 等 owner-bound helper。

### 新 `Tool` trait

不用引入 `async-trait`：

```rust
pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + 'a>>;

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn execute<'a>(&'a self, ctx: ToolContext<'a>, args: serde_json::Value) -> ToolFuture<'a>;
}
```

### Registry

```rust
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Arc<BTreeMap<ToolName, Arc<dyn Tool>>>,
}

impl ToolRegistry {
    pub fn builder() -> ToolRegistryBuilder;
    pub fn get(&self, name: &ToolName) -> Option<Arc<dyn Tool>>;
    pub fn specs(&self, enabled: &BTreeSet<ToolName>) -> Result<Vec<ToolSpec>, ToolError>;
}
```

builder 必须拒绝重名工具。Runtime open 后 registry immutable。

### Policy

```rust
pub trait ToolPolicy: Send + Sync {
    fn decide(&self, request: &ToolRequest, ctx: &ToolContextView) -> ToolDecision;
}

pub enum ToolDecision {
    Allow,
    Deny { reason: String },
    Ask { question: UserQuestion },
}
```

默认可提供：

- `AllowConfiguredTools`：仅允许 Session `enabled_tools`。
- `ProcessPolicy`：单独约束 `run_command`。

工具自己的正常业务交互（`ask_user`）与 policy approval 不应复用复杂绑定协议：两者统一产生 `UserQuestion`，但 event 中保留 `QuestionKind::{ToolInput, Approval}`。

### 执行规则

1. 一个模型响应中的工具调用按 `call_index` 顺序执行。
2. 每次执行前验证：名称存在、Session 已启用、JSON args 可反序列化、policy Allow/Ask/Deny。
3. tool future 接收当前 Turn cancellation token。
4. panic 用单一 `catch_unwind` 边界转换为 `ToolError::Panicked`；不要为 planner/factory/executor 分三层 panic 边界。
5. 工具结果无论成功失败都形成模型可读 `ToolOutput`；只有 runtime/store 内部失败终止 Turn。

### 为什么不再需要文件 mutation queue

v0.2 默认顺序执行工具；同一 Session 同时只有一个 active Turn。因此文件写天然串行。若未来增加并行工具，应在 `ToolRegistry` 增加显式 executor policy，而不是提前保留 per-path ticket 系统。

### 验收

- 自定义第三方工具可在不修改 `tools.rs` match 的情况下注册。
- 新增一个测试 `EchoTool` 只需实现 trait 并注册。
- `tools/` 核心生产代码总量建议小于 2,000 行，不含 builtins。

---

## 5.14 内置工具文件

### `ask_user.rs`

重写为普通 Tool：

```rust
struct AskUserArgs { question: String, choices: Option<Vec<String>> }
```

`execute()` 调用 `ctx.ask_user(question).await`，返回回答文本。删除 frozen plan、answer binding permit、exact request identity。

验收：

- 提问后 snapshot 为 `WaitingForInput`。
- 错误 interaction id 被拒绝。
- cancel 时等待回答的 tool future 返回 Cancelled。

### `read_file.rs` / `list_directory.rs` / `write_file.rs`

1. 把三者重复的 path JSON 解析、最大长度、错误映射放入 `builtins/path_args.rs`。
2. 每个 builtin 只做：反序列化 → 调 Workspace 方法 → 格式化 ToolOutput。
3. 保留 UTF-8、大小上限、条目数上限和 symlink 防护。
4. `write_file` 使用 Workspace 的单一原子/替换写方法；删除 mutation ticket/permit。

建议默认限制：

```text
read_file:      256 KiB
write_file:     256 KiB
list_directory: 1,000 entries
```

原限制可以继续使用，但应集中定义于 `ToolLimits`，不得散落在 Wire `ProtocolLimits`。

### `fetch_url.rs`

- 从默认 registry 移除。
- 放入 `fetch-url` feature。
- 保留 exact-origin/pinned transport 也可以，但不允许其类型渗透进 Runtime config 主结构；使用 `FetchUrlTool::new(config)` 注册。
- 若维护成本仍高，可完全删除，交由宿主自定义工具。

### 新增 `run_command.rs`

详见第 7 节；这是 v0.2 完整 coding agent 的硬性验收项。

---

## 5.15 `src/workspace.rs` — 重写并拆分

### 必须保留的安全语义

- root 在 Session 创建/加载时解析一次。
- 文件工具只接受相对路径。
- 拒绝绝对路径、`..` 逃逸、NUL、平台前缀。
- 对最终写目标执行 no-follow 或 capability-relative 安全打开。
- 读取/list 不跟随能逃出 root 的 symlink。

### 必须删除的类型/机制

- `WorkspaceRootKey`
- 多 `WorkspaceRootInput/Spec`
- `WorkspaceRootRole`
- `WorkspaceTrustLevel/Revision`
- `WorkspaceSourcePolicy`
- `WorkspaceAuthority*`
- `WorkspaceFilesystemAccessControl` 动态 revoke/recovery
- Prompt/Skill source authorization/capture/context
- `WorkspaceSnapshotCandidate` / finish proof
- 多 root summary/view
- mutation identity/key（顺序工具后不需要）
- resolver test hooks/barriers（移到 tests）

### 新 API

```rust
pub struct Workspace {
    root_path: PathBuf,
    root_dir: cap_std::fs::Dir,
    access: WorkspaceAccess,
}

pub enum WorkspaceAccess { ReadOnly, ReadWrite }

impl Workspace {
    pub fn open(root: PathBuf, access: WorkspaceAccess) -> Result<Self, WorkspaceError>;
    pub async fn read_text(&self, path: &RelativePath, max: usize) -> Result<String, WorkspaceError>;
    pub async fn list(&self, path: &RelativePath, limit: usize) -> Result<Vec<DirectoryEntry>, WorkspaceError>;
    pub async fn write_text(&self, path: &RelativePath, content: &str) -> Result<(), WorkspaceError>;
    pub fn command_cwd(&self, path: Option<&RelativePath>) -> Result<PathBuf, WorkspaceError>;
}
```

`RelativePath` 在 `workspace/path.rs` 统一校验，不使用 Wire lexical 类型。

### 重要安全说明

`run_command` 启动的普通 OS 进程不受 cap-std 文件 capability 沙箱限制；`cwd` 限制不等于进程沙箱。文档和 API 必须明确：启用 `run_command` 即授予宿主进程权限范围内的执行能力，应在容器/低权限用户中运行，或通过 `ProcessPolicy` 限制程序和环境。

### 验收

- `workspace/root.rs + path.rs` 生产代码合计不超过 1,200 行。
- 单测覆盖 Unix/Windows 路径前缀、`..`、symlink、写入最终组件替换。
- Workspace 不导入 prompt、skills、runtime、session。

---

## 5.16 `src/prompt.rs` — 重写为 PromptBuilder

### 当前必须删除

- Prompt source adapter/catalog/reload candidate。
- Agent/Session/Workspace prompt selection 与 provenance。
- assembly proof/contribution stamp。
- Skills intent 和 source 发现。
- 在 prompt 模块中定义 provider/turn owner identity。
- 与 `agent_session_lifecycle`、`live_conversation`、`model_gateway` 的循环引用。

### 新职责

```rust
pub struct PromptBuilder {
    system_prompt: Arc<str>,
    coding_instructions: Arc<str>,
}

impl PromptBuilder {
    pub fn build(
        &self,
        conversation: &PromptConversationView,
        tools: &[ToolSpec],
        limits: ModelLimits,
    ) -> Result<ModelRequest, PromptError>;
}
```

输入只包含 provider-neutral DTO：

- `ModelMessage`
- `ToolSpec`
- 最新 compaction summary
- summary 之后的 conversation entries

PromptBuilder 不读取磁盘、不访问 DurableState、不解析 provider credential、不发布事件。

### 验收

- `prompt` 只依赖 `model/types`、`tools/types` 和 conversation 的只读 projection。
- 能用纯同步单测验证消息顺序、tool schema、summary 截断、token budget。
- 不再存在 PromptService、PromptResourceView、PromptSourceAdapter。

---

## 5.17 `src/skills.rs` — 从核心删除

v0.2 中 skill 等价于宿主提供的 system prompt 片段。删除该模块和所有 source capture、selection、intent、wire DTO。

若未来重新引入，只允许从一个简单值对象开始：

```rust
pub struct Skill {
    pub name: String,
    pub instructions: String,
}
```

它由宿主在 SessionConfig 构造时拼入 prompt，不引入热重载/source ecosystem。

---

## 5.18 `src/model_gateway.rs` — 拆分并精简

### 保留

- provider-neutral model selection。
- 模型请求/响应、tool call、usage、finish reason。
- `ProviderAdapter` 的异步执行边界。
- credential 解析和安全 retry delivery state。
- OpenAI/Anthropic production adapters。
- 流式增量发布。

### 删除或迁移到测试

- `ScriptedModelFixture` 及大量 fixture builder 从生产文件移到 `tests/common/scripted_model.rs` 或 `#[cfg(test)] mod fixtures` 独立文件。
- dynamic `ModelSourceAdapter` / reload candidate；改为启动时构造 immutable registry。
- `TurnModelIdentity`/exact Arc owner proof。
- public 未激活的 structured output foundation；可在独立 feature 中保留，但默认不编译。
- 与 PromptSet/LiveConversation 具体类型的引用。

### 目标文件

#### `model/types.rs`

```rust
ModelSelection
ModelLimits
ReasoningPreference
ModelMessage
AssistantPart
ToolCall
ModelRequest
ModelResponse
ModelEvent
ModelUsage
ModelFinishReason
ModelError
DeliveryState
```

#### `model/provider.rs`

```rust
pub trait ModelProvider: Send + Sync {
    fn id(&self) -> &ProviderId;
    fn models(&self) -> &[ModelDescriptor];
    fn generate<'a>(&'a self, request: ModelRequest, ctx: ModelCallContext) -> ModelFuture<'a>;
}
```

#### `model/gateway.rs`

```rust
pub struct ModelGateway {
    providers: BTreeMap<ProviderId, Arc<dyn ModelProvider>>,
    retry: RetryPolicy,
}

impl ModelGateway {
    pub fn resolve(&self, selection: &ModelSelection) -> Result<ResolvedModel, ModelError>;
    pub async fn generate(&self, request: ModelRequest, ctx: ModelCallContext)
        -> Result<ModelResponse, ModelError>;
}
```

### 对当前方法的处理

- `resolve_for_turn` → `resolve`，不返回 exact owner proof。
- `generate_model_turn` → `generate`。
- `build_reload_candidate` / `initialize` source discovery → 删除；registry 在 Runtime open 直接校验。
- structured schema parse/validate 函数 → 删除或 feature-gate。

### retry 规则

- `max_attempts` 默认 2 或 3。
- 仅 provider 返回 `NotSent`/明确 safe-to-retry 时重试。
- 请求可能已送达且结果未知时不自动重试。
- cancellation 不重试。
- 不再维护 logical retry basis、steer safe point 或跨 Turn handoff。

### 验收

- `model/types.rs` 不导入 prompt/session/runtime。
- `model/gateway.rs` 不超过 700 行。
- scripted provider 不出现在 public production API。

---

## 5.19 Provider 与 Transport 文件

### 当前文件

- `model_gateway/openai_responses.rs`
- `model_gateway/anthropic_messages.rs`
- `model_gateway/provider_installation.rs`
- `model_gateway/provider_transport.rs`
- `http_transport.rs`

### 目标

- `model/providers/openai.rs`
- `model/providers/anthropic.rs`
- `model/transport.rs`
- provider registry/config 放 `model/provider.rs`

### 修改要求

1. 先保持现有 wire encode、SSE/stream parse、error mapping 行为，避免在结构重构同时重写协议。
2. 把每个 adapter 的大段内嵌测试迁到 `tests/providers.rs` 或 `model/providers/*_tests.rs`。
3. 每个 provider 文件再按私有子模块分 `request`、`stream`、`error`；单文件目标不超过 1,500 行。
4. 合并 `provider_transport.rs` 和 `http_transport.rs` 的重复 client/timeout/stream owner。
5. credential 由 RuntimeConfig/provider 构造，Session 存储只保存 provider/model id。
6. 不在核心支持动态 credential/catalog hot install；宿主需要换 credential 时重开 Runtime。

### 验收

- 现有录制 fixture 能验证两种 provider 的请求体和流式终止。
- provider adapter 不导入 SessionActor、ConversationLog 或 ToolRegistry。

---

## 5.20 `turn_execution_context.rs`、`turn_item_interaction.rs` — 删除并归位

### `turn_execution_context.rs`

拆成：

- `agent/turn.rs::TurnContext`
- `tools/context.rs::ToolContext`
- `model/types.rs::ModelCallContext`

不要保留一个收集所有 owner/proof/resource snapshot 的万能 context。

### `turn_item_interaction.rs`

拆成：

- `ids.rs`：`TurnId`、`ToolCallId`、`InteractionId`
- `model/types.rs`：assistant/tool item DTO
- `tools/types.rs`：`UserQuestion`、`UserAnswer`
- `session/conversation.rs`：持久化 entry

删除 Item/Interaction 多层 revision/relationship owner。一个 pending interaction 由当前 ActiveTurn 的 `InteractionId` 唯一标识即可。

---

## 5.21 `src/wire/*` — 核心全部删除或外移

### 必须从核心删除的文件

```text
wire/bootstrap.rs
wire/bounded_json.rs
wire/conversation_jsonl.rs
wire/conversation_jsonl_scanner.rs
wire/durable_store.rs
wire/json_number.rs
wire/json_number_tests.rs
wire/lexical.rs
wire/limits.rs
wire/limits_tests.rs
wire/path.rs
wire/public_protocol.rs
wire/scalar.rs
wire/schema.rs
wire/typed_json.rs
wire/value.rs
wire/mod.rs
```

### 替代原则

- 领域 DTO 直接 derive serde。
- JSONL 格式属于 `session/store.rs`，不是 Wire。
- path 校验属于 `workspace/path.rs`。
- tool JSON schema 使用 `serde_json::Value` 或小型 `JsonSchema` 值对象；不需要 bounded JSON 类型树。
- 大小限制放在 `RuntimeLimits/ToolLimits/ModelLimits`，按所有者定义。

### 兼容选项

只有存在真实下游依赖时，才创建：

```text
crates/minicore-wire-legacy/
```

该 crate 依赖新的 minicore core，并实现旧 JSON ↔ 新 typed API 映射。它不得被核心反向依赖，也不得阻止核心删除旧 Agent/Fork 等语义；无法映射的命令返回明确 `UnsupportedInV2`。

### 验收

- `rg 'crate::wire|use minicore_runtime::wire' src` 无结果。
- `wire/public_protocol.rs` 不存在于核心 crate。
- `serde_json` 普通 derive roundtrip 测试替代大量 carrier/shape 测试。

---

## 5.22 `tests/` 与内嵌测试

### 当前问题

现有 integration tests 大量以里程碑和 Wire 机制命名，例如 `m2_*`、`m7_*`、`wire_*`、`public_manifest_active`。巨型生产文件内也嵌有数千到数万行测试。

### 处理原则

1. 先创建第 10 节的用户可观察验收测试。
2. 新测试全绿后，删除只验证旧内部机制的测试。
3. provider protocol fixture 可以保留，但移动到 provider 目录。
4. 不以“测试数量不能下降”为目标；以功能矩阵覆盖为目标。
5. 测试辅助类型放 `tests/common`，不得放生产模块并以 `pub(crate)` 暴露。

### 删除候选

- `m2_create_session_codec.rs`
- `m2_snapshot_event_codec.rs`
- `m7_command_codec.rs`
- `m7_command_response_codec.rs`
- `protocol_bootstrap_router.rs`
- `public_manifest_active.rs`
- `public_route_codec.rs`
- `typed_json_codec.rs`
- `wire_carriers.rs`
- `wire_limits.rs`
- `wire_paths.rs`
- `wire_values.rs`
- 仅测试 old owner values/CAS 的文件

在删除前确认其中没有 provider、path-security 或 JSONL recovery 的独立价值；有价值场景移写为新 API 测试，而不是保留旧 fixture。

### 验收

- 生产源文件内测试部分原则上不超过该文件生产代码体量。
- 复杂 race 测试只保留用户可观察取消/关闭/重启结果，不测试内部 permit identity。

---

## 5.23 `docs/`

### 最终保留

```text
docs/
├── architecture.md
├── api.md
├── persistence.md
├── tool-security.md
└── migration-v0.1-v0.2.md
```

### 修改

- 重写 `architecture.md`，只描述第 3 节架构和四态状态机。
- 删除或归档大量 current ADR/module 文档；最终权威合同不得要求 AI 阅读数十份 ADR 才能理解一个方法。
- `persistence.md` 固定 format version 2、原子写和 JSONL recovery。
- `tool-security.md` 明确文件 capability 与子进程权限边界。
- `migration-v0.1-v0.2.md` 列出破坏性 API 和存储变化。

### 验收

- README 能在一屏内说明项目是什么、如何启动一个 coding agent、内置工具、安全边界和不支持范围。
- 文档不再把历史 milestone closure 当作当前架构说明。

---

## 6. 新模块具体设计

## 6.1 `ids.rs`

使用一个小宏或显式 newtype：

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(UuidLike128);
```

可继续使用 `getrandom` 生成 128-bit ID，并编码为固定小写 hex/base32。不要为了每种 ID 建立独立 grammar、Wire input/output 和 owner constructor。

要求：

- `SessionId`、`TurnId`、`InteractionId`、`ToolCallId`。
- parse/display/serde 统一实现。
- 最大长度固定并测试。
- 不暴露底层随机字节。

## 6.2 `error.rs`

```rust
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("invalid configuration: {0}")]
    InvalidConfiguration(String),
    #[error("runtime is shutting down")]
    Closing,
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("internal task failure")]
    Internal,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session not found")]
    NotFound,
    #[error("session is already loaded")]
    AlreadyLoaded,
    #[error("session is busy")]
    Busy,
    #[error("session is closing")]
    Closing,
    #[error("interaction does not match current turn")]
    InteractionMismatch,
    #[error("workspace unavailable: {0}")]
    Workspace(#[from] WorkspaceError),
    #[error("model unavailable: {0}")]
    Model(#[from] ModelError),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("session actor stopped")]
    ActorStopped,
}
```

原则：错误 enum 按 owner 定义，只在边界做一次 `From`；不要为 runtime/residency/executor 同一错误各定义一份。

## 6.3 `event.rs`

```rust
pub enum SessionEvent {
    Snapshot(Arc<SessionSnapshot>),
    TurnStarted { turn_id: TurnId },
    TextDelta { turn_id: TurnId, delta: String },
    ReasoningDelta { turn_id: TurnId, delta: String },
    ToolStarted { turn_id: TurnId, call: ToolCallSummary },
    ToolFinished { turn_id: TurnId, result: ToolResultSummary },
    InputRequested { turn_id: TurnId, question: UserQuestion },
    TurnFinished { turn_id: TurnId, outcome: TurnOutcomeSummary },
    ResyncRequired,
    Closed,
}
```

可靠性策略：

- snapshot 由 `watch` 保存最新值。
- delta 由 bounded `broadcast` 发送，允许 lag。
- 状态变更同时更新 watch；因此即使 terminal event lag，宿主也能 resnapshot 得到 terminal state。
- event stream 初始化先 yield 当前 snapshot。

## 6.4 `session/actor.rs`

实现时优先使用一个清晰的 `tokio::select!`：

```rust
loop {
    tokio::select! {
        Some(command) = self.commands.recv() => self.handle_command(command).await?,
        Some(event) = self.turn_events.recv(), if self.active.is_some() => {
            self.handle_turn_event(event).await?;
        }
        else => break,
    }
}
```

active task 的正常 completion 通过 `TurnActorEvent::Finished` 返回；不要同时维护多套 operation completion channel。spawn wrapper 必须在最外层捕获 panic，并始终发送一次 `Finished(FailedInternal)`：

```rust
tokio::spawn(async move {
    let outcome = AssertUnwindSafe(run_turn(ctx))
        .catch_unwind()
        .await
        .unwrap_or(TurnOutcome::Failed(TurnError::InternalPanic));
    let _ = turn_event_tx.send(TurnActorEvent::Finished { turn_id, outcome }).await;
});
```

actor 主动 abort task 时，由 actor 自己完成 closing 清理，不等待第二个 terminal signal。

## 6.5 `agent/runner.rs`

`TurnContext` 至少包含：

```rust
pub(crate) struct TurnContext {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub config: Arc<SessionConfig>,
    pub model: Arc<ModelGateway>,
    pub tools: Arc<ToolRegistry>,
    pub policy: Arc<dyn ToolPolicy>,
    pub workspace: Arc<Workspace>,
    pub conversation: Arc<ConversationLog>,
    pub events: EventSink,
    pub interactions: InteractionSink,
    pub cancellation: CancellationToken,
}
```

执行不变量：

- 一个 round 只有一次 model terminal response。
- tool call id 在一个 assistant response 内唯一；重复则 Turn 失败。
- 工具按顺序执行，每个调用必须生成一个结果。
- 只有 complete assistant response 和 complete tool result 写入 conversation；delta 不持久化。
- 达到 `max_tool_rounds` 时写 terminal failed entry。
- 任何取消都写 terminal cancelled entry，并返回 actor。

## 6.6 `tools/context.rs`

```rust
pub struct ToolContext<'a> {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub workspace: &'a Workspace,
    pub cancellation: CancellationToken,
    pub interactions: &'a InteractionClient,
    pub process_policy: &'a ProcessPolicy,
}

impl ToolContext<'_> {
    pub async fn ask_user(&self, question: UserQuestion) -> Result<UserAnswer, ToolError>;
}
```

不要让 Tool 访问 Runtime、SessionActor sender、store internals 或 provider credential。

---

## 7. `run_command` 详细规格

这是本重构中新增的核心功能，必须按本节实现和验收。

### 7.1 Tool schema

```json
{
  "name": "run_command",
  "description": "Run one executable with structured arguments in the workspace",
  "input_schema": {
    "type": "object",
    "properties": {
      "program": { "type": "string", "minLength": 1, "maxLength": 256 },
      "args": {
        "type": "array",
        "items": { "type": "string", "maxLength": 8192 },
        "maxItems": 128
      },
      "cwd": { "type": "string", "maxLength": 4096 },
      "timeout_ms": { "type": "integer", "minimum": 100, "maximum": 600000 },
      "env": {
        "type": "object",
        "additionalProperties": { "type": "string", "maxLength": 8192 },
        "maxProperties": 32
      }
    },
    "required": ["program"],
    "additionalProperties": false
  }
}
```

Rust 参数：

```rust
#[derive(Deserialize)]
struct RunCommandArgs {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
    timeout_ms: Option<u64>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}
```

### 7.2 禁止 shell 字符串接口

核心不提供：

```rust
run_shell { command: "cargo test && ..." }
```

模型必须使用 `program + args`。宿主若需要 shell，可注册自定义工具并承担安全责任。

### 7.3 ProcessPolicy

```rust
pub struct ProcessPolicy {
    pub enabled: bool,
    pub allowed_programs: ProgramPolicy,
    pub inherit_env: bool,
    pub allowed_env: BTreeSet<String>,
    pub default_timeout: Duration,
    pub max_timeout: Duration,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

pub enum ProgramPolicy {
    Any,
    AllowList(BTreeSet<String>),
}
```

建议 production 默认：`enabled = false`；提供 `ProcessPolicy::coding_agent_local()` 显式启用。README 的完整示例必须启用它。

### 7.4 启动流程

1. 校验 tool 已启用、policy enabled。
2. `program` 不得为空、含 NUL；按 policy 校验 basename 或完整值。
3. `cwd` 使用 `RelativePath` 解析，并通过 `Workspace::command_cwd` 得到 root 内目录。
4. 环境默认 `env_clear()`，然后按配置复制最小环境：
   - Windows 可能需要 `SYSTEMROOT`。
   - 常用 `PATH` 只有 policy 允许时继承。
   - 额外 env key 必须在 allowlist。
5. `tokio::process::Command`：
   - `.current_dir(cwd)`
   - `.stdin(Stdio::null())`
   - `.stdout(Stdio::piped())`
   - `.stderr(Stdio::piped())`
   - `.kill_on_drop(true)`
6. 并发读取 stdout/stderr；达到上限立即触发取消并终止进程，保留截断内容。
7. `tokio::select!` 等待：
   - child exit
   - Turn cancellation
   - timeout
   - output limit exceeded
8. cancellation/timeout/limit 时调用 `start_kill()`，再在小的 cleanup timeout 内 wait；失败也要返回明确状态。

### 7.5 输出

```rust
#[derive(Serialize)]
struct RunCommandOutput {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    cancelled: bool,
    output_truncated: bool,
    duration_ms: u64,
}
```

- 非 UTF-8 使用 `String::from_utf8_lossy` 并标记 `lossy: true`，或明确只返回 base64；选择一种并测试。推荐 lossy + flag，便于模型使用。
- 非零 exit code 是正常 ToolOutput，不是 runtime error。
- spawn 失败、policy deny、cwd 错误是 ToolError/Denied。

### 7.6 安全边界

- `cwd` 在 workspace 内不等于子进程只能访问 workspace。
- 默认不读取 stdin，防止挂起等待输入。
- 不继承全部环境和秘密。
- 不宣称跨平台杀死整个进程树；v0.2 至少保证直接 child 被终止。进程组/Job Object 可作为后续平台模块。

### 7.7 测试

1. `program` + args 原样传递，不经过 shell。
2. cwd 确实位于 workspace 子目录。
3. `..` 和绝对 cwd 被拒绝。
4. 非零退出码返回正常结果。
5. stdout/stderr 同时大量输出不会 deadlock。
6. 超时终止 child。
7. Runtime cancel 终止 child。
8. 输出超过上限终止并标记 truncated。
9. env allowlist 生效，未允许的 secret 不可见。
10. Windows 和 Unix 使用平台无关的小测试程序或当前 test binary helper，避免依赖 `/bin/sh`。

---

## 8. 持久化与迁移策略

### 8.1 v0.2 persistence 不变量

1. `session.json` 永远是一个完整 JSON 文档。
2. `conversation.jsonl` 每行一个完整语义事件，以 `\n` 结束。
3. 一个 Session 同时只有一个 conversation writer。
4. delta 不落盘。
5. seq 单调递增，不复用。
6. 读取遇到末尾半行可修复；中间损坏明确失败。
7. Session 关闭不改写全部历史。
8. compaction 只追加 summary，不删除历史。

### 8.2 v0.1 Store 迁移

推荐 **不在 Runtime 热路径自动兼容两套 Store**。如果已有重要数据：

- 创建一次性 `examples/migrate_store_v1.rs` 或独立 binary。
- 输入旧 data root，输出新的 data root。
- 只迁移：当前 Session definition、metadata、conversation terminal history。
- 不迁移：Agent catalog、revision history、fork provenance、archive 状态、Wire cursor。
- 转换后运行新 store 的 load/replay 校验。
- 迁移失败不得修改原目录。

不要让 `SessionStore::open` 同时理解 Store V1 generation 和 v2 layout；这会把已删除复杂度重新引入核心。

---

## 9. 分阶段实施任务

以下任务可以原样交给代码模型执行。每个任务必须形成一个可审查 commit；除 `P8` 切换外，尽量保持旧核心仍可编译。

## P0 — 固定基线与验收护栏

**目标**：在重构前证明基线可构建，并建立新的行为验收骨架。

**操作**：

1. 从 `5088bc254548b3e80e87179898ebb7abbea52c7d` 创建分支 `refactor/v0.2-core-reset`。
2. 运行并保存输出：

```bash
cargo +1.85.0 fmt --all -- --check
cargo +1.85.0 check --all-targets
cargo +1.85.0 clippy --all-targets --all-features -- -D warnings
cargo +1.85.0 test --lib
cargo +1.85.0 test --tests
```

3. 新建 `docs/migration-v0.1-v0.2.md`，声明 breaking change。
4. 新建 `tests/v2_acceptance.rs`，先用 `#[ignore]` 放入第 10 节场景名称，不复制旧内部测试。
5. 记录当前依赖和模块图。

**完成条件**：基线结果可复现；任何现有失败被记录，而不是在重构中悄悄修掉。

---

## P1 — 建立无循环的基础 DTO

**文件**：

```text
src/ids.rs
src/error.rs
src/event.rs
src/model/types.rs
src/tools/types.rs
src/session/state.rs
src/session/snapshot.rs
```

**操作**：

1. 定义统一 ID newtype。
2. 定义 provider-neutral `ModelMessage/ModelRequest/ModelResponse/ToolCall/Usage`。
3. 定义 `ToolSpec/ToolOutput/UserQuestion/UserAnswer`。
4. 定义四态 `SessionStatus` 和最小 snapshot/event。
5. 所有类型 derive serde；不得依赖 `wire`。
6. 加入 compile-time 模块引用检查：基础 DTO 不导入 runtime/session actor/provider。

**测试**：ID parse/serde、DTO JSON roundtrip、snapshot legal shape。

**完成条件**：新基础层可独立编译，且无旧领域 owner 引用。

---

## P2 — 新 ToolRegistry 与精简 Workspace

**文件**：

```text
src/tools/registry.rs
src/tools/policy.rs
src/tools/context.rs
src/workspace/root.rs
src/workspace/path.rs
src/tools/builtins/{ask_user,path_args,read_file,list_directory,write_file}.rs
```

**操作**：

1. 实现 `Tool` trait、registry builder、重名检查。
2. 实现 `ToolPolicy` 最小 allow/deny/ask。
3. 从旧 workspace 移植 capability-relative 安全打开代码，但只保留单 root。
4. 逐个重写四个 builtin；不得复用 `ToolExecutionPlan`、`ToolStartGate`、mutation queue。
5. 建立 `EchoTool` 测试证明动态注册。

**测试**：文件读/list/写、path traversal、symlink、read-only、ask_user mock interaction。

**完成条件**：新 tools/workspace 不导入旧 `tools.rs`、`workspace.rs`、`wire`。

---

## P3 — 模型层拆分并适配旧 Provider

**文件**：

```text
src/model/provider.rs
src/model/gateway.rs
src/model/transport.rs
src/model/providers/openai.rs
src/model/providers/anthropic.rs
```

**操作**：

1. 实现 immutable provider registry。
2. 把 `resolve_for_turn`/`generate_model_turn` 语义迁入新 gateway。
3. 先用 adapter bridge 调旧 OpenAI/Anthropic encode/stream 代码，保持 fixture 行为。
4. 再将协议代码移动到新路径。
5. scripted fixture 移到 tests/common。
6. 移除 dynamic source reload 和 structured output 默认路径。

**测试**：请求编码、SSE/stream、tool calls、usage、取消、safe retry。

**完成条件**：新 model 层不导入 prompt/live conversation/session owner。

---

## P4 — 新 Store 与 ConversationLog

**文件**：

```text
src/session/store.rs
src/session/conversation.rs
```

**操作**：

1. 实现 v2 layout 和 atomic `session.json`。
2. 实现 JSONL append/replay/seq。
3. 实现 transcript `after_seq + limit`。
4. 实现末尾半行恢复。
5. 实现未完成 tool call 的 restart repair 策略。
6. 实现 summary entry 和 prompt view。

**测试**：create/load/list/delete、append/restart、partial line、middle corruption、tool repair、transcript range。

**完成条件**：新 store 不引用 DurableState、Wire store、Fork 类型。

---

## P5 — PromptBuilder 与 Compaction

**文件**：

```text
src/prompt/builder.rs
src/prompt/compaction.rs
```

**操作**：

1. 用直接 system prompt + conversation projection + tool specs 构造 ModelRequest。
2. 移植 token estimate 的最小必要逻辑。
3. 使用最新 summary + 后续 entries。
4. compaction 追加 summary，不 live replace。

**测试**：消息顺序、tool schema、summary selection、预算超限、摘要失败。

**完成条件**：prompt 模块无 source/catalog/provenance/skills。

---

## P6 — SessionActor 与 Agent Loop

**文件**：

```text
src/session/command.rs
src/session/actor.rs
src/agent/turn.rs
src/agent/runner.rs
```

**操作**：

1. 实现一个 mailbox、四态 actor。
2. 实现 ActiveTurn cancellation、completion 和 interaction channel。
3. 实现 model → tools → model 循环。
4. 顺序执行工具，写入完整 conversation entries。
5. 实现 snapshot watch 和 event broadcast。
6. 实现 close timeout；禁止 PrepareUnload/security/probe/publication 状态。

**测试**：简单回复、工具一轮/多轮、ask_user、cancel、busy、stale completion、close active turn。

**完成条件**：新的 v2 acceptance 中除命令工具外的场景全部通过。

---

## P7 — Runtime、SessionManager 与 `run_command`

**文件**：

```text
src/config.rs
src/runtime/runtime.rs
src/runtime/session_manager.rs
src/tools/builtins/run_command.rs
```

**操作**：

1. 实现 typed Runtime API。
2. 实现简单 Session map 和 load/create/remove/list。
3. 实现 Runtime shutdown_all。
4. 按第 7 节实现 run_command。
5. README 添加完整 coding agent 配置示例。

**测试**：runtime API、并发不同 sessions、process success/failure/timeout/cancel/output/env/cwd。

**完成条件**：第 10 节所有核心场景通过，宿主不使用 dispatch/wire 即可完成 coding agent。

---

## P8 — 切换公开 API 并删除旧实现

**操作顺序**：

1. 更新 `lib.rs`，让新 API 成为唯一公开入口。
2. 更新所有新 integration tests 使用正式路径。
3. 删除：

```text
src/agent_session_lifecycle.rs
src/compaction.rs
src/conversation_storage.rs
src/durable_state.rs
src/http_transport.rs             # 新 transport 已迁移后
src/live_conversation.rs
src/model_gateway.rs
src/model_gateway/
src/prompt.rs
src/runtime.rs
src/runtime_interface.rs
src/runtime_task.rs               # 新 TaskGroup 已迁移后
src/session_execution.rs
src/session_ingress.rs
src/session_residency.rs
src/session_transcript.rs
src/skills.rs
src/tools.rs
src/tools/                        # 旧 builtin；新路径已存在时谨慎逐文件替换
src/turn_execution_context.rs
src/turn_item_interaction.rs
src/wire/
src/workspace.rs
```

4. 删除旧 integration tests 和 fixtures。
5. `rg` 清理所有旧类型名。

**禁止**：为了让旧测试通过而添加长期 compatibility shim。

**完成条件**：`cargo check/test/clippy` 在没有旧模块声明的情况下通过。

---

## P9 — 依赖、文档与规模门禁

**操作**：

1. 删除无用 crate feature/dependency。
2. 拆分超过目标长度的 provider 文件。
3. 移出生产内嵌 fixture。
4. 重写 README 和五份权威文档。
5. 运行模块循环检查和代码规模检查。
6. 生成 v0.1 → v0.2 breaking API 表。

**完成条件**：满足第 11 节全部质量门禁。

---

## 10. 必须通过的验收矩阵

| ID | 场景 | 验收结果 |
|---|---|---|
| AT-01 | Model-only Turn | submit 后收到增量，terminal 为 Completed，restart 后 transcript 存在 |
| AT-02 | Read file | 模型调用 `read_file`，结果回送模型，模型给最终回答 |
| AT-03 | Edit file | `write_file` 修改目标，内容正确，path 不能逃逸 root |
| AT-04 | Run tests | `run_command(program="cargo", args=["test", ...])` 返回 exit/stdout/stderr |
| AT-05 | 多轮工具 | 模型连续两轮 tool call 后完成，所有 call 各有一个 result |
| AT-06 | Ask user | Session 进入 WaitingForInput；正确 answer 恢复，错误 id 被拒绝 |
| AT-07 | Cancel model | 取消时 provider future 观察 token，Turn 为 Cancelled，Session 回 Idle |
| AT-08 | Cancel process | 长命令被取消，直接 child 终止，无悬挂 wait task |
| AT-09 | Runtime restart | terminal conversation、usage、summary 可加载；可继续 submit |
| AT-10 | Partial JSONL | 最后一行半写时自动回到上一完整行；中间损坏明确报错 |
| AT-11 | Compaction | 超阈值产生 summary；下一请求不重复发送 summary 前全部消息 |
| AT-12 | Workspace security | 绝对 path、`..`、symlink escape、read-only write 全部拒绝 |
| AT-13 | Provider conformance | OpenAI/Anthropic fixture 的文本、tool call、usage、error、取消正确 |
| AT-14 | Session isolation | 两个 Session 并行运行互不阻塞、不会串 conversation/event |
| AT-15 | Event lag | subscriber lag 后收到 ResyncRequired，snapshot 可恢复正确终态 |
| AT-16 | Busy rule | active Turn 时第二个 submit 返回 Busy，不产生隐式队列 |
| AT-17 | Close | active Turn close 时先 cancel，超时后 abort，registry 最终移除 |
| AT-18 | Custom Tool | 宿主注册 EchoTool，无需修改 runtime/tools match 即可被模型调用 |
| AT-19 | Secret env | run_command 默认看不到未 allowlist 的宿主 secret |
| AT-20 | No legacy coupling | `src` 中无 `wire`、DurableState、SessionResidency、AgentRevisionRef 引用 |

---

## 11. 质量和规模门禁

### 11.1 编译与测试

```bash
cargo +1.85.0 fmt --all -- --check
cargo +1.85.0 check --all-targets --all-features
cargo +1.85.0 clippy --all-targets --all-features -- -D warnings
cargo +1.85.0 test --lib
cargo +1.85.0 test --tests
cargo +stable check --all-targets --all-features
cargo +stable test --all-targets --all-features
```

### 11.2 静态规则

以下命令必须无生产结果：

```bash
rg '#!\[allow\(dead_code\)\]|#\[allow\(dead_code\)\]' src
rg 'crate::wire|mod wire|pub mod wire' src
rg 'DurableState|SessionResidency|SessionIngress|AgentRevisionRef' src
rg 'dispatch_once|RuntimeCommandInFlight|ToolStartGate|SessionFileMutationQueue' src
```

### 11.3 规模目标

- 单一生产 `.rs` 文件：建议 ≤ 1,500 行；provider adapter 可 ≤ 2,000，超出必须再分私有模块。
- 普通函数：建议 ≤ 120 行；硬上限 200 行。
- 一个 enum 的业务 variant：建议 ≤ 12。
- public core 类型：约 20 个，不含 provider/tool 扩展类型。
- 核心生产代码（不含 provider adapters、测试）：建议 15k–25k LOC。
- 全部生产代码：建议小于 40k LOC；这是方向性门禁，不得为了数字牺牲安全检查。

### 11.4 架构规则

- 模块依赖图无 strongly connected component。
- 每个事实只有一个 owner。
- 不存在“为了 stale completion”而创建的 pointer identity proof；使用 id 比较。
- 不存在同一命令在 runtime/residency/executor 三层重复错误 enum。
- 不存在领域 DTO 与 Wire DTO 的双份定义。
- 测试辅助对象不进入 production struct。

---

## 12. 风险与控制

### 12.1 Provider 行为回归

**风险**：结构迁移时改变 OpenAI/Anthropic request/stream/error 细节。  
**控制**：P3 先 bridge 旧 adapter，保留 fixture；在新 Agent Loop 稳定后再物理移动和拆分，不同时重写协议语义。

### 12.2 文件安全退化

**风险**：精简 Workspace 时把 capability-relative 安全实现错误替换为简单 `canonicalize + join`，引入 TOCTOU/symlink 逃逸。  
**控制**：保留 cap-std 核心打开逻辑；只删除多 root/authority/source 层。所有 path-security 测试必须先迁移再删旧模块。

### 12.3 命令执行权限过大

**风险**：子进程拥有宿主进程权限，cwd 限制不是沙箱。  
**控制**：显式 opt-in、环境清空、program policy、timeout/output cap、文档警告；生产部署建议容器/低权限用户。

### 12.4 持久化兼容

**风险**：删除 Store V1 后旧数据无法直接加载。  
**控制**：提供一次性离线迁移器；不在核心维护双格式。

### 12.5 删除旧测试导致缺陷漏检

**风险**：旧竞态测试中可能包含真实用户可观察 bug。  
**控制**：先建立 AT-01～AT-20；逐个旧测试判断其保护的是用户语义还是内部机制。用户语义重写后才能删除。

### 12.6 大爆炸切换

**风险**：同时改动所有模块，难以定位回归。  
**控制**：P1～P7 新旧并存；P8 只做公开切换和删除。每个阶段独立 commit，可回滚到上一个通过点。

---

## 13. 交给代码模型的总指令

以下内容可作为执行提示词附在本文件前：

```text
你正在重构 zqcli/minicore-runtime，固定基线为
5088bc254548b3e80e87179898ebb7abbea52c7d。

严格按《MiniCore Runtime v0.2 精简重构实施规格》执行。
目标是完成允许 breaking change 的 v0.2 Core Reset，不是兼容旧 Wire/API。

非协商规则：
1. 先完成对应阶段的新行为测试，再删除旧实现。
2. 不得通过新增 compatibility wrapper 长期保留 dispatch/query/wire。
3. 不得保留 Agent durable entity、revision/status、Fork/Archive、shared reload、
   security invalidation、SessionResidency、SessionIngress、DurableState generation。
4. 每 Session 只保留一个 actor、一个 mailbox、一个 active Turn；状态只有
   Idle/Running/WaitingForInput/Closing。
5. Tool 必须使用通用 Tool trait + ToolRegistry；默认顺序执行。
6. 必须实现 run_command，并满足 timeout/cancel/output/env/cwd 安全验收。
7. 文件工具必须保留 capability-relative 和 symlink 防护，不能退化为不安全 path join。
8. Provider 协议行为先保持，再拆文件；不要在同一步重写协议语义。
9. 生产代码不得新增 allow(dead_code) 或测试 gate。
10. 每个任务结束运行 fmt/check/clippy/test，并在提交说明中列出：
    - 修改文件
    - 删除的旧概念
    - 新增测试
    - 未完成项

从 P0 开始，按依赖顺序逐任务实施。不要跨过未通过的完成条件。

使用gpt-5.6-luna为有状态subagent，max思考级别来完成代码，为了加快速度允许多个subagent并行，你自己再来合并冲突，允许多个worktree，你自己来组织即可。

每完成一个合理的部分就进行本地add commit，完成一个大的P重构再进行全量测试。
```

---

## 14. 最终完成定义

只有同时满足以下条件，才可声明本项目完成精简：

1. 一个宿主通过 typed API 能创建 Session、提交 coding 任务、观察流式输出、执行文件与命令工具、回答问题、取消和重启恢复。
2. 当前 README 中 Post-MVP 的 `process adapter` 与 generic Tool registry 已在简化形态闭合。
3. `runtime.rs`、`session_execution.rs`、`durable_state.rs`、`wire/public_protocol.rs` 四个复杂度中心已经删除，而不是改名或包一层。
4. 默认核心不存在 Agent 管理面、Fork/Archive、共享资源重载和动态 security recovery。
5. 模块无循环依赖、无生产 `allow(dead_code)`、无重复 Wire/领域 DTO。
6. AT-01～AT-20 全部通过，Rust 1.85 和 stable 门禁全绿。
7. README、architecture、API、persistence、tool-security 与实际代码一致。

---

## 15. 审查依据与固定链接

- 固定提交：<https://github.com/zqcli/minicore-runtime/commit/5088bc254548b3e80e87179898ebb7abbea52c7d>
- 固定代码树：<https://github.com/zqcli/minicore-runtime/tree/5088bc254548b3e80e87179898ebb7abbea52c7d>
- README：<https://github.com/zqcli/minicore-runtime/blob/5088bc254548b3e80e87179898ebb7abbea52c7d/README.md>
- 架构文档：<https://github.com/zqcli/minicore-runtime/blob/5088bc254548b3e80e87179898ebb7abbea52c7d/docs/architecture.md>
- `runtime.rs`：<https://github.com/zqcli/minicore-runtime/blob/5088bc254548b3e80e87179898ebb7abbea52c7d/src/runtime.rs>
- `session_execution.rs`：<https://github.com/zqcli/minicore-runtime/blob/5088bc254548b3e80e87179898ebb7abbea52c7d/src/session_execution.rs>
- `session_residency.rs`：<https://github.com/zqcli/minicore-runtime/blob/5088bc254548b3e80e87179898ebb7abbea52c7d/src/session_residency.rs>
- `durable_state.rs`：<https://github.com/zqcli/minicore-runtime/blob/5088bc254548b3e80e87179898ebb7abbea52c7d/src/durable_state.rs>
- `tools.rs`：<https://github.com/zqcli/minicore-runtime/blob/5088bc254548b3e80e87179898ebb7abbea52c7d/src/tools.rs>
- `workspace.rs`：<https://github.com/zqcli/minicore-runtime/blob/5088bc254548b3e80e87179898ebb7abbea52c7d/src/workspace.rs>
- `model_gateway.rs`：<https://github.com/zqcli/minicore-runtime/blob/5088bc254548b3e80e87179898ebb7abbea52c7d/src/model_gateway.rs>
- `prompt.rs`：<https://github.com/zqcli/minicore-runtime/blob/5088bc254548b3e80e87179898ebb7abbea52c7d/src/prompt.rs>
- `wire/public_protocol.rs`：<https://github.com/zqcli/minicore-runtime/blob/5088bc254548b3e80e87179898ebb7abbea52c7d/src/wire/public_protocol.rs>

