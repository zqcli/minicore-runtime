# ADR 0144：Production list_directory 使用有界的Workspace capability枚举

状态：Accepted
日期：2026-08-12

> 2026-08-12：[ADR 0146](0146-production-write-file-binds-capability-targets-to-session-fifo.md)把production selection扩为四个bool/16种closed形状与固定顺序`ask_user → read_file → list_directory → write_file`，并把owner-held revocation推广为filesystem read/write共同revocation。本文的`list_directory` schema、ReadOnly-only行为、direct enumeration与bounds保持不变；文中“三个bool/八种selection”描述其冻结时点。

## 背景

ADR 0140冻结了Tool Sandbox在start前fail-closed admission，ADR 0143又以production `read_file`证明了第一个真实`FilesystemRead` resource-level route：Runtime显式opt-in、per-admission绑定exact WorkspaceSnapshot、captured `cap_std::fs::Dir` capability-relative open、ReadOnly authority ceiling与per-Session永久revocation都已可运行。

但模型若不知道cwd里有哪些文件，通常只能猜路径或由host预先提供目录结构。为此需要第二个OS-backed read adapter。直接引入递归walk、glob、metadata树、absolute/additional-root addressing、分页cursor或generic filesystem ToolService会同时扩大授权模型、输出上限和public surface；这些都不是“列出一个目录的直接子项”所必需。

本ADR冻结第三个production builtin `list_directory`：一个closed、default-off、Runtime-owned、Workspace-bound的直接目录枚举Tool。它复用ADR 0143已经证明的read-only authority与per-admission capability basis，不新增write/mutation语义、public Tool DTO、Wire/Store字段或generic registry。

## 决策

1. **Exact closed surface。** ToolName恰为`list_directory`，mode为`Parallel`，description恰为：

   > List direct entries in one directory relative to the workspace working directory and return sorted JSON names and types. Use for discovering files and subdirectories without reading file contents.

   input schema是closed JSON object：恰一个required `path` string（`maxLength: 4096`、`additionalProperties: false`）。empty path合法并表示captured cwd；没有recursive、glob、depth、metadata、absolute path、additional-root addressing、pagination或options。private serde mirror使用`deny_unknown_fields`，semantic authority仍是`WorkspaceRelativePath`。

2. **Default-off opt-in与八种closed selection。** `MiniCoreRuntimeConfig::with_list_directory_tool()`是唯一production安装入口，default-off、idempotent，并与`with_ask_user_tool()`/`with_read_file_tool()`相互独立。`ProductionToolConfig::new(ask_user, read_file, list_directory)`冻结三个bool，恰有八种selection；definition/spec固定顺序为`ask_user → read_file → list_directory`，只包含enabled成员且无重复。没有generic registry、arbitrary merge、dynamic executor callback或open-world lookup。

3. **Per-admission Workspace-bound materialization。** ask-only/default selection继续复用`new`时捕获的base `Arc<ToolSet>`；只要`read_file`或`list_directory`任一开启，每次Turn admission都在exact WorkspaceSnapshot捕获后materialize一个新ToolSet，绑定该snapshot的`WorkspaceToolContext`与exact `RuntimeTaskContext`。Workspace reload/definition update只影响future admissions；active Turn保留已captured ToolSet。

4. **复用同一read-only authority与永久revocation。** `read_file_tool || list_directory_tool`任一开启时，`open`选择同一个`WorkspaceResolver::new_with_read_access`与同一个owner-held `WorkspaceReadAccessControl`。每个declared root的filesystem ceiling恰为`ReadOnly`，requested `ReadWrite`收紧为`ReadOnly`，Prompt/Skill source ceilings保持false，trust保持`Restricted`。host invalidation先永久revoke该Session read grant再signal/re-resolve；revoked Session在本Runtime lifetime内无unrevoke，future resolution只得到filesystem `None`。不建立第二个directory authority或revocation registry。

5. **cwd-relative directory authorization。** `WorkspaceAccessView::authorize_read_directory(&WorkspaceRelativePath)`与`authorize_read`共享唯一containing-root/read-grant/cwd-relative prepend/fully-normal containment实现。file read仍拒绝empty target；directory read允许empty target表示cwd。返回opaque `AuthorizedWorkspaceReadDirectory`：只保存captured root `Arc<cap_std::fs::Dir>`与normalized capability-relative target，Debug固定redacted，不暴露ambient absolute path。empty target通过`Dir::try_clone`打开cwd=root；其他target通过captured root `Dir::open_dir` capability-relative打开；symlink escape在该open处fail closed。

6. **Exact FilesystemRead admission。** valid call产生`ToolExecutionPlan::Execute`，permission恰为`ToolCapabilityClass::FilesystemRead`；outer sandbox contract available exactly for`FilesystemRead`。authorization在任何start factory存在前同步完成。parse/semantic failure固定为`PreExecution + Failed`文本`tool arguments are invalid`；任何authorization failure固定为`PreExecution + Denied`文本`workspace directory access is denied`。absolute、dot/dotdot、prefix等非法路径在更早的`WorkspaceRelativePath` parse阶段归入invalid arguments。

