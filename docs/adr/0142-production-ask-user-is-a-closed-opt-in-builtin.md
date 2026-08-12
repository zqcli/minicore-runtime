# ADR 0142: Production ask-user 是 Closed、Default-Off、Runtime-Owned Builtin

状态：Accepted
日期：2026-08-12

> 2026-08-12：第二个production builtin与第一个resource-level Workspace read grant已由[ADR 0143](0143-production-read-file-uses-workspace-capabilities.md)冻结：生产ToolSet selection扩展为empty/`ask_user`/`read_file`/`ask_user`+`read_file`四种frozen形状（`ask_user`在前、no generic registry），`read_file`按per-admission WorkspaceSnapshot-bound materialization安装。本ADR的`ask_user`冻结surface、zero-permission与closed opt-in决策不变；其后果一节所列pending项中，resource-level workspace read grant已由0143局部交付，其余保持pending。

## 背景

M8/M13已实现crate-private scripted approval/UserQuestion控制正确性seam（typed `ToolExecutionPlan::{Approval, UserQuestion}`、move-only `UserQuestionAnswerBinding`、hoisted exclusive question调度与signal-first settlement），但production ask-user builtin的ToolName、input schema与answer→model-visible ToolResult text/render格式一直未冻结，任何Runtime ToolSet在production下仍为空。M14需要第一个production Tool slice，同时不能引入generic ToolService/registry、host callback/executor安装、authoring格式或public Tool DTO——那些仍属于后续独立slice。

本ADR冻结：`ask_user`是唯一production builtin Tool，默认Runtime ToolSet保持空，host必须通过`MiniCoreRuntimeConfig::with_ask_user_tool()`显式opt-in，`open`恰好选择一次immutable ToolSet并经既有residency capture路径安装。该builtin是Runtime-owned、immutable after open、零capability permission的closed production Tool slice，不是OS-backed Sandbox completion；`MiniCoreRuntimeConfig::with_ask_user_tool()`是idempotent opt-in（重复调用结果相同），不冻结ToolService/registry、不增加Runtime capability、不改变Wire/Store schema或版本。

## 决策

1. **Closed builtin with exact frozen surface。** ToolName恰为`ask_user`；description恰为：

   > Ask the user one or more non-secret text or single-choice questions and return the answers. Use only when the task cannot continue without user input. Never request passwords, API keys, tokens, credentials, or other secrets.

   input schema是closed JSON object：optional nullable `title` + 1..32 `questions`；每question为`questionIndex`(u32)、`prompt`、`required`、`input`；`input`是strict adjacent object，恰为`{"type":"text","data":{"multiline":bool}}`或`{"type":"single_choice","data":{"options":[{"optionIndex":u32,"label":string},...]}}`。未知字段在每一层严格拒绝（schema用`additionalProperties: false`，parse用`deny_unknown_fields` serde mirrors）。schema是披露guidance，不是semantic authority：byte/count/index验证一律由既有semantic constructors（`UserQuestionChoice::reconstruct`、`UserQuestionField::reconstruct`、`UserQuestionRequest::reconstruct`）执行。

2. **Default-off opt-in。** 默认Runtime ToolSet保持`ToolSet::empty()`不变；`MiniCoreRuntimeConfig::with_ask_user_tool()`（idempotent）是唯一production非空ToolSet入口。`open`恰好选择一次immutable ToolSet并把它传入既有`start_with_turn_resources_and_tools_and_compaction_and_unload_grace` residency capture；不新增generic ToolService/registry、host callback/executor安装、authoring format、public Tool DTO、Runtime capability、Wire/Store schema/version变化或session file mutation queue。

3. **零权限production Tool slice。** builtin truthfully要求零`ToolCapabilityClass` permission并使用available empty sandbox contract（`ToolSandboxContract::available([])`）。它绝不创建`ToolExecutionStart`、executor future、cancellation pair、start-gate reservation、approval或任何OS/network/process/file资源；这是production Tool slice，不是OS-backed Sandbox completion，也不为任何future OS-backed adapter预留行为。

4. **Plan闭合。** 一次call的plan只有两种形状：valid call→typed `ToolExecutionPlan::UserQuestion`；任何parse/semantic failure→frozen `PreExecution + Failed`，恰一个Text part、文本恰为`tool arguments are invalid`，且无Interaction。arguments从`BoundedJsonObject::canonical_json()`用private strict serde mirrors解析；omitted与null `title`都映射`None`；question/option indices必须严格递增但不要求从0开始或连续。除既有owner constructors的normalization（newline normalize、safe-text验证）外不做silent normalization。

5. **Answer binding闭合。** valid call的answer binding先经exact `UserQuestionRequest::validate_answer`验证，再产生`PreExecution + Succeeded`、恰一个deterministic compact JSON Text part：`{"answers":[{"questionIndex":3,"value":{"type":"text","data":"hello"}}]}`或`{"answers":[{"questionIndex":7,"value":{"type":"choice","data":{"optionIndex":11}}}]}`；answers保持升序，optional未答可渲染`{"answers":[]}`。JSON escaping是canonical/deterministic（serde固定struct/enum field order，serde escaping不超过canonical escaping计数），输出受既有`ToolResultContent`约束（单part ≤65,536 bytes：projection envelope与`user_answer_encoded_len`同构，故validated answer恒在界内）。binding内的answer re-validation mismatch或render失败是dynamic invariant，fail closed为identity-bound `Abandoned { RuntimeFailure }`，绝不产生malformed model-visible output。

6. **实现位置。** 唯一implementation locality是`src/tools/ask_user.rs`（child module直接访问parent-private `ToolDefinition`/`ToolSpec`/`ToolSetInner`/`ToolPlanner`/`UserQuestionAnswerBinding`字段，只暴露`pub(super) fn build_tool_set() -> Arc<ToolSet>`）；`src/tools.rs`只增加narrow `ToolSet::ask_user_builtin()` production constructor/delegation；generic scripted constructors保持`cfg(test)`。

## 后果

- 第一个production Tool slice闭合交付：Runtime可以真实呈现UserQuestion、接收host answer并继续同一Turn，全程无executor、无start-gate reservation/start、无approval、无OS资源。
- 默认Runtime行为完全不变（empty ToolSet），host显式opt-in后才披露/执行`ask_user`。
- 仍pending：production ToolService/executor/adapters、完整schema/hooks/policy/Sandbox enforcement、resource-level grants、Session-local mutation queue/mutation permit attachment to Settling、public Tool DTO与具体Skill composition/source；真实credentials与OS-backed Tool/Sandbox adapters继续等待独立contract/ADR。M14保持in progress。
- ADR 0113的MiniCore-owned interaction protocol与UI presentation职责分离继续有效；本ADR冻结其production builtin ToolName/schema与answer render格式，并把生产实现路径从scripted seam切换到closed builtin。

## 被否决的方案

- **Generic ToolService/registry与host executor安装**：引入generic seam会冻结public interface并放大M14范围；当前只有单一builtin，narrow `ToolSet::ask_user_builtin()`足够，generic注册留待未来source adapter slice。
- **默认开启或自动注册ask-user**：默认非空ToolSet会改变所有既有Runtime行为并扩大model-visible surface；closed opt-in保持默认行为bit-exact不变。
- **用schema做semantic authority**：schema是JSON Schema guidance，不能可靠表达byte/newline/safe-text语义；semantic constructors保持authoritative，schema只做closed shape disclosure。
