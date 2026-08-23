use super::*;

fn build_request(
    spec: SessionSpec,
    durable_max_tool_rounds: u16,
    supplied_max_tool_rounds: u16,
) -> Result<TurnRunnerRequest, TurnRunnerRequestError> {
    let model = ScriptModel::new(4_096, Vec::new());
    let conversation = initial_conversation(&spec, durable_max_tool_rounds);
    let (critical_tx, _critical_rx) = mpsc::channel(1);
    let (progress_tx, _progress_rx) = mpsc::channel(1);
    TurnRunnerRequest::new(
        TurnRunnerIdentity {
            session_id: session_id(),
            instance_id: instance_id(),
            turn_id: turn_id(),
        },
        spec,
        supplied_max_tool_rounds,
        session_bindings(model, None, Vec::new(), None),
        conversation,
        TurnRunnerKernel::from_kernel(&KernelConfig::default_checked().unwrap()).unwrap(),
        TurnRunnerControl {
            cancellation: CancellationToken::new(),
            deadline: Instant::now() + Duration::from_secs(30),
            critical_tx,
            progress_tx,
        },
    )
}

#[test]
fn request_rejects_supplied_rounds_above_the_durable_active_turn_value() {
    assert!(matches!(
        build_request(session_spec(&[], 4), 1, 4),
        Err(TurnRunnerRequestError::Conversation)
    ));
}

#[test]
fn request_rejects_supplied_rounds_below_the_durable_active_turn_value() {
    assert!(matches!(
        build_request(session_spec(&[], 4), 4, 1),
        Err(TurnRunnerRequestError::Conversation)
    ));
}

#[test]
fn request_accepts_the_exact_durable_lower_round_value() {
    assert!(build_request(session_spec(&[], 4), 1, 1).is_ok());
}
