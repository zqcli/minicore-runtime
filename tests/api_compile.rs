//! Future v0.3 public API compile contract.
//!
//! This module is intentionally disabled until P4/P5 implement the owner,
//! actor, and runner surface. Activation is only removing `cfg(any())`; the
//! contract must compile against the direct v0.3 API, never a v0.2 alias or
//! compatibility wrapper.

#[cfg(any())]
mod v03_public_api_compile_contract {
    use std::sync::Arc;

    use tokio::sync::{mpsc, watch};

    use minicore_runtime::compaction::CompactionStrategy;
    use minicore_runtime::context::ContextProvider;
    use minicore_runtime::conversation::TranscriptPage;
    use minicore_runtime::error::{
        EventStreamTakenError, SessionError, SessionOpenError, SessionShutdownError, TurnWaitError,
    };
    use minicore_runtime::ids::TurnId;
    use minicore_runtime::model::Model;
    use minicore_runtime::session::{
        SessionEventEnvelope, SessionEventStream, SessionState, TurnOutcome,
    };
    use minicore_runtime::storage::SessionLog;
    use minicore_runtime::tools::{Tool, ToolSet};
    use minicore_runtime::{
        ApprovalDecision, CompactionConfig, InteractionAnswer, InteractionId, KernelConfig,
        SessionBindings, SessionHandle, SessionId, SessionRuntime, SessionRuntimeOptions,
        SessionSpec, ToolInputAnswer, TurnHandle, TurnOptions, UserInput,
    };

    async fn compile_contract(
        session_id: SessionId,
        spec: SessionSpec,
        create_log: Box<dyn SessionLog>,
        load_log: Box<dyn SessionLog>,
        model: Arc<dyn Model>,
        tools: ToolSet,
        tool: Arc<dyn Tool>,
        context: Arc<dyn ContextProvider>,
        compaction: Arc<dyn CompactionStrategy>,
        runtime: tokio::runtime::Handle,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let bindings = SessionBindings::new(model, tools, None, Some(context), Some(compaction));
        let options = SessionRuntimeOptions::new(
            KernelConfig::default_checked()?,
            bindings.clone(),
            runtime.clone(),
        )?;
        let create_future = SessionRuntime::create(session_id, spec.clone(), create_log, options);
        let create_result: Result<SessionRuntime, SessionOpenError> = create_future.await;
        let mut owner = create_result?;

        let owner_session_id = owner.session_id();
        let owner_instance_id = owner.instance_id();
        let take_result: Result<SessionEventStream, EventStreamTakenError> = owner.take_events();
        let mut events = take_result?;
        let received: Option<SessionEventEnvelope> = events.recv().await;
        let tried: Result<SessionEventEnvelope, mpsc::error::TryRecvError> = events.try_recv();
        let _ = (received, tried);

        let handle: SessionHandle = owner.handle();
        assert_eq!(handle.session_id(), owner_session_id);
        assert_eq!(handle.instance_id(), owner_instance_id);
        let current: SessionState = handle.state();
        let state_watch: watch::Receiver<SessionState> = handle.watch_state();
        let _ = (current, state_watch.borrow().clone());

        let input = UserInput::text("compile contract")?;
        let submit_result: Result<TurnHandle, SessionError> =
            handle.submit(input, TurnOptions::default()).await;
        let turn = submit_result?;
        assert_eq!(turn.session_id(), owner_session_id);
        assert_eq!(turn.instance_id(), owner_instance_id);
        let turn_id: TurnId = turn.turn_id();
        let cancel_result: bool = turn.cancel();
        let finished_result: bool = turn.is_finished();
        let wait_result: Result<TurnOutcome, TurnWaitError> = turn.wait().await;
        let _ = (turn_id, cancel_result, finished_result, wait_result);

        let approval_result: Result<(), SessionError> = handle
            .answer(
                InteractionId::new()?,
                InteractionAnswer::Approval(ApprovalDecision::Deny),
            )
            .await;
        let input_result: Result<(), SessionError> = handle
            .answer(
                InteractionId::new()?,
                InteractionAnswer::ToolInput(ToolInputAnswer::Text(
                    minicore_runtime::BoundedText::new("answer")?,
                )),
            )
            .await;
        let transcript_result: Result<TranscriptPage, SessionError> =
            handle.transcript(None, 32).await;
        let _ = (approval_result, input_result, transcript_result);

        let shutdown_result: Result<(), SessionShutdownError> = owner.shutdown().await;
        shutdown_result?;

        let load_options =
            SessionRuntimeOptions::new(KernelConfig::default_checked()?, bindings, runtime)?;
        let load_future = SessionRuntime::load(session_id, load_log, load_options);
        let load_result: Result<SessionRuntime, SessionOpenError> = load_future.await;
        let mut loaded = load_result?;
        let _loaded_events: SessionEventStream = loaded.take_events()?;
        let _loaded_page: TranscriptPage = loaded.handle().transcript(None, 32).await?;
        let loaded_shutdown: Result<(), SessionShutdownError> = loaded.shutdown().await;
        loaded_shutdown?;
        let _ = (tool, CompactionConfig::Disabled);
        Ok(())
    }

    fn compile_contract_function_item_reference() {
        let _ = compile_contract;
    }

    const _: fn() = compile_contract_function_item_reference;
}
