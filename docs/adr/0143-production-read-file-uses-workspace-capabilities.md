# ADR 0143：Production read_file 使用Workspace capabilities——Closed、Default-Off、Per-Admission、Read-Only Builtin

状态：Accepted
日期：2026-08-12

> 2026-08-12：[ADR 0144](0144-production-list-directory-uses-bounded-capability-enumeration.md)复用本ADR冻结的ReadOnly authority、per-Session permanent revocation、per-admission Workspace capability materialization与`FilesystemRead` sandbox，增加独立closed `list_directory` route，并把`ProductionToolConfig`从两个bool/四种selection扩为三个bool/八种selection。本ADR的`read_file` schema、result、bounded regular-file read与cancellation合同保持不变。

## 背景

M14需要第一个production OS-backed Tool adapter slice。ADR 0140已建立class-level admission门禁（`FilesystemRead`等closed capability class、`ToolSandboxContract`、direct Execute admission与frozen denial），ADR 0142已冻结第一个production builtin `ask_user`（zero permission、无OS资源、closed opt-in），但production ToolSet仍无任何OS-backed adapter：Workspace的resource-level read grant、capability directory capture、read authority与per-Session revocation都没有production consumer，任何Runtime ToolSet在production下要么为空要么只有`ask_user`。

本ADR冻结第二个production builtin `read_file`：一个closed、default-off、Runtime-owned、Workspace-bound的本地UTF-8文本读取Tool。它是ADR 0140门禁的第一个真实`FilesystemRead` resource-level grant consumer，也是Workspace read authority与既有host invalidation seam的第一个production consumer。本ADR只冻结这一个exact local read route；generic ToolService、writes/network/process adapters、mutation queue与public Tool DTO仍不属于本slice。

## 决策

1. **Closed builtin with exact frozen surface。** ToolName恰为`read_file`，mode为`Parallel`（一次call一次有界regular-file read；不向future composed ToolSet中的无关操作强加Serial语义）；description恰为：

   > Read one UTF-8 text file relative to the workspace working directory and return its full contents as a single text part. Use for reading source code, configuration, and other text files inside the workspace.

   input schema是closed JSON object：恰一个required `path` string（`maxLength` 4096、`additionalProperties: false`、不披露`minLength`——empty path是cwd自身，由Workspace authorization作为read target拒绝，不由schema拒绝）。schema是披露guidance，不是semantic authority：`WorkspaceRelativePath` grammar（canonical cwd-relative path、无absolute/dot segment、至多4,096 bytes、至多256 segments）由semantic constructor执行，parse用`deny_unknown_fields` serde mirror。唯一arguments形状是`{"path": "..."}`；无offset/range/encoding/options/absolute path/write/binary/base64输出/mutation queue。

2. **Default-off opt-in与四种frozen production selection。** 默认Runtime ToolSet保持`ToolSet::empty()`；`MiniCoreRuntimeConfig::with_read_file_tool()`（idempotent、与`with_ask_user_tool`相互独立）是唯一production `FilesystemRead`入口。`open`恰好一次冻结`ProductionToolConfig::new(ask_user, read_file)`，产生恰好四种selection：empty、`ask_user`、`read_file`、`ask_user`+`read_file`——composed set的definition/spec顺序固定为`ask_user`在前、`read_file`在后（prompt spec projection mirror同一顺序），composed planner只路由这两个frozen name，unknown name走正常ToolSet lookup保持unavailable。没有generic registry、arbitrary merge API、dynamic callback或name-based open-world lookup。`ToolSet::ask_user_builtin()`/`read_file_builtin()`保持crate-private，只供focused tests；production selection只走`ProductionToolConfig`。

3. **Per-admission WorkspaceSnapshot-bound materialization。** `open`不materialize任何ToolSet：`TurnToolResources::Production(config)`在每次Turn admission中、exact Workspace snapshot已经捕获后且`TurnExecutionContext` capture之前，经`config.for_workspace(workspace, task_context)`对该snapshot的`WorkspaceToolContext` materialize一次（`materialize` consume自身，恰好一次）。`WorkspaceToolContext` cloneable，不携带Prompt/Skill source、approval、registry或provider facts。`read_file` off时返回`new`时冻结的base `Arc`（empty或单`ask_user` builtin）；on时按selection materialize单`read_file` set或composed set。Workspace reload/definition update只影响future Turn admissions；shared-resource reload原样保留frozen config。不存在单例static ToolSet。执行器pinned到exact `RuntimeTaskContext`（owner-tracked job admission）。

