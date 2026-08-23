use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::compaction_support::*;
use super::super::*;
use crate::agent::runner::compaction::{
    CompactionState, prepare_model_request, turn_control_outcome,
};
use crate::agent::turn_context::TurnRunnerContext;
use crate::compaction::CompactionStrategy;

struct CancellingOversizedContext {
    calls: AtomicUsize,
}

impl ContextProvider for CancellingOversizedContext {
    fn provide<'a>(&'a self, request: ContextRequest) -> ContextFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            request.cancellation.cancel();
            Ok(ContextBundle {
                blocks: vec![ContextBlock {
                    source: "cancelled-context".parse().unwrap(),
                    slot: ContextSlot::TurnContext,
                    priority: 0,
                    content: BoundedText::new("x".repeat(8_000)).unwrap(),
                }],
            })
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn provider_cancellation_after_success_wins_forced_overflow_interpretation() {
    let provider = Arc::new(CancellingOversizedContext {
        calls: AtomicUsize::new(0),
    });
    let strategy = ScriptCompaction::new(vec![CompactionBehavior::Proposal(proposal(
        3,
        "must not commit",
    ))]);
    let model = ScriptModel::new(1_000, Vec::new());
    let spec = enabled_spec(&[], 4, 10_000, 100);
    let conversation = active_conversation(&spec, 4, "old history");
    let mut bindings = session_bindings(Arc::clone(&model), None, Vec::new(), None);
    let context_port: Arc<dyn ContextProvider> = provider.clone();
    let strategy_port: Arc<dyn CompactionStrategy> = strategy.clone();
    bindings.context = Some(context_port);
    bindings.compaction = Some(strategy_port);
    let (request, mut critical_rx, _progress_rx) = runner_request(spec, 4, bindings, conversation);
    let task = tokio::spawn(run_turn(request));
    assert!(matches!(
        critical_rx.recv().await,
        Some(RunnerEvent::Finish {
            outcome: RunnerOutcome::Cancelled { .. }
        })
    ));
    assert_finished(task.await.unwrap());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(strategy.calls(), 0);
    assert!(model.requests().is_empty());
    assert!(critical_rx.try_recv().is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn expired_turn_after_context_success_wins_without_strategy_or_boundary() {
    let provider = ScriptContext::new(vec![Ok(ContextBundle { blocks: Vec::new() })]);
    let model = ScriptModel::new(4_096, Vec::new());
    let spec = session_spec(&[], 4);
    let conversation = initial_conversation(&spec, 4);
    let (request, _critical_rx, _progress_rx) = runner_request(
        spec,
        4,
        session_bindings(
            Arc::clone(&model),
            Some(Arc::clone(&provider)),
            Vec::new(),
            None,
        ),
        conversation,
    );
    let mut context = TurnRunnerContext::new(request).unwrap();
    context
        .context
        .provide_detailed(ContextRequest {
            session_id: context.session_id,
            instance_id: context.instance_id,
            turn_id: context.turn_id,
            model_round: 0,
            conversation: context.conversation.clone(),
            remaining_context_budget: u64::MAX,
            cancellation: context.cancellation.clone(),
            deadline: context.deadline,
        })
        .await
        .unwrap();
    context.deadline = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let usage = Usage::new(7, 5, 3);
    assert!(matches!(
        turn_control_outcome(&context, usage),
        Some(RunnerOutcome::BudgetExceeded { usage: actual }) if actual == usage
    ));
    let mut state = CompactionState::default();
    assert!(matches!(
        prepare_model_request(&mut context, 0, usage, &mut state).await,
        Err(RunnerOutcome::BudgetExceeded { usage: actual }) if actual == usage
    ));
    assert_eq!(provider.requests().len(), 1);
    assert!(model.requests().is_empty());
}
