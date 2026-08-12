# ADR 0146：Production write_file 将Capability Target绑定到Session-local FIFO

状态：Accepted
日期：2026-08-12

## 背景

M14已经交付closed/default-off的`ask_user`、`read_file`与`list_directory` production builtins，但仍没有真实file mutation consumer。ADR 0116要求首个file mutation adapter同时交付Session-local canonical-target FIFO、按原始`call_index`预留ticket、waiting cancellation与permit-through-settlement；ADR 0140又要求`FilesystemWrite`在任何side effect start前被Sandbox与resource-level Workspace authority共同强制。

直接在`write_file` executor中调用`Dir::open_with(create + truncate)`不能满足这些约束：raw relative path不能证明两个symlink alias是同一physical file；不存在目标没有file identity；异步future的poll顺序不能替代assistant原始call顺序；executor自己释放锁也会在Emergency signal进入`Settling`后过早放行下一项。另一方面，Runtime现有read opt-in只安装ReadOnly authority，不能因为增加一个Tool bool就把requested ReadOnly Workspace静默提升为可写。

本ADR冻结第一个mutation slice：一个覆盖或创建单个UTF-8 text file的closed `write_file` builtin，以及它真实需要的Workspace capability target、Session-local queue和`ToolOperationSlot` permit ownership。它不建立generic Tool registry、多资源锁、跨Session协调、atomic replace、mkdir、append、patch、rename或public Tool DTO。

## 决策

1. **Closed builtin with exact surface。** ToolName恰为`write_file`，mode为`Parallel`：相同physical target由Session-local queue串行，不同target可并行；它不把整个普通Tool batch降级为`Serial`。description恰为：

   > Write UTF-8 text to one file relative to the workspace working directory, replacing its full contents or creating the file when its parent directory exists.

   input schema是closed JSON object，恰有两个required string：`path`（`maxLength: 4096`）与`content`（`maxLength: 16384`），`additionalProperties: false`。`WorkspaceRelativePath`仍是path semantic authority；empty/root path拒绝。`content`必须是safe UTF-8 text、允许empty、不做newline normalization，exact bytes写入。16,384是`BoundedJsonObject` decoded string的真实上限；不虚构不可达的65,536-byte输入合同。

2. **Default-off fourth opt-in and frozen order。** `MiniCoreRuntimeConfig::with_write_file_tool()`是唯一production入口，default-off、idempotent，并与三个既有opt-in独立。`ProductionToolConfig`冻结四个bool与16种closed selection；definition/spec顺序固定为`ask_user → read_file → list_directory → write_file`。没有generic registry、arbitrary merge、dynamic executor callback或name-based open-world installation。

3. **Write opt-in installs a ReadWrite authority ceiling, not an unconditional grant。** 没有write opt-in时，既有read opt-in仍只选择ReadOnly ceiling。write opt-in存在时，Runtime选择ReadWrite filesystem authority ceiling，但`intersect_filesystem_grants`保持权威：declared root请求`ReadOnly`时最终仍是`ReadOnly`并拒绝write；只有请求`ReadWrite`且authority未revoke的root得到`ReadWrite`。Prompt/Skill source ceilings保持false，trust保持`Restricted`。Tool installation、Workspace requested access、authority ceiling与Sandbox四者缺一不可。

4. **Filesystem revocation covers read and write。** 既有per-Session permanent read revocation收窄为同一个Runtime-owned filesystem access control：host `invalidate_session_workspace_authority`仍在signal/re-resolve前幂等发布永久revocation；之后该Session的每次resolve/revalidation都得到filesystem `None`，read与write routes共同Denied，且本Runtime lifetime无unrevoke。default Runtime不安装该control。

5. **Capability-relative write authorization。** `WorkspaceAccessView::authorize_write`只接受captured cwd所在exact root内的non-empty canonical `WorkspaceRelativePath`，并要求该root最终grant恰为`ReadWrite`。返回opaque `AuthorizedWorkspaceWritePath`：captured root `Dir`加normalized capability-relative target；Debug不披露path，production无ambient absolute accessor。所有target preparation与write都只经该capability解析。

