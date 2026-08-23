use minicore_runtime::conversation::{ConversationSeq, TurnTerminal};
use minicore_runtime::error::{
    DiagnosticCategory, DiagnosticCode, DiagnosticSummary, EventStreamTakenError,
};
use minicore_runtime::model::Usage;
use minicore_runtime::session::{
    InteractionKind, InteractionResolutionSummary, OutputChannel, PendingInteraction, SessionEvent,
    SessionEventEnvelope, SessionEventStream, SessionHealth, SessionState, SessionStateError,
    SessionStatus, ToolResultSummary, TurnOutcome,
};
use minicore_runtime::tools::{ApprovalRequest, ApprovalRisk, ToolProgress, ToolResultOutcome};
use minicore_runtime::{
    BoundedText, InteractionId, SessionId, SessionInstanceId, ToolCallId, TurnId,
};

fn session_id() -> SessionId {
    "ses_00000000000000000000000000000001".parse().unwrap()
}

fn instance_id() -> SessionInstanceId {
    "ins_00000000000000000000000000000001".parse().unwrap()
}

fn turn_id(value: u8) -> TurnId {
    format!("trn_{value:032}").parse().unwrap()
}

fn call_id() -> ToolCallId {
    "call_00000000000000000000000000000001".parse().unwrap()
}

fn interaction_id() -> InteractionId {
    "int_00000000000000000000000000000001".parse().unwrap()
}

fn diagnostic(message: &str) -> DiagnosticSummary {
    DiagnosticSummary::new(
        DiagnosticCode::RuntimeTerminated,
        DiagnosticCategory::Internal,
        BoundedText::new(message).unwrap(),
        false,
    )
}

fn interaction(turn_id: TurnId) -> PendingInteraction {
    PendingInteraction {
        interaction_id: interaction_id(),
        turn_id,
        tool_call_id: call_id(),
        tool_name: "safe_tool".parse().unwrap(),
        kind: InteractionKind::Approval(
            ApprovalRequest::new("approval-prompt-secret", ApprovalRisk::High).unwrap(),
        ),
    }
}

fn outcome(turn_id: TurnId, terminal: TurnTerminal) -> TurnOutcome {
    TurnOutcome {
        turn_id,
        terminal,
        usage: Usage::new(3, 2, 1),
    }
}

fn state(
    status: SessionStatus,
    active_turn: Option<TurnId>,
    pending_interaction: Option<PendingInteraction>,
    last_terminal: Option<TurnOutcome>,
) -> SessionState {
    SessionState {
        session_id: session_id(),
        instance_id: instance_id(),
        status,
        health: SessionHealth::Healthy,
        active_turn,
        pending_interaction,
        conversation_seq: ConversationSeq::new(7),
        last_terminal,
    }
}

fn event_name(event: &SessionEvent) -> &'static str {
    match event {
        SessionEvent::TurnStarted { .. } => "turn_started",
        SessionEvent::ModelStarted { .. } => "model_started",
        SessionEvent::OutputDelta { .. } => "output_delta",
        SessionEvent::ModelFinished { .. } => "model_finished",
        SessionEvent::ToolStarted { .. } => "tool_started",
        SessionEvent::ToolProgress { .. } => "tool_progress",
        SessionEvent::ToolFinished { .. } => "tool_finished",
        SessionEvent::InteractionRequested { .. } => "interaction_requested",
        SessionEvent::InteractionResolved { .. } => "interaction_resolved",
        SessionEvent::HealthChanged { .. } => "health_changed",
        SessionEvent::TurnFinished { .. } => "turn_finished",
        SessionEvent::EventsDropped { .. } => "events_dropped",
    }
}

fn output_channel_name(channel: OutputChannel) -> &'static str {
    match channel {
        OutputChannel::Text => "text",
        OutputChannel::Reasoning => "reasoning",
    }
}

fn resolution_name(resolution: InteractionResolutionSummary) -> &'static str {
    match resolution {
        InteractionResolutionSummary::Approved => "approved",
        InteractionResolutionSummary::Denied => "denied",
        InteractionResolutionSummary::InputProvided => "input_provided",
    }
}

fn status_name(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Idle => "idle",
        SessionStatus::Running => "running",
        SessionStatus::WaitingForInput => "waiting_for_input",
        SessionStatus::Closing => "closing",
    }
}

fn health_name(health: &SessionHealth) -> &'static str {
    match health {
        SessionHealth::Healthy => "healthy",
        SessionHealth::Degraded { .. } => "degraded",
    }
}

