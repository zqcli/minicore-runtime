# ADR 0133：Runtime Public Payload必须可从Snapshot恢复并安全操作

状态：Partially Superseded by ADR 0135
日期：2026-07-31

> [ADR 0134](0134-public-and-conversation-wire-use-bounded-v1-schemas.md)冻结本ADR后置的public JSON v1、typed scalar carriers、ProtocolLimits、bounded JSON与conversation JSONL scanner基础；各semantic payload owner保持不变，Stored DTO/format projection继续由对应module同步。

> [ADR 0135](0135-workspace-public-input-is-host-neutral.md)把本ADR示例中的Workspace public root从durable `WorkspaceRootSpec { path: PathBuf }`细化为Workspace-owned `WorkspaceRootInput { path: CanonicalFileUri }`；JSON V1 shape不变，checked native-path lowering发生在typed command admission之后。

## 背景

ADR 0108、0113、0114和0118已经确定transport-neutral Runtime facade、typed Interaction、snapshot-first stream与Cancel语义，但第四轮实现评审发现public payload仍不能直接生成Rust协议crate或host contract tests：

- SessionSnapshot只有queue count，重连host无法枚举可取消的Submit、Steer和FollowUp；
- QueryResult、RuntimeSnapshot、SessionSnapshot、Item/terminal/usage/diagnostic仍有placeholder或generic payload；
- ToolApproval/UserQuestion request与answer shape、redaction、secret policy和resolution idempotency未冻结；
- Agent/Session metadata update缺少独立CAS read/write闭环；
- Starting Submit在Input apply前被用户Cancel时没有typed original completion；
- module error到public code/retry advice没有canonical mapping；
- `PromptBodyIntent::Template`已经出现在public enum，但没有TemplateId、argument grammar、render、limits或capture contract。

snapshot-first的关键问题不是“Snapshot字段够多”，而是断线后host必须只凭新Snapshot和safe catalog构造当前仍合法的public command；event不能作为恢复所需的唯一事实来源。

## 决策

1. Runtime public protocol的closed roots固定为`RuntimeCommand`、`CommandCompletion`、`RuntimeQuery/QueryResult`、`Snapshot/RuntimeSnapshot/SessionSnapshot`、`StateEvent`和`ProgressEvent`。所有variant使用concrete typed payload；不保留`/* ... */`、generic map、raw module error或未定义future variant。

2. Agent与Session metadata拥有独立opaque monotonic revision：

   ```text
   AgentMetadataRevision
   SessionMetadataRevision
   ```

   Create/read projection返回definition revision与metadata revision；UpdateMetadata只CAS并递增对应metadata revision，返回typed outcome并发布独立metadata event。Runtime event detail携带mutation后的complete safe AgentSummary/SessionSummary和new token；loaded Session event另携带new SessionSnapshot。canonical no-op不递增revision、不写conversation JSONL、不发布event。definition与metadata revision不能互换。

3. `dispatch()`只在协议envelope无法进入Runtime时返回`RuntimeDispatchError`。每个成功进入Runtime的dispatch invocation最终得到一个`CommandCompletion = Completed | Rejected`。原command仍in-flight时，duplicate same CommandId + same canonical command加入同一shared completion；same in-flight ID + different command返回`CommandConflict`。Runtime不持久化或无限保留completed-command cache；response丢失后host用Snapshot/Query确认事实，并为新command生成new CommandId。

4. user Cancel在Starting Submit的Input live apply前先赢时：Cancel command立即返回`CancelAccepted`，所有joined original Submit caller完成为`Completed(SubmitCancelled)`，且不创建Turn。Input apply先赢时原Submit完成为`TurnStarted`，sticky cancel绑定该Turn，随后通过Snapshot/StateEvent观察Interrupted。SecurityRevoked、lifecycle shutdown和invalid input不是`SubmitCancelled`。

5. public `CommandErrorCode + RetryAdvice + subject`是module failure的唯一host decision surface。Runtime Interface拥有canonical mapping table；Prompt、Session Execution、Tool、ModelGateway和Storage继续拥有各自internal error type。host不得解析message或provider字符串决定retry；不建立global Error module、registry或severity hierarchy。

6. SessionSnapshot完整列出当前process内public可操作队列：

   - pre-Turn Submit admission：`CommandId + phase`；
   - Steer：`CommandId + target TurnId`；
   - FollowUp：`CommandId`。

   lane-local顺序稳定，全部列表受ProtocolLimits约束，不公开queued prompt body、Skill selection或preview。每个entry都能直接构造`Cancel(Submit)`或`CancelQueuedMessage`；events/count不能成为恢复这些targets的唯一来源。