6. **Preparation opens identity without mutation。** synchronous Tool planner只构造一个move-only typed mutation preparation factory；它不执行I/O或await。round owner在任何Tool start前按`call_index`调用该factory，调度一个owner-tracked blocking preparation并等待同一个job完整settle；preparation可以打开capability handles和读取metadata，但不得create、truncate或write。普通OS/target failure冻结为`PreExecution + Failed`；`RuntimeTaskError`或preparation panic冻结为identity-bound`Abandoned { RuntimeFailure }`。Emergency signal若在factory调用前已赢则不启动job；job一旦scheduled，即使signal随后到达也必须等待settlement，然后保持unstarted cancellation，不预留ticket或开始mutation。

7. **Existing target key and carrier use the exact opened file。** preparation首先通过captured root capability以write-only、nonblocking、follow-final-symlink方式打开目标，不truncate；symlink escape由capability resolver fail closed。成功后必须以fstat证明regular file，并从该exact open file clone建立`same_file::Handle`。`FileMutationKey::Existing`使用该opaque physical identity；executor随后就在preparation保留的同一open file上执行mutation，不按path重新打开。因此direct path、in-root symlink alias与hard-link alias命中同一key，且path在preparation后被替换也不会把已授权mutation重定向到replacement。

8. **Create target key uses the exact opened parent plus final name。** 只有target open返回NotFound时才进入create preparation。`write_file`不创建目录，所以direct parent必须已经存在并能通过captured root capability打开；其physical identity来自该exact parent `Dir`的`same_file::Handle`，key为`Create { parent_identity, normalized_final_name }`。这是ADR 0116“nearest-existing ancestor + normalized remaining suffix”在本narrow Tool中的精确形状：因为不mkdir，nearest existing ancestor必须是direct parent，remaining suffix恰为一个filename。executor只通过保留的parent `Dir`打开final name。

9. **Create execution rejects final symlinks。** create carrier在start后用write/create/truncate且final-component no-follow的capability open；不存在目标被创建，preparation后已出现的regular file可被同一覆盖语义打开，但dangling/late symlink、directory或special entry fail closed。实现使用pinned exact`cap-std`/`cap-primitives` no-follow option，不能退回ambient path或check-then-follow。外部process在preparation后改变目录内容不受Session queue协调；若外部参与者恰在两个sibling preparation之间改变target的不存在/已存在分类，本Runtime不声称这两个不同时间观察到的keys仍能提供跨参与者线性化。capability containment与no-follow仍保证不会经create path写到别处。

10. **Session owner and per-admission capture。** 每个loaded `SessionExecutor`创建且私有持有一个`Arc<SessionFileMutationQueue>`，直到executor unload/drop；Agent、Session/Turn public DTO、Runtime-global service与Workspace snapshot都不拥有它。每次Turn admission在exact Workspace snapshot已经captured后，将同一Session queue与Workspace/task context一起传入production ToolSet materialization。不同Session永不共享queue，即使指向同一physical file。

11. **Mutation plan is a real lifecycle input, not a guessed parameter convention。** `write_file`产生typed single-file mutation preparation；Session Execution不根据Tool name、argument field或path string猜资源。ordinary non-mutation Execute保持现有路径。UserQuestion仍先全部hoist并按`call_index`settle；随后remaining operations按`call_index`串行完成mutation preparation和ticket reservation，再允许不同key并行drive。

12. **FIFO ticket is exact-request-bound。** `SessionFileMutationQueue::reserve`同步把`FileMutationKey`与exact `ToolExecutionRequest` capture绑定为move-only ticket。相同key按reservation顺序FIFO；不同key独立。waiting ticket future被取消/drop时从队列删除并唤醒next；最后一个ticket/permit离开后删除该key entry。queue不持有SessionId；SessionId只通过queue实例ownership区分。

13. **Permit acquisition precedes Tool start; permit ownership belongs to the slot。** Prepared slot持有ticket并在drive时等待其turn；Emergency signal等待期间获胜则ticket取消、executor不start并settle matching PreExecution Cancelled。ticket成为permit后，slot才通过既有Emergency owner mutex预留`ToolStartGate`；signal若在两者之间获胜则不start并释放未使用permit。成功start后，`Running` variant持有permit，进入`Settling`时permit随variant移动。

