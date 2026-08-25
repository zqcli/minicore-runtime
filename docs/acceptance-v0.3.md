# v0.3 Acceptance Matrix

This matrix is generated from `scripts/acceptance_v03.json`. The mapping is reviewed traceability: the documentation checker validates exact identity, criterion/status/evidence equality, allowed gates, attributed non-ignored Rust tests in Cargo-enabled reachable library sources or direct integration targets, and the current Markdown authority inventory. It does not semantically prove behavior; the remote Rust gates execute the cited evidence.

All functional criteria AT-K01 through AT-K96 passed on the remote Linux validation checkout. GitHub Actions [run 32755428283](https://github.com/zqcli/minicore-runtime/actions/runs/32755428283) for exact code commit `815494dad38c34c585dfeda3c0845ccc7c1fb7d0` passed all four jobs (Rust stable, Rust 1.85.0, `macos-latest`, and `windows-latest` MSVC), validating the review fixes across AT-K01 through AT-K96. No package or tag release has occurred.

## Validation Environment

| Evidence | Result |
| --- | --- |
| Remote checkout | Linux, `/root/minicore-runtime-v03` |
| Stable toolchain | `rustc 1.98.0`, `cargo 1.98.0`, `clippy 1.98.0` |
| Stable gate | `scripts/check.sh` passed in full |
| Root tests | 290 library tests passed; cleaned integration suites also passed |
| Provider evidence | provider-gate tests and warnings-denied Clippy passed through `scripts/check.sh` |
| MSRV | `rustc 1.85.0`, `cargo 1.85.0`; `scripts/check-msrv.sh` passed |
| Documentation | `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked` passed |
| Architecture | authoritative scanner passed with `production_files=144` |
| Dependencies | 8 direct dependencies; root lock contains 37 package records |
| P6 lock diff | 39 records removed, 0 added, 0 retained-package version drift |
| GitHub Actions CI | [Run 32755428283](https://github.com/zqcli/minicore-runtime/actions/runs/32755428283) for commit `815494dad38c34c585dfeda3c0845ccc7c1fb7d0` passed all four jobs (Rust stable, Rust 1.85.0, `macos-latest`, `windows-latest` MSVC) |

## OS Matrix

| Operating system | Status | Evidence |
| --- | --- | --- |
| Linux | Passed | Full functional matrix and all validation commands above ran in remote Linux validation checkout. |
| macOS | Passed | Native `macos-latest` job passed in GitHub Actions [run 32755428283](https://github.com/zqcli/minicore-runtime/actions/runs/32755428283). |
| Windows | Passed | Native MSVC `windows-latest` job passed in GitHub Actions [run 32755428283](https://github.com/zqcli/minicore-runtime/actions/runs/32755428283). |

All functional acceptance criteria passed on Linux, and native macOS and Windows CI jobs passed. The review fixes are validated across AT-K01 through AT-K96. No package or tag release has occurred; release validation is complete and ready for publication.

## Public API And Lifecycle

| ID | Criterion | Status | Evidence |
| --- | --- | --- | --- |
| AT-K01 | The public API exposes no Runtime, RuntimeClient, or SessionManager. | Passed | [`tests/p1_surface.rs`](../tests/p1_surface.rs) — `canonical_modules_keep_only_the_current_root_facade`; [v0.3 architecture gate](../scripts/check_v03_architecture.py) |
| AT-K02 | SessionRuntime is not Clone; SessionHandle and TurnHandle are Clone. | Passed | [`tests/session_handle_contract.rs`](../tests/session_handle_contract.rs) — `session_error_handle_and_runtime_surface_are_exact`; [`tests/turn_handle_contract.rs`](../tests/turn_handle_contract.rs) — `public_turn_surface_is_exact_clone_send_sync_and_process_local`; [api_compile all-target target](../tests/api_compile.rs) |
| AT-K03 | Create initializes the manifest and empty Conversation and returns Idle and Healthy. | Passed | [`tests/session_runtime_owner_contract.rs`](../tests/session_runtime_owner_contract.rs) — `create_initializes_zero_head_returns_no_snapshot_and_shutdown_is_a_barrier`; [`src/session/actor/tests.rs`](../src/session/actor/tests.rs) — `initial_state_rehydrates_head_terminal_and_final_handle_identity` |
| AT-K04 | Create on an already initialized log fails, closes it, and leaks no owner task. | Passed | [`tests/session_runtime_lifecycle_evidence.rs`](../tests/session_runtime_lifecycle_evidence.rs) — `create_on_already_initialized_log_fails_closes_and_leaves_no_task_owner` |
| AT-K05 | Load validates manifest SessionId, spec, and bindings. | Passed | [`tests/session_runtime_owner_contract.rs`](../tests/session_runtime_owner_contract.rs) — `load_repairs_before_ready_and_model_mismatch_closes_before_replay`; [`tests/session_runtime_owner_contract.rs`](../tests/session_runtime_owner_contract.rs) — `open_errors_preserve_identity_log_and_secondary_close_distinctions` |
| AT-K06 | A second take_events call returns AlreadyTaken. | Passed | [`tests/session_runtime_owner_contract.rs`](../tests/session_runtime_owner_contract.rs) — `create_initializes_zero_head_returns_no_snapshot_and_shutdown_is_a_barrier` |
| AT-K07 | Shutdown closes the active Turn, log, and every Core-owned task. | Passed | [`tests/session_runtime_turn_contract.rs`](../tests/session_runtime_turn_contract.rs) — `active_shutdown_settles_shutdown_terminal_and_closes_old_handle`; [`src/session/runtime/tests/post_ready_panic.rs`](../src/session/runtime/tests/post_ready_panic.rs) — `post_ready_actor_panic_joins_pending_runner_before_close` |
| AT-K08 | After shutdown, an old SessionHandle returns Closed. | Passed | [`tests/session_runtime_turn_contract.rs`](../tests/session_runtime_turn_contract.rs) — `active_shutdown_settles_shutdown_terminal_and_closes_old_handle` |
| AT-K09 | Reloading the same SessionId creates a different SessionInstanceId. | Passed | [`tests/session_runtime_lifecycle_evidence.rs`](../tests/session_runtime_lifecycle_evidence.rs) — `reload_changes_instance_and_stale_handles_turns_and_events_are_isolated` |
| AT-K10 | Stale handles, Turns, and events cannot affect a new Session instance. | Passed | [`tests/session_runtime_lifecycle_evidence.rs`](../tests/session_runtime_lifecycle_evidence.rs) — `reload_changes_instance_and_stale_handles_turns_and_events_are_isolated` |

## Turn And State Machine

| ID | Criterion | Status | Evidence |
| --- | --- | --- | --- |
| AT-K11 | Submit returns a TurnHandle only after UserMessage is durable. | Passed | [`tests/session_runtime_turn_contract.rs`](../tests/session_runtime_turn_contract.rs) — `model_only_turn_is_durable_before_completion_and_transcript_visible` |
| AT-K12 | A UserMessage append failure does not call Model or enter Running. | Passed | [`tests/session_runtime_lifecycle_evidence.rs`](../tests/session_runtime_lifecycle_evidence.rs) — `user_message_append_failure_never_starts_turn_or_model`; [`tests/p6_session_surface.rs`](../tests/p6_session_surface.rs) — `actor_priority_submit_commit_and_settlement_order_are_source_locked`; [`src/conversation/log/append_tests.rs`](../src/conversation/log/append_tests.rs) — `known_failure_and_validation_failure_leave_confirmed_state_unchanged` |
| AT-K13 | A second submit returns Busy with the exact active TurnId and appends no second UserMessage. | Passed | [`tests/session_runtime_lifecycle_evidence.rs`](../tests/session_runtime_lifecycle_evidence.rs) — `second_submit_is_busy_with_exact_turn_and_does_not_append_another_user` |
| AT-K14 | TurnHandle cancellation is exact and idempotent. | Passed | [`src/session/turn_handle.rs`](../src/session/turn_handle.rs) — `cancellation_and_completion_share_one_linearization_point` |
| AT-K15 | Dropping TurnHandle does not cancel the Turn. | Passed | [`src/session/turn_handle.rs`](../src/session/turn_handle.rs) — `repeated_cancel_and_drop_do_not_create_new_cancellation` |
| AT-K16 | Multiple TurnHandle clones wait for the same outcome. | Passed | [`src/session/turn_handle.rs`](../src/session/turn_handle.rs) — `clones_wait_for_one_first_wins_completion` |
| AT-K17 | All four Session states and health invariants are complete and enforced. | Passed | [`tests/session_state_event_contract.rs`](../tests/session_state_event_contract.rs) — `state_surface_and_legal_matrix_are_exact`; [`tests/session_state_event_contract.rs`](../tests/session_state_event_contract.rs) — `state_rejects_every_illegal_shape` |
| AT-K18 | Ordinary Model and Tool errors do not make SessionHealth Degraded. | Passed | [`tests/session_runtime_context_failure_evidence.rs`](../tests/session_runtime_context_failure_evidence.rs) — `ordinary_model_error_is_durable_failed_and_session_remains_healthy`; [`tests/session_runtime_tool_policy_failure_evidence.rs`](../tests/session_runtime_tool_policy_failure_evidence.rs) — `ordinary_tool_error_is_durable_failed_result_and_session_remains_healthy` |
| AT-K19 | A SessionLog UnknownOutcome degrades the Session and causes later submit rejection. | Passed | [`tests/session_runtime_lifecycle_evidence.rs`](../tests/session_runtime_lifecycle_evidence.rs) — `unknown_active_append_rejects_later_submit_without_new_append_or_model`; [`tests/session_runtime_turn_contract.rs`](../tests/session_runtime_turn_contract.rs) — `unknown_assistant_commit_degrades_without_fabricated_terminal`; [`tests/session_handle_contract.rs`](../tests/session_handle_contract.rs) — `actor_latches_active_commit_failures_and_authenticates_suspensions` |
| AT-K20 | A full command queue returns Backpressure instead of waiting without bound. | Passed | [`tests/session_runtime_command_contract.rs`](../tests/session_runtime_command_contract.rs) — `dropped_queued_submit_is_skipped_and_full_mailbox_is_backpressure` |

## Conversation And Log

| ID | Criterion | Status | Evidence |
| --- | --- | --- | --- |
| AT-K21 | Malformed or partial Model responses never enter Conversation. | Passed | [`src/model/driver/tests/assembly.rs`](../src/model/driver/tests/assembly.rs) — `rejects_every_malformed_stream_sequence`; [`src/model/driver/tests/settlement.rs`](../src/model/driver/tests/settlement.rs) — `finish_then_stream_error_is_not_success_or_retryable_after_observation` |
| AT-K22 | ToolCall and ToolResult identities match strictly and each result is unique. | Passed | [`src/conversation/validator/tests.rs`](../src/conversation/validator/tests.rs) — `duplicate_ids_results_terminal_and_incomplete_exchange_are_rejected` |
| AT-K23 | Multiple ToolCalls execute and commit in original order. | Passed | [`src/agent/runner/tests/tools.rs`](../src/agent/runner/tests/tools.rs) — `multiple_tools_are_sequential_and_every_commit_ack_precedes_continuation` |
| AT-K24 | Confirmed Conversation state changes only after a durable AppendReceipt. | Passed | [`src/conversation/log/append_tests.rs`](../src/conversation/log/append_tests.rs) — `append_assigns_ordered_seq_and_timestamp_and_updates_projection_after_durable_append`; [`src/conversation/log/append_tests.rs`](../src/conversation/log/append_tests.rs) — `unknown_outcome_timeout_panic_and_bad_receipt_never_commit_memory` |
| AT-K25 | ToolFinished is attempted only after ToolResult is durable. | Passed | [`src/agent/runner/tests/interactions.rs`](../src/agent/runner/tests/interactions.rs) — `tool_input_orders_started_before_suspend_and_finished_after_commit_without_reexecution` |
| AT-K26 | TurnHandle completion and TurnFinished happen only after terminal durability. | Passed | [`tests/session_runtime_turn_contract.rs`](../tests/session_runtime_turn_contract.rs) — `model_only_turn_is_durable_before_completion_and_transcript_visible` |
| AT-K27 | A Turn receives exactly one terminal entry. | Passed | [`src/session/actor/tests/scheduling.rs`](../src/session/actor/tests/scheduling.rs) — `simultaneous_finish_and_join_readiness_settles_exactly_once` |
| AT-K28 | An active expected-head conflict degrades the Session, rejects submit, and creates no terminal. | Passed | [`tests/session_runtime_lifecycle_evidence.rs`](../tests/session_runtime_lifecycle_evidence.rs) — `active_conflict_degrades_rejects_submit_and_creates_no_terminal` |
| AT-K29 | Transcript contains only durable entries. | Passed | [`src/conversation/log/transcript_close_tests.rs`](../src/conversation/log/transcript_close_tests.rs) — `transcript_is_confirmed_bounded_and_validates_page_contract` |
| AT-K30 | An invalid Summary boundary or proposal is rejected without append. | Passed | [`src/conversation/validator/tests/summary.rs`](../src/conversation/validator/tests/summary.rs) — `active_turn_summary_cannot_cross_a_nonterminal_or_fake_boundary`; [`src/session/actor/tests/summary.rs`](../src/session/actor/tests/summary.rs) — `stale_summary_snapshot_is_rejected_before_append_without_latching`; [`src/compaction/driver/tests/validation.rs`](../src/compaction/driver/tests/validation.rs) — `proposal_must_use_an_exact_newer_completed_boundary_within_snapshot` |

## Restart Recovery

| ID | Criterion | Status | Evidence |
| --- | --- | --- | --- |
| AT-K31 | An unfinished Turn without ToolCalls receives one CancelledByRestart terminal. | Passed | [`src/conversation/log/recovery_tests.rs`](../src/conversation/log/recovery_tests.rs) — `recovery_repairs_no_tools_with_one_exact_terminal_batch` |
| AT-K32 | Unresolved ToolCalls receive stable ordered cancelled results during restart repair. | Passed | [`src/conversation/log/recovery_tests.rs`](../src/conversation/log/recovery_tests.rs) — `recovery_repairs_tools_in_order_with_exact_cancelled_entries` |
| AT-K33 | A pending approval is not restored after restart. | Passed | [`tests/session_runtime_restart_event_evidence.rs`](../tests/session_runtime_restart_event_evidence.rs) — `pending_approval_history_restarts_cancelled_without_restoring_interaction` |
| AT-K34 | A pending ToolInput request is not restored after restart. | Passed | [`tests/session_runtime_restart_event_evidence.rs`](../tests/session_runtime_restart_event_evidence.rs) — `pending_tool_input_history_restarts_cancelled_without_restoring_interaction` |
| AT-K35 | Already terminal history loads without an additional repair append. | Passed | [`src/conversation/log/replay_tests.rs`](../src/conversation/log/replay_tests.rs) — `replay_accepts_empty_and_multi_page_history_without_repair` |
| AT-K36 | A restart repair UnknownOutcome fails load and does not spawn a ready actor. | Passed | [`tests/session_runtime_owner_contract.rs`](../tests/session_runtime_owner_contract.rs) — `recovery_uncertainty_and_shutdown_close_errors_stay_typed`; [`src/conversation/log/recovery_tests.rs`](../src/conversation/log/recovery_tests.rs) — `recovery_unknown_and_bad_receipt_preserve_uncertain_primary_and_close_once` |
| AT-K37 | Sequence gaps, multiple terminals, and unmatched ToolResults fail load. | Passed | [`src/conversation/log/replay_tests.rs`](../src/conversation/log/replay_tests.rs) — `replay_rejects_manifest_identity_and_semantic_or_page_contract_errors`; [`src/conversation/validator/tests.rs`](../src/conversation/validator/tests.rs) — `duplicate_ids_results_terminal_and_incomplete_exchange_are_rejected` |

## Interaction

| ID | Criterion | Status | Evidence |
| --- | --- | --- | --- |
| AT-K38 | RequireApproval enters WaitingForInput and updates SessionState. | Passed | [`tests/session_runtime_interaction_contract.rs`](../tests/session_runtime_interaction_contract.rs) — `interaction_state_precedes_event_and_answers_are_exactly_once` |
| AT-K39 | AllowOnce executes the frozen exact arguments. | Passed | [`src/agent/tool_driver/tests/approval.rs`](../src/agent/tool_driver/tests/approval.rs) — `approval_suspends_exact_identity_then_allow_once_executes_exactly_once` |
| AT-K40 | Deny does not call Tool and commits a Denied result. | Passed | [`src/agent/tool_driver/tests/approval.rs`](../src/agent/tool_driver/tests/approval.rs) — `approval_deny_and_wrong_answer_never_execute_tool`; [`tests/session_runtime_interaction_contract.rs`](../tests/session_runtime_interaction_contract.rs) — `interaction_state_precedes_event_and_answers_are_exactly_once` |
| AT-K41 | Text such as yes or allow cannot replace typed approval. | Passed | [`tests/tool_policy_interaction_contract.rs`](../tests/tool_policy_interaction_contract.rs) — `policy_port_is_async_send_sync_and_returns_only_typed_decisions` |
| AT-K42 | After RequestInput the Tool future is finished and the answer directly forms an InputProvided result. | Passed | [`src/agent/tool_driver/tests/input_progress.rs`](../src/agent/tool_driver/tests/input_progress.rs) — `text_input_drops_future_escapes_json_and_rejects_controls`; [`src/agent/tool_driver/tests/input_progress.rs`](../src/agent/tool_driver/tests/input_progress.rs) — `choice_input_includes_selected_text_without_reexecuting_tool` |
| AT-K43 | Interaction ID or kind mismatch and repeated answers are rejected. | Passed | [`tests/session_runtime_interaction_contract.rs`](../tests/session_runtime_interaction_contract.rs) — `interaction_state_precedes_event_and_answers_are_exactly_once` |
| AT-K44 | Cancelling while waiting clears pending interaction and settles Cancelled. | Passed | [`tests/session_runtime_interaction_contract.rs`](../tests/session_runtime_interaction_contract.rs) — `cancelling_while_waiting_settles_missing_tool_result_once`; [`src/agent/runner/tests/interactions.rs`](../src/agent/runner/tests/interactions.rs) — `suspension_cancellation_and_deadline_retain_usage_with_exact_outcomes` |

## State And Events

| ID | Criterion | Status | Evidence |
| --- | --- | --- | --- |
| AT-K45 | The state watch has its initial value before SessionRuntime returns. | Passed | [`src/session/actor/tests.rs`](../src/session/actor/tests.rs) — `initial_state_rehydrates_head_terminal_and_final_handle_identity` |
| AT-K46 | A slow or absent event consumer does not block Turn completion. | Passed | [`src/session/event_stream.rs`](../src/session/event_stream.rs) — `channel_capacity_is_checked_and_closed_receiver_is_terminal`; [`src/agent/runner/tests/model_only.rs`](../src/agent/runner/tests/model_only.rs) — `progress_full_or_closed_never_controls_model_completion` |
| AT-K47 | A full event queue accumulates loss and attaches it to the next delivered ordinary event. | Passed | [`src/session/event_stream.rs`](../src/session/event_stream.rs) — `capacity_one_recovers_ordinary_events_with_dropped_count`; [`src/session/event_stream.rs`](../src/session/event_stream.rs) — `cumulative_and_saturating_drop_counts_are_exact` |
| AT-K48 | If InteractionRequested is dropped, pending state still allows the interaction to be answered. | Passed | [`tests/session_runtime_restart_event_evidence.rs`](../tests/session_runtime_restart_event_evidence.rs) — `dropped_interaction_and_turn_events_leave_state_wait_and_transcript_authoritative` |
| AT-K49 | If TurnFinished is dropped, TurnHandle wait and transcript still expose the result. | Passed | [`tests/session_runtime_restart_event_evidence.rs`](../tests/session_runtime_restart_event_evidence.rs) — `dropped_interaction_and_turn_events_leave_state_wait_and_transcript_authoritative` |
| AT-K50 | Every event envelope and scoped variant carries the correct SessionId, SessionInstanceId, TurnId, and dropped-before count. | Passed | [`tests/session_state_event_contract.rs`](../tests/session_state_event_contract.rs) — `event_variants_envelope_and_stream_surface_are_exact`; [`src/session/event_stream.rs`](../src/session/event_stream.rs) — `cumulative_and_saturating_drop_counts_are_exact` |

## Ports Cancellation And Panic

| ID | Criterion | Status | Evidence |
| --- | --- | --- | --- |
| AT-K51 | Context blocks sort stably by slot, descending priority, and source. | Passed | [`tests/p3_context_compaction_ports.rs`](../tests/p3_context_compaction_ports.rs) — `context_bundle_checks_limits_duplicates_and_deterministic_order` |
| AT-K52 | Context error, timeout, and panic each produce a Failed terminal while the Session remains healthy. | Passed | [`tests/session_runtime_context_failure_evidence.rs`](../tests/session_runtime_context_failure_evidence.rs) — `context_error_timeout_and_panic_each_fail_turn_and_keep_session_healthy` |
| AT-K53 | Model Started and Unknown delivery are not automatically retried. | Passed | [`src/model/driver/tests/retry.rs`](../src/model/driver/tests/retry.rs) — `excessive_retry_after_started_unknown_and_nonretryable_do_not_retry` |
| AT-K54 | NotStarted retry sleep responds to cancellation. | Passed | [`src/model/driver/tests/retry.rs`](../src/model/driver/tests/retry.rs) — `cancellation_interrupts_retry_sleep_without_another_attempt` |
| AT-K55 | Tool timeout and panic produce a Failed ToolResult and the actor survives to finish. | Passed | [`tests/session_runtime_tool_policy_failure_evidence.rs`](../tests/session_runtime_tool_policy_failure_evidence.rs) — `tool_timeout_and_panic_each_commit_failed_result_and_actor_finishes` |
| AT-K56 | Policy error, timeout, and panic fail closed with a Denied ToolResult and the actor survives. | Passed | [`tests/session_runtime_tool_policy_failure_evidence.rs`](../tests/session_runtime_tool_policy_failure_evidence.rs) — `policy_error_timeout_and_panic_fail_closed_and_actor_finishes` |
| AT-K57 | A full ToolProgress queue does not block Tool execution. | Passed | [`src/agent/tool_driver/tests/input_progress.rs`](../src/agent/tool_driver/tests/input_progress.rs) — `progress_is_lossy_for_accepted_full_closed_and_invalid_values` |
| AT-K58 | Cancellation propagates to Model, Tool, Policy, Context, Compaction, and interaction wait. | Passed | [`src/context/driver/tests/behavior.rs`](../src/context/driver/tests/behavior.rs) — `cancellation_during_provider_drops_future`; [`src/model/driver/tests/cancellation.rs`](../src/model/driver/tests/cancellation.rs) — `cancellation_during_stream_drops_stream_and_tracks_delivery`; [`src/agent/tool_driver/tests/policy.rs`](../src/agent/tool_driver/tests/policy.rs) — `turn_cancellation_during_policy_cancels_child_before_drop`; [`src/agent/tool_driver/tests/execution.rs`](../src/agent/tool_driver/tests/execution.rs) — `turn_cancellation_during_tool_cancels_child_before_drop`; [`src/compaction/driver/tests/cancellation.rs`](../src/compaction/driver/tests/cancellation.rs) — `cancellation_during_strategy_cancels_child_before_future_drop`; [`src/agent/tool_driver/tests/approval.rs`](../src/agent/tool_driver/tests/approval.rs) — `cancel_and_deadline_interrupt_waiting_for_approval_answer`; [`src/agent/tool_driver/tests/input_progress.rs`](../src/agent/tool_driver/tests/input_progress.rs) — `cancel_and_deadline_while_waiting_for_tool_input_are_exact_suspension_errors` |
| AT-K59 | After shutdown no Core-owned task remains live. | Passed | [`src/session/runtime/tests/post_ready_panic.rs`](../src/session/runtime/tests/post_ready_panic.rs) — `post_ready_actor_panic_joins_pending_runner_before_close`; [`tests/session_runtime_owner_contract.rs`](../tests/session_runtime_owner_contract.rs) — `shutdown_timeout_aborts_and_awaits_the_same_owner_task` |
| AT-K60 | Drop uses no mem::forget or block_on and test processes can exit. | Passed | [`tests/session_runtime_owner_contract.rs`](../tests/session_runtime_owner_contract.rs) — `dropped_and_cancelled_open_owners_close_without_orphans`; [`src/session/runtime/tests.rs`](../src/session/runtime/tests.rs) — `owner_source_has_exact_fields_final_handle_and_no_manager_or_serde`; [full stable quality gate](../scripts/check.sh) |

## Boundary And Concurrency

| ID | Criterion | Status | Evidence |
| --- | --- | --- | --- |
| AT-K61 | Production source contains no std::fs, std::process, reqwest, cap_std, or fs4 authority. | Passed | [v0.3 architecture gate](../scripts/check_v03_architecture.py) |
| AT-K62 | Production source contains no Workspace, builtin Tool, concrete Provider, or concrete Store. | Passed | [v0.3 architecture gate](../scripts/check_v03_architecture.py) |
| AT-K63 | Production source contains no Subagent, AgentSpawner, or parent-child Agent graph. | Passed | [v0.3 architecture gate](../scripts/check_v03_architecture.py) |
| AT-K64 | Production source contains no SessionSnapshot, ObservationFrame, or ResyncRequired. | Passed | [`tests/session_state_event_contract.rs`](../tests/session_state_event_contract.rs) — `event_variants_envelope_and_stream_surface_are_exact`; [v0.3 architecture gate](../scripts/check_v03_architecture.py) |
| AT-K65 | Two SessionRuntime owners on one Tokio runtime keep cancellation and state isolated. | Passed | [`tests/session_runtime_owner_contract.rs`](../tests/session_runtime_owner_contract.rs) — `two_same_id_owners_open_concurrently_with_isolated_cancellation`; [`tests/session_runtime_shared_ports_evidence.rs`](../tests/session_runtime_shared_ports_evidence.rs) — `two_runtimes_share_exact_ports_concurrently_with_cancellation_isolation` |
| AT-K66 | Two SessionRuntime owners can share the exact same Model, ToolPolicy, and ContextProvider Arcs concurrently. | Passed | [`tests/session_runtime_shared_ports_evidence.rs`](../tests/session_runtime_shared_ports_evidence.rs) — `two_runtimes_share_exact_ports_concurrently_with_cancellation_isolation` |
| AT-K67 | Each SessionLog is called serially through its actor's exclusive mutable ownership. | Passed | [`tests/session_log_contract.rs`](../tests/session_log_contract.rs) — `fake_session_log_enforces_head_pages_errors_close_and_operation_order`; [`tests/session_runtime_owner_contract.rs`](../tests/session_runtime_owner_contract.rs) — `create_initializes_zero_head_returns_no_snapshot_and_shutdown_is_a_barrier` |
| AT-K68 | Cancelling create or load before actor readiness leaves no actor or log owner. | Passed | [`tests/session_runtime_open_cancellation_contract.rs`](../tests/session_runtime_open_cancellation_contract.rs) — `pre_poll_caller_cancellation_is_closed_by_existing_watcher`; [`tests/session_runtime_open_cancellation_contract.rs`](../tests/session_runtime_open_cancellation_contract.rs) — `cancellation_during_later_replay_page_eventually_closes_without_append`; [`tests/session_runtime_owner_contract.rs`](../tests/session_runtime_owner_contract.rs) — `dropped_and_cancelled_open_owners_close_without_orphans` |
| AT-K69 | Root cancellation completes SessionRuntime shutdown even while the command mailbox is full. | Passed | [`tests/session_runtime_lifecycle_evidence.rs`](../tests/session_runtime_lifecycle_evidence.rs) — `full_command_mailbox_cannot_block_root_shutdown`; [`src/session/actor/tests/scheduling.rs`](../src/session/actor/tests/scheduling.rs) — `root_then_critical_progress_ahead_of_ready_command_flood` |
| AT-K70 | Runner submits only unsequenced drafts; ConversationLog alone assigns sequence and timestamp. | Passed | [`src/conversation/log/append_tests.rs`](../src/conversation/log/append_tests.rs) — `draft_types_are_unsequenced_and_do_not_contain_timestamp_fields` |
| AT-K71 | Settlement appends every missing ToolResult and the TurnTerminal in one atomic batch. | Passed | [`tests/session_runtime_restart_event_evidence.rs`](../tests/session_runtime_restart_event_evidence.rs) — `cancellation_settlement_appends_all_missing_results_and_terminal_atomically` |
| AT-K72 | A known active Turn append failure degrades the Session, returns DurabilityUnavailable, and fabricates no terminal. | Passed | [`tests/session_runtime_turn_contract.rs`](../tests/session_runtime_turn_contract.rs) — `known_assistant_commit_failure_latches_without_settlement_append` |
| AT-K73 | Event summaries contain no complete Tool output, arguments, interaction answer, or raw adapter error. | Passed | [`tests/event_summary_structure_contract.rs`](../tests/event_summary_structure_contract.rs) — `public_event_summaries_exclude_payloads_arguments_answers_and_raw_errors`; [`tests/session_state_event_contract.rs`](../tests/session_state_event_contract.rs) — `diagnostic_state_and_event_debug_are_payload_redacted` |
| AT-K74 | A SessionSpec with enabled tools exceeding default limits can create a session under matching runtime limits. | Passed | [`tests/session_runtime_semantic_limits_evidence.rs`](../tests/session_runtime_semantic_limits_evidence.rs) — `create_with_custom_max_tool_count_allows_tools_exceeding_default_limit` |
| AT-K75 | SessionManifest serde roundtrips without enforcing default semantic limits during deserialization. | Passed | [`tests/session_runtime_semantic_limits_evidence.rs`](../tests/session_runtime_semantic_limits_evidence.rs) — `session_manifest_serde_roundtrip_preserves_custom_tool_count_for_instance_validation` |
| AT-K76 | A session created with custom limits can be shutdown and reloaded under matching runtime limits. | Passed | [`tests/session_runtime_semantic_limits_evidence.rs`](../tests/session_runtime_semantic_limits_evidence.rs) — `create_shutdown_load_roundtrip_with_custom_max_tool_count` |
| AT-K77 | Narrower runtime limits reject creation before log initialization without spawning an owner. | Passed | [`tests/session_runtime_semantic_limits_evidence.rs`](../tests/session_runtime_semantic_limits_evidence.rs) — `narrower_instance_limits_reject_create_before_log_initialize` |
| AT-K78 | SessionSpec and SessionManifest reject inputs exceeding absolute structural limits during construction and deserialization. | Passed | [`tests/session_runtime_semantic_limits_evidence.rs`](../tests/session_runtime_semantic_limits_evidence.rs) — `session_spec_and_manifest_enforce_absolute_structural_bounds` |

## Conversation And Log

| ID | Criterion | Status | Evidence |
| --- | --- | --- | --- |
| AT-K79 | A Store Conflict error during transcript reading returns a LogConflict diagnostic and degrades session health. | Passed | [`tests/session_runtime_transcript_degraded_evidence.rs`](../tests/session_runtime_transcript_degraded_evidence.rs) — `transcript_store_conflict_returns_log_conflict_and_degrades_session` |
| AT-K80 | A Store Corrupt error during transcript reading degrades session health and emits a HealthChanged event. | Passed | [`tests/session_runtime_transcript_degraded_evidence.rs`](../tests/session_runtime_transcript_degraded_evidence.rs) — `transcript_store_corrupt_emits_health_changed_and_degrades_session` |
| AT-K81 | An observed head mismatch between store transcript page and confirmed head degrades session health. | Passed | [`tests/session_runtime_transcript_degraded_evidence.rs`](../tests/session_runtime_transcript_degraded_evidence.rs) — `transcript_observed_head_mismatch_degrades_session` |
| AT-K82 | A transcript page contract violation or projection mismatch degrades session health. | Passed | [`tests/session_runtime_transcript_degraded_evidence.rs`](../tests/session_runtime_transcript_degraded_evidence.rs) — `transcript_page_contract_violation_degrades_session` |
| AT-K83 | Invalid caller transcript cursor or limit returns InvalidInput and preserves healthy session state. | Passed | [`tests/session_runtime_transcript_degraded_evidence.rs`](../tests/session_runtime_transcript_degraded_evidence.rs) — `transcript_caller_invalid_cursor_and_limit_return_invalid_input_and_remain_healthy` |
| AT-K84 | A temporary Store Unavailable error during transcript reading returns a retryable error and preserves healthy session state. | Passed | [`tests/session_runtime_transcript_degraded_evidence.rs`](../tests/session_runtime_transcript_degraded_evidence.rs) — `transcript_temporary_store_unavailable_returns_retryable_and_remains_healthy`; [`tests/session_runtime_transcript_degraded_evidence.rs`](../tests/session_runtime_transcript_degraded_evidence.rs) — `transcript_store_closed_and_internal_preserve_healthy_session` |
| AT-K85 | A degraded session permits state inspection, transcript read, and shutdown while rejecting turn submission. | Passed | [`tests/session_runtime_transcript_degraded_evidence.rs`](../tests/session_runtime_transcript_degraded_evidence.rs) — `degraded_session_permits_state_read_and_shutdown_while_rejecting_submit`; [`tests/session_runtime_transcript_degraded_evidence.rs`](../tests/session_runtime_transcript_degraded_evidence.rs) — `active_turn_transcript_conflict_degrades_cancels_and_prevents_settlement` |

## Ports Cancellation And Panic

| ID | Criterion | Status | Evidence |
| --- | --- | --- | --- |
| AT-K86 | Explicit NotStarted retryable ModelError triggers driver retry and succeeds on subsequent attempt. | Passed | [`tests/session_runtime_model_retry_evidence.rs`](../tests/session_runtime_model_retry_evidence.rs) — `model_retry_explicit_not_started_retries_and_succeeds` |
| AT-K87 | Model timeout with Unknown delivery state is not automatically retried. | Passed | [`tests/session_runtime_model_retry_evidence.rs`](../tests/session_runtime_model_retry_evidence.rs) — `model_retry_unknown_timeout_does_not_retry` |
| AT-K88 | Stream interruption with Started delivery state is not automatically retried. | Passed | [`tests/session_runtime_model_retry_evidence.rs`](../tests/session_runtime_model_retry_evidence.rs) — `model_retry_started_stream_interrupted_does_not_retry` |
| AT-K89 | RateLimited ModelError retry behavior depends strictly on explicit delivery state and retry hint. | Passed | [`tests/session_runtime_model_retry_evidence.rs`](../tests/session_runtime_model_retry_evidence.rs) — `model_retry_rate_limited_retry_depends_on_explicit_delivery_and_hint` |
| AT-K90 | Model retry backoff sleep responds promptly to cancellation without executing subsequent attempts. | Passed | [`tests/session_runtime_model_retry_evidence.rs`](../tests/session_runtime_model_retry_evidence.rs) — `model_retry_sleep_responds_to_cancellation` |

## Event Stream Cutover

| ID | Criterion | Status | Evidence |
| --- | --- | --- | --- |
| AT-K91 | A capacity-one event queue resumes ordinary delivery and attaches accumulated drops to the next delivered event. | Passed | [`src/session/event_stream.rs`](../src/session/event_stream.rs) — `capacity_one_recovers_ordinary_events_with_dropped_count` |
| AT-K92 | Event loss counts accumulate exactly, saturate at u64::MAX, and reset after delivery. | Passed | [`src/session/event_stream.rs`](../src/session/event_stream.rs) — `cumulative_and_saturating_drop_counts_are_exact` |
| AT-K93 | Dropping SessionEventStream does not block Turn completion. | Passed | [`tests/session_state_event_contract.rs`](../tests/session_state_event_contract.rs) — `dropping_session_event_stream_does_not_block_turn_completion` |

## API Ergonomics

| ID | Criterion | Status | Evidence |
| --- | --- | --- | --- |
| AT-K94 | SessionEventStream supports StreamExt::next() and receives live events. | Passed | [`tests/session_runtime_event_stream_evidence.rs`](../tests/session_runtime_event_stream_evidence.rs) — `session_event_stream_next_receives_events` |
| AT-K95 | SessionEventStream can be selected against an external CancellationToken. | Passed | [`tests/session_runtime_event_stream_evidence.rs`](../tests/session_runtime_event_stream_evidence.rs) — `session_event_stream_next_can_be_cancelled_by_external_token` |
| AT-K96 | After SessionRuntime shutdown, SessionEventStream drains queued events and eventually returns None. | Passed | [`tests/session_runtime_event_stream_evidence.rs`](../tests/session_runtime_event_stream_evidence.rs) — `session_event_stream_drains_after_shutdown_and_then_ends` |

## Acceptance Conclusion

All functional rows above are Passed on Linux. Cross-platform release validation is complete across Linux, macOS, and Windows. The review fixes are validated across AT-K01 through AT-K96. No package or tag release has occurred; the repository is ready for publication. See the [v0.3 release note](release-v0.3.md) and [migration guide](migrations/v0.2-to-v0.3.md).
