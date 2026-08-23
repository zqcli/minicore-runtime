fn compact(source: &str) -> String {
    source.split_whitespace().collect()
}

#[test]
fn final_session_handle_commands_and_actor_are_canonical_and_legacy_is_quarantined() {
    let module = include_str!("../src/session/mod.rs");
    let compact_module = compact(module);
    for required in [
        "modactor;",
        "modcommand;",
        "modhandle;",
        "pubusehandle::SessionHandle;",
        "#[cfg(test)]pub(crate)modlegacy_actor;",
        "#[cfg(test)]pub(crate)modlegacy_command;",
    ] {
        assert!(
            compact_module.contains(required),
            "session module misses {required}"
        );
    }
    assert!(!module.contains("pub use legacy_actor"));
    assert!(!module.contains("pub use legacy_command"));

    let command = include_str!("../src/session/command.rs");
    let compact_command = compact(command);
    for required in [
        "pub(crate)enumSessionCommand{",
        concat!(
            "Submit{input:UserInput,options:TurnOptions,",
            "reply:oneshot::Sender<Result<TurnHandle,SessionError>>,}"
        ),
        concat!(
            "Answer{interaction_id:InteractionId,answer:InteractionAnswer,",
            "reply:oneshot::Sender<Result<(),SessionError>>,}"
        ),
        concat!(
            "Transcript{after:Option<ConversationSeq>,limit:usize,",
            "reply:oneshot::Sender<Result<TranscriptPage,SessionError>>,}"
        ),
    ] {
        assert!(
            compact_command.contains(required),
            "command shape misses {required}"
        );
    }
    for forbidden in ["Cancel {", "Close {", "SessionLog", "CancellationToken"] {
        assert!(!command.contains(forbidden));
    }

    let handle = include_str!("../src/session/handle.rs");
    let compact_handle = compact(handle);
    for required in [
        "#[derive(Clone)]pubstructSessionHandle{",
        "session_id:SessionId,",
        "instance_id:SessionInstanceId,",
        "commands:mpsc::Sender<SessionCommand>,",
        "state:watch::Receiver<SessionState>,",
        "pubasyncfnsubmit(",
        "pubasyncfnanswer(",
        "pubasyncfntranscript(",
        "self.commands.try_send(command)",
        "TrySendError::Full(_)=>SessionError::Backpressure",
        "TrySendError::Closed(_)=>SessionError::Closed",
    ] {
        assert!(
            compact_handle.contains(required),
            "handle misses {required}"
        );
    }
    for forbidden in [
        "SessionLog",
        "JoinHandle",
        "CancellationToken",
        "shutdown",
        "close(",
        "impl Drop",
    ] {
        assert!(!handle.contains(forbidden), "handle owns {forbidden}");
    }
}

#[test]
fn actor_priority_submit_commit_and_settlement_order_are_source_locked() {
    let actor = include_str!("../src/session/actor.rs");
    let run = include_str!("../src/session/actor/run.rs");
    let commands = include_str!("../src/session/actor/commands.rs");
    let compact_commands = compact(commands);
    let runner = include_str!("../src/session/actor/runner.rs");
    let settlement = include_str!("../src/session/actor/settlement.rs");
    let supervisor = include_str!("../src/session/actor/supervisor.rs");
    for required in [
        "pub(crate) struct SessionActor",
        "conversation: ConversationLog",
        "commands: mpsc::Receiver<SessionCommand>",
        "state_tx: watch::Sender<SessionState>",
        "events: InternalEventSink",
        "root_cancel: CancellationToken",
        "active: Option<ActiveTurn>",
    ] {
        assert!(actor.contains(required), "actor misses {required}");
    }
    let root = run.find("_ = root.cancelled()").unwrap();
    let critical = run.find("event = active.critical.recv()").unwrap();
    let exit = run.find("exit = await_runner(&mut active.runner)").unwrap();
    let command = run.find("command = commands.recv()").unwrap();
    let progress = run.find("progress = active.progress.recv()").unwrap();
    assert!(root < critical && critical < exit && exit < command && command < progress);

    let append = compact_commands
        .find(".append_validated(vec![UnsequencedEntry::UserMessage")
        .unwrap();
    let running = compact_commands
        .find("state.status=SessionStatus::Running")
        .unwrap();
    let reply = compact_commands.find("reply.send(Ok(handle))").unwrap();
    let started = compact_commands.find("SessionEvent::TurnStarted").unwrap();
    let spawn = compact_commands.find("tokio::spawn(asyncmove{").unwrap();
    assert!(append < running && running < reply && reply < started && started < spawn);
    let run_turn = compact_commands.find("run_turn(request).await").unwrap();
    let install = compact_commands
        .find(".install_abort(generation,runner.abort_handle())")
        .unwrap();
    assert!(spawn < run_turn && run_turn < install);

    let stale = runner
        .find("if self.conversation.head() != snapshot_head")
        .unwrap();
    let summary = runner
        .find("self.commit_one(UnsequencedEntry::Summary(draft))")
        .unwrap();
    assert!(stale < summary);
    assert!(runner.contains("append_validated(vec![draft]).await"));
    assert!(runner.contains("conversation: self.conversation.view()"));

    assert_eq!(
        settlement.matches("append_validated(drafts).await").count(),
        1
    );
    let state = settlement.find("self.publish_state(state)").unwrap();
    let complete = settlement
        .find("active.completion.finish(outcome.clone())")
        .unwrap();
    let event = settlement.find("SessionEvent::TurnFinished").unwrap();
    assert!(state < complete && complete < event);
    assert!(supervisor.contains("AssertUnwindSafe(actor.run()).catch_unwind()"));
    assert!(supervisor.contains("actor.close_after_panic().await"));

    for source in [actor, run, commands, runner, settlement, supervisor] {
        assert!(source.lines().count() < 500);
        assert!(!source.contains("#[allow"));
        assert!(!source.contains("#[expect"));
    }
}
