# Acceptance Matrix (V4-001..V4-070)

v0.4 delivery acceptance. Rows are generated from
`scripts/acceptance_v04.json` by `scripts/check_acceptance.py`; edit the JSON,
not this file. Evidence names real tests, gates, docs, or examples.

Phase: complete (all rows Passed).

| ID | Status | Summary | Evidence |
| --- | --- | --- | --- |
| V4-001 | Passed | Runtime不拥有Session；无session生命周期/打开/保存 | `scripts/check_v04_architecture.py`; `docs/contracts/agent-loop.md`; `tests/p2_agent_loop.rs::owner_drop_after_completion_keeps_the_completed_outcome` |
| V4-002 | Passed | Runtime不解析JSONL；无conversation ledger/JSONL写入 | `scripts/check_v04_architecture.py`; `docs/migrations/v0.3-to-v0.4.md` |
| V4-003 | Passed | Runtime不公开SessionLog类型 | `scripts/check_v04_architecture.py`; `src/lib.rs` |
| V4-004 | Passed | Runtime不公开Manifest | `scripts/check_v04_architecture.py`; `docs/architecture.md` |
| V4-005 | Passed | Runtime不提供Transcript | `scripts/check_v04_architecture.py`; `docs/contracts/history.md` |
| V4-006 | Passed | 无Degraded/durability/recovery状态 | `scripts/check_v04_architecture.py`; `docs/architecture.md` |
| V4-007 | Passed | AgentLoop单次使用、不可resume/reopen | `docs/contracts/agent-loop.md`; `tests/p2_agent_loop.rs::cancel_and_shutdown_after_completion_do_not_reopen_the_loop`; `src/agent_loop/control.rs::finish_once_seals_exactly_once_and_closes_accepting` |
| V4-008 | Passed | 多轮由上层多次创建AgentLoop（MemoryAgent） | `examples/memory_agent.rs`; `docs/contracts/agent-loop.md` |
| V4-009 | Passed | 不提供旧v0.3 API兼容层 | `scripts/check_v04_architecture.py`; `src/lib.rs` |
| V4-010 | Passed | crate不依赖具体Provider/Workspace/Storage | `scripts/check_v04_architecture.py`; `Cargo.toml` |
| V4-011 | Passed | Model接口保留 | `src/model/model.rs`; `tests/p1_v04_loop_dtos.rs::execution_config_accepts_a_text_only_model` |
| V4-012 | Passed | Tool/ToolSet接口保留 | `src/tools/tool.rs`; `src/tools/set.rs`; `tests/p1_v04_loop_dtos.rs::execution_config_accepts_tool_capable_model_with_tools` |
| V4-013 | Passed | ToolPolicy/Interaction保留 | `src/tools/policy.rs`; `docs/contracts/tool-policy-interaction.md`; `tests/p2_agent_loop.rs::approved_interaction_resumes_the_loop`; `tests/tool_policy_interaction_contract.rs::invalid_tool_input_requests_rejected_by_validate`; `tests/p2_agent_loop.rs::invalid_tool_input_empty_prompt_fails_without_interaction` |
| V4-014 | Passed | PromptProvider替代Context/Compaction | `src/prompt.rs`; `docs/contracts/prompt.md`; `tests/p2_agent_loop.rs::default_provider_projects_history_in_order_with_summary_and_optional_system` |
| V4-015 | Passed | start/handle/events/join/shutdown可用 | `src/agent_loop/mod.rs`; `examples/agent_loop.rs`; `tests/p2_agent_loop.rs::shutdown_cancels_and_joins` |
| V4-016 | Passed | steer/update/answer/cancel/wait可用 | `src/agent_loop/handle.rs`; `examples/agent_loop.rs`; `tests/p2_agent_loop.rs::steer_during_model_reaches_the_next_request` |
| V4-017 | Passed | LoopReport返回当前增量 | `tests/p3_agent_loop_closeout.rs::report_appended_is_only_the_loop_delta_not_the_base_history`; `docs/contracts/agent-loop.md` |
| V4-018 | Passed | HistoryItem可序列化但Core不持久化 | `tests/p1_v04_loop_dtos.rs::history_items_serialize_with_typed_tags_and_round_trip`; `src/history.rs`; `docs/contracts/history.md` |
| V4-019 | Passed | 初始config revision为0 | `tests/p1_v04_loop_dtos.rs::config_revision_is_initial_zero_and_round_trips`; `src/execution.rs` |
| V4-020 | Passed | update只在下一request生效 | `tests/p2_agent_loop.rs::update_during_model_reaches_the_next_request`; `tests/p2_agent_loop.rs::update_during_prompt_prep_is_rebuilt_without_advancing_the_index` |
| V4-021 | Passed | 当前Tool batch使用旧snapshot | `tests/p2_agent_loop.rs::update_during_tools_keeps_the_batch_on_the_old_snapshot`; `tests/p3_agent_loop_closeout.rs::policy_update_applies_the_full_snapshot_at_the_next_request_batch` |
| V4-022 | Passed | latest update wins | `tests/p2_agent_loop.rs::multiple_updates_keep_only_the_latest`; `src/agent_loop/control.rs::updates_are_monotonic_and_latest_wins_until_commit`; `src/agent_loop/control.rs::pending_config_drop_on_update_replace_occurs_outside_control_mutex` |
| V4-023 | Passed | model/reasoning可热更新 | `tests/p2_agent_loop.rs::request_started_records_the_actual_snapshot_revision_and_reasoning`; `tests/p2_agent_loop.rs::update_during_model_reaches_the_next_request` |
| V4-024 | Passed | ToolSet/Policy/Prompt可原子热更新 | `tests/p2_agent_loop.rs::toolset_switch_applies_to_the_next_request`; `tests/p2_agent_loop.rs::prompt_provider_switch_applies_to_the_next_request`; `tests/p3_agent_loop_closeout.rs::policy_update_applies_the_full_snapshot_at_the_next_request_batch` |
| V4-025 | Passed | RequestStarted报告实际config | `tests/p2_agent_loop.rs::request_started_records_the_actual_snapshot_revision_and_reasoning` |
| V4-026 | Passed | update不强制增加request | `tests/p2_agent_loop.rs::update_alone_does_not_extend_the_final_request` |
| V4-027 | Passed | 无静默能力降级（非法update被拒绝、失败保留） | `tests/p2_agent_loop.rs::invalid_config_update_does_not_consume_a_revision`; `tests/p2_agent_loop.rs::start_rejects_config_outside_loop_limits` |
| V4-028 | Passed | model中steer下一request生效 | `tests/p2_agent_loop.rs::steer_during_model_reaches_the_next_request` |
| V4-029 | Passed | tool中steer在batch后生效 | `tests/p2_agent_loop.rs::steer_during_tool_batch_reaches_the_next_request` |
| V4-030 | Passed | prompt中steer导致rebuild | `tests/p2_agent_loop.rs::steer_during_prompt_discards_the_stale_prompt` |
| V4-031 | Passed | 多steer有序 | `tests/p2_agent_loop.rs::multiple_steers_apply_in_accept_order` |
| V4-032 | Passed | queue有界 | `tests/p2_agent_loop.rs::steer_queue_is_bounded_and_full_reports_queue_full` |
| V4-033 | Passed | final race线性化（steer wins / seal wins） | `tests/p2_agent_loop.rs::final_race_steer_wins_and_keeps_the_loop_alive`; `tests/p2_agent_loop.rs::final_race_seal_wins_returns_not_active`; `src/agent_loop/control.rs::begin_final_with_pending_steer_keeps_accepting_open`; `src/agent_loop/control.rs::pending_config_drop_on_begin_final_seal_occurs_outside_control_mutex` |
| V4-034 | Passed | applied steer进入LoopReport | `tests/p3_agent_loop_closeout.rs::steer_applied_before_a_prompt_failure_stays_in_the_report`; `tests/p2_agent_loop.rs::update_and_steer_combine_on_the_next_request` |
| V4-035 | Passed | 未应用steer不持久化 | `tests/p2_agent_loop.rs::steer_accepted_but_cancelled_before_application_stays_out_of_report` |
| V4-036 | Passed | WaitingForInput拒绝steer | `tests/p2_agent_loop.rs::steer_is_rejected_while_waiting_for_interaction` |
| V4-037 | Passed | 当前Model Response严格验证 | `tests/p2_agent_loop.rs::unknown_tool_call_closes_the_loop_with_invalid_response`; `tests/p3_agent_loop_closeout.rs::missing_tool_call_terminal_is_invalid_and_not_appended`; `src/model/driver/tests/semantics.rs::multiple_tool_specs_reject_second_tool_call_when_limit_is_one`; `tests/p3_agent_loop_closeout.rs::loop_completes_with_multiple_registered_tools_under_single_call_limit` |
| V4-038 | Passed | 当前ToolCallId唯一 | `tests/p3_agent_loop_closeout.rs::duplicate_tool_call_ids_are_rejected_and_not_appended` |
| V4-039 | Passed | 当前ToolCall都有受限且确定的ToolResult | `tests/p3_agent_loop_closeout.rs::cancelling_a_multi_tool_batch_settles_each_call_exactly_once`; `tests/p2_agent_loop.rs::cancel_during_tool_ends_cancelled_with_results_for_every_call`; `tests/p2_agent_loop.rs::tool_output_exceeding_limit_fails_and_original_content_omitted_from_history`; `tests/p2_agent_loop.rs::tool_finished_event_output_bytes_matches_history_bounded_bytes` |
| V4-040 | Passed | Tool按顺序执行 | `tests/p2_agent_loop.rs::sequential_tool_calls_execute_in_order` |
| V4-041 | Passed | Cancel传播到Model/Prompt/Tool/Waiting | `tests/p2_agent_loop.rs::cancel_during_model_ends_cancelled_with_report`; `tests/p2_agent_loop.rs::cancel_during_tool_ends_cancelled_with_results_for_every_call`; `tests/p2_agent_loop.rs::loop_deadline_during_a_tool_call_cancels`; `tests/p2_agent_loop.rs::cancel_while_waiting_for_input_ends_the_loop` |
| V4-042 | Passed | 一个Loop只完成一次 | `src/agent_loop/control.rs::finish_once_seals_exactly_once_and_closes_accepting`; `tests/p2_agent_loop.rs::late_subscriber_gets_the_same_report_immediately`; `src/agent_loop/control.rs::pending_config_drop_on_finish_once_occurs_outside_control_mutex` |
| V4-043 | Passed | invalid response不入History | `tests/p3_agent_loop_closeout.rs::missing_tool_call_terminal_is_invalid_and_not_appended`; `tests/p3_agent_loop_closeout.rs::duplicate_tool_call_ids_are_rejected_and_not_appended`; `tests/p3_agent_loop_closeout.rs::partial_stream_failure_is_a_model_failure_without_an_assistant_item` |
| V4-044 | Passed | failure/cancel仍返回Report | `tests/p2_agent_loop.rs::prompt_error_is_a_prompt_failure`; `tests/p2_agent_loop.rs::model_start_panic_yields_model_failure_without_history`; `tests/p2_agent_loop.rs::cancel_during_model_ends_cancelled_with_report`; `tests/p2_agent_loop.rs::model_error_auth_rejected_preserves_classification_and_diagnostic`; `tests/p2_agent_loop.rs::loop_failure_debug_does_not_leak_diagnostic_message` |
| V4-045 | Passed | max rounds生效 | `tests/p2_agent_loop.rs::max_tool_rounds_ends_with_failed_budget_and_complete_results` |
| V4-046 | Passed | Starting/Model/Tools/Waiting/Finishing/Finished状态正确 | `tests/p2_agent_loop.rs::tool_loop_final_state_request_index_is_last_issued`; `src/agent_loop/state.rs`; `tests/p2_agent_loop.rs::first_request_start_error_counts_as_one_request_with_state_zero` |
| V4-047 | Passed | Event full不阻塞 | `tests/p2_agent_loop.rs::bounded_event_queue_attaches_dropped_before_to_the_next_success` |
| V4-048 | Passed | Event drop报告dropped_before | `tests/p2_agent_loop.rs::bounded_event_queue_attaches_dropped_before_to_the_next_success`; `tests/p2_agent_loop.rs::model_burst_progress_drops_are_recorded_in_dropped_before`; `tests/p2_agent_loop.rs::tool_burst_progress_drops_are_recorded_in_dropped_before` |
| V4-049 | Passed | Event consumer可缺失 | `tests/p3_agent_loop_closeout.rs::loop_completes_without_an_event_consumer`; `tests/p2_agent_loop.rs::closing_the_event_stream_does_not_stop_the_loop` |
| V4-050 | Passed | Finished Event非权威 | `docs/contracts/event-stream.md`; `tests/p2_agent_loop.rs::event_stream_is_takeable_once_and_best_effort` |
| V4-051 | Passed | wait/join返回同一Report | `tests/p2_agent_loop.rs::multiple_waiters_receive_the_same_report`; `tests/p2_agent_loop.rs::late_subscriber_gets_the_same_report_immediately` |
| V4-052 | Passed | 多waiter安全 | `tests/p2_agent_loop.rs::multiple_waiters_receive_the_same_report` |
| V4-053 | Passed | 单spawn门禁+join完成/取消隔离无残留迹象 | `tests/p2_agent_loop.rs::concurrent_loops_finish_independently_without_extra_tasks`; `tests/p3_agent_loop_closeout.rs::shared_model_and_toolset_loops_cancel_isolated_without_orphans` |
| V4-054 | Passed | Base history由Host拥有 | `docs/contracts/history.md`; `tests/p3_agent_loop_closeout.rs::report_appended_is_only_the_loop_delta_not_the_base_history` |
| V4-055 | Passed | Core不做全局history ledger验证 | `tests/p2_agent_loop.rs::inconsistent_old_tool_history_fails_as_prompt_but_start_succeeds`; `docs/contracts/history.md` |
| V4-056 | Passed | History超限安全失败 | `tests/p3_agent_loop_closeout.rs::start_rejects_history_over_item_and_byte_limits`; `src/agent_loop/mod.rs` |
| V4-057 | Passed | Report不复制base history | `tests/p3_agent_loop_closeout.rs::report_appended_is_only_the_loop_delta_not_the_base_history` |
| V4-058 | Passed | 同一Loop可记录多个Model | `tests/p2_agent_loop.rs::update_during_model_reaches_the_next_request`; `tests/p2_agent_loop.rs::default_provider_accepts_mixed_model_refs_in_the_base` |
| V4-059 | Passed | Summary无durable boundary语义 | `src/history.rs`; `docs/contracts/history.md` |
| V4-060 | Passed | DefaultPrompt可运行简单Agent | `tests/p2_agent_loop.rs::default_provider_runs_a_loop_end_to_end_with_system_prompt` |
| V4-061 | Passed | Rust 1.85通过 | `scripts/check-msrv.sh` |
| V4-062 | Passed | stable通过 | `scripts/check.sh` |
| V4-063 | Passed | Linux/macOS/Windows CI矩阵四job（macos/windows/stable/1.85）在run 33750189748全绿 | `.github/workflows/ci.yml`; `docs/release-v0.4.md` |
| V4-064 | Passed | cargo fmt通过 | `scripts/check.sh`; `scripts/check-msrv.sh` |
| V4-065 | Passed | clippy -D warnings通过 | `scripts/check.sh` |
| V4-066 | Passed | rustdoc -D warnings通过 | `scripts/check.sh` |
| V4-067 | Passed | 无unsafe生产代码 | `scripts/check_v04_architecture.py`; `src/lib.rs` |
| V4-068 | Passed | architecture gate通过 | `scripts/check-architecture.sh`; `scripts/check_v04_architecture.py` |
| V4-069 | Passed | 旧durable模块不存在 | `scripts/check_v04_architecture.py`; `docs/migrations/v0.3-to-v0.4.md` |
| V4-070 | Passed | README与实际边界一致 | `README.md`; `scripts/check_docs.py` |
