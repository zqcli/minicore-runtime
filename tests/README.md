# Test Inventory

The root test suite covers only the final v0.3 public surface and private execution architecture. Historical workspace, concrete storage, provider registry, old actor/observation, and compatibility targets are deleted rather than kept disabled.

## Public Values And Ports

- `p1_value.rs`, `p1_ids.rs`, `p1_kernel_config.rs`, `p1_session_spec.rs`, `p1_session_manifest.rs`, and `p1_dto.rs` cover checked values, identifiers, configuration, manifests, Model/Tool DTOs, serde strictness, and redacted Debug output.
- `model_port_contract.rs` covers the direct streaming Model Port, descriptor/context/request/event grammar, delivery-aware errors, and shared concurrency.
- `model_error_contract.rs` covers ModelError delivery and retry invariants, strict nested wire deserialization, unknown field rejection, and diagnostic normalization.
- `tool_set_contract.rs` covers Tool, ToolSet construction, frozen specs, duplicate/panic handling, concurrent dispatch, and owner-neutral ToolContext.
- `tool_policy_interaction_contract.rs` covers ToolPolicy, approval/input DTOs, process-local interactions, and redaction.
- `p3_context_compaction_ports.rs` covers ContextProvider and CompactionStrategy Ports and checked DTOs.
- `session_log_contract.rs` covers the SessionLog Port, canonical conversation DTOs, fake adapter behavior, and root/storage reexports.
- `session_bindings_contract.rs` covers the exact immutable adapter bundle and pure compatibility validation.

## Private Drivers And Runner

- `model_driver_contract.rs`, `tool_driver_contract.rs`, `context_prompt_driver_contract.rs`, and `compaction_driver_contract.rs` source-check the private drivers, authority boundaries, deadline provenance, panic/cancellation behavior, and focused file limits. Behavioral tests live beside those private modules.
- `turn_runner_contract.rs` protects ordinary execution, exact prefix/Summary acknowledgements, durable rounds, compaction recovery, critical taxonomy, cancellation-first control, usage retention, and owner neutrality.

## Session Ownership

- `session_state_event_contract.rs` and `turn_handle_contract.rs` cover state invariants, event redaction/loss behavior, cancellation, completion, and waiter races.
- `session_runtime_owner_contract.rs`, `session_runtime_open_cancellation_contract.rs`, and `session_runtime_timer_contract.rs` cover create/load ordering, recovery, open cancellation, runtime selection, timeout cleanup, shutdown, and concurrent owner isolation.
- `session_handle_contract.rs` and `p6_session_surface.rs` protect the final handle/command/actor source shape, active commit latches, suspension proof, settlement order, and panic ownership.
- `session_runtime_turn_contract.rs` covers model-only execution, exact-once settlement, durability failures, transcript behavior, and active shutdown.
- `session_runtime_interaction_contract.rs` covers approval suspension, denial, cancellation, ToolResult durability, and answer/cancellation races.
- `session_runtime_command_contract.rs` covers bounded mailbox pressure and submit receiver-loss ownership.
- `session_runtime_compaction_commit_contract.rs` covers actor-owned Summary failure handling and continuation suppression.
- `session_runtime_lifecycle_evidence.rs` covers already-initialized create failure, reload/stale-instance isolation, exact Busy admission, conflict degradation, and shutdown under a full mailbox.
- `session_runtime_context_failure_evidence.rs` covers ordinary Model failure and real-runtime Context error/timeout/panic settlement with Healthy actor state.
- `session_runtime_tool_policy_failure_evidence.rs` covers ordinary Tool errors plus Tool and Policy timeout/panic behavior through SessionRuntime.
- `session_runtime_restart_event_evidence.rs` covers nondurable interaction restart repair, forced event loss with authoritative state/Turn/transcript, and atomic unresolved-tool settlement.
- `session_runtime_shared_ports_evidence.rs` proves two owners use the exact same Model/ToolPolicy/ContextProvider Arcs concurrently while cancellation remains isolated.
- `session_runtime_semantic_limits_evidence.rs` covers instance-level limits, custom `max_tool_count` create/load roundtrip, early rejection before log initialize, and absolute structural bounds.
- `session_runtime_transcript_degraded_evidence.rs` covers transcript consistency classification, store Conflict/Corrupt degradation, observed head and contract violation degradation, and Healthy isolation for caller input and temporary unavailable errors.
- `event_summary_structure_contract.rs` locks the public event summary field set against payload, argument, answer, and raw-error expansion.
- Private runtime test `post_ready_actor_panic_joins_pending_runner_before_close` proves the real post-ready panic supervisor joins the pending runner before close.

## Architecture

- `p1_surface.rs` and `final_architecture_contract.rs` protect the exact root facade and physical absence of removed implementation paths.
- `api_compile.rs` type-checks the complete final public workflow.
- `examples/session_runtime_lifecycle.rs` is an all-target compiled, failure-safe Host lifecycle example; the documentation checker requires README to contain its exact marked body.
- `scripts/check_v03_architecture.py` is the authoritative module/dependency/path/Port/DAG gate; its fixture self-tests live in `scripts/check_v03_architecture_test.py`.

The standalone `provider-gate/` package is deterministic historical protocol evidence and does not define a root-crate adapter API.

The canonical criterion-by-criterion mapping is `scripts/acceptance_v03.json`; [docs/acceptance-v0.3.md](../docs/acceptance-v0.3.md) is generated from it. This is reviewed traceability, not semantic proof by the documentation checker: remote Rust gates execute the attributed tests. The mapping records AT-K01 through AT-K90, the remote Rust/doc environment, and the passing cross-platform CI matrix.
