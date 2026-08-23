fn compact(source: &str) -> String {
    source.split_whitespace().collect()
}

#[test]
fn session_error_handle_and_runtime_surface_are_exact() {
    let error = include_str!("../src/error.rs");
    let compact_error = compact(error);
    for required in [
        "#[non_exhaustive]#[derive(Clone,Debug,Eq,Error,PartialEq)]pubenumSessionError{",
        "Closed,",
        "Busy{active_turn:TurnId},",
        "Degraded(DiagnosticSummary),",
        "Backpressure,",
        "InvalidInput(DiagnosticSummary),",
        "InteractionNotFound,",
        "InteractionKindMismatch,",
        "InteractionAlreadyResolved,",
        "TranscriptUnavailable(DiagnosticSummary),",
    ] {
        assert!(
            compact_error.contains(required),
            "SessionError misses {required}"
        );
    }
    assert!(error.contains("pub(crate) enum LegacySessionError"));

    let handle = include_str!("../src/session/handle.rs");
    let compact_handle = compact(handle);
    for required in [
        "#[derive(Clone)]pubstructSessionHandle{",
        "session_id:SessionId,",
        "instance_id:SessionInstanceId,",
        "commands:mpsc::Sender<SessionCommand>,",
        "state:watch::Receiver<SessionState>,",
        "pubconstfnsession_id(&self)->SessionId",
        "pubconstfninstance_id(&self)->SessionInstanceId",
        "pubfnstate(&self)->SessionState",
        "pubfnwatch_state(&self)->watch::Receiver<SessionState>",
        "pubasyncfnsubmit(",
        "pubasyncfnanswer(",
        "pubasyncfntranscript(",
        "TrySendError::Full(_)=>SessionError::Backpressure",
        "TrySendError::Closed(_)=>SessionError::Closed",
    ] {
        assert!(
            compact_handle.contains(required),
            "SessionHandle misses {required}"
        );
    }
    for forbidden in ["SessionLog", "JoinHandle", "CancellationToken", "impl Drop"] {
        assert!(!handle.contains(forbidden));
    }

    let runtime = include_str!("../src/session/runtime.rs");
    assert!(runtime.contains("handle: SessionHandle"));
    assert!(runtime.contains("pub fn handle(&self) -> SessionHandle"));
    assert!(runtime.contains("self.handle.clone()"));
    let root = include_str!("../src/lib.rs");
    assert!(root.contains("SessionEventStream, SessionHandle, SessionHealth"));
    assert!(!root.contains("pub mod runtime"));
}

#[test]
fn actor_latches_active_commit_failures_and_authenticates_suspensions() {
    let actor = compact(include_str!("../src/session/actor.rs"));
    assert!(
        actor.contains("structActiveCommitFailure{diagnostic:DiagnosticSummary,unknown:bool,}")
    );
    assert!(actor.contains("commit_failure:Option<ActiveCommitFailure>"));
    assert!(actor.contains("closing_durability_failure:Option<DiagnosticSummary>"));

    let runner = compact(include_str!("../src/session/actor/runner.rs"));
    let append = runner.find("append_validated(vec![draft]).await").unwrap();
    let latch = runner
        .find("Err(error)=>Err(self.latch_commit_failure(error))")
        .unwrap();
    assert!(append < latch);
    assert!(runner.contains("active.cancellation.cancel()"));
    assert!(runner.contains("ifactive.commit_failure.is_none()"));
    let phase = runner
        .find("self.conversation.is_next_unresolved_tool(")
        .unwrap();
    let interaction = runner
        .find("letinteraction_id=matchInteractionId::new()")
        .unwrap();
    assert!(phase < interaction);
    let conversation = compact(include_str!("../src/conversation/log.rs"));
    assert!(conversation.contains("unresolved_tool_calls().first().is_some_and("));

    let settlement = compact(include_str!("../src/session/actor/settlement.rs"));
    let failure = settlement
        .find("ifletSome(failure)=active.commit_failure.take()")
        .unwrap();
    let drafts = settlement.find(".settlement_drafts(").unwrap();
    assert!(failure < drafts);

    let supervisor = compact(include_str!("../src/session/actor/supervisor.rs"));
    let await_installed = supervisor
        .find(".and_then(|active|active.runner.as_mut())")
        .unwrap();
    let take_active = supervisor
        .find("ifletSome(mutactive)=self.active.take()")
        .unwrap();
    assert!(await_installed < take_active);

    let run = compact(include_str!("../src/session/actor/run.rs"));
    let close = run
        .find("letclose_error=self.conversation.close().await.err()")
        .unwrap();
    let durability = run
        .find("ifself.closing_durability_failure.take().is_some()")
        .unwrap();
    assert!(close < durability);
}
