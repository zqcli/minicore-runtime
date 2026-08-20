# Production Provider Gate M12 Fixtures

> Archived V2 evidence. This mapping remains historical reference and is not run by default.

本目录保存production OpenAI Responses/Anthropic Messages direct adapters之前的provider-neutral delivery/error contract。它不是public Wire或Conversation format，也不包含credential、raw request/response body或human error message。

## Authority

- canonical behavior：[Model Gateway](../../../../modules/model-gateway.md)；
- protocol decision：[ADR 0138](../../../../adr/0138-production-provider-baseline-uses-verified-rig-contracts.md)；
- Rust 1.85 implementation decision：[ADR 0139](../../../../adr/0139-rig-is-evidence-only-under-rust-1-85.md)；
- current production ownership：P3 provider suites与AT-13；本文件是archived fixture evidence；
- real Rig HTTP evidence：`provider-gate/tests/m12_rig_*.rs`。

M14 production adapters必须消费同一fixture或生成等价的table-driven mapping tests，不能复制后再独立漂移。

## `error-mapping-v1.json`

顶层字段：

- `version`：当前固定为`1`；
- `protocols`：exact `openai_responses`与`anthropic_messages`；
- `sources`：provider文档与exact Rig version；
- `cases`：两协议各13个closed mapping cases。

每个case包含：

- `id`、`protocol`、`category`；
- `observation.stage`：`connect | http_response | completed_response | stream`；
- optional `httpStatus`、typed `errorType`/`errorCode`、`retryAfterSeconds`；
- `semanticOutputStarted`与`terminalObserved`；
- expected `reason`、`delivery`、`normalizedReason`与`policy`。

fixture故意没有`message`或raw field。OpenAI `context_length_exceeded`只有exact machine-readable code时才映射`context_overflow`；Anthropic `invalid_request_error`没有稳定overflow subtype时保持`invalid_request`。实现不得解析human message prose。

## Safety Rules

- `logical_retry`当且仅当delivery为`not_sent | rejected_before_execution`、reason为允许的transient reason，且`rate_limited`拥有不超过60秒的hint；
- `compaction`当且仅当reason为`context_overflow`；
- transient `accepted_no_output | unknown`归一化为`request_outcome_unknown`；
- transient `output_started`归一化为`stream_interrupted`；
- non-transient reason保持原值；
- HTTP 500/503/504不自动证明pre-execution rejection；
- early EOF不能因Rig synthetic zero-usage `Final`而变成成功。

fixture只冻结provider-neutral mapping。status/body/envelope保存、stream terminal、metadata allowlist和single-request事实由real loopback tests分别证明。
