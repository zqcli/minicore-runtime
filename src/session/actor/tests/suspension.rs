use tokio::sync::oneshot;

use crate::agent::{SuspensionError, TurnSuspension};
use crate::conversation::{ConversationEntry, ToolResultDraft, UnsequencedEntry};
use crate::interaction::{InteractionAnswer, InteractionKind};
use crate::model::Usage;
use crate::tools::{ApprovalDecision, ApprovalRequest, ApprovalRisk, ToolName, ToolResultOutcome};
use crate::value::BoundedText;

use super::super::run::ActorSignal;
use super::super::*;
use super::support::{actor_fixture, actor_fixture_with_tool_calls};

#[tokio::test]
async fn suspension_without_durable_unresolved_tool_is_rejected_without_publication() {
    let mut fixture = actor_fixture(false).await;
    let before = fixture.actor.state();
    let (suspension, receiver) =
        suspension(fixture.turn_id, fixture.tool_call_id, fixture.tool_name);
    fixture.actor.register_suspension(suspension);
    assert_eq!(receiver.await.unwrap(), Err(SuspensionError::InvalidState));
    assert_eq!(fixture.actor.state(), before);
    assert!(fixture.ready.events.try_recv().is_err());
}

#[tokio::test]
async fn suspension_wrong_call_or_name_is_rejected_without_publication() {
    for wrong_call in [true, false] {
        let mut fixture = actor_fixture(true).await;
        let before = fixture.actor.state();
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
        let (suspension, receiver) = suspension(fixture.turn_id, call_id, tool_name);
        fixture.actor.register_suspension(suspension);
        assert_eq!(receiver.await.unwrap(), Err(SuspensionError::InvalidState));
        assert_eq!(fixture.actor.state(), before);
        assert!(fixture.ready.events.try_recv().is_err());
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
async fn suspension_requires_the_next_unresolved_tool_in_durable_order() {
    let mut fixture = actor_fixture_with_tool_calls(2).await;
    let before = fixture.actor.state();
    let (second, second_receiver) = suspension(
        fixture.turn_id,
        fixture.second_tool_call_id.clone(),
        fixture.second_tool_name.clone(),
    );
    fixture.actor.register_suspension(second);
    assert_eq!(
        second_receiver.await.unwrap(),
        Err(SuspensionError::InvalidState)
    );
    assert_eq!(fixture.actor.state(), before);
    assert!(fixture.ready.events.try_recv().is_err());

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
    let mut state = fixture.actor.state();
    state.conversation_seq = batch.head;
    fixture.actor.publish_state(state);

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
    let cancellation = fixture.actor.active.as_ref().unwrap().cancellation.clone();
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