#[test]
fn state_surface_and_legal_matrix_are_exact() {
    let turn = turn_id(1);
    assert_eq!(status_name(SessionStatus::Idle), "idle");
    let previous = outcome(turn_id(2), TurnTerminal::Completed);
    for valid in [
        state(SessionStatus::Idle, None, None, Some(previous.clone())),
        state(SessionStatus::Running, Some(turn), None, None),
        state(
            SessionStatus::WaitingForInput,
            Some(turn),
            Some(interaction(turn)),
            None,
        ),
        state(SessionStatus::Closing, None, None, None),
        state(SessionStatus::Closing, Some(turn), None, None),
    ] {
        assert_eq!(valid.validate(), Ok(()));
    }

    let source = include_str!("../src/session/state.rs");
    let fields = source
        .split_once("pub struct SessionState")
        .and_then(|(_, tail)| tail.split_once('}'))
        .map(|(body, _)| body)
        .unwrap();
    let actual = fields
        .lines()
        .filter_map(|line| line.trim().strip_prefix("pub "))
        .map(|line| line.split(':').next().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        vec![
            "session_id",
            "instance_id",
            "status",
            "health",
            "active_turn",
            "pending_interaction",
            "conversation_seq",
            "last_terminal",
        ]
    );
    for forbidden in [
        "Serialize",
        "Deserialize",
        "serde",
        "Snapshot",
        "cursor",
        "revision",
        "epoch",
        "Runtime",
        "SessionHandle",
        "Workspace",
    ] {
        assert!(!source.contains(forbidden));
    }
}

#[test]
fn state_rejects_every_illegal_shape() {
    let turn = turn_id(1);
    let other = turn_id(2);
    for (invalid, error) in [
        (
            state(SessionStatus::Idle, Some(turn), None, None),
            SessionStateError::IdleHasActiveTurn,
        ),
        (
            state(SessionStatus::Idle, None, Some(interaction(turn)), None),
            SessionStateError::IdleHasPendingInteraction,
        ),
        (
            state(SessionStatus::Running, None, None, None),
            SessionStateError::RunningMissingActiveTurn,
        ),
        (
            state(
                SessionStatus::Running,
                Some(turn),
                Some(interaction(turn)),
                None,
            ),
            SessionStateError::RunningHasPendingInteraction,
        ),
        (
            state(SessionStatus::WaitingForInput, None, None, None),
            SessionStateError::WaitingMissingActiveTurn,
        ),
        (
            state(SessionStatus::WaitingForInput, Some(turn), None, None),
            SessionStateError::WaitingMissingInteraction,
        ),
        (
            state(
                SessionStatus::WaitingForInput,
                Some(turn),
                Some(interaction(other)),
                None,
            ),
            SessionStateError::WaitingTurnMismatch,
        ),
        (
            state(
                SessionStatus::Closing,
                Some(turn),
                Some(interaction(turn)),
                None,
            ),
            SessionStateError::ClosingHasPendingInteraction,
        ),
        (
            state(
                SessionStatus::Running,
                Some(turn),
                None,
                Some(outcome(turn, TurnTerminal::Completed)),
            ),
            SessionStateError::ActiveTurnAlreadyTerminal,
        ),
    ] {
        assert_eq!(invalid.validate(), Err(error));
    }
}

#[test]
fn diagnostic_state_and_event_debug_are_payload_redacted() {
    let secret = "diagnostic-message-secret";
    let diagnostic = diagnostic(secret);
    let wire = serde_json::to_value(&diagnostic).unwrap();
    assert_eq!(wire["message"], secret);
    assert_eq!(
        serde_json::from_value::<DiagnosticSummary>(wire).unwrap(),
        diagnostic
    );
    let health = SessionHealth::Degraded {
        diagnostic: diagnostic.clone(),
    };
    assert_eq!(health_name(&health), "degraded");
    let degraded = SessionState {
        health: health.clone(),
        ..state(SessionStatus::Idle, None, None, None)
    };
    let progress = ToolProgress::new(
        Some(BoundedText::new("progress-message-secret").unwrap()),
        Some(1),
        Some(2),
    )
    .unwrap();
    let failed = outcome(
        turn_id(1),
        TurnTerminal::Failed {
            diagnostic: diagnostic.clone(),
        },
    );
    let events = vec![
        SessionEvent::OutputDelta {
            turn_id: turn_id(1),
            channel: OutputChannel::Text,
            delta: BoundedText::new("output-delta-secret").unwrap(),
        },
        SessionEvent::ToolProgress {
            turn_id: turn_id(1),
            tool_call_id: call_id(),
            progress,
        },
        SessionEvent::InteractionRequested {
            interaction: interaction(turn_id(1)),
        },
        SessionEvent::HealthChanged { health },
        SessionEvent::TurnFinished {
            turn_id: turn_id(1),
            outcome: failed,
        },
    ];
    let debug = format!("{diagnostic:?} {degraded:?} {events:?}");
    for secret in [
        secret,
        "progress-message-secret",
        "output-delta-secret",
        "approval-prompt-secret",
    ] {
        assert!(!debug.contains(secret), "debug leaked {secret}");
    }
    for safe in [
        "message_bytes",
        "OutputDelta",
        "ToolProgress",
        "TurnFinished",
    ] {
        assert!(debug.contains(safe));
    }
}

