use super::*;
use crate::conversation::TurnTerminal;
use crate::model::Usage;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(test)]
mod scheduling;
#[cfg(test)]
mod summary;
#[cfg(test)]
mod support;
#[cfg(test)]
mod suspension;

pub(crate) use support::{ActorFixture, actor_fixture};

type PostReadyPanicScripts = Mutex<BTreeMap<SessionId, Arc<tokio::sync::Barrier>>>;
type PostReadyPanicBarrier = Arc<tokio::sync::Barrier>;

pub(crate) fn script_post_ready_panic(session_id: SessionId) -> PostReadyPanicBarrier {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let replaced = post_ready_panic_scripts()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(session_id, Arc::clone(&barrier));
    assert!(
        replaced.is_none(),
        "post-ready actor panic script collision"
    );
    barrier
}

pub(super) fn take_post_ready_panic(session_id: SessionId) -> Option<PostReadyPanicBarrier> {
    post_ready_panic_scripts()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&session_id)
}

fn post_ready_panic_scripts() -> &'static PostReadyPanicScripts {
    static SCRIPTS: OnceLock<PostReadyPanicScripts> = OnceLock::new();
    SCRIPTS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[test]
fn initial_state_rehydrates_head_terminal_and_final_handle_identity() {
    let session_id = "ses_00000000000000000000000000000041".parse().unwrap();
    let instance_id = "ins_00000000000000000000000000000041".parse().unwrap();
    let turn_id = "trn_00000000000000000000000000000041".parse().unwrap();
    let outcome = TurnOutcome {
        turn_id,
        terminal: TurnTerminal::Completed,
        usage: Usage::new(1, 2, 3),
    };
    let core = ActorCoreState {
        active: None,
        health: SessionHealth::Healthy,
        closing: false,
        closing_durability_failure: None,
        last_terminal: Some(outcome.clone()),
        last_resolved_interaction: None,
    };
    let state = derive_state(session_id, instance_id, ConversationSeq::new(9), &core);
    assert_eq!(state.status, SessionStatus::Idle);
    assert_eq!(state.conversation_seq, ConversationSeq::new(9));
    assert_eq!(state.last_terminal, Some(outcome));
    assert!(state.validate().is_ok());
}

#[test]
fn actor_source_keeps_root_critical_exit_command_progress_priority() {
    let source = include_str!("run.rs");
    let root = source.find("_ = root.cancelled()").unwrap();
    let critical = source.find("event = active.critical.recv()").unwrap();
    let exit = source
        .find("exit = await_runner(&mut active.runner)")
        .unwrap();
    let command = source.find("command = commands.recv()").unwrap();
    let progress = source.find("progress = active.progress.recv()").unwrap();
    assert!(root < critical && critical < exit && exit < command && command < progress);
}

#[tokio::test]
async fn derived_state_uses_core_phase_priority_and_log_head() {
    let mut fixture = actor_fixture(false).await;
    let running = fixture.actor.state();
    assert_eq!(running.status, SessionStatus::Running);
    assert_eq!(running.conversation_seq, fixture.actor.conversation.head());
    assert!(running.active_turn.is_some());
    assert!(running.pending_interaction.is_none());

    fixture.actor.core.closing = true;
    fixture.actor.publish_state();
    let closing = fixture.actor.state();
    assert_eq!(closing.status, SessionStatus::Closing);
    assert_eq!(closing.conversation_seq, fixture.actor.conversation.head());
    assert_eq!(closing.active_turn, running.active_turn);
    assert!(closing.pending_interaction.is_none());
}