14. **Permit covers the complete mutation and outcome-capture window。** natural completion、executor panic、start-factory invariant与signal cancellation的started paths都必须先进入携带permit的`Settling`；signal path继续await同一个run/blocked job。只有exact outcome已经绑定且slot从`Settling`转为`Terminal`时才能drop permit并唤醒next。executor future、blocking closure与`write_file` module本身无权直接释放queue permit。

15. **In-place full replacement semantics。** existing target在保留的exact file handle上从offset zero truncate并`write_all(content)`；create target在保留parent capability下open/create/truncate后`write_all(content)`。不append、不自动加newline、不preserve old tail、不atomic rename、不fsync、不声称crash durability。write syscall失败是truthful`Completed + Failed`，目标可能已经被truncate或partially written；owner/join failure使最终完成事实不可确认时必须`Abandoned { RuntimeFailure }`，不能伪装success或retry。

16. **Cancellation and fixed results。** 全部model-visible文本fixed、bounded、nonsecret：

   - parse/path/content semantic failure：`tool arguments are invalid`，`PreExecution + Failed`；
   - no ReadWrite grant/Workspace basis：`workspace file write access is denied`，`PreExecution + Denied`；
   - preparation ordinary target/OS failure：`file could not be written`，`PreExecution + Failed`；
   - start后、blocking write job创建前cancellation赢且可证明零mutation：`file write was cancelled`，`Completed + Cancelled`；
   - scheduled write已settle为open/truncate/write failure：`file could not be written`，`Completed + Failed`；
   - success：`file written`，`Completed + Succeeded`；
   - tracked owner/worker/panic/join uncertainty：`Abandoned { RuntimeFailure }`，无fabricated text。

   write job一旦scheduled就不drop/detach；后到cancellation保持await同一job并保留truthful success/failure。application bytes/allocation有界，但ordinary filesystem wall-clock不声明timeout。

## 可执行证据

- Workspace tests：ReadOnly/ReadWrite requested-access intersection、per-Session filesystem revocation、cwd-relative write authorization、existing regular/symlink/hard-link identity、symlink escape、missing target parent identity、missing parent与special entry rejection、create final no-follow；
- queue/slot tests：same-key call-index FIFO、different-key parallel、waiting cancellation wakes next、two Session queues不协调、last entry cleanup、foreign exact request fail closed、signal after start keeps permit through `Settling` until the same run settles；
- `write_file` tests：exact definition/schema/order、16,384-byte/unsafe content boundaries、empty overwrite、existing/create success、read-only denial、no mkdir、symlink alias FIFO、create no-follow、before-job cancellation zero mutation、after-job cancellation truthful settlement、partial/failure与tracked RuntimeFailure mapping；
- Runtime tests：default-off/idempotent/independent opt-in、16种closed disclosure、ReadOnly root denial、ReadWrite end-to-end write并继续Turn、host invalidation后read/write共同revoked。

## 后果

- ADR 0116首次获得真实production consumer；其“fully canonical path”语言由本ADR细化为capability-opened physical handles，避免把ambient path text误当作authority或identity。
- `ToolOperationSlot`不再把mutation permit视为future work：started mutation的Running/Settling ownership与FIFO release成为同一个deep lifecycle。
- `write_file`只保证同一loaded Session内、同physical target sibling calls的deterministic FIFO；跨Session、跨Runtime、编辑器和外部process仍由host/worktree隔离。
- generic ToolService、多文件patch/rename/delete、atomic replace、network/process adapters与public Tool DTO继续等待独立consumer和contract。

## 被否决的方案

- **按raw relative path加Mutex。** symlink/hard-link alias会分裂key，且path不是physical authority。
- **ambient canonicalize后按absolute path排队。** 泄露并重新引入ambient authority，且key与最终capability open可能race到不同对象。
- **在executor future内首次reserve。** 顺序依赖future poll而非assistant `call_index`。
- **executor完成I/O后自己drop permit。** signal path会在slot仍Settling/结果未知时提前放行next。
- **write opt-in无条件授予ReadWrite。** 绕过Workspace definition的requested access，混淆Tool安装与host authority。
- **atomic temp-file rename。** 会替换inode并改变symlink/hard-link语义，还引入directory sync、mode/ownership与cleanup合同；不是本narrow overwrite consumer。
- **自动创建父目录。** 将单target write扩大为多目录mutation，key与rollback语义不再是single-file queue。
