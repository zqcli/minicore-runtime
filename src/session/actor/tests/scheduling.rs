use std::task::Poll;

use futures_util::poll;

use crate::agent::{RunnerEvent, RunnerOutcome, TurnRunnerExit};
use crate::conversation::{AssistantMessageDraft, ConversationEntry, TurnTerminal};
use crate::model::{ModelFinishReason, Usage};
use crate::value::BoundedText;

use super::super::run::ActorSignal;
use super::super::*;
use super::support::actor_fixture;

#[tokio::test]
async fn root_then_critical_progress_ahead_of_ready_command_flood() {
    let mut fixture = actor_fixture(false).await;
    let handle = fixture.ready.handle.clone();
    let mut commands = Vec::new();
    for _ in 0..4 {
        let mut command = Box::pin(handle.transcript(None, 1));
        assert!(matches!(poll!(command.as_mut()), Poll::Pending));
        commands.push(command);
    }
    let (reply, receiver) = tokio::sync::oneshot::channel();
    fixture
        .critical_tx
        .send(RunnerEvent::CommitAssistant {
            draft: AssistantMessageDraft {
                turn_id: fixture.turn_id,
                model: fixture.actor.environment.session_inputs().0.model.clone(),
                text: Some(BoundedText::new("answer").unwrap()),
                reasoning: None,
                tool_calls: Vec::new(),
                usage: Usage::new(1, 1, 0),
                finish_reason: ModelFinishReason::Stop,
            },
            reply,
        })
        .await
        .unwrap();
    fixture.actor.root_cancel.cancel();
    assert!(matches!(
        fixture.actor.next_signal().await,
        super::super::run::ActorSignal::RootCancelled
    ));
    fixture.actor.begin_shutdown();
    let critical = fixture.actor.next_signal().await;
    assert!(matches!(
        &critical,
        ActorSignal::Critical(Some(RunnerEvent::CommitAssistant { .. }))
    ));
    if let ActorSignal::Critical(Some(event)) = critical {
        fixture.actor.handle_runner_event(event).await;
    }
    assert_eq!(
        receiver.await.unwrap().unwrap().entry.seq(),
        ConversationSeq::new(2)
    );
    drop(commands);
}

#[tokio::test]
async fn simultaneous_finish_and_join_readiness_settles_exactly_once() {
    let mut fixture = actor_fixture(false).await;
    let ready = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let task_ready = std::sync::Arc::clone(&ready);
    let outcome = RunnerOutcome::Cancelled {
        usage: Usage::new(1, 2, 0),
    };
    fixture.install_runner({
        let outcome = outcome.clone();
        async move {
            task_ready.add_permits(1);
            TurnRunnerExit::Finished { outcome }
        }
    });
    fixture
        .critical_tx
        .send(RunnerEvent::Finish {
            outcome: outcome.clone(),
        })
        .await
        .unwrap();
    let permit = ready.acquire_owned().await.unwrap();
    permit.forget();
    let critical = fixture.actor.next_signal().await;
    assert!(matches!(&critical, ActorSignal::Critical(Some(_))));
    if let ActorSignal::Critical(Some(event)) = critical {
        fixture.actor.handle_runner_event(event).await;
    }
    let exit = fixture.actor.next_signal().await;
    assert!(matches!(&exit, ActorSignal::RunnerExited(Some(Ok(_)))));
    if let ActorSignal::RunnerExited(exit) = exit {
        fixture.actor.handle_runner_exit(exit).await;
    }
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
        super::super::super::event::SessionEvent::TurnFinished { outcome, .. }
            if outcome.terminal == TurnTerminal::CancelledByUser
    ));
    assert!(fixture.ready.events.try_recv().is_err());
}
