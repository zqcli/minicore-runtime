#[test]
fn p6_session_actor_surface_is_private_and_owner_proof_free() {
    let session = include_str!("../src/session/mod.rs");
    assert!(session.contains("pub(crate) mod command;"));
    assert!(session.contains("pub(crate) mod actor;"));
    assert!(session.contains("pub(crate) use actor::"));
    assert!(session.contains("pub(crate) use command::SessionHandle;"));
    assert!(!session.contains("pub mod actor"));
    assert!(!session.contains("pub mod command"));

    let command = include_str!("../src/session/command.rs");
    let tools_context = include_str!("../src/tools/legacy_context.rs");
    let tools_mod = include_str!("../src/tools/mod.rs");
    assert!(tools_context.contains("pub(crate) fn claim_response"));
    assert!(tools_context.contains("pub(crate) struct InteractionResponse"));
    assert!(tools_context.contains("impl Drop for InteractionResponse"));
    assert!(!tools_mod.contains("InteractionResponse"));
    let actor = include_str!("../src/session/actor.rs");
    let production = actor
        .split("#[cfg(test)]")
        .next()
        .expect("actor production section");
    assert!(production.contains("JoinHandle<TurnTaskResult>"));
    assert!(production.contains("runtime.spawn(run_turn(context))"));
    assert!(production.contains("active.task.abort()"));
    assert!(production.contains("(&mut active.task).await"));
    assert!(production.contains("self.conversation.wait_idle().await"));
    assert!(production.contains("self.refresh_projection().await;"));
    assert!(production.contains("close_session(Some(error))"));
    let abort = production.find("active.task.abort()").unwrap();
    let wait_idle = production
        .find("self.conversation.wait_idle().await")
        .unwrap();
    let refresh = production
        .find("self.conversation.wait_idle().await;\n                    self.refresh_projection().await;")
        .unwrap();
    let finish = production
        .find("self.finish_active(result, true).await")
        .unwrap();
    assert!(abort < wait_idle && wait_idle == refresh && refresh < finish);
    assert!(production.contains("SessionObservation"));
    assert!(production.contains("CancelSlot"));
    assert!(production.contains("cancel_slot.install(turn_id, cancellation.clone())"));
    let slot_install = production
        .find("cancel_slot.install(turn_id, cancellation.clone())")
        .unwrap();
    let final_close_check = production
        .find("if close_won || self.close_requested.is_cancelled()")
        .unwrap();
    let spawn = production.find("runtime.spawn(run_turn(context))").unwrap();
    assert!(slot_install < final_close_check && final_close_check < spawn);
    assert!(production.contains("stored.usage()"));
    assert!(production.contains("snapshot.usage()"));
    assert!(
        production.contains("self.refresh_projection().await;\n\n        let cancel_slot_owner")
    );
    assert!(production.lines().count() <= 1_200);

    let command_enum = command
        .split("pub(crate) enum SessionCommand")
        .nth(1)
        .and_then(|value| value.split("}\n\npub(crate) struct CancelSlot").next())
        .expect("SessionCommand definition");
    assert!(command_enum.contains("Submit"));
    assert!(command_enum.contains("Answer"));
    assert!(!command_enum.contains("Cancel"));
    assert!(!command_enum.contains("Close"));
    assert!(command.contains("try_send(SessionCommand::Submit"));
    assert!(command.contains("try_send(SessionCommand::Answer"));
    assert!(command.contains("close_requested.cancel()"));
    assert!(command.contains("request_close(&self.close_requested)"));
    assert!(command.contains("if let Some((_, cancellation)) = slot.as_ref()"));
    assert!(command.contains("cancellation.cancel();"));
    assert!(production.contains("request.claim_response()"));
    assert!(
        production.contains("self.close_requested.is_cancelled() || cancellation.is_cancelled()")
    );
    assert!(command.contains("CloseCompletion"));
    assert!(command.contains("Option<Result<(), SessionError>>"));
    assert!(!command.contains("watch<bool>"));
    assert!(!command.contains("SessionCommand::Cancel"));
    assert!(!command.contains("SessionCommand::Close"));

    for source in [production, command] {
        for forbidden in [
            "crate::agent_session_lifecycle",
            "crate::conversation_storage",
            "crate::durable_state",
            "crate::live_conversation",
            "crate::session_execution",
            "crate::session_ingress",
            "crate::session_residency",
            "crate::runtime",
            "crate::runtime_task",
            "crate::wire",
            "crate::compaction::",
            "crate::model_gateway",
            "crate::turn_execution_context",
            "SessionExecutor",
            "SessionIngress",
            "SessionResidency",
            "owner-proof",
            "OwnerProof",
            "generation",
            "epoch",
            "permit",
            "proof",
            "tokio::spawn",
            "spawn_blocking",
            "allow(",
        ] {
            assert!(
                !source.contains(forbidden),
                "forbidden P6 actor concept: {forbidden}"
            );
        }
    }

    let conversation = include_str!("../src/storage/conversation.rs");
    let support = include_str!("../src/storage/conversation/actor_support.rs");
    let usage = include_str!("../src/storage/conversation/usage.rs");
    assert!(conversation.contains("validate_user_text"));
    assert!(support.contains("MAX_USER_TEXT_BYTES"));
    assert!(support.contains("pub(crate) fn usage(&self)"));
    assert!(usage.contains("usage_from_entries"));
    let state = include_str!("../src/session/state.rs");
    assert_eq!(state.matches("pub enum SessionStatus").count(), 1);
    for variant in ["Idle", "Running", "WaitingForInput", "Closing"] {
        assert!(state.contains(variant));
    }
    for removed in ["FollowUp", "Steer", "Queued", "Preparing", "Finishing"] {
        assert!(
            !state.contains(removed),
            "legacy session state remains: {removed}"
        );
    }
}