4. **Read-only Workspace authority ceiling。** opt-in同时安装read-only resolver：`WorkspaceResolver::new_with_read_access` → `ReadOnlyWorkspaceAuthority`。每个declared root的filesystem ceiling恰为`ReadOnly`，永不为`ReadWrite`；requested-access intersection仍权威（`intersect_filesystem_grants`）：requested `ReadOnly`保持`ReadOnly`、requested `ReadWrite`收紧为`ReadOnly`。Prompt与Skill source ceilings保持false（绝不silently授权source discovery）；trust保持`Restricted`（filesystem grant与source trust独立，不是`Trusted`）。默认Runtime的`RestrictedWorkspaceAuthority`行为完全不变。

5. **Per-Session permanent revocation integrated with host invalidation。** `WorkspaceReadAccessControl`是process-local、concrete、owner-held register：一个synchronized `HashSet<SessionId>`，无generic policy store、无trait、无callback、无public DTO。`revoke`幂等、per-Session、对该Runtime lifetime永久（无unrevoke/recovery）。authority持有同一control，revocation check在authority future构造前同步发生：被revoke Session的filesystem grant为`None`（永不再`ReadOnly`，也非`AuthorityDenied`），Prompt/Skill保持false、trust保持`Restricted`。集成：host `invalidate_session_workspace_authority`在Open lifecycle与residency存在性检查之后、SystemClock timestamp采样与residency invalidation（signal/re-resolve）之前先发布permanent read revocation——hard restriction在recovery重新resolve前已current，即使residency返回SessionNotLoaded/Closing/internal也保持published。default/no-read Runtime持有None control并保持既有invalidation行为。

6. **cwd-relative授权与opaque capability-relative read path。** `WorkspaceAccessView::authorize_read`只授权cwd-relative `WorkspaceRelativePath`：containing root是captured canonical cwd所在exact root；cwd在该root内的相对位置prepend到requested relative path，结果target必须fully normal；root path自身拒绝为`InvalidPath`（cwd是directory，永不是read target）；无readable grant拒绝为`NotAuthorized`；basis/root不可用拒绝为`Unavailable`——所有authorization error collapse为一个frozen non-secret Denied text。额外roots不可直接寻址。返回的`AuthorizedWorkspaceReadPath`是Tool唯一可消费的read value：captured root `Dir` + normalized capability-relative target，永远不暴露ambient absolute path（Debug输出`{ .. }`，`relative_path` accessor仅`cfg(test)`），open一律经captured `Dir`解析，不能逃逸其绑定root。

7. **Capability directory在owner-tracked workspace path phase捕获。** `resolve_path_phase`在单个owner-tracked blocking worker上完整运行：`open_directory`是Workspace path上唯一的ambient-authority使用——一步同时capture root capability（`cap_std::fs::Dir`）与exact `same_file` identity（`WorkspaceRootIdentity(Arc<same_file::Handle>)`）；此后一切（cwd证明、model/Tool path）只经captured `Dir` capability-relative解析，绝无ambient reopen。cwd在该phase内经exact captured root `Dir` capability-relative打开，并与ambient canonical path做same-file证明——root rename/replacement发生在root capture与cwd proof之间时fail closed，绝不把cwd绑定到replacement。

8. **Exact identity与final candidate revalidation。** `ResolvedWorkspaceRoot`的等式比较captured safe identity而非仅canonical path text；`revalidate_candidate`在Load的final durable recheck前重新resolve并比较完整resolution（`has_same_resolution_as`），same path上的root replacement在candidate revalidation处fail closed。`WorkspaceToolContext`只从finished snapshot投影；snapshot finish还执行source↔root authorization匹配检查（`AuthorizationMismatch` fail closed）。

9. **`FilesystemRead` class与exact available sandbox。** valid call的plan是`ToolExecutionPlan::Execute`，携带恰为`ToolPermissionSet::new([FilesystemRead])`与move-only start factory；ToolSet的outer sandbox contract恰为`ToolSandboxContract::available([FilesystemRead])`，admission对该单一class恰好revalidate一次，其他class fail closed为capability gap。composed set的outer contract同为此exact contract：read route恰好admit一次；ask_user route（UserQuestion/PreExecution shapes、零permission）在`FilesystemRead` contract下保持admitted，从不触碰admission。authorization与parse都在start factory存在前同步完成。