7. **只枚举直接entry，不读取内容、不跟随entry symlink。** started executor只经`AuthorizedWorkspaceReadDirectory::open`获得目录capability，然后调用该Dir的`entries()`。每个in-bound entry（前256项）只读取bare name与`DirEntry::file_type()`；成功取得第257项后立即证明overflow而不再检查其name/type；不构造ambient path、不调用`std::fs::read_dir`/`canonicalize`/`current_dir`，不递归、不打开entry、不读取文件内容。type映射恰为`file | directory | symlink | other`；entry symlink按自身file type分类，不跟随target。

8. **Deterministic compact JSON output。** 成功返回恰一个safe Text part，compact JSON shape恰为`{"entries":[{"name":"...","type":"file"},...]}`。每个entry字段顺序固定`name`后`type`；entries按UTF-8 name bytes升序。empty directory返回`{"entries":[]}`。不返回mtime、size、permissions、inode、absolute/relative full path或其他metadata。

9. **有界枚举、复制与render。** 最多保留256个direct entries；为证明overflow最多消费第257个iterator item，且iteration error优先保持unlistable，只有成功取得第257个entry才证明too large。所有retained UTF-8 name bytes总和最多8,192；每个name先借用检查UTF-8与累计budget，通过后才复制为owned String。最终JSON最多65,536 bytes并通过safe-Text validation。长度/safe-text超界统一为`directory listing is too large`。该budget不声称控制filesystem/cap-std为单个DirEntry构造name的内部瞬时分配，但Runtime自身不会在retained budget确认前复制over-budget name，也不会继续无界枚举。

10. **Fixed completed outcomes。** model-visible completed texts恰为：

    - `directory listing was cancelled`：start后、blocking job创建前cancellation赢，`Completed + Cancelled`且证明零I/O；
    - `directory could not be listed`：missing/not-directory/open/iteration error、任一被检查的in-bound entry file-type error或目标目录symlink escape，`Completed + Failed`；
    - `directory contains an unsupported entry name`：任一被检查的in-bound entry name非UTF-8，`Completed + Failed`；
    - `directory listing is too large`：entry/name/render bound超限，`Completed + Failed`。

    closed JSON serialization失败属于内部invariant：blocking operation panic由owner-tracked task捕获并最终settle`Abandoned { RuntimeFailure }`，不得伪装成too large。

11. **Owner-tracked no-detach settlement。** open、枚举、UTF-8/type/bounds、排序、render和safe-text验证都在一个`RuntimeTaskContext::spawn_blocking_tracked` job内。cancellation在job scheduling前赢时不创建job；一旦scheduled，cancellation继续await同一job并保留其truthful settled result，绝不drop/detach或把known success/failure改写为Cancelled。`RuntimeTaskError`统一为`Abandoned { RuntimeFailure }`。枚举item数与Runtime retained allocation有界；ordinary filesystem open/enumeration的wall-clock仍由OS/filesystem决定，本ADR不伪造timeout或globally bounded wall-clock cleanup。

12. **不改变public/storage contracts。** 本builtin只通过host-only `MiniCoreRuntimeConfig`安装并沿既有Tool disclosure/ToolCall/ToolResult路径运行；不新增Runtime capability、Wire command/DTO、Conversation JSONL variant、Durable Store字段/version或public Tool registry。Wire V1 manifest与Store V1 exact documents保持不变。

## 可执行证据

- `src/workspace.rs`：`authorize_read_directory`、共享`containing_read_root`、opaque `AuthorizedWorkspaceReadDirectory::open`及cwd/nested/no-grant/symlink-escape tests；
- `src/tools/list_directory.rs`：exact definition/schema、strict plan、bounded blocking enumeration、deterministic JSON、fixed outcomes、cancellation/no-detach与17项focused tests；
- `src/tools.rs`：三个bool的八种closed `ProductionToolConfig` selection、fixed order、per-admission materialization与read/list exact routing；
- `src/runtime.rs`：`with_list_directory_tool()`、共享read-only resolver/control、八种Runtime disclosure、真实list→ToolResult→next model end-to-end及invalidation后future denial。

## 后果

- Runtime现在有两个真实OS-backed read-only Tool adapters：`read_file`读取一个bounded UTF-8 regular file，`list_directory`发现一个目录的bounded direct entries；二者共享一个Workspace capability/authority/revocation事实源，但各自拥有独立closed schema与executor truth。
- 默认Runtime仍是empty ToolSet；host必须逐项opt-in。`ask_user`仍零permission，两个filesystem routes都恰为`FilesystemRead`。
- M14仍未关闭：真实provider credential smoke、write/network/process adapters、mutation queue/permit、generic ToolService/schema/hooks/full policy/approval与public Tool DTO仍pending。首个mutation adapter仍必须满足ADR 0116。

## 被否决的方案

- **递归walk/glob/pagination。** 会扩大输出、授权与continuation模型；当前直接枚举的256-entry bound足够验证真实directory capability route。
- **返回size/mtime/permissions或跟随symlink。** 需要更多metadata truth、platform差异与target authority；当前只报告entry自身的closed type。
- **用ambient `read_dir`或先canonicalize。** 会重建TOCTOU containment检查并绕开captured capability；directory open必须相对于exact root `Dir`。
- **复用read_file executor或抽generic filesystem adapter。** 两者只有permission/capability basis相同，I/O、bounds、result和error truth不同；现在抽象只会放大interface而不消除真实复杂性。
- **为Structured activation顺便修改Wire/Store。** Public Wire V1与Durable Store V1是closed exact schemas；任何SessionDefinition structured字段都需要独立protocol minor与storage migration，不属于本adapter milestone。
