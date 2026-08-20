# ADR 0140：Tool Sandbox admission在start前fail closed

状态：Accepted
日期：2026-08-11

> 2026-08-12：production OS-backed `FilesystemRead` routes已由[ADR 0143](0143-production-read-file-uses-workspace-capabilities.md)与[ADR 0144](0144-production-list-directory-uses-bounded-capability-enumeration.md)交付：`read_file`与`list_directory`都是closed、default-off、Workspace-bound builtins，经本ADR的admission门禁（exact `FilesystemRead` ceiling、available sandbox contract、frozen denial）进入Execute。本ADR的fail-closed admission、approval pairing与truthful settlement规则继续有效；其第8条列举的production ToolService、generic permission producer与public Tool DTO等仍pending。

## 背景

第四轮评审的conditional V4-C0-1（第一轮O1、第二轮R7）要求：首个production OS/network/process Tool或Sandbox adapter出现前，MiniCore必须能判断adapter是否真正声明了所需capability class；无法强制时必须在任何side effect start前拒绝，approval和Sandbox故障都不能退化为裸执行。

现有`ToolOperationSlot`已经拥有exact request、`ToolStartGate`、EmergencyControl first-wins reservation、Running cooperative cancellation和truthful settlement，但这些机制只回答“何时允许开始”，不回答“当前Sandbox能否强制这次执行要求的能力”。如果把capability检查留给start factory内部，factory可能已经构造future、取得外部handle或进入副作用路径；如果把approval当作Sandbox替代，host一次允许也可能扩大adapter无法强制的能力。

M13因此先建立adapter-independent class-level contract和fake backend evidence，再允许M14实现production Tool/Sandbox adapters。该门禁不负责提前发明resource-level path/host/process grant，也不负责在尚无production file Tool和canonical mutation target consumer时实现Session-local file mutation queue。

## 决策

1. Tools拥有一个closed、crate-private capability class集合：

   - `FilesystemRead`；
   - `FilesystemWrite`；
   - `Network`；
   - `Process`。

   `ToolPermissionSet`是一个plan最终class-level要求的唯一事实；raw bits保持private。Restricted candidate只能等于或缩小current ceiling，新增任何class都返回closed typed error。

2. 每个captured ToolSet持有一个`ToolSandboxContract`：`Available(enforceable)`或`Unavailable`。`admit(final_permissions)`只有在Sandbox available且`final_permissions ⊆ enforceable`时成功；capability gap精确保存`final − enforceable`，Unavailable不伪造enforceable set。

3. Direct `Execute` plan在离开Tools owner前完成admission。失败把未调用的move-only start factory直接丢弃，并冻结为fixed、bounded、non-secret的`PreExecution + Denied` ToolResult；任何`ToolStartGate` reservation、factory invocation或executor future poll都不得发生。该结果仍在slot的unstarted-settlement first-wins下绑定：若Emergency signal已经先赢，slot记录matching `PreExecution + Cancelled`而不是Denial，但factory同样保持未调用。

4. Approval plan携带同一个final permission ceiling。host只能选择exact request提供的option index或Deny：

   - `AsRequested`映射private `AllowOnce`，resume前重新admit原ceiling；
   - `Restricted`映射private `AllowWith(candidate)`，先在release code证明candidate不宽于ceiling，再admit candidate；
   - elevation、capability gap或Sandbox unavailable都直接settle fixed `PreExecution + Denied`，不进入start gate。

   Approval只表达用户授权，永远不替代Workspace/policy/Sandbox enforcement。M14 production constructor必须在release code强制`AsRequested ↔ AllowOnce`、`Restricted ↔ AllowWith` exact pairing；不能只依赖safe view kind或测试构造器。

5. Model-visible denial不泄露missing capability、path、host、process参数、adapter internals或raw error。Capability gap固定为`tool capabilities cannot be enforced`，Sandbox unavailable固定为`tool sandbox is unavailable`。Exact missing set只留在private typed error和tests中。

6. Sandbox admission不改变`ToolOperationSlot`生命周期：

   - signal或stale basis在start前获胜时，同一个已admit plan仍settle matching `PreExecution + Cancelled`，factory不调用；
   - start获胜后slot进入Running，signal只触发operation-local cancellation，Settling继续await同一个run至cooperative cleanup/result，最终保存truthful Executed或Abandoned outcome；
   - Sandbox unavailable/capability gap是pre-execution denial；只有更早的Emergency unstarted-settlement winner可以把尚未绑定的frozen denial记录为matching Cancelled，它永远不伪造成Executed或Abandoned；
   - Approval resolution permit先赢后产生的denial按该exact permit继续绑定Denial；后到signal不撤销已经授权的resolution settlement。

7. Production adapter不能只声明capability而把raw executor交给MiniCore。M14中每个adapter必须用local mock/fake backend证明其effective enforcement与声明一致；initialization/enforcement failure必须投影为Unavailable或pre-execution denial，不能fallback到无Sandbox执行。返回前还必须提供有界、可确认的cleanup，使Running settlement合同可兑现。

8. M13/V4-C0-1以本ADR、class-level contract、direct Execute/approval wiring和adapter-independent round conformance关闭。关闭门禁不等于production ToolService、permission producer、resource-level grants、Sandbox adapter或public Tool DTO已经实现；这些仍属于M14。

9. ADR 0116的Session-local file mutation queue仍有效，但与本门禁分开实施。它要求真实file Tool先产生authorized canonical target，才能验证same-Session alias FIFO、different-key parallel和cross-Session no coordination。没有该consumer时不创建测试专用queue。首个production file-mutation adapter必须先实现并通过ADR 0116测试；这不阻塞provider adapter或不操作文件的production Tool adapter。

## 可执行证据

- `src/tools.rs`：closed capability set、`ToolPermissionSet` narrowing、`ToolSandboxContract::admit`、exact gap、fixed denial、direct Execute admission和approval resume revalidation；
- `src/tools.rs` tests：Available/Unavailable fake backend、exact difference、empty/non-empty set、Restricted narrowing/elevation rejection、direct plan gate untouched、approval AllowOnce/AllowWith matrix；
- `src/session_execution.rs` tests：non-empty admitted plan下SecurityRevoked-before-start不调用factory并记录matching Cancelled；Sandbox unavailable round记录Denied且继续完整Tool exchange；Running admitted plan观察operation cancellation、等待cooperative cleanup并记录`Executed + Cancelled`；
- `docs/modules/tools.md`：canonical policy/approval/Sandbox order、execution lifecycle和future production adapter contract。

## 后果

M14可以实现production adapters，但不能绕过Tools-owned admission或把adapter错误降级成裸执行。Class-level set刻意很小：它判断“该类能力是否可强制”，不尝试取代Workspace authorized path、network host policy、process executable/argument policy或具体OS Sandbox配置。

该设计不提供动态撤销已经进入kernel/provider的副作用。SecurityRevoked只与start reservation first-wins；已经Running的operation按cooperative cancellation和truthful settlement收口。

File mutation queue继续由ADR 0116拥有。等真实file Tool/canonical target seam存在时再实现它，避免为关闭无关门禁建立没有consumer的浅模块。

本ADR关闭第四轮V4-C0-1、第一轮O1与第二轮R7，并允许M14开始production Provider和Tool/Sandbox adapters；它不supersede ADR 0116、0121、0124、0126或0133。
