# V2 ADR Index

ADR记录决策理由；当前行为仍以[`architecture.md`](../architecture.md)和[`modules/`](../modules/README.md)为最高权威。实现者必须按本索引区分current、later-refined与historical决策，不能只按编号新旧判断。

## Current

| ADR | Decision |
| --- | --- |
| [0101](0101-workspace-ownership.md) | Workspace属于Session definition |
| [0102](0102-prompt-tool-skill-are-distinct-subsystems.md) | Prompt、Tool、Skill保持独立module |
| [0127](0127-session-recording-omits-turn-lifecycle.md) | Conversation recording不保存Turn lifecycle |
| [0128](0128-prompt-content-is-materialized-before-publication.md) | Prompt content在publication前materialize |
| [0129](0129-user-message-contributions-use-part-level-safe-provenance.md) | 用户消息贡献使用part-level安全provenance |
| [0130](0130-user-message-composition-resolves-skills-asynchronously.md) | 用户消息异步解析captured Skill |
| [0131](0131-conversation-recording-excludes-session-definition-and-lifecycle.md) | Conversation recording排除Session definition/lifecycle |
| [0132](0132-compaction-derives-markers-from-live-stable-units.md) | Compaction从live stable units派生marker |
| [0135](0135-workspace-public-input-is-host-neutral.md) | Workspace public input在command application前保持host-neutral |
| [0136](0136-durablestate-operation-owned-generations.md) | DurableState使用operation-owned immutable generations、permanent reservations与root lease；read with ADR 0137 |
| [0137](0137-tokio-owner-tracked-async-foundation.md) | Tokio owner-tracked async foundation与deterministic persistent seams；refines ADR 0117 and supports ADR 0136 |
| [0140](0140-tool-sandbox-admission-fails-closed-before-start.md) | Tool Sandbox capability admission在start前fail closed；ADR 0143/0144提供production `FilesystemRead` consumers |
| [0141](0141-provider-calls-are-stateless-full-request.md) | Provider调用是无状态full-request wire policy：一次invocation零或一次`ProviderAdapter::execute`、独立地零或一次POST（若发送则携带完整full request）、无optimization fallback/continuation；显式cache annotation与continuation保持omission |
| [0142](0142-production-ask-user-is-a-closed-opt-in-builtin.md) | Production `ask_user`是closed、default-off、Runtime-owned builtin：`MiniCoreRuntimeConfig::with_ask_user_tool()` idempotent opt-in、zero permission、仅UserQuestion或frozen PreExecution failure plans、deterministic compact JSON answer；ADR 0143/0144增加独立可组合的Workspace read selections |
| [0143](0143-production-read-file-uses-workspace-capabilities.md) | Production `read_file`是closed、default-off、Workspace-bound builtin：`MiniCoreRuntimeConfig::with_read_file_tool()` idempotent opt-in、ReadOnly Workspace authority ceiling（requested ReadWrite收紧为ReadOnly、Prompt/Skill source保持false）、per-admission WorkspaceSnapshot-bound materialization、cwd-relative `WorkspaceRelativePath` only、`FilesystemRead` exact sandbox、fixed result texts与65,536-byte单Text part bound、per-Session permanent revocation integrated with host invalidation |
| [0144](0144-production-list-directory-uses-bounded-capability-enumeration.md) | Production `list_directory`是closed、default-off、Workspace-bound direct enumeration builtin：与`read_file`共享ReadOnly authority/revocation，empty path表示cwd，capability-relative directory open，不递归/不跟随entry symlink，256-entry/8,192-name-byte/65,536-JSON bounds与deterministic compact JSON |

## Current With Later Refinements

这些ADR仍包含有效原则，但必须同时阅读所列later decisions；冲突条款以后者和current modules为准。

| ADR | Read With |
| --- | --- |
| [0100](0100-domain-model-and-ownership.md) | ADR 0126 |
| [0103](0103-turn-item-interaction-model.md) | ADR 0124、0126、0127 |
| [0105](0105-session-executor-owns-loaded-session.md) | ADR 0126、0127、0139 |
| [0106](0106-model-gateway-is-single-deep-operation.md) | ADR 0139、0141 |
| [0108](0108-runtime-public-protocol.md) | ADR 0126、0127、0133 |
| [0109](0109-review-b-determinism-and-serialized-operations.md) | ADR 0124、0126 |
| [0110](0110-prompt-and-skill-use-shared-reloadable-views.md) | ADR 0127、0129 |
| [0111](0111-session-ingress-separates-control-and-work-lanes.md) | ADR 0124、0126、0127 |
| [0113](0113-user-question-uses-runtime-protocol-and-ui-presentation.md) | ADR 0124、0126、0127、0133、0142 |
| [0114](0114-runtime-observation-uses-snapshot-first-streams.md) | ADR 0126、0127、0133 |
| [0116](0116-file-mutations-use-session-local-queues.md) | ADR 0126 |
| [0117](0117-async-synchronization-uses-single-owner-and-typed-permits.md) | ADR 0124、0125、0126、0127、0136、0137 |
| [0118](0118-cancel-acknowledges-immediately-and-followup-waits-for-settlement.md) | ADR 0124、0126、0127、0133 |
| [0119](0119-model-calls-use-session-logical-retries.md) | ADR 0126、0139、0141 |
| [0120](0120-failures-stay-with-owning-modules.md) | ADR 0126 |
| [0121](0121-workspace-updates-require-idle.md) | ADR 0124、0126、0127、0140、0143、0144 |
| [0123](0123-identity-uses-refs-and-explicit-reload.md) | ADR 0124、0126、0127、0129、0132、0141 |
| [0124](0124-session-replay-is-tolerant-and-links-are-minimal.md) | ADR 0126、0127、0131、0132、0134、0136、0137 |
| [0125](0125-model-gateway-has-no-local-call-permits.md) | ADR 0141 |
| [0126](0126-turn-execution-is-async-and-session-recording-is-best-effort.md) | ADR 0127、0130、0132、0136、0137、0139 |
| [0133](0133-runtime-public-payload-is-snapshot-recoverable.md) | ADR 0135、0136、0137 |
| [0134](0134-public-and-conversation-wire-use-bounded-v1-schemas.md) | ADR 0135 |
| [0138](0138-production-provider-baseline-uses-verified-rig-contracts.md) | ADR 0139、0141 |
| [0139](0139-rig-is-evidence-only-under-rust-1-85.md) | ADR 0141 |

## Historical / Superseded

这些文件只解释历史选择，不能作为current实现合同。M0归档后原路径保留短redirect stub，原文位于`docs/archive/v2/adr/`。

| ADR | Successor |
| --- | --- |
| [0104](0104-session-storage-is-durable-truth.md) | ADR 0126 |
| [0107](0107-compaction-uses-strict-stable-suffix.md) | ADR 0112，最终由ADR 0124/0132收口 |
| [0112](0112-compaction-supports-active-turn-checkpoints.md) | ADR 0124/0132 |
| [0115](0115-agent-loop-is-first-party-state-machine.md) | ADR 0126 |
| [0122](0122-workspace-fingerprints-are-runtime-local.md) | ADR 0123 |

V1 ADR全部位于[`docs/archive/v1/adr/`](../archive/v1/adr/)，不进入本索引。

## ADR规则

- Accepted/current ADR不倒改历史理由；新决策通过新ADR或明确refinement note表达；
- fully superseded ADR可归档，partially superseded ADR保持稳定路径直到剩余规则被完整接管；
- module已是semantic owner时，ADR只解释why，不复制第二份可漂移interface；
- 新横切决策回写顺序：canonical module → consumers → ADR/index → development plan/review。
