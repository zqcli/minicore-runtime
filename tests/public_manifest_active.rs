use std::path::{Path, PathBuf};
use std::str::FromStr;

use minicore_runtime::agent_session_lifecycle::{
    AgentStatus, AgentUsableStatus, ForkAnchor, ForkSourceKind,
};
use minicore_runtime::model_gateway::ReasoningPreference;
use minicore_runtime::prompt::PromptBodyIntent;
use minicore_runtime::runtime_interface::{
    AgentCommand, CommandCompletion, CommandErrorCode, CommandOutcome, EventFrame, EventRoute,
    InteractionCommand, ItemContentView, ProgressEventKind, ProgressUpdate, PublicCancelTarget,
    PublicSubject, QueryErrorCode, RetryAdvice, RuntimeCommand, RuntimeEventDetail, RuntimeRequest,
    RuntimeStateEventKind, SessionCommand, SessionEventDetail, SessionLifecycleView,
    SessionStateEventKind, SnapshotResponse, StateEventMsg, SubscriptionClosed, TurnCommand,
    TurnFailureView, TurnInterruptionView, TurnStatusView, TurnTerminalView,
};
use minicore_runtime::tools::UserQuestionAnswerValue;
use minicore_runtime::turn_item_interaction::{InteractionRequestView, InteractionResolutionInput};
use minicore_runtime::wire::{
    CanonicalFileUri, FileUriFamily, IncrementalRuntimeProtocolV1, ProtocolBootstrapResponse,
    ProtocolBootstrapRouter, ProtocolRejectReason, ProtocolVersion, RuntimeCapabilities,
    RuntimeCapability, RuntimeRequestKind, TypedJsonError, decode_protocol_bootstrap_response_v1,
    decode_protocol_hello_v1, encode_protocol_bootstrap_response_v1, encode_protocol_hello_v1,
};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicManifest {
    version: u32,
    vectors: Vec<PublicVector>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicVector {
    path: String,
    target: String,
    direction: VectorDirection,
    status: VectorStatus,
    slice: VectorSlice,
    expected: PublicExpectation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum VectorDirection {
    ClientToRuntime,
    RuntimeToClient,
    Bidirectional,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VectorStatus {
    Active,
    Pending,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VectorSlice {
    Foundation,
    M1,
    M2Initial,
    M7,
    M8,
    M9,
    M10,
    M11,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(dead_code, reason = "fields mirror the public manifest contract")]
struct PublicExpectation {
    decode: String,
    canonical_reencode: Option<String>,
    valid_round_trip: Option<String>,
    invalid: Option<String>,
    assert: Option<String>,
    stage: Option<String>,
    code: Option<String>,
    ignored_json_pointers: Option<Vec<String>>,
    canonical_reencode_path: Option<String>,
    runtime_encoder: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FileUriVectors {
    version: u32,
    target: String,
    valid: Vec<ValidFileUri>,
    invalid: Vec<InvalidFileUri>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ValidFileUri {
    wire: String,
    family: String,
    authority: Option<String>,
    decoded_path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InvalidFileUri {
    wire: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NegotiationVectors {
    runtime_supported_versions: Vec<ProtocolVersion>,
    runtime_capabilities: Vec<String>,
    cases: Vec<NegotiationCase>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NegotiationCase {
    hello_path: String,
    expected_response_path: String,
    expected_selected_version: Option<ProtocolVersion>,
    expected_capabilities: Option<Vec<String>>,
    expected_reject_reason: Option<String>,
}

#[test]
fn every_active_public_manifest_vector_uses_an_exported_production_seam() {
    let manifest: PublicManifest = read_json(&fixture_root().join("public/manifest.json"));
    assert_eq!(manifest.version, 2);

    let mut active = 0_usize;
    for vector in &manifest.vectors {
        match vector.direction {
            VectorDirection::ClientToRuntime
            | VectorDirection::RuntimeToClient
            | VectorDirection::Bidirectional => {}
        }
        match vector.slice {
            VectorSlice::Foundation
            | VectorSlice::M1
            | VectorSlice::M2Initial
            | VectorSlice::M7
            | VectorSlice::M8
            | VectorSlice::M9
            | VectorSlice::M10
            | VectorSlice::M11 => {}
        }
        match vector.status {
            VectorStatus::Active => {
                active += 1;
                run_active_vector(vector);
            }
            VectorStatus::Pending => {}
        }
    }
    assert!(active > 0);
}

fn run_active_vector(vector: &PublicVector) {
    match vector.target.as_str() {
        "ProtocolHello" => run_protocol_hello(vector),
        "ProtocolBootstrapResponse" => run_bootstrap_response(vector),
        "CommandRequest" => run_request(vector, RuntimeRequestKind::Dispatch),
        "CommandResponse" => run_command_response(vector),
        "RuntimeQuery" => run_request(vector, RuntimeRequestKind::Query),
        "SnapshotRequest" => run_request(vector, RuntimeRequestKind::Snapshot),
        "SubscriptionRequest" => run_request(vector, RuntimeRequestKind::Subscribe),
        "QueryResponse" => run_query_response(vector),
        "RuntimeDispatchError" => run_runtime_dispatch_error(vector),
        "QueryError" => run_query_error(vector),
        "EventFrame" => run_event_frame(vector),
        "CanonicalFileUriVectorSet" => run_file_uri_vectors(vector),
        "ProtocolNegotiationCaseSet" => run_negotiation_vectors(vector),
        target => panic!("active manifest target has no Rust handler: {target}"),
    }
}

fn run_event_frame(vector: &PublicVector) {
    assert_eq!(vector.direction, VectorDirection::RuntimeToClient);
    let raw = read_fixture(&vector.path);
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    match vector.expected.decode.as_str() {
        "accepted" => {
            let frame = protocol.decode_event_frame(&raw).unwrap();
            assert_event_frame_semantics(vector, &frame);
            assert_eq!(
                protocol.encode_event_frame(&frame).unwrap(),
                canonical_target(vector, &raw),
                "{}",
                vector.path,
            );
        }
        "protocol_error" => {
            assert_eq!(
                vector.expected.runtime_encoder.as_deref(),
                Some("must_not_send")
            );
            let error = protocol.decode_event_frame(&raw).unwrap_err();
            assert_manifest_fault(vector, error);
        }
        decode => panic!("unsupported EventFrame expectation {decode}"),
    }
}

fn assert_event_frame_semantics(vector: &PublicVector, frame: &EventFrame) {
    match (vector.expected.assert.as_deref(), frame) {
        (
            Some("idle_session_snapshot"),
            EventFrame::Snapshot(SnapshotResponse::Session(snapshot)),
        ) => {
            assert_eq!(
                snapshot.session_id().to_string(),
                "ses_22222222222222222222222222222222"
            );
            assert_eq!(snapshot.definition().revision().get(), 1);
            assert_eq!(snapshot.definition().workspace().roots().len(), 1);
            assert_eq!(snapshot.usage().unwrap().model_calls(), 0);
        }
        (
            Some("starting_session_snapshot"),
            EventFrame::Snapshot(SnapshotResponse::Session(snapshot)),
        ) => {
            assert_eq!(
                snapshot.execution(),
                minicore_runtime::runtime_interface::SessionExecutionView::Starting
            );
            assert_eq!(snapshot.queues().submit_admissions().len(), 1);
            assert_eq!(snapshot.queues().steers().len(), 0);
            assert_eq!(snapshot.queues().follow_ups().len(), 0);
        }
        (
            Some("running_session_snapshot"),
            EventFrame::Snapshot(SnapshotResponse::Session(snapshot)),
        ) => {
            assert_eq!(
                snapshot.execution(),
                minicore_runtime::runtime_interface::SessionExecutionView::Running
            );
            assert!(matches!(
                snapshot.current_turn().unwrap().status(),
                TurnStatusView::Running
            ));
            assert_eq!(snapshot.active_items().len(), 2);
            assert_eq!(snapshot.pending_interactions().len(), 1);
            assert!(matches!(
                snapshot.pending_interactions()[0].request(),
                InteractionRequestView::UserQuestion(_)
            ));
            assert!(matches!(
                snapshot.active_items()[0].content(),
                ItemContentView::UserMessage { .. }
            ));
        }
        (
            Some("running_approval_session_snapshot"),
            EventFrame::Snapshot(SnapshotResponse::Session(snapshot)),
        ) => {
            assert_eq!(
                snapshot.execution(),
                minicore_runtime::runtime_interface::SessionExecutionView::Running
            );
            assert!(matches!(
                snapshot.current_turn().unwrap().status(),
                TurnStatusView::Running
            ));
            assert_eq!(snapshot.active_items().len(), 2);
            assert_eq!(snapshot.pending_interactions().len(), 1);
            assert!(matches!(
                snapshot.pending_interactions()[0].request(),
                minicore_runtime::turn_item_interaction::InteractionRequestView::ToolApproval(_)
            ));
        }
        (Some("runtime_catalog_invalidated_state"), EventFrame::State(event)) => {
            assert_eq!(event.route(), EventRoute::Runtime);
            let StateEventMsg::Runtime { kind, snapshot, .. } = event.msg() else {
                panic!("runtime state assertion requires a Runtime message");
            };
            assert_eq!(*kind, RuntimeStateEventKind::CommandCatalogInvalidated);
            assert!(snapshot.loaded_sessions().is_empty());
        }
        (
            Some(
                assertion @ ("agent_created_state"
                | "agent_definition_updated_state"
                | "agent_metadata_updated_state"
                | "agent_disabled_state"
                | "agent_deleted_state"),
            ),
            EventFrame::State(event),
        ) => {
            let StateEventMsg::Runtime {
                kind,
                snapshot,
                detail: Some(RuntimeEventDetail::AgentChanged { agent }),
            } = event.msg()
            else {
                panic!("runtime Agent state assertion requires an Agent summary");
            };
            let EventRoute::Agent { agent_id } = event.route() else {
                panic!("runtime Agent state assertion requires an Agent route");
            };
            assert_eq!(agent.agent_id(), agent_id);
            assert!(snapshot.loaded_sessions().is_empty());
            let (expected_kind, expected_status) = match assertion {
                "agent_created_state" => {
                    (RuntimeStateEventKind::AgentCreated, AgentStatus::Enabled)
                }
                "agent_definition_updated_state" => (
                    RuntimeStateEventKind::AgentDefinitionUpdated,
                    AgentStatus::Enabled,
                ),
                "agent_metadata_updated_state" => (
                    RuntimeStateEventKind::AgentMetadataUpdated,
                    AgentStatus::Enabled,
                ),
                "agent_disabled_state" => (
                    RuntimeStateEventKind::AgentStatusChanged,
                    AgentStatus::Disabled,
                ),
                "agent_deleted_state" => (
                    RuntimeStateEventKind::AgentStatusChanged,
                    AgentStatus::Deleted,
                ),
                _ => unreachable!(),
            };
            assert_eq!(*kind, expected_kind);
            assert_eq!(agent.status(), expected_status);
        }
        (
            Some(
                assertion @ ("session_created_state"
                | "session_loaded_state"
                | "session_unloaded_state"
                | "session_archived_state"
                | "session_unarchived_state"
                | "session_deleted_state"
                | "session_forked_state"),
            ),
            EventFrame::State(event),
        ) => {
            let StateEventMsg::Runtime {
                kind,
                snapshot,
                detail,
            } = event.msg()
            else {
                panic!("runtime Session state assertion requires a Runtime message");
            };
            let expected_kind = match assertion {
                "session_created_state" => RuntimeStateEventKind::SessionCreated,
                "session_loaded_state" => RuntimeStateEventKind::SessionLoaded,
                "session_unloaded_state" => RuntimeStateEventKind::SessionUnloaded,
                "session_archived_state" => RuntimeStateEventKind::SessionArchived,
                "session_unarchived_state" => RuntimeStateEventKind::SessionUnarchived,
                "session_deleted_state" => RuntimeStateEventKind::SessionDeleted,
                "session_forked_state" => RuntimeStateEventKind::SessionForked,
                _ => unreachable!(),
            };
            assert_eq!(*kind, expected_kind);
            let EventRoute::Session { session_id } = event.route() else {
                panic!("runtime Session state assertion requires a Session route");
            };
            match kind {
                RuntimeStateEventKind::SessionCreated
                | RuntimeStateEventKind::SessionArchived
                | RuntimeStateEventKind::SessionUnarchived
                | RuntimeStateEventKind::SessionDeleted
                | RuntimeStateEventKind::SessionForked => {
                    let Some(RuntimeEventDetail::SessionChanged { session }) = detail else {
                        panic!("durable Session state requires a safe Session summary");
                    };
                    assert_eq!(session.session_id(), session_id);
                    let expected_lifecycle = match kind {
                        RuntimeStateEventKind::SessionArchived => SessionLifecycleView::Archived,
                        RuntimeStateEventKind::SessionDeleted => SessionLifecycleView::Deleted,
                        _ => SessionLifecycleView::Open,
                    };
                    assert_eq!(session.lifecycle(), expected_lifecycle);
                }
                RuntimeStateEventKind::SessionLoaded => {
                    assert!(detail.is_none());
                    assert!(
                        snapshot
                            .loaded_sessions()
                            .iter()
                            .any(|session| session.session_id() == session_id)
                    );
                }
                RuntimeStateEventKind::SessionUnloaded => {
                    assert!(detail.is_none());
                    assert!(
                        snapshot
                            .loaded_sessions()
                            .iter()
                            .all(|session| session.session_id() != session_id)
                    );
                }
                RuntimeStateEventKind::AgentCreated
                | RuntimeStateEventKind::AgentDefinitionUpdated
                | RuntimeStateEventKind::AgentMetadataUpdated
                | RuntimeStateEventKind::AgentStatusChanged
                | RuntimeStateEventKind::SessionDefinitionUpdated
                | RuntimeStateEventKind::SessionMetadataUpdated
                | RuntimeStateEventKind::CommandCatalogInvalidated => unreachable!(),
            }
        }
        (Some("session_execution_changed_finishing_state"), EventFrame::State(event)) => {
            assert_eq!(
                event.route(),
                EventRoute::Session {
                    session_id: "ses_22222222222222222222222222222222".parse().unwrap(),
                }
            );
            assert_eq!(
                event.msg().session_kind(),
                Some(SessionStateEventKind::SessionExecutionChanged)
            );
            assert!(event.msg().session_detail().is_none());
            assert_eq!(
                event.msg().session_snapshot().unwrap().execution(),
                minicore_runtime::runtime_interface::SessionExecutionView::Finishing
            );
        }
        (Some("session_metadata_updated_runtime_state"), EventFrame::State(event)) => {
            let StateEventMsg::Runtime {
                kind,
                snapshot,
                detail: Some(RuntimeEventDetail::SessionChanged { session }),
            } = event.msg()
            else {
                panic!("runtime Session metadata state requires a SessionChanged summary");
            };
            assert_eq!(*kind, RuntimeStateEventKind::SessionMetadataUpdated);
            let EventRoute::Session { session_id } = event.route() else {
                panic!("runtime Session metadata state requires a Session route");
            };
            assert_eq!(session.session_id(), session_id);
            assert_eq!(session.metadata().revision().get(), 2);
            assert_eq!(session.metadata().name(), Some("Session v2"));
            assert_eq!(
                session.metadata().description(),
                Some("Plans and reviews implementation work")
            );
            assert_eq!(session.lifecycle(), SessionLifecycleView::Open);
            assert!(!session.forked());
            assert!(snapshot.loaded_sessions().is_empty());
        }
        (Some("session_metadata_updated_session_state"), EventFrame::State(event)) => {
            assert_eq!(
                event.route(),
                EventRoute::Session {
                    session_id: "ses_22222222222222222222222222222222".parse().unwrap(),
                }
            );
            assert_eq!(
                event.msg().session_kind(),
                Some(SessionStateEventKind::SessionMetadataUpdated)
            );
            assert!(event.msg().session_detail().is_none());
            let snapshot = event.msg().session_snapshot().unwrap();
            assert_eq!(snapshot.metadata().revision().get(), 2);
            assert_eq!(snapshot.metadata().name(), Some("Session v2"));
            assert_eq!(
                snapshot.metadata().description(),
                Some("Plans and reviews implementation work")
            );
            assert_eq!(
                snapshot.execution(),
                minicore_runtime::runtime_interface::SessionExecutionView::Idle
            );
        }
        (Some("session_definition_updated_runtime_state"), EventFrame::State(event)) => {
            let StateEventMsg::Runtime {
                kind,
                snapshot,
                detail: Some(RuntimeEventDetail::SessionChanged { session }),
            } = event.msg()
            else {
                panic!("runtime Session definition state requires a SessionChanged summary");
            };
            assert_eq!(*kind, RuntimeStateEventKind::SessionDefinitionUpdated);
            let EventRoute::Session { session_id } = event.route() else {
                panic!("runtime Session definition state requires a Session route");
            };
            assert_eq!(session.session_id(), session_id);
            assert_eq!(session.definition_revision().get(), 2);
            assert_eq!(session.lifecycle(), SessionLifecycleView::Open);
            assert!(!session.forked());
            assert!(snapshot.loaded_sessions().is_empty());
        }
        (Some("session_definition_updated_session_state"), EventFrame::State(event)) => {
            assert_eq!(
                event.route(),
                EventRoute::Session {
                    session_id: "ses_22222222222222222222222222222222".parse().unwrap(),
                }
            );
            assert_eq!(
                event.msg().session_kind(),
                Some(SessionStateEventKind::SessionDefinitionUpdated)
            );
            assert!(event.msg().session_detail().is_none());
            let snapshot = event.msg().session_snapshot().unwrap();
            assert_eq!(snapshot.definition().revision().get(), 2);
            assert_eq!(
                snapshot.definition().model().reasoning(),
                ReasoningPreference::High
            );
            assert_eq!(snapshot.definition().prompts().enabled().len(), 1);
            assert_eq!(
                snapshot.execution(),
                minicore_runtime::runtime_interface::SessionExecutionView::Idle
            );
        }
        (Some("session_workspace_reloaded_state"), EventFrame::State(event)) => {
            assert_eq!(
                event.route(),
                EventRoute::Session {
                    session_id: "ses_22222222222222222222222222222222".parse().unwrap(),
                }
            );
            assert_eq!(
                event.msg().session_kind(),
                Some(SessionStateEventKind::SessionWorkspaceReloaded)
            );
            assert!(event.msg().session_detail().is_none());
            let snapshot = event.msg().session_snapshot().unwrap();
            assert_eq!(snapshot.definition().revision().get(), 2);
            assert_eq!(snapshot.definition().workspace().roots().len(), 1);
            assert_eq!(
                snapshot.execution(),
                minicore_runtime::runtime_interface::SessionExecutionView::Idle
            );
        }
        (Some("turn_completed_state"), EventFrame::State(event)) => {
            assert_turn_terminal_event(event.msg(), SessionStateEventKind::TurnCompleted, None);
        }
        (Some("turn_failed_model_state"), EventFrame::State(event)) => {
            assert_turn_terminal_event(
                event.msg(),
                SessionStateEventKind::TurnFailed,
                Some(TerminalReason::Failure(TurnFailureView::Model)),
            );
        }
        (Some("turn_interrupted_user_cancelled_state"), EventFrame::State(event)) => {
            assert_turn_terminal_event(
                event.msg(),
                SessionStateEventKind::TurnInterrupted,
                Some(TerminalReason::Interruption(
                    TurnInterruptionView::UserCancelled,
                )),
            );
        }
        (Some("model_item_delta_progress"), EventFrame::Progress(event)) => {
            assert_eq!(event.kind(), ProgressEventKind::Model);
            let EventRoute::Item { item_id, .. } = event.route() else {
                panic!("model Item delta requires an Item route");
            };
            assert!(matches!(
                event.update(),
                ProgressUpdate::ItemDelta {
                    item_id: update_item_id,
                    content_index: 0,
                    delta,
                    ..
                } if *update_item_id == item_id && delta.as_ref() == "hel"
            ));
        }
        (
            Some("subscription_backpressure_closed"),
            EventFrame::Closed(SubscriptionClosed::Backpressure),
        ) => {}
        (assertion, frame) => panic!("event assertion {assertion:?} does not match {frame:?}"),
    }
}

fn assert_turn_terminal_event(
    msg: &StateEventMsg,
    expected_kind: SessionStateEventKind,
    expected_reason: Option<TerminalReason>,
) {
    let StateEventMsg::Session { kind, detail, .. } = msg else {
        panic!("turn terminal assertion requires a Session message");
    };
    assert_eq!(*kind, expected_kind);
    let Some(SessionEventDetail::TurnTerminal { terminal, .. }) = detail else {
        panic!("turn terminal assertion requires terminal detail");
    };
    match (terminal, expected_reason) {
        (TurnTerminalView::Completed { .. }, None) => {}
        (TurnTerminalView::Failed { reason, .. }, Some(TerminalReason::Failure(expected))) => {
            assert_eq!(*reason, expected);
        }
        (
            TurnTerminalView::Interrupted { reason, .. },
            Some(TerminalReason::Interruption(expected)),
        ) => {
            assert_eq!(*reason, expected);
        }
        value => panic!("unexpected terminal {value:?}"),
    }
}

#[derive(Debug)]
enum TerminalReason {
    Failure(TurnFailureView),
    Interruption(TurnInterruptionView),
}

fn run_command_response(vector: &PublicVector) {
    assert_eq!(vector.direction, VectorDirection::RuntimeToClient);
    let raw = read_fixture(&vector.path);
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    match vector.expected.decode.as_str() {
        "accepted" => {
            let response = protocol.decode_command_response(&raw).unwrap();
            assert_command_response_semantics(vector, &response);
            assert_eq!(
                protocol.encode_command_response(&response).unwrap(),
                canonical_target(vector, &raw),
                "{}",
                vector.path,
            );
        }
        "protocol_error" => {
            assert_eq!(
                vector.expected.runtime_encoder.as_deref(),
                Some("must_not_send")
            );
            let error = protocol.decode_command_response(&raw).unwrap_err();
            assert_manifest_fault(vector, error);
        }
        decode => panic!("unsupported CommandResponse expectation {decode}"),
    }
}

fn assert_command_response_semantics(
    vector: &PublicVector,
    response: &minicore_runtime::runtime_interface::CommandResponse,
) {
    match (vector.expected.assert.as_deref(), response.completion()) {
        (
            Some("turn_started_completion"),
            CommandCompletion::Completed {
                outcome: CommandOutcome::TurnStarted { turn_id },
                output: None,
            },
        ) => {
            assert_eq!(turn_id.to_string(), "trn_33333333333333333333333333333333");
        }
        (
            Some("submit_cancelled_completion"),
            CommandCompletion::Completed {
                outcome: CommandOutcome::SubmitCancelled,
                output: None,
            },
        ) => {}
        (
            Some("cancel_accepted_completion"),
            CommandCompletion::Completed {
                outcome:
                    CommandOutcome::CancelAccepted {
                        target: PublicCancelTarget::Turn(turn_id),
                        cancel_epoch,
                    },
                output: None,
            },
        ) => {
            assert_eq!(turn_id.to_string(), "trn_66666666666666666666666666666666");
            assert_eq!(*cancel_epoch, 7);
        }
        (
            Some("session_definition_updated_completion"),
            CommandCompletion::Completed {
                outcome:
                    CommandOutcome::SessionDefinitionUpdated {
                        definition_revision,
                    },
                output: None,
            },
        ) => {
            assert_eq!(definition_revision.get(), 2);
        }
        (
            Some("agent_created_completion"),
            CommandCompletion::Completed {
                outcome:
                    CommandOutcome::AgentCreated {
                        agent_id,
                        definition_revision,
                        metadata_revision,
                    },
                output: None,
            },
        ) => {
            assert_eq!(agent_id.to_string(), "agt_11111111111111111111111111111111");
            assert_eq!(definition_revision.get(), 1);
            assert_eq!(metadata_revision.get(), 1);
        }
        (
            Some("agent_definition_updated_completion"),
            CommandCompletion::Completed {
                outcome:
                    CommandOutcome::AgentDefinitionUpdated {
                        definition_revision,
                    },
                output: None,
            },
        ) => assert_eq!(definition_revision.get(), 2),
        (
            Some("agent_metadata_updated_completion"),
            CommandCompletion::Completed {
                outcome: CommandOutcome::AgentMetadataUpdated { metadata_revision },
                output: None,
            },
        ) => assert_eq!(metadata_revision.get(), 2),
        (
            Some("agent_status_changed_completion"),
            CommandCompletion::Completed {
                outcome:
                    CommandOutcome::AgentStatusChanged {
                        status: AgentStatus::Disabled,
                    },
                output: None,
            },
        )
        | (
            Some("agent_deleted_completion"),
            CommandCompletion::Completed {
                outcome: CommandOutcome::AgentDeleted,
                output: None,
            },
        ) => {}
        (
            Some("session_forked_completion"),
            CommandCompletion::Completed {
                outcome:
                    CommandOutcome::SessionForked {
                        session_id,
                        source: ForkSourceKind::LiveSnapshot,
                    },
                output: None,
            },
        ) => {
            assert_eq!(
                session_id.to_string(),
                "ses_33333333333333333333333333333333"
            );
        }
        (
            Some("session_metadata_updated_completion"),
            CommandCompletion::Completed {
                outcome: CommandOutcome::SessionMetadataUpdated { metadata_revision },
                output: None,
            },
        ) => assert_eq!(metadata_revision.get(), 2),
        (
            Some("workspace_reloaded_completion"),
            CommandCompletion::Completed {
                outcome: CommandOutcome::WorkspaceReloaded,
                output: None,
            },
        ) => {}
        (
            Some("session_archived_completion"),
            CommandCompletion::Completed {
                outcome: CommandOutcome::SessionArchived,
                output: None,
            },
        )
        | (
            Some("session_unarchived_completion"),
            CommandCompletion::Completed {
                outcome: CommandOutcome::SessionUnarchived,
                output: None,
            },
        )
        | (
            Some("session_deleted_completion"),
            CommandCompletion::Completed {
                outcome: CommandOutcome::SessionDeleted,
                output: None,
            },
        )
        | (
            Some("no_change_completion"),
            CommandCompletion::Completed {
                outcome: CommandOutcome::NoChange,
                output: None,
            },
        ) => {}
        (Some("session_busy_rejection"), CommandCompletion::Rejected(error)) => {
            assert_eq!(error.code(), CommandErrorCode::SessionBusy);
            assert_eq!(error.retry(), RetryAdvice::RefreshAndRetry);
            assert!(matches!(error.subject(), Some(PublicSubject::Session(_))));
        }
        (Some("ingress_backoff_rejection"), CommandCompletion::Rejected(error)) => {
            assert!(matches!(
                error.retry(),
                RetryAdvice::RetryWithBackoff {
                    retry_after: Some(_)
                }
            ));
        }
        (
            Some("command_output_completion"),
            CommandCompletion::Completed {
                outcome: CommandOutcome::CommandOutput,
                output: Some(output),
            },
        ) => {
            assert_eq!(output.text(), "status ok");
        }
        (assertion, completion) => {
            panic!("response assertion {assertion:?} does not match {completion:?}")
        }
    }
}

fn run_request(vector: &PublicVector, kind: RuntimeRequestKind) {
    assert_eq!(vector.direction, VectorDirection::ClientToRuntime);
    let raw = read_fixture(&vector.path);
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    match vector.expected.decode.as_str() {
        "accepted" => {
            let request = protocol.decode_request(kind, &raw).unwrap();
            assert_request_semantics(vector, &request);
            assert_eq!(
                protocol.encode_request(&request).unwrap(),
                canonical_target(vector, &raw),
                "{}",
                vector.path,
            );
        }
        "rejected" => {
            let error = protocol.decode_request(kind, &raw).unwrap_err();
            assert_manifest_fault(vector, error);
        }
        decode => panic!("unsupported request expectation {decode}"),
    }
}

fn assert_request_semantics(vector: &PublicVector, request: &RuntimeRequest) {
    let Some(assertion) = vector.expected.assert.as_deref() else {
        return;
    };
    let RuntimeRequest::Dispatch(request) = request else {
        panic!("request assertion {assertion} requires a dispatch root");
    };
    match (assertion, request.command()) {
        (
            "interaction_resolve_user_answer",
            RuntimeCommand::Interaction(InteractionCommand::Resolve {
                session_id,
                expected_turn_id,
                item_id,
                request_id,
                resolution: InteractionResolutionInput::UserAnswer(answer),
                resolution_key,
            }),
        ) => {
            assert_eq!(
                session_id.to_string(),
                "ses_22222222222222222222222222222222"
            );
            assert_eq!(
                expected_turn_id.to_string(),
                "trn_33333333333333333333333333333333"
            );
            assert_eq!(item_id.to_string(), "itm_88888888888888888888888888888888");
            assert_eq!(
                request_id.to_string(),
                "req_66666666666666666666666666666666"
            );
            assert!(resolution_key == &"irk_77777777777777777777777777777777".parse().unwrap());
            assert!(matches!(
                answer.answers(),
                [field]
                    if field.question_index() == 0
                        && matches!(
                            field.value(),
                            UserQuestionAnswerValue::Choice { option_index: 1 }
                        )
            ));
        }
        (
            "runtime_reload_shared_resources",
            RuntimeCommand::Runtime(
                minicore_runtime::runtime_interface::RuntimeLifecycleCommand::ReloadSharedResources,
            ),
        ) => {}
        (
            "agent_create",
            RuntimeCommand::Agent(AgentCommand::Create {
                definition,
                metadata,
            }),
        ) => {
            assert_eq!(
                definition
                    .prompts()
                    .enabled()
                    .iter()
                    .next()
                    .unwrap()
                    .as_str(),
                "code-review"
            );
            assert_eq!(metadata.name(), "Planner");
            assert_eq!(metadata.description(), Some("Plans implementation work"));
        }
        (
            "agent_update_definition",
            RuntimeCommand::Agent(AgentCommand::UpdateDefinition {
                agent_id,
                expected_revision,
                patch,
            }),
        ) => {
            assert_eq!(agent_id.to_string(), "agt_11111111111111111111111111111111");
            assert_eq!(expected_revision.get(), 1);
            assert_eq!(patch.prompts().unwrap().enabled().len(), 2);
        }
        (
            assertion @ ("agent_update_metadata_set"
            | "agent_update_metadata_clear"
            | "agent_update_metadata_keep"),
            RuntimeCommand::Agent(AgentCommand::UpdateMetadata {
                agent_id,
                expected_revision,
                patch,
            }),
        ) => {
            assert_eq!(agent_id.to_string(), "agt_11111111111111111111111111111111");
            match assertion {
                "agent_update_metadata_set" => {
                    assert_eq!(expected_revision.get(), 1);
                    assert_eq!(patch.name(), Some("Planner v2"));
                    assert_eq!(
                        patch.description().set_value(),
                        Some("Plans and reviews implementation work")
                    );
                }
                "agent_update_metadata_clear" => {
                    assert_eq!(expected_revision.get(), 2);
                    assert!(patch.name().is_none());
                    assert!(patch.description().is_clear());
                }
                "agent_update_metadata_keep" => {
                    assert_eq!(expected_revision.get(), 2);
                    assert!(patch.name().is_none());
                    assert!(patch.description().is_keep());
                }
                _ => unreachable!(),
            }
        }
        (
            "agent_set_status",
            RuntimeCommand::Agent(AgentCommand::SetStatus {
                agent_id,
                expected_status: AgentStatus::Enabled,
                status: AgentUsableStatus::Disabled,
            }),
        )
        | (
            "agent_delete",
            RuntimeCommand::Agent(AgentCommand::Delete {
                agent_id,
                expected_status: AgentStatus::Disabled,
            }),
        ) => {
            assert_eq!(agent_id.to_string(), "agt_11111111111111111111111111111111");
        }
        (
            "session_create_file_uri",
            RuntimeCommand::Session(SessionCommand::Create {
                agent_id,
                definition,
                metadata,
            }),
        ) => {
            assert_eq!(agent_id.to_string(), "agt_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
            let root = definition.workspace().primary_root();
            assert_eq!(root.key().as_str(), "repo");
            assert_eq!(root.path().as_str(), "file:///Users/alice/project");
            assert_eq!(root.path().family(), FileUriFamily::Posix);
            assert_eq!(definition.workspace().cwd().relative_path().as_str(), "src");
            assert!(metadata.name().is_none());
        }
        ("session_load", RuntimeCommand::Session(SessionCommand::Load { session_id })) => {
            assert_eq!(
                session_id.to_string(),
                "ses_22222222222222222222222222222222"
            );
        }
        ("session_unload", RuntimeCommand::Session(SessionCommand::Unload { session_id })) => {
            assert_eq!(
                session_id.to_string(),
                "ses_22222222222222222222222222222222"
            );
        }
        ("session_archive", RuntimeCommand::Session(SessionCommand::Archive { session_id }))
        | (
            "session_unarchive",
            RuntimeCommand::Session(SessionCommand::Unarchive { session_id }),
        )
        | ("session_delete", RuntimeCommand::Session(SessionCommand::Delete { session_id })) => {
            assert_eq!(
                session_id.to_string(),
                "ses_22222222222222222222222222222222"
            );
        }
        (
            "session_fork_after_user",
            RuntimeCommand::Session(SessionCommand::Fork {
                source_session_id,
                anchor: ForkAnchor::AfterUserMessage { item_id },
            }),
        ) => {
            assert_eq!(
                source_session_id.to_string(),
                "ses_22222222222222222222222222222222"
            );
            assert_eq!(item_id.to_string(), "itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        }
        (
            assertion @ ("session_update_metadata_set"
            | "session_update_metadata_clear"
            | "session_update_metadata_keep"),
            RuntimeCommand::Session(SessionCommand::UpdateMetadata {
                session_id,
                expected_revision,
                patch,
            }),
        ) => {
            assert_eq!(
                session_id.to_string(),
                "ses_22222222222222222222222222222222"
            );
            match assertion {
                "session_update_metadata_set" => {
                    assert_eq!(expected_revision.get(), 1);
                    assert_eq!(patch.name().set_value(), Some("Session v2"));
                    assert_eq!(
                        patch.description().set_value(),
                        Some("Plans and reviews implementation work")
                    );
                }
                "session_update_metadata_clear" => {
                    assert_eq!(expected_revision.get(), 2);
                    assert!(patch.name().is_keep());
                    assert!(patch.description().is_clear());
                }
                "session_update_metadata_keep" => {
                    assert_eq!(expected_revision.get(), 2);
                    assert!(patch.name().is_keep());
                    assert!(patch.description().is_keep());
                }
                _ => unreachable!(),
            }
        }
        (
            "session_update_definition_set",
            RuntimeCommand::Session(SessionCommand::UpdateDefinition {
                session_id,
                expected_revision,
                patch,
            }),
        ) => {
            assert_eq!(
                session_id.to_string(),
                "ses_22222222222222222222222222222222"
            );
            assert_eq!(expected_revision.get(), 1);
            let workspace = patch.workspace().expect("the patch replaces the Workspace");
            assert_eq!(workspace.primary_root().key().as_str(), "repo");
            assert_eq!(
                workspace.primary_root().path().as_str(),
                "file:///Users/alice/project"
            );
            assert_eq!(workspace.cwd().relative_path().as_str(), "src");
            assert_eq!(
                patch.model().unwrap().reasoning(),
                ReasoningPreference::High
            );
            assert_eq!(
                patch
                    .prompts()
                    .unwrap()
                    .enabled()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                ["base"]
            );
        }
        (
            assertion @ ("session_upgrade_agent_revision_current"
            | "session_upgrade_agent_revision_exact"),
            RuntimeCommand::Session(SessionCommand::UpgradeAgentRevision {
                session_id,
                expected_revision,
                target,
            }),
        ) => {
            assert_eq!(
                session_id.to_string(),
                "ses_22222222222222222222222222222222"
            );
            assert_eq!(expected_revision.get(), 1);
            match assertion {
                "session_upgrade_agent_revision_current" => {
                    assert!(
                        target.is_none(),
                        "the current fixture resolves the Agent current revision"
                    );
                }
                "session_upgrade_agent_revision_exact" => {
                    let target = target.expect("the exact fixture pins one target revision");
                    assert_eq!(
                        target.agent_id().to_string(),
                        "agt_11111111111111111111111111111111"
                    );
                    assert_eq!(target.revision().get(), 2);
                }
                _ => unreachable!(),
            }
        }
        (
            "session_reload_workspace",
            RuntimeCommand::Session(SessionCommand::ReloadWorkspace { session_id }),
        ) => {
            assert_eq!(
                session_id.to_string(),
                "ses_22222222222222222222222222222222"
            );
        }
        (
            "turn_cancel_turn",
            RuntimeCommand::Turn(TurnCommand::Cancel {
                target: PublicCancelTarget::Turn(turn_id),
                ..
            }),
        ) => {
            assert_eq!(turn_id.to_string(), "trn_66666666666666666666666666666666");
        }
        (
            "turn_cancel_submit",
            RuntimeCommand::Turn(TurnCommand::Cancel {
                target: PublicCancelTarget::Submit(command_id),
                ..
            }),
        ) => {
            assert_eq!(
                command_id.to_string(),
                "cmd_88888888888888888888888888888888"
            );
        }
        ("turn_submit_text_skill", RuntimeCommand::Turn(TurnCommand::Submit { intent, .. })) => {
            let PromptBodyIntent::Text(text) = intent.body() else {
                panic!("submit assertion requires text body");
            };
            assert_eq!(text.text(), "hello");
            assert_eq!(intent.skills().len(), 1);
            assert_eq!(intent.skills()[0].skill_id().as_str(), "code-review");
        }
        (assertion, command) => {
            panic!("request assertion {assertion} does not match {command:?}")
        }
    }
}

fn run_query_response(vector: &PublicVector) {
    assert_eq!(vector.direction, VectorDirection::RuntimeToClient);
    let raw = read_fixture(&vector.path);
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    match vector.expected.decode.as_str() {
        "accepted" => {
            let response = protocol.decode_query_response(&raw).unwrap();
            assert_eq!(
                protocol.encode_query_response(&response).unwrap(),
                canonical_target(vector, &raw),
                "{}",
                vector.path,
            );
        }
        "protocol_error" => {
            assert_eq!(
                vector.expected.runtime_encoder.as_deref(),
                Some("must_not_send")
            );
            let error = protocol.decode_query_response(&raw).unwrap_err();
            assert_manifest_fault(vector, error);
        }
        decode => panic!("unsupported QueryResponse expectation {decode}"),
    }
}

fn run_runtime_dispatch_error(vector: &PublicVector) {
    assert_eq!(vector.direction, VectorDirection::RuntimeToClient);
    assert_eq!(vector.expected.decode, "accepted");
    let raw = read_fixture(&vector.path);
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let error = protocol.decode_runtime_dispatch_error(&raw).unwrap();
    assert_eq!(
        protocol.encode_runtime_dispatch_error(error).unwrap(),
        canonical_target(vector, &raw),
        "{}",
        vector.path,
    );
}

fn run_query_error(vector: &PublicVector) {
    assert_eq!(vector.direction, VectorDirection::RuntimeToClient);
    assert_eq!(vector.expected.decode, "accepted");
    let raw = read_fixture(&vector.path);
    let protocol = IncrementalRuntimeProtocolV1::v1_0();
    let error = protocol.decode_query_error(&raw).unwrap();
    assert_eq!(error.code(), QueryErrorCode::StaleCursor);
    assert_eq!(error.retry(), RetryAdvice::RefreshAndRetry);
    assert!(error.subject().is_none());
    assert_eq!(
        protocol.encode_query_error(&error).unwrap(),
        canonical_target(vector, &raw),
        "{}",
        vector.path,
    );
}

fn run_protocol_hello(vector: &PublicVector) {
    assert_eq!(vector.direction, VectorDirection::ClientToRuntime);
    let raw = read_fixture(&vector.path);
    match vector.expected.decode.as_str() {
        "accepted" => {
            let hello = decode_protocol_hello_v1(&raw).unwrap();
            let encoded = encode_protocol_hello_v1(&hello).unwrap();
            assert_eq!(encoded, canonical_target(vector, &raw), "{}", vector.path);
        }
        "rejected" => {
            let error = decode_protocol_hello_v1(&raw).unwrap_err();
            assert_manifest_fault(vector, error);
        }
        decode => panic!("unsupported ProtocolHello expectation {decode}"),
    }
}

fn run_bootstrap_response(vector: &PublicVector) {
    assert_eq!(vector.direction, VectorDirection::RuntimeToClient);
    assert_eq!(vector.expected.decode, "accepted");
    let raw = read_fixture(&vector.path);
    let response = decode_protocol_bootstrap_response_v1(&raw).unwrap();
    let encoded = encode_protocol_bootstrap_response_v1(&response).unwrap();
    assert_eq!(encoded, canonical_target(vector, &raw), "{}", vector.path);
}

fn run_file_uri_vectors(vector: &PublicVector) {
    assert_eq!(vector.direction, VectorDirection::Bidirectional);
    assert_eq!(vector.expected.decode, "vector_set");
    assert_eq!(
        vector.expected.valid_round_trip.as_deref(),
        Some("same_wire")
    );
    assert_eq!(vector.expected.invalid.as_deref(), Some("typed_reject"));
    let vectors: FileUriVectors = read_json(&fixture_root().join("public").join(&vector.path));
    assert_eq!(vectors.version, 1);
    assert_eq!(vectors.target, "CanonicalFileUri");
    for case in vectors.valid {
        let uri = CanonicalFileUri::from_str(&case.wire).unwrap();
        let family = match case.family.as_str() {
            "posix" => FileUriFamily::Posix,
            "drive" => FileUriFamily::Drive,
            "unc" => FileUriFamily::Unc,
            family => panic!("unknown file URI family {family}"),
        };
        assert_eq!(uri.family(), family, "{}", case.wire);
        assert_eq!(uri.authority(), case.authority.as_deref(), "{}", case.wire);
        assert_eq!(uri.decoded_path(), case.decoded_path, "{}", case.wire);
        assert_eq!(uri.as_str(), case.wire);
    }
    for case in vectors.invalid {
        assert!(
            CanonicalFileUri::from_str(&case.wire).is_err(),
            "accepted {} ({})",
            case.wire,
            case.reason,
        );
    }
}

fn run_negotiation_vectors(vector: &PublicVector) {
    assert_eq!(vector.direction, VectorDirection::Bidirectional);
    assert_eq!(vector.expected.decode, "case_set");
    assert_eq!(
        vector.expected.assert.as_deref(),
        Some("highest_exact_version_and_capability_intersection")
    );
    let vectors: NegotiationVectors = read_json(&fixture_root().join("public").join(&vector.path));
    assert_eq!(vectors.runtime_supported_versions, [ProtocolVersion::V1_0]);
    let fixture_capabilities = vectors
        .runtime_capabilities
        .iter()
        .map(|value| runtime_capability(value))
        .collect::<Vec<_>>();
    let capabilities = RuntimeCapabilities::for_v1(fixture_capabilities.clone()).unwrap();
    assert_eq!(fixture_capabilities, capabilities.values());
    let router = ProtocolBootstrapRouter::new("minicore-runtime", "0.1.0", capabilities).unwrap();

    for case in vectors.cases {
        let route = router.route(&read_fixture(&case.hello_path)).unwrap();
        let expected_bytes = read_fixture(&case.expected_response_path);
        let expected = decode_protocol_bootstrap_response_v1(&expected_bytes).unwrap();
        assert_eq!(route.response(), &expected, "{}", case.hello_path);
        assert_eq!(
            encode_protocol_bootstrap_response_v1(route.response()).unwrap(),
            without_final_lf(&expected_bytes),
            "{}",
            case.hello_path,
        );
        match route.response() {
            ProtocolBootstrapResponse::Welcome(welcome) => {
                assert!(route.codec().is_some());
                assert_eq!(
                    Some(welcome.selected_version()),
                    case.expected_selected_version
                );
                assert_eq!(
                    welcome.capabilities().values().to_vec(),
                    case.expected_capabilities
                        .unwrap()
                        .iter()
                        .map(|value| runtime_capability(value))
                        .collect::<Vec<_>>(),
                );
                assert!(case.expected_reject_reason.is_none());
            }
            ProtocolBootstrapResponse::Reject(reject) => {
                assert!(route.codec().is_none());
                assert_eq!(
                    reject.reason(),
                    ProtocolRejectReason::UnsupportedProtocolVersion
                );
                assert_eq!(
                    case.expected_reject_reason.as_deref(),
                    Some("unsupported_protocol_version")
                );
            }
        }
    }
}

fn runtime_capability(value: &str) -> RuntimeCapability {
    match value {
        "state_events" => RuntimeCapability::StateEvents,
        "progress_events" => RuntimeCapability::ProgressEvents,
        "runtime_snapshot" => RuntimeCapability::RuntimeSnapshot,
        "session_snapshot" => RuntimeCapability::SessionSnapshot,
        "paged_queries" => RuntimeCapability::PagedQueries,
        "command_catalog" => RuntimeCapability::CommandCatalog,
        "interaction_resolution" => RuntimeCapability::InteractionResolution,
        "session_fork" => RuntimeCapability::SessionFork,
        capability => panic!("unknown fixture runtime capability {capability}"),
    }
}

fn assert_manifest_fault(vector: &PublicVector, error: TypedJsonError) {
    let fault = error.public_decode_error().unwrap_or_else(|| {
        panic!(
            "unclassified public decode error for {}: {error:?}",
            vector.path
        )
    });
    assert_eq!(
        Some(fault.stage().as_str()),
        vector.expected.stage.as_deref()
    );
    assert_eq!(Some(fault.code().as_str()), vector.expected.code.as_deref());
}

fn canonical_target(vector: &PublicVector, raw: &[u8]) -> Vec<u8> {
    if vector.expected.canonical_reencode.as_deref() == Some("same_bytes") {
        return without_final_lf(raw);
    }
    let path = vector
        .expected
        .canonical_reencode_path
        .as_deref()
        .unwrap_or_else(|| panic!("missing canonical target for {}", vector.path));
    let target = read_fixture(path);
    let Some(pointers) = vector.expected.ignored_json_pointers.as_deref() else {
        assert!(vector.expected.runtime_encoder.is_none(), "{}", vector.path);
        return without_final_lf(&target);
    };
    assert_eq!(
        vector.expected.runtime_encoder.as_deref(),
        Some("must_not_send_in_1_0"),
        "{}",
        vector.path,
    );
    let mut stripped: Value = serde_json::from_slice(raw).unwrap();
    for pointer in pointers {
        remove_json_pointer(&mut stripped, pointer);
    }
    let expected: Value = serde_json::from_slice(&target).unwrap();
    assert_eq!(
        stripped, expected,
        "stale ignored pointers for {}",
        vector.path
    );
    without_final_lf(&target)
}

fn remove_json_pointer(value: &mut Value, pointer: &str) {
    let (parent_pointer, token) = pointer
        .rsplit_once('/')
        .unwrap_or_else(|| panic!("invalid JSON pointer {pointer}"));
    let token = token.replace("~1", "/").replace("~0", "~");
    let parent = value
        .pointer_mut(parent_pointer)
        .unwrap_or_else(|| panic!("missing JSON pointer parent {parent_pointer}"));
    match parent {
        Value::Object(object) => {
            assert!(
                object.remove(&token).is_some(),
                "missing JSON pointer {pointer}"
            );
        }
        Value::Array(array) => {
            let index = token
                .parse::<usize>()
                .unwrap_or_else(|_| panic!("invalid array JSON pointer {pointer}"));
            assert!(index < array.len(), "missing JSON pointer {pointer}");
            array.remove(index);
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            panic!("non-container JSON pointer parent {parent_pointer}")
        }
    }
}

fn read_fixture(relative: &str) -> Vec<u8> {
    std::fs::read(fixture_root().join("public").join(relative)).unwrap()
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> T {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn without_final_lf(bytes: &[u8]) -> Vec<u8> {
    bytes.strip_suffix(b"\n").unwrap_or(bytes).to_vec()
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/fixtures/wire-v1")
}
