# ADR 0116：文件 mutation 使用 Session-local FIFO 队列

状态：Partially Superseded by ADR 0126
日期：2026-07-27

> 2026-07-30：Session-local file mutation queue与跨Session不协调规则保留；current Turn的ActiveTurnTask使用该queue，旧SessionExecutor single mutable owner表述由ADR 0126修订。

> 2026-07-31：构造路径明确为SessionExecutor持有queue，并在Starting candidate capture时把同一`Arc<SessionFileMutationQueue>`连同Turn-scoped `ToolExecutionControl` handle注入ToolSet；ToolSet不在task spawn后补注入依赖。

> 2026-08-12：[ADR 0146](0146-production-write-file-binds-capability-targets-to-session-fifo.md)交付首个真实consumer并细化本ADR的canonical-target规则：existing target使用capability-opened exact physical file identity，create target使用exact direct-parent identity+normalized final filename；queue、exact-request-bound ticket、waiting cancellation与permit-through-Settling均已实现。跨Session不协调与multi-resource abstraction rejection保持不变。

## 背景

模型可以在同一个 assistant step 中返回多个 ToolCall。若同一 Session 的两个 `edit` / `write` 调用并行读取同一个旧文件并分别写回，后完成的调用会覆盖先完成的修改。MiniCore 能完整控制这些 sibling ToolCall 的本地调度，因此需要给出确定性顺序。

多个 Session 指向同一个 Workspace 时，场景等价于多个独立 Agent/CLI 实例操作同一个仓库。仅在一个 MiniCore Runtime 内增加跨 Session 资源锁无法覆盖其他 Runtime、编辑器、Git 客户端或外部脚本，会形成不完整的并发保证。跨 Session 的工作区隔离应由 host/user 通过独立 Workspace、worktree 或外部协调完成。

同类产品采用两类保守方案：pi 使用进程模块内、按 canonical file path 建立的 mutation promise queue，使同文件写入串行而不同文件仍可并行；Codex 和 Gemini CLI 对文件 mutation Tool 使用 Turn/batch 级串行。MiniCore 采用 pi 的单文件队列算法，并采用显式 Session ownership；多文件与 open-world Tool 使用更粗粒度的批次串行。

## 决策

1. **每个 loaded Session 拥有独立的 `SessionFileMutationQueue`**。它是 `SessionExecutor` 的执行期私有状态；当前 Turn 的 `ToolSet`只持有共享引用。Agent、Session、Turn 领域对象和 Runtime-global `ToolService`不拥有该队列。
2. **只排队文件 mutation**。`read`、`list`、`search`、`stat` 等只读文件操作不进入队列。`edit`、`write`、`delete` 等只有一个明确 mutation target 的调用按 `FileMutationKey` FIFO 串行；不同 key 可以并行。
3. **队列覆盖完整 mutation window**。对 read-modify-write Tool，从首次读取目标到最终写入和原始 outcome 捕获都持有 permit。异常路径使用 RAII/finally释放；底层 I/O 尚未收口时不得因 cancellation 提前释放。
4. **FIFO 顺序来自原始 ToolCall 顺序**。ToolSet 按 `call_index` 串行完成 preflight、authorization、canonicalization 和 ticket reservation，再启动允许并发的调用。这样不依赖异步任务poll顺序，也不需要 pi 的 process-global registration queue。
5. **`FileMutationKey` 使用 Session 内 canonical physical identity**。现有目标使用 fully canonical path；不存在的 create target使用canonical nearest-existing ancestor加规范化剩余path。raw参数名、裸相对路径、`WorkspaceId`和SessionId不能代替canonical target。SessionId只决定队列实例，不进入file key。
6. **多文件和 open-world Tool 降级为 `Serial`**。rename/move、多文件patch、无法证明单一mutation target的copy，以及bash/process-spawning/open-world Tool在MVP中按Serial处理。若同一assistant step的runnable调用中存在任一Serial调用，整个普通Tool批次按原始顺序执行。
7. **等待响应 cancellation**。尚未开始的cancelled ticket从队列移除或标记跳过，并唤醒下一个waiter；已经开始的mutation等待底层操作确认完成或形成outcome unknown后再释放permit。
8. **不提供跨 Session、跨 Runtime或跨进程文件协调**。两个Session即使拥有相同`FileMutationKey`也使用不同队列，可能并发写同一文件。该行为是明确的best-effort共享Workspace语义，host/user负责隔离与冲突处理。
9. **删除MVP通用资源锁协议**。不建立`ToolResourceLocks`、`ToolResourceAccess`、`ToolResourceKey`或多资源锁总序；外部资源并发同样不由MiniCore统一协调。`ToolRequirements`继续表达权限需求，授权后的结构化文件权限由ToolSet私有地投影为`None | SingleFile(FileMutationKey) | Serial`调度分类，禁止根据参数名称猜测。
10. **UserQuestion和approval发生在排队前**。等待人工输入时不预留mutation ticket、不持有file mutation permit或TurnControl reservation。普通Tool在开始mutation后不得发起UserQuestion。

## 理由

- Session内sibling ToolCall是MiniCore主动创建的并发，队列可以完整覆盖并提供确定性收益。
- 跨Session锁只能协调单Runtime中的参与者，无法构成真实文件系统隔离；明确要求worktree/独立Workspace更诚实。
- 单key FIFO没有多锁死锁、锁升级和部分持锁等待问题；多文件Tool通过批次串行收口。
- Session-owned对象具有明确生命周期、测试隔离和diagnostics归属，避免Rust全局static状态让多个Runtime测试或embedding实例意外耦合。
- 当前每个Session最多一个Starting/Running Turn，跨Turn本身已串行；queue只需处理同一Tool批次中的sibling mutation。

## 后果

- 同一Session同一文件的并行edit/write不会因读取同一旧版本而相互覆盖；不同文件仍可并行。
- 读操作不参加队列，因此不提供read/write snapshot consistency；需要read-after-write顺序的Tool必须进入Serial批次或由后续模型step发起。
- 多Session共享Workspace可能出现lost update、Git index冲突或读取其他Session中间结果；这些属于host/user管理范围。
- `ToolService`不再拥有通用资源锁；`SessionExecutor`新增一个小型、非持久化的`SessionFileMutationQueue`。
- O4“多资源锁稳定总序”通过删除跨Session多资源锁协议和禁止并行多文件mutation而关闭。

## 测试要求

- 同Session、同canonical file的两个mutation按`call_index` FIFO执行；
- 同Session、不同file key的mutation可以并行；
- read/list/search/stat不进入queue；
- symlink alias与等价absolute/relative path在同Session命中同一key；
- create target通过canonical parent生成稳定key；
- waiting ticket cancellation不会阻塞后续ticket；in-flight I/O未结束时permit不提前释放；
- 任一Serial Tool使同批普通ToolCall按原始顺序执行；
- 两个Session对同一physical file不共享queue，测试明确展示其无协调语义；
- queue entry在最后一个ticket完成后清理，不随Session unload残留。

## 修订关系

本ADR修订[ADR 0105](0105-session-executor-owns-loaded-session.md)的跨Session Tool resource lock结论、[ADR 0111](0111-session-ingress-separates-control-and-work-lanes.md)与[ADR 0113](0113-user-question-uses-runtime-protocol-and-ui-presentation.md)中的共享resource-lock表述，并关闭`docs/review/v2-design-review.md`的O4。SessionExecutor单owner、Workspace授权、Sandbox、approval和truthful outcome顺序保持不变；ADR 0124随后以owner-local ToolStartPermit取代durable `ToolExecutionStarted`。