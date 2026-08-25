use tokio::sync::oneshot;

use crate::agent::{SuspensionError, TurnSuspension};
use crate::conversation::{ConversationEntry, ToolResultDraft, UnsequencedEntry};
use crate::interaction::{InteractionAnswer, InteractionKind};
use crate::model::Usage;
use crate::tools::{ApprovalDecision, ApprovalRequest, ApprovalRisk, ToolName, ToolResultOutcome};
use crate::value::BoundedText;

use super::super::run::ActorSignal;
use super::super::*;
use super::support::{ActorFixture, actor_fixture, actor_fixture_with_tool_calls};

#[tokio::test]
async fn suspension_without_durable_unresolved_tool_is_rejected_without_publication() {
    let mut fixture = actor_fixture(false).await;
    assert_rejected_exact_suspension(&mut fixture, SuspensionError::InvalidState).await;
}

#[tokio::test]
async fn suspension_wrong_call_or_name_is_rejected_without_publication() {
    for wrong_call in [true, false] {
        let mut fixture = actor_fixture(true).await;
        let call_id = if wrong_call {
            "call_00000000000000000000000000000082".parse().unwrap()
        } else {
            fixture.tool_call_id.clone()
        };
        let tool_name = if wrong_call {
            fixture.tool_name.clone()
        } else {
            "other".parse().unwrap()
        };
        let turn_id = fixture.turn_id;
        assert_rejected_suspension(
            &mut fixture,
            turn_id,
            call_id,
            tool_name,
            SuspensionError::InvalidState,
        )
        .await;
    }
}

#[tokio::test]
async fn suspension_for_exact_durable_unresolved_tool_publishes_state_before_event() {
    let mut fixture = actor_fixture(true).await;
    let (suspension, receiver) = suspension(
        fixture.turn_id,
        fixture.tool_call_id.clone(),
        fixture.tool_name.clone(),
    );
    fixture.actor.register_suspension(suspension);
    let state = fixture.actor.state();
    assert_eq!(state.status, SessionStatus::WaitingForInput);
    let pending = state.pending_interaction.unwrap();
    assert_eq!(pending.tool_call_id, fixture.tool_call_id);
    assert_eq!(pending.tool_name, fixture.tool_name);
    assert!(matches!(
        fixture.ready.events.try_recv().unwrap().event,
        super::super::super::event::SessionEvent::InteractionRequested { interaction }
            if interaction == pending
    ));
    drop(fixture.actor);
    assert_eq!(receiver.await.unwrap(), Err(SuspensionError::RuntimeClosed));
}

#[tokio::test]
async fn stale_suspension_after_cancellation_is_rejected_without_publication() {
    let mut fixture = actor_fixture(true).await;
    fixture
        .actor
        .core
        .active
        .as_ref()
        .unwrap()
        .cancellation
        .cancel();
    assert_rejected_exact_suspension(&mut fixture, SuspensionError::Cancelled).await;
}

#[tokio::test]
async fn stale_suspension_after_outcome_or_commit_failure_is_rejected_without_publication() {
    for commit_failure in [false, true] {
        let mut fixture = actor_fixture(true).await;
        if commit_failure {
            fixture.actor.core.active.as_mut().unwrap().commit_failure =
                Some(ActiveCommitFailure {
                    diagnostic: SessionActor::diagnostic(
                        DiagnosticCode::Internal,
                        DiagnosticCategory::Storage,
                        "test commit failure",
                        false,
                    ),
                    unknown: false,
                });
        } else {
            fixture.actor.core.active.as_mut().unwrap().outcome = Some(RunnerOutcome::Completed {
                usage: Usage::default(),
            });
        }
        assert_rejected_exact_suspension(&mut fixture, SuspensionError::InvalidState).await;
    }
}

#[tokio::test]
async fn suspension_rejects_root_cancellation_and_closing_without_publication() {
    let mut root_cancelled = actor_fixture(true).await;
    root_cancelled.actor.root_cancel.cancel();
    assert_rejected_exact_suspension(&mut root_cancelled, SuspensionError::Cancelled).await;

    let mut closing = actor_fixture(true).await;
    closing.actor.core.closing = true;
    closing.actor.publish_state();
    assert_rejected_exact_suspension(&mut closing, SuspensionError::Cancelled).await;
}

#[tokio::test]
async fn suspension_rejects_real_degraded_state_with_cancellation_priority() {
    let mut degraded = actor_fixture(true).await;
    degraded.actor.degrade_on_transcript_failure(
        SessionActor::diagnostic(
            DiagnosticCode::Internal,
            DiagnosticCategory::Storage,
            "test degraded state",
            false,
        ),
        false,
    );
    assert!(matches!(
        degraded.actor.core.health,
        SessionHealth::Degraded { .. }
    ));
    assert!(degraded.actor.core.active.as_ref().is_some_and(|active| {
        active.cancellation.is_cancelled() && active.commit_failure.is_some()
    }));
    assert!(matches!(
        degraded.ready.events.try_recv().unwrap().event,
        super::super::super::event::SessionEvent::HealthChanged { .. }
    ));
    assert_rejected_exact_suspension(&mut degraded, SuspensionError::Cancelled).await;
}

