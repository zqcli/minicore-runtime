# ADR 0135：Workspace Public Input在Command Application前保持Host-Neutral

状态：Accepted
日期：2026-07-31

## 背景

ADR 0133把`WorkspaceDefinitionInput`冻结为public command payload，ADR 0134把absolute Workspace input path冻结为canonical `file:` URI；但早期module示例又直接复用了durable `WorkspaceRootSpec { path: PathBuf }`。若Wire codec在decode时立即把URI转换为current-host `PathBuf`，同一个canonical POSIX/drive/UNC request会因host不同而被当作protocol-invalid，且accepted command之前就执行了Workspace-owned host conversion。

## 决策

1. Workspace owner区分两种值：
   - `WorkspaceRootInput { path: CanonicalFileUri, ... }`是host-neutral public command intent；
   - `WorkspaceRootSpec { path: PathBuf, ... }`是checked lowering后进入durable `Workspace`的current-host definition。
2. `WorkspaceDefinitionInput`组合`WorkspaceRootInput`，由Workspace owner在typed command构造前验证root count、duplicate key/URI和cwd root引用。constructor失败表示public input未形成，不进入Runtime completion owner。`CanonicalFileUri`仍是Wire-owned shared typed carrier；该组合不是Wire shadow DTO。
3. Wire只负责URI lexical/canonical validation。canonical URI在所有host上都能形成host-neutral input与typed command；command进入Runtime completion owner后，Workspace才按current host family checked-lower为`WorkspaceRootSpec`。
4. admission后的unsupported host family、无法lossless形成native path或其他host lowering failure返回`CommandError::InvalidArgument + DoNotRetry`。它们不是`TypedJsonError`、`RuntimeDispatchError`或temporary Workspace availability failure。
5. Create/Update只有在lowering成功后才能分配WorkspaceRevision、构造durable SessionDefinition或开始Session/Header staging。filesystem existence、symlink/junction canonicalization、trust、containment和source authorization仍属于后续Workspace resolve。
6. 本决策不改变public JSON V1 fields或bytes，不要求protocol minor、capability或fixture migration。
7. Wire conformance在每个host上接受相同canonical POSIX/drive/UNC bytes。Workspace lowering使用trusted `WorkspacePathTarget = Posix | Windows`：Posix只接受POSIX family；Windows接受drive与UNC family；其他组合返回`UnsupportedHostFamily`。测试使用显式target，不允许ambient test OS重新定义Wire validity。

## 不采用

- 在Wire decode中立即转换为`PathBuf`：把host语义放进Wire owner，并使valid conformance vector依赖测试host。
- 为不同OS维护互斥的“valid public fixture”：同一selected V1 representation不应因host改变lexical validity。
- 保留Wire-private DTO直到M7：会绕过Workspace semantic owner并让raw projection越过Wire seam。

## 修订关系

本ADR细化ADR 0133中的Workspace public semantic input，并澄清ADR 0134第8条的URI carrier/Workspace semantic handoff；JSON V1 representation保持不变。