#[test]
fn event_variants_envelope_and_stream_surface_are_exact() {
    fn assert_send<T: Send>() {}
    assert_send::<SessionEventStream>();
    let turn = turn_id(1);
    let events = vec![
        SessionEvent::TurnStarted { turn_id: turn },
        SessionEvent::ModelStarted {
            turn_id: turn,
            round: 0,
        },
        SessionEvent::OutputDelta {
            turn_id: turn,
            channel: OutputChannel::Reasoning,
            delta: BoundedText::new("x").unwrap(),
        },
        SessionEvent::ModelFinished {
            turn_id: turn,
            round: 0,
            usage: Usage::default(),
        },
        SessionEvent::ToolStarted {
            turn_id: turn,
            tool_call_id: call_id(),
            tool_name: "safe_tool".parse().unwrap(),
        },
        SessionEvent::ToolProgress {
            turn_id: turn,
            tool_call_id: call_id(),
            progress: ToolProgress::new(None, Some(1), Some(2)).unwrap(),
        },
        SessionEvent::ToolFinished {
            turn_id: turn,
            tool_call_id: call_id(),
            result: ToolResultSummary {
                outcome: ToolResultOutcome::Success,
                content_bytes: 42,
            },
        },
        SessionEvent::InteractionRequested {
            interaction: interaction(turn),
        },
        SessionEvent::InteractionResolved {
            interaction_id: interaction_id(),
            resolution: InteractionResolutionSummary::Approved,
        },
        SessionEvent::HealthChanged {
            health: SessionHealth::Healthy,
        },
        SessionEvent::TurnFinished {
            turn_id: turn,
            outcome: outcome(turn, TurnTerminal::Completed),
        },
        SessionEvent::EventsDropped { count: 3 },
    ];
    for event in &events {
        assert!(!event_name(event).is_empty());
    }
    assert_eq!(output_channel_name(OutputChannel::Text), "text");
    assert_eq!(
        resolution_name(InteractionResolutionSummary::InputProvided),
        "input_provided"
    );
    let envelope = SessionEventEnvelope {
        session_id: session_id(),
        instance_id: instance_id(),
        event: events[0].clone(),
    };
    assert_eq!(envelope.session_id, session_id());
    assert_eq!(envelope.instance_id, instance_id());

    assert_eq!(
        EventStreamTakenError::AlreadyTaken.to_string(),
        "session event stream was already taken"
    );
    let stream = include_str!("../src/session/event_stream.rs");
    assert!(stream.contains("receiver: mpsc::Receiver<SessionEventEnvelope>"));
    assert!(stream.contains("self.sender.try_send"));
    assert!(stream.contains("pub(crate) struct InternalEventSink"));
    for forbidden in [
        "impl Clone for SessionEventStream",
        "broadcast",
        "watch::",
        "snapshot(",
        "subscribe",
        "ResyncRequired",
        "cursor",
        "revision",
        "epoch",
    ] {
        assert!(!stream.contains(forbidden));
    }
    let event_source = include_str!("../src/session/event.rs");
    for forbidden in [
        "Serialize",
        "Deserialize",
        "serde_json",
        "ToolOutput",
        "ToolInvocation",
        "InteractionAnswer",
        "ModelError",
        "String",
    ] {
        assert!(!event_source.contains(forbidden));
    }
    let session = include_str!("../src/session/mod.rs");
    assert!(session.contains("pub use event_stream::SessionEventStream;"));
    assert!(!session.contains("legacy_"));
    for final_source in [
        include_str!("../src/session/state.rs"),
        event_source,
        stream,
    ] {
        for forbidden in ["SessionSnapshot", "SessionObservation", "ResyncRequired"] {
            assert!(!final_source.contains(forbidden));
        }
    }
}