10. **Strict schema与fixed result texts。** 全部model-visible结果文本frozen，非secret、有界：

    - `tool arguments are invalid`：任何parse或semantic argument failure，`PreExecution + Failed`；
    - `workspace file access is denied`：任何authorization failure（无readable grant、root path自身或Workspace capability basis不可用），`PreExecution + Denied`，绝不泄露missing capability/path/root/error细节；absolute、dot/dotdot、平台prefix与其他非canonical relative path在更早的`WorkspaceRelativePath` semantic parse阶段归入`tool arguments are invalid`。denial在构造start factory前同步发生，gate slot的reservation/start不受影响；
    - `file read was cancelled`：start后、blocking job创建前已赢得（或race scheduling）的cancellation，`Completed + Cancelled`；
    - `file could not be read`：missing path、directory或non-regular entry（含FIFO）、WouldBlock或其他open/read error、capability open处拒绝的symlink escape、或valid UTF-8但protocol不能disclose为单个safe Text part，`Completed + Failed`；
    - `file is not valid UTF-8`：invalid UTF-8，`Completed + Failed`；
    - `file is too large`：content超过65,536 bytes，`Completed + Failed`；
    - 成功：恰一个Text part。

11. **Nonblocking open + fstat regular-file gate。** executor绝不使用ambient path、`std::fs::read`或`canonicalize`：以O_NONBLOCK capability open（pinned cap-std的`_cap_fs_ext_nonblock`扩展）打开exact authorized target——symlink escape在该open失败；FIFO等special entry不需要writer配对、open立即返回，随后fstat `is_file()`检查拒绝non-regular entry（不hang）；metadata size单独超过bound时未读一字节即拒绝。

12. **Bounded read与固定分配预算。** content bound恰为65,536 bytes（`MAX_FILE_CONTENT_BYTES`）；读取至多65,537 bytes（`MAX_READ_BYTES`）以检测oversize——固定`Vec::with_capacity(65,537)` + 4,096-byte buffer，循环在超过bound瞬间停止，oversize检测从不分配超过固定budget。UTF-8验证与safe-text验证（`validate_safe_text`，同一`ToolResultContent` owner contract）都在blocking closure内完成；成功content已经在closure内通过owner的safe-Text与byte gate，render失败是invariant，fail closed为identity-bound `Abandoned { RuntimeFailure }`。

13. **Owner-tracked blocking job与no detach。** 全部文件处理（capability open、regular-file检查、有界read、oversize、UTF-8、safe-Text）在一个`spawn_blocking_tracked` job（`TrackedBlockingJob<ReadFileOutcome>`）内完成，executor只映射settled outcome；`RuntimeTaskError`（owner closing、worker unavailable、operation panic）settle `Abandoned { RuntimeFailure }`。start一旦发生，cancellation语义：biased select下，已赢得或race scheduling的cancellation在blocking job创建前获胜——零I/O被证明，settle `Completed + Cancelled`与frozen text（truthful disposition，绝不`OutcomeUnknown`）；job被scheduled后永不被drop或detach——cancellation保持await同一个tracked job直到它settle并preserve truthful result，已知成功/失败永不被rewrite。工作量和分配预算有界，但等待该settlement的wall-clock不作上界承诺，见下一条。signal-before-start由gate slot拥有，在executor构造前settle自己的PreExecution Cancelled。

14. **Boundedness是application-level，不是wall-clock guarantee。** 本builtin保证application work/allocation有界：一次nonblocking open + 至多65,537 bytes read、固定分配预算、无unbounded allocation、无FIFO hang。但ordinary regular-file I/O的wall-clock完成时间仍是OS/filesystem-dependent（例如slow或network-mounted filesystem可任意慢返回read）；本ADR不声称timeout，也不声称globally bounded wall-clock cleanup。Cancelled/Abandoned语义基于已settle的事实（零I/O或job已settle），不是wall-clock承诺。

## 可执行证据