#[tokio::test]
async fn suspension_requires_the_next_unresolved_tool_in_durable_order() {
    let mut fixture = actor_fixture_with_tool_calls(2).await;
    let turn_id = fixture.turn_id;
    let second_tool_call_id = fixture.second_tool_call_id.clone();
    let second_tool_name = fixture.second_tool_name.clone();
    assert_rejected_suspension(
        &mut fixture,
        turn_id,
        second_tool_call_id,
        second_tool_name,
        SuspensionError::InvalidState,
    )
    .await;

    let batch = fixture
        .actor
        .conversation
        .append_validated(vec![UnsequencedEntry::ToolResult(ToolResultDraft {
            turn_id: fixture.turn_id,
            tool_call_id: fixture.tool_call_id.clone(),
            tool_name: fixture.tool_name.clone(),
            outcome: ToolResultOutcome::Success,
            content: BoundedText::new("first complete").unwrap(),
        })])
        .await
        .unwrap();
    assert_eq!(fixture.actor.conversation.head(), batch.head);
    fixture.actor.publish_state();

    let (second, second_receiver) = suspension(
        fixture.turn_id,
        fixture.second_tool_call_id.clone(),
        fixture.second_tool_name.clone(),
    );
    fixture.actor.register_suspension(second);
    let pending = fixture
        .actor
        .state()
        .pending_interaction
        .expect("second tool should become pending");
    assert_eq!(pending.tool_call_id, fixture.second_tool_call_id);
    assert_eq!(pending.tool_name, fixture.second_tool_name);
    drop(fixture.actor);
    assert_eq!(
        second_receiver.await.unwrap(),
        Err(SuspensionError::RuntimeClosed)
    );
}

#[tokio::test]
async fn failed_resume_send_clears_pending_without_resolution_event_and_settles() {
    let mut fixture = actor_fixture(true).await;
    let cancellation = fixture
        .actor
        .core
        .active
        .as_ref()
        .unwrap()
        .cancellation
        .clone();
    let started = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let task_started = std::sync::Arc::clone(&started);
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let task_barrier = std::sync::Arc::clone(&barrier);
    fixture.install_runner(async move {
        task_started.add_permits(1);
        cancellation.cancelled().await;
        task_barrier.wait().await;
        TurnRunnerExit::Finished {
            outcome: RunnerOutcome::Completed {
                usage: Usage::default(),
            },
        }
    });
    let permit = started.acquire_owned().await.unwrap();
    permit.forget();
    let (suspension, receiver) = suspension(
        fixture.turn_id,
        fixture.tool_call_id.clone(),
        fixture.tool_name.clone(),
    );
    fixture.actor.register_suspension(suspension);
    let requested = fixture.ready.events.try_recv().unwrap();
    let interaction_id = match requested.event {
        super::super::super::event::SessionEvent::InteractionRequested { interaction } => {
            interaction.interaction_id
        }
        _ => panic!("expected interaction request"),
    };
    drop(receiver);
    assert!(matches!(
        fixture.actor.handle_answer(
            interaction_id,
            InteractionAnswer::Approval(ApprovalDecision::Deny),
        ),
        Err(crate::error::SessionError::Closed)
    ));
    assert_eq!(fixture.actor.state().status, SessionStatus::Running);
    assert!(fixture.actor.state().pending_interaction.is_none());
    assert!(fixture.ready.events.try_recv().is_err());
    assert!(matches!(
        fixture
            .actor
            .core
            .active
            .as_ref()
            .and_then(|active| active.outcome.as_ref()),
        Some(RunnerOutcome::Failed { .. })
    ));
    barrier.wait().await;
    let exit = fixture.actor.next_signal().await;
    assert!(matches!(&exit, ActorSignal::RunnerExited(Some(Ok(_)))));
    if let ActorSignal::RunnerExited(exit) = exit {
        fixture.actor.handle_runner_exit(exit).await;
    }
    let state = fixture.actor.state();
    assert_eq!(state.status, SessionStatus::Idle);
    assert!(state.pending_interaction.is_none());
    assert!(state.last_terminal.is_some());
    assert!(state.validate().is_ok());
    assert_eq!(
        fixture
            .actor
            .conversation
            .view()
            .entries()
            .iter()
            .filter(|entry| matches!(entry, ConversationEntry::TurnTerminal(_)))
            .count(),
        1
    );
    assert!(matches!(
        fixture.ready.events.try_recv().unwrap().event,
        super::super::super::event::SessionEvent::TurnFinished { .. }
    ));
    assert!(fixture.ready.events.try_recv().is_err());
}

fn suspension(
    turn_id: TurnId,
    tool_call_id: crate::ids::ToolCallId,
    tool_name: ToolName,
) -> (
    TurnSuspension,
    oneshot::Receiver<Result<crate::interaction::InteractionAnswer, SuspensionError>>,
) {
    let (resume, receiver) = oneshot::channel();
    (
        TurnSuspension {
            turn_id,
            tool_call_id,
            tool_name,
            kind: InteractionKind::Approval(
                ApprovalRequest::new("approve", ApprovalRisk::Medium).unwrap(),
            ),
            resume,
        },
        receiver,
    )
}

async fn assert_rejected_exact_suspension(fixture: &mut ActorFixture, expected: SuspensionError) {
    let turn_id = fixture.turn_id;
    let tool_call_id = fixture.tool_call_id.clone();
    let tool_name = fixture.tool_name.clone();
    assert_rejected_suspension(fixture, turn_id, tool_call_id, tool_name, expected).await;
}

async fn assert_rejected_suspension(
    fixture: &mut ActorFixture,
    turn_id: TurnId,
    tool_call_id: crate::ids::ToolCallId,
    tool_name: ToolName,
    expected: SuspensionError,
) {
    let before = fixture.actor.state();
    let (suspension, receiver) = suspension(turn_id, tool_call_id, tool_name);
    fixture.actor.register_suspension(suspension);
    assert_eq!(receiver.await.unwrap(), Err(expected));
    assert_eq!(fixture.actor.state(), before);
    assert!(
        fixture
            .actor
            .core
            .active
            .as_ref()
            .is_none_or(|active| active.pending.is_none())
    );
    assert!(fixture.ready.events.try_recv().is_err());
}
