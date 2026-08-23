use super::super::compaction_support::*;
use super::super::*;

async fn acknowledge_tool_round(
    critical_rx: &mut mpsc::Receiver<RunnerEvent>,
    initial: &ConversationView,
    spec: &SessionSpec,
) -> ConversationView {
    let conversation = match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => {
            let acknowledgement = ack_assistant(initial, &draft, spec);
            let conversation = acknowledgement.conversation.clone();
            reply.send(Ok(acknowledgement)).unwrap();
            conversation
        }
        event => panic!("unexpected event: {event:?}"),
    };
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitToolResult { draft, reply } => {
            let acknowledgement = ack_tool(&conversation, &draft, spec);
            let conversation = acknowledgement.conversation.clone();
            reply.send(Ok(acknowledgement)).unwrap();
            conversation
        }
        event => panic!("unexpected event: {event:?}"),
    }
}

fn usage_case(
    context_window: u64,
    trigger_tokens: u64,
    strategy: Arc<ScriptCompaction>,
    usage: Usage,
) -> (
    SessionSpec,
    ConversationView,
    TurnRunnerRequest,
    mpsc::Receiver<RunnerEvent>,
    Arc<ScriptModel>,
) {
    let model = ScriptModel::new(
        context_window,
        vec![ModelBehavior::Events(tool_events(&[(91, "search")], usage))],
    );
    let tool = ScriptTool::new(
        "search",
        vec![ToolBehavior::Complete(
            ToolOutput::new("x".repeat(8_000)).unwrap(),
        )],
    );
    let spec = enabled_spec(&["search"], 4, trigger_tokens, 100);
    let initial = active_conversation(&spec, 4, "old history");
    let bindings = bindings_with_compaction(
        Arc::clone(&model),
        None,
        vec![tool],
        Some(ScriptPolicy::new(vec![ToolDecision::Allow])),
        strategy,
    );
    let (request, critical_rx, _progress_rx) =
        runner_request(spec.clone(), 4, bindings, initial.clone());
    (spec, initial, request, critical_rx, model)
}

#[tokio::test(flavor = "current_thread")]
async fn proactive_commit_failure_after_model_usage_preserves_usage() {
    let usage = Usage::new(11, 7, 3);
    let strategy =
        ScriptCompaction::new(vec![CompactionBehavior::Proposal(proposal(3, "summary"))]);
    let (spec, initial, request, mut critical_rx, model) =
        usage_case(20_000, 1_000, strategy, usage);
    let task = tokio::spawn(run_turn(request));
    let _conversation = acknowledge_tool_round(&mut critical_rx, &initial, &spec).await;
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitSummary { reply, .. } => {
            reply.send(Err(RunnerCommitError::Degraded)).unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    let outcome = match critical_rx.recv().await.unwrap() {
        RunnerEvent::Finish { outcome } => outcome,
        event => panic!("unexpected event: {event:?}"),
    };
    assert_eq!(outcome.usage(), usage);
    let diagnostic = outcome.diagnostic().unwrap();
    assert_eq!(
        diagnostic.code,
        crate::error::DiagnosticCode::SessionDegraded
    );
    assert_eq!(
        diagnostic.category,
        crate::error::DiagnosticCategory::Storage
    );
    assert_finished(task.await.unwrap());
    assert_eq!(model.requests().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn forced_failure_after_model_usage_preserves_usage() {
    let usage = Usage::new(13, 8, 4);
    let strategy = ScriptCompaction::new(vec![CompactionBehavior::Error(
        crate::compaction::CompactionError::Internal,
    )]);
    let (spec, initial, request, mut critical_rx, model) =
        usage_case(1_000, 10_000, strategy, usage);
    let task = tokio::spawn(run_turn(request));
    let _conversation = acknowledge_tool_round(&mut critical_rx, &initial, &spec).await;
    let outcome = match critical_rx.recv().await.unwrap() {
        RunnerEvent::Finish { outcome } => outcome,
        event => panic!("unexpected event: {event:?}"),
    };
    assert_eq!(outcome.usage(), usage);
    assert_eq!(
        outcome.diagnostic().unwrap().category,
        crate::error::DiagnosticCategory::Compaction
    );
    assert_finished(task.await.unwrap());
    assert_eq!(model.requests().len(), 1);
}