- `src/tools/read_file.rs`：frozen definition/description/schema、`ReadFileArguments` strict mirror、`plan`三形状、`read_regular_file` bounded closure、`execute_read` cancellation语义；tests覆盖default empty、schema canonical bytes、parse/semantic failures、authorization denials（无grant/root path自身）、65,536/65,537 boundary（ASCII与multi-byte）、invalid UTF-8/unsafe text/empty file、missing/directory、symlink escape、FIFO（无writer不hang）、cancellation before scheduling（Executed+Cancelled且零I/O）、cancellation after scheduling（同一job、truthful result保留）、tracked runtime failure（Abandoned RuntimeFailure）、unknown name（plan None、无start factory）；
- `src/tools.rs`：`ProductionToolConfig`四种frozen selection与frozen order、`TurnToolResources::materialize` per-admission consumption、`composed_ask_user_read_file` planner-level composition与exact `FilesystemRead` outer contract；
- `src/workspace.rs`：`ReadOnlyWorkspaceAuthority`（ReadOnly ceiling、Prompt/Skill false、Restricted trust）、`WorkspaceReadAccessControl`（process-local、idempotent revoke、authorize前同步check）、`WorkspaceAccessView::authorize_read`、opaque `AuthorizedWorkspaceReadPath`与`open_nonblocking`、owner-tracked `resolve_path_phase`的`open_directory` capability+identity capture与cwd same-file proof、`revalidate_candidate`与`has_same_resolution_as`；
- `src/runtime.rs`：`MiniCoreRuntimeConfig::with_read_file_tool`（idempotent、独立于ask_user）、`open`的resolver选择与`ProductionToolConfig`冻结、`invalidate_session_workspace_authority` revoke-first集成；tests覆盖opt-in idempotency/independence、四种selection disclosure、end-to-end read file并继续同一Turn、authority invalidation后revoke并恢复no-grant（后续admission以revoked snapshot materialize）；
- `src/session_residency.rs`：`start_with_turn_resources_and_production_tools_and_compaction_and_unload_grace`与per-admission materialization；
- `src/session_execution.rs`：`with_turn_resources_and_tool_resources_and_compaction`的`TurnToolResources`接受与materialize。

## 后果

- 第一个production OS-backed Tool slice闭合交付：Runtime可以真实读取Workspace cwd内UTF-8文本文件并继续同一Turn，全程无ambient path、无writes、无approval、无mutation queue；host invalidation现在可以per-Session永久撤销该read authority。
- 默认Runtime行为完全不变（empty ToolSet）；host显式opt-in后才披露/执行`read_file`，且opt-in同时是Tool安装、read-only authority ceiling与per-Session revocable grant。
- Resource-level enforcement只对本exact local read route完整；仍pending：generic ToolService/registry、完整schema/hooks/policy/approval、`FilesystemWrite`/`Network`/`Process` adapters、ADR 0116的Session-local file mutation queue与canonical mutation target、public Tool DTO与具体Skill composition/source。真实credentials与其他OS-backed adapters继续等待独立contract/ADR。M14保持in progress。
- ADR 0116的mutation queue继续由该ADR拥有；本slice不创建测试专用queue。
- ADR 0140的admission门禁与ADR 0142的`ask_user`冻结surface继续有效；本ADR是它们的第一/第二个production consumer，不修改其历史决策条款。

## 被否决的方案

- **Generic ToolService/registry与host executor安装**：引入generic seam会冻结public interface并放大M14范围；当前只有两个closed builtin，narrow `ProductionToolConfig`（四种frozen selection、planner-level composition）足够，generic注册留待未来source adapter slice。
- **默认开启或自动注册read_file**：默认非空ToolSet会改变所有既有Runtime行为并扩大model-visible surface；closed opt-in保持默认行为bit-exact不变。
- **用ambient path、`std::fs::read`或`canonicalize`执行读取**：绕过captured capability containment，无法在authorized root之外fail closed；capability open是唯一allowed路径。
- **绝对路径或additional-root寻址、range/encoding/binary输出**：放大authorization模型与model-visible surface；本slice刻意窄，`WorkspaceAccessView`只授权cwd-relative target。
- **为读路径声称timeout或globally bounded wall-clock cleanup**：application work/allocation可以bounded，但OS/filesystem的wall-clock行为不由Runtime强制；声称timeout或全局wall-clock清理会伪造未实现的guarantee。