7. SessionSnapshot只列Pending Interaction，并携带足以完整渲染和构造合法Resolve的safe request。resolved request从Snapshot移除；`InteractionResolved` StateEvent携带safe resolution detail。prepared Tool args、executor route、private option-to-permission map、sandbox internals和credential不得进入public view。

8. Tool approval是request-scoped option selection：host只提交exact pending request提供的`option_index`或Deny。`AsRequested`映射private `AllowOnce`，`Restricted`映射不能扩大权限的private `AllowWith(ToolPermissionSet)`。MiniCore验证identity、family、index、current policy/Sandbox和ToolStartGate；host不能构造PermissionSet或授权request未提供的能力。

9. MVP UserQuestion只支持明确non-secret、recordable、model-visible的Text/SingleChoice fields。request/answer可以进入live state、conversation JSONL、event/history、ask-user ToolResult并发送给模型；protocol没有password、credential、file upload、arbitrary JSON或`secret` variant。future secret input必须使用独立secure host capability与non-recorded one-time reference，不能通过给Text加flag扩展。

10. Presentation Adapter为每次logical Resolve生成不可预测random 128-bit `InteractionResolutionKey`，retry exact same canonical resolution时复用。scope是exact Session/Turn/Item/Request：same key + same payload在current run幂等且不产生第二mutation/record/event；same key + different payload为`CommandConflict`；different key after terminal为`InteractionAlreadyResolved`。key不是authorization capability。

11. Query/Snapshot/Item public read models只包含UI-safe稳定事实与current loaded execution view。historical Turn不伪造execution status；raw provider error、raw Tool args/result、absolute path、internal cache/storage state和unbounded blob只进入private diagnostics或bounded redacted summaries。

12. MVP `PromptBodyIntent`只允许`Empty | Text(TextIntent)`；Text在boundary normalization后必须non-empty并满足ProtocolLimits。未定义的Template variant从public enum、query和decoder删除。future Prompt template必须一起定义stable TemplateId、argument grammar、materialized render、limits、reload/capture和protocol capability。

13. 本ADR只冻结semantic payload和恢复/操作规则。其acceptance时后置的field casing、enum tagging、base ID/path carrier、Timestamp、Money、ProtocolLimits、unknown variant policy、PageCursor和conversation format基础已由[ADR 0134](0134-public-and-conversation-wire-use-bounded-v1-schemas.md)统一拥有；Stored DTO projection与golden fixtures现由[Format V1](../formats/conversation-jsonl-v1.md)和[Wire V1 Fixtures](../fixtures/wire-v1/README.md)关闭。

## 结果

- 新host可以只用snapshot-first首帧恢复所有current public cancel/resolve affordance；不需要event replay或private executor handle。
- Runtime协议crate可以用closed Rust enums/structs表达command、outcome、query、snapshot、item、Interaction和error mapping。
- approval不会因host任意提交PermissionSet而扩大；UserQuestion对secret fail closed。
- metadata CAS、Submit cancel race和Interaction retry都有可测试的exact completion semantics。
- Snapshot可能比只返回count更大，但内容bounded且只包含操作identity，不复制queued prompt正文。
- Prompt template、secure credential input与wire编码仍可未来扩展，但必须通过明确capability和新typed contract，不能占用未定义variant。

## 实现约束

- Snapshot capture必须在Session owner内原子读取queue、current Turn/Items、Pending Interaction、usage、recording和diagnostic views；不得拼接多个时点的mutable state。
- public projection必须在module owner处完成redaction，再交给Transport/Presentation Adapter；adapter不能接收private object后自行删字段。
- same-key Interaction幂等retry不能重复record、event或Tool resume。
- ProtocolLimits具体数值与bounded decoder以ADR 0134/Wire Schema为唯一owner；各module不得选择不兼容override。
- contract tests至少覆盖queue snapshot recovery、metadata CAS、Submit cancel两条race、error mapping、approval option scope、non-secret question validation、Interaction key conflict和Template variant拒绝。

## 修订关系

本ADR补充并部分修订ADR 0108、0113、0114、0118和0129。它不改变ADR 0126的async Turn/best-effort recording、ADR 0127/0131的conversation-only storage或ADR 0132的Compaction contract。
