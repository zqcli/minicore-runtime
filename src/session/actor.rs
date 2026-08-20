use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::command::{CancelSlot, CloseCompletion, SessionCommand, SessionHandle};
use super::conversation::{
    ConversationError, ConversationHealth, ConversationLog, NewConversationEntry, StoredTurnOutcome,
};
use super::event::SessionEvent;
use super::event_stream::SessionObservation;
use super::snapshot::{SessionSnapshot, SnapshotHistory, TurnOutcome, TurnSummary, TurnTerminal};
use super::state::SessionStatus;
use super::store::StoredSessionConfig;
use crate::agent::{
    RetryPolicy, RunnerEvent, RunnerEventSink, TimestampSource, TurnContext,
    TurnContextDependencies, TurnFailure, TurnTaskResult, run_turn,
};
use crate::error::{PublicErrorCode, PublicErrorSummary, SessionError};
use crate::ids::{InteractionId, TurnId};
use crate::model::{
    ModelDescriptor, ModelEvent, ModelGateway, ModelSelection, ReasoningPreference, Usage,
};
use crate::prompt::{CompactionConfig, Compactor, PromptBuildOptions, PromptBuilder};
use crate::tools::{
    InteractionClient, InteractionReceiver, InteractionRequest, ToolError, ToolPolicy,
    ToolRegistry, UserAnswer, UserQuestion,
};
use crate::workspace::Workspace;

const MAX_CLOSE_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) struct SessionActorDependencies {
    pub(crate) model_gateway: ModelGateway,
    pub(crate) tool_registry: ToolRegistry,
    pub(crate) tool_policy: Arc<dyn ToolPolicy>,
    pub(crate) coding_instructions: Arc<str>,
    pub(crate) retry_policy: RetryPolicy,
    pub(crate) timestamp_source: TimestampSource,
    pub(crate) runtime: Handle,
    pub(crate) close_timeout: Duration,
    pub(crate) command_capacity: usize,
    pub(crate) event_capacity: usize,
    pub(crate) runner_event_capacity: usize,
}

struct SessionResources {
    prompt_builder: PromptBuilder,
    prompt_options: PromptBuildOptions,
    compactor: Compactor,
    enabled_tools: BTreeSet<crate::tools::ToolName>,
    max_tool_rounds: u8,
}

struct ActiveTurn {
    turn_id: TurnId,
    cancellation: CancellationToken,
    task: JoinHandle<TurnTaskResult>,
    events: mpsc::Receiver<RunnerEvent>,
    events_open: bool,
}

struct PendingInteraction {
    turn_id: TurnId,
    interaction_id: InteractionId,
    question: UserQuestion,
    request: Option<InteractionRequest>,
}

enum ActiveSignal {
    Close,
    Command(Option<SessionCommand>),
    Interaction(Option<InteractionRequest>),
    Event(Option<RunnerEvent>),
    Finished(Option<TurnTaskResult>),
}

pub(crate) struct SessionActor {
    config: StoredSessionConfig,
    resources: SessionResources,
    dependencies: Arc<SessionActorDependencies>,
    status: SessionStatus,
    active: Option<ActiveTurn>,
    pending: Option<PendingInteraction>,
    forced_failure: Option<TurnFailure>,
    unavailable: bool,
    commands: mpsc::Receiver<SessionCommand>,
    interactions: InteractionReceiver,
    interaction_client: InteractionClient,
    cancel_slot: Arc<CancelSlot>,
    close_requested: CancellationToken,
    close_complete: CloseCompletion,
    observation: SessionObservation,
    conversation: Arc<ConversationLog>,
    workspace: Arc<Workspace>,
    usage: Usage,
    conversation_seq: u64,
    last_error: Option<PublicErrorSummary>,
    last_terminal: Option<TurnTerminal>,
    interactions_open: bool,
}

impl SessionActorDependencies {
    fn validate(&self) -> Result<(), SessionError> {
        if self.close_timeout.is_zero() || self.close_timeout > MAX_CLOSE_TIMEOUT {
            return Err(SessionError::InvalidInput);
        }
        if !(1..=super::command::MAX_COMMAND_CAPACITY).contains(&self.command_capacity)
            || !(1..=super::event_stream::MAX_EVENT_CAPACITY).contains(&self.event_capacity)
            || !(1..=crate::agent::MAX_RUNNER_EVENT_CAPACITY).contains(&self.runner_event_capacity)
        {
            return Err(SessionError::InvalidInput);
        }
        Ok(())
    }
}

impl SessionActor {
    pub(crate) async fn new(
        config: StoredSessionConfig,
        conversation: Arc<ConversationLog>,
        workspace: Arc<Workspace>,
        dependencies: SessionActorDependencies,
    ) -> Result<(SessionHandle, Self), SessionError> {
        dependencies.validate()?;
        if conversation.session_id() != config.session_id() {
            return Err(SessionError::InvalidInput);
        }
        let stored = conversation.snapshot().await;
        if stored.health() == ConversationHealth::Degraded {
            return Err(SessionError::Unavailable);
        }
        let prompt_builder = PromptBuilder::new(
            config.system_prompt().to_owned(),
            dependencies.coding_instructions.to_string(),
        )
        .map_err(|_| SessionError::InvalidInput)?;
        let selection = config.model().selection().clone();
        let resolved = dependencies
            .model_gateway
            .resolve(&selection)
            .map_err(|_| SessionError::Unavailable)?;
        validate_descriptor(resolved.descriptor(), &selection)?;
        dependencies
            .tool_registry
            .specs(config.execution().enabled_tools())
            .map_err(|_| SessionError::InvalidInput)?;
        let compactor = Compactor::new(
            CompactionConfig::new(
                config.execution().compaction().trigger_tokens(),
                config.execution().compaction().target_tokens(),
            )
            .map_err(|_| SessionError::InvalidInput)?,
        );
        let latest_terminal = stored
            .latest_terminal()
            .map(|(turn_id, outcome)| terminal_from_stored(turn_id, outcome));
        let last_error = stored
            .has_failed_terminal()
            .then(|| PublicErrorSummary::with_retryable(PublicErrorCode::Internal, false));
        let usage = stored.usage();
        let initial = SessionSnapshot::new(
            config.session_id(),
            SessionStatus::Idle,
            None,
            None,
            usage,
            SnapshotHistory::new(last_error.clone(), latest_terminal.clone()),
            stored.max_seq(),
        )
        .map_err(|_| SessionError::InvalidInput)?;
        let observation = SessionObservation::new(initial, dependencies.event_capacity)?;
        let (commands, receiver) = mpsc::channel(dependencies.command_capacity);
        let (interaction_client, interactions) = InteractionClient::channel();
        let cancel_slot = Arc::new(CancelSlot::new());
        let close_requested = CancellationToken::new();
        let (close_complete, complete_receiver) = CloseCompletion::channel();
        let handle = SessionHandle::new_for_actor(
            commands,
            observation.clone(),
            Arc::clone(&cancel_slot),
            close_requested.clone(),
            complete_receiver,
        );
        let resources = SessionResources {
            prompt_builder,
            prompt_options: PromptBuildOptions::new(
                selection,
                *resolved.descriptor().limits(),
                ReasoningPreference::Auto,
            ),
            compactor,
            enabled_tools: config.execution().enabled_tools().clone(),
            max_tool_rounds: config.execution().max_tool_rounds(),
        };
        Ok((
            handle,
            Self {
                config,
                resources,
                dependencies: Arc::new(dependencies),
                status: SessionStatus::Idle,
                active: None,
                pending: None,
                forced_failure: None,
                unavailable: false,
                commands: receiver,
                interactions,
                interaction_client,
                cancel_slot,
                close_requested,
                close_complete,
                observation,
                conversation,
                workspace,
                usage,
                conversation_seq: stored.max_seq(),
                last_error,
                last_terminal: latest_terminal,
                interactions_open: true,
            },
        ))
    }

    pub(crate) async fn run(mut self) -> Result<(), SessionError> {
        loop {
            if self.status == SessionStatus::Closing {
                return self.close_session(None).await;
            }
            let step = match self.status {
                SessionStatus::Idle => {
                    let command = tokio::select! {
                        biased;
                        _ = self.close_requested.cancelled() => None,
                        command = self.commands.recv() => command,
                    };
                    match command {
                        Some(command) => self.handle_idle_command(command).await,
                        None => {
                            self.close_requested.cancel();
                            self.mark_closing()
                        }
                    }
                }
                SessionStatus::Running { .. } | SessionStatus::WaitingForInput { .. } => {
                    self.active_step().await
                }
                SessionStatus::Closing => Ok(()),
            };
            if let Err(error) = step {
                self.close_requested.cancel();
                let _ = self.mark_closing();
                return self.close_session(Some(error)).await;
            }
        }
    }

    async fn handle_idle_command(&mut self, command: SessionCommand) -> Result<(), SessionError> {
        match command {
            SessionCommand::Submit { input, reply } => {
                let result = self.handle_submit(input).await;
                let _ = reply.send(result);
            }
            SessionCommand::Answer { reply, .. } => {
                let _ = reply.send(Err(SessionError::InteractionMismatch));
            }
        }
        Ok(())
    }

    async fn active_step(&mut self) -> Result<(), SessionError> {
        let signal = {
            let active = self.active.as_mut().ok_or(SessionError::Internal)?;
            tokio::select! {
                biased;
                _ = self.close_requested.cancelled() => ActiveSignal::Close,
                result = &mut active.task => ActiveSignal::Finished(result.ok()),
                event = recv_runner_event(&mut active.events, active.events_open) => ActiveSignal::Event(event),
                request = self.interactions.recv(), if self.interactions_open => {
                    ActiveSignal::Interaction(request)
                }
                command = self.commands.recv() => ActiveSignal::Command(command),
            }
        };
        match signal {
            ActiveSignal::Close => self.mark_closing(),
            ActiveSignal::Command(Some(command)) => self.handle_active_command(command).await,
            ActiveSignal::Command(None) => {
                self.close_requested.cancel();
                self.mark_closing()
            }
            ActiveSignal::Interaction(Some(request)) => self.handle_interaction(request).await,
            ActiveSignal::Interaction(None) => {
                self.interactions_open = false;
                self.force_failure(TurnFailure::Internal);
                Ok(())
            }
            ActiveSignal::Event(Some(event)) => self.handle_runner_event(event).await,
            ActiveSignal::Event(None) => {
                if let Some(active) = self.active.as_mut() {
                    active.events_open = false;
                }
                Ok(())
            }
            ActiveSignal::Finished(result) => {
                self.drain_runner_events().await?;
                self.finish_active(result, false).await
            }
        }
    }

    async fn handle_active_command(&mut self, command: SessionCommand) -> Result<(), SessionError> {
        match command {
            SessionCommand::Submit { reply, .. } => {
                let _ = reply.send(Err(SessionError::Busy));
            }
            SessionCommand::Answer {
                interaction_id,
                answer,
                reply,
            } => {
                let result = if matches!(self.status, SessionStatus::WaitingForInput { .. }) {
                    self.handle_answer(interaction_id, answer).await
                } else {
                    Err(SessionError::InteractionMismatch)
                };
                let _ = reply.send(result);
            }
        }
        Ok(())
    }

    async fn handle_submit(&mut self, input: String) -> Result<TurnId, SessionError> {
        if self.unavailable {
            return Err(SessionError::Unavailable);
        }
        if self.conversation.snapshot().await.health() == ConversationHealth::Degraded {
            self.unavailable = true;
            return Err(SessionError::Unavailable);
        }
        if self.close_requested.is_cancelled() {
            return Err(SessionError::Closing);
        }
        let turn_id = TurnId::new().map_err(|_| SessionError::Internal)?;
        let cancellation = CancellationToken::new();
        let (events, runner_events) =
            RunnerEventSink::channel(self.dependencies.runner_event_capacity)
                .map_err(|_| SessionError::InvalidInput)?;
        let context = self.build_context(turn_id, cancellation.clone(), events)?;
        if self.close_requested.is_cancelled() {
            self.mark_closing()?;
            return Err(SessionError::Closing);
        }
        let timestamp =
            (self.dependencies.timestamp_source)().map_err(|_| SessionError::Internal)?;
        let entry = NewConversationEntry::User {
            turn_id,
            timestamp,
            text: input,
        };
        let conversation = Arc::clone(&self.conversation);
        let mut append = Box::pin(async move { conversation.append(entry).await });
        let (append_result, close_won) = tokio::select! {
            result = &mut append => (result, false),
            _ = self.close_requested.cancelled() => {
                self.mark_closing()?;
                (append.await, true)
            }
        };
        if let Err(error) = append_result {
            self.refresh_projection().await;
            return if close_won {
                Err(SessionError::Closing)
            } else {
                Err(map_conversation_error(error))
            };
        }
        self.refresh_projection().await;
        self.cancel_slot.install(turn_id, cancellation.clone());
        if close_won || self.close_requested.is_cancelled() {
            self.cancel_slot.clear(turn_id);
            cancellation.cancel();
            self.append_cancelled_before_spawn(turn_id).await;
            return Err(SessionError::Closing);
        }
        let task = self.dependencies.runtime.spawn(run_turn(context));
        self.active = Some(ActiveTurn {
            turn_id,
            cancellation,
            task,
            events: runner_events,
            events_open: true,
        });
        self.status = SessionStatus::Running { turn_id };
        self.publish_event(SessionEvent::TurnStarted { turn_id })?;
        Ok(turn_id)
    }

    fn build_context(
        &self,
        turn_id: TurnId,
        cancellation: CancellationToken,
        events: RunnerEventSink,
    ) -> Result<TurnContext, SessionError> {
        TurnContext::new(
            self.config.session_id(),
            turn_id,
            self.resources.enabled_tools.clone(),
            self.resources.max_tool_rounds,
            TurnContextDependencies {
                prompt_builder: self.resources.prompt_builder.clone(),
                prompt_options: self.resources.prompt_options.clone(),
                compactor: self.resources.compactor,
                gateway: self.dependencies.model_gateway.clone(),
                tools: self.dependencies.tool_registry.clone(),
                policy: Arc::clone(&self.dependencies.tool_policy),
                workspace: Arc::clone(&self.workspace),
                conversation: Arc::clone(&self.conversation),
                interactions: self.interaction_client.clone(),
                cancellation,
                timestamp_source: self.dependencies.timestamp_source,
                retry_policy: self.dependencies.retry_policy,
                events,
            },
        )
        .map_err(|error| match error {
            crate::agent::TurnContextError::ModelUnavailable => SessionError::Unavailable,
            crate::agent::TurnContextError::InvalidModelConfiguration
            | crate::agent::TurnContextError::UnknownTool
            | crate::agent::TurnContextError::InvalidToolRounds => SessionError::InvalidInput,
        })
    }

    async fn handle_interaction(
        &mut self,
        request: InteractionRequest,
    ) -> Result<(), SessionError> {
        let Some((active_turn_id, cancellation)) = self
            .active
            .as_ref()
            .map(|active| (active.turn_id, active.cancellation.clone()))
        else {
            let _ = request.reject(ToolError::Internal);
            return Ok(());
        };
        if request.turn_id() != active_turn_id || self.pending.is_some() {
            let _ = request.reject(ToolError::Internal);
            self.force_failure(TurnFailure::Internal);
            return Ok(());
        }
        let interaction_id = match InteractionId::new() {
            Ok(interaction_id) => interaction_id,
            Err(_) => {
                let _ = request.reject(ToolError::Internal);
                self.force_failure(TurnFailure::Internal);
                return Ok(());
            }
        };
        let question = match request.user_question(interaction_id) {
            Ok(question) => question,
            Err(error)
                if matches!(error, ToolError::InteractionClosed | ToolError::Cancelled)
                    && (self.close_requested.is_cancelled() || cancellation.is_cancelled()) =>
            {
                let _ = request.reject(ToolError::Cancelled);
                return Ok(());
            }
            Err(error) => {
                let _ = request.reject(error);
                self.force_failure(TurnFailure::Internal);
                return Ok(());
            }
        };
        self.refresh_projection().await;
        if self.close_requested.is_cancelled() || cancellation.is_cancelled() {
            let _ = request.reject(ToolError::Cancelled);
            return Ok(());
        }
        self.pending = Some(PendingInteraction {
            turn_id: active_turn_id,
            interaction_id,
            question: question.clone(),
            request: Some(request),
        });
        self.status = SessionStatus::WaitingForInput {
            turn_id: active_turn_id,
            interaction_id,
        };
        if let Err(error) = self.publish_event(SessionEvent::InputRequested {
            turn_id: active_turn_id,
            question,
        }) {
            self.reject_pending();
            self.force_failure(TurnFailure::Internal);
            return Err(error);
        }
        Ok(())
    }

    async fn handle_answer(
        &mut self,
        interaction_id: InteractionId,
        answer: UserAnswer,
    ) -> Result<(), SessionError> {
        let Some(pending) = self.pending.as_ref() else {
            return Err(SessionError::InteractionMismatch);
        };
        if pending.interaction_id != interaction_id {
            return Err(SessionError::InteractionMismatch);
        }
        let turn_id = pending.turn_id;
        let question = pending.question.clone();
        let cancellation = self
            .active
            .as_ref()
            .filter(|active| active.turn_id == turn_id)
            .map(|active| active.cancellation.clone())
            .ok_or(SessionError::InteractionMismatch)?;

        let response = {
            let cancel_slot_owner = Arc::clone(&self.cancel_slot);
            let cancel_slot = cancel_slot_owner
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let owns_turn = cancel_slot
                .as_ref()
                .is_some_and(|(current, _)| *current == turn_id);
            if !owns_turn || self.close_requested.is_cancelled() || cancellation.is_cancelled() {
                let close_requested = self.close_requested.is_cancelled();
                return Err(if close_requested {
                    SessionError::Closing
                } else {
                    SessionError::InteractionMismatch
                });
            }
            let mut pending = self.pending.take().ok_or(SessionError::Internal)?;
            let Some(request) = pending.request.take() else {
                self.pending = Some(pending);
                return Err(SessionError::InteractionMismatch);
            };
            let response = match request.claim_response() {
                Ok(response) => response,
                Err(_) => {
                    self.pending = Some(pending);
                    return Err(if self.close_requested.is_cancelled() {
                        SessionError::Closing
                    } else {
                        SessionError::InteractionMismatch
                    });
                }
            };
            self.pending = Some(pending);
            response
        };

        let timestamp = match (self.dependencies.timestamp_source)() {
            Ok(timestamp) => timestamp,
            Err(_) => {
                self.pending.take();
                let _ = response.reject(ToolError::Internal);
                self.force_failure(TurnFailure::Internal);
                return Err(SessionError::Internal);
            }
        };
        let entry = NewConversationEntry::Interaction {
            turn_id,
            timestamp,
            interaction_id,
            question,
            answer: answer.clone(),
        };
        let conversation = Arc::clone(&self.conversation);
        let append_result = conversation.append(entry).await;
        if let Err(error) = append_result {
            let close_requested = self.close_requested.is_cancelled();
            let cancelled = cancellation.is_cancelled();
            self.pending.take();
            let reject_error = if close_requested || cancelled {
                ToolError::Cancelled
            } else {
                ToolError::Internal
            };
            let _ = response.reject(reject_error);
            if close_requested {
                self.mark_closing()?;
                return Err(SessionError::Closing);
            }
            if cancelled {
                return Err(SessionError::InteractionMismatch);
            }
            self.force_failure(map_conversation_failure(error));
            return Err(SessionError::Internal);
        }
        self.refresh_projection().await;

        let cancel_slot_owner = Arc::clone(&self.cancel_slot);
        let cancel_slot = cancel_slot_owner
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let close_requested = self.close_requested.is_cancelled();
        let cancelled = cancellation.is_cancelled();
        if close_requested || cancelled {
            let response_result = response.respond(answer);
            if response_result.is_err() {
                self.force_failure(TurnFailure::Internal);
            }
            if close_requested && self.mark_closing().is_err() {
                self.force_failure(TurnFailure::Internal);
            }
            drop(cancel_slot);
            return Ok(());
        }

        self.pending.take();
        self.status = SessionStatus::Running { turn_id };
        if self.publish_snapshot().is_err() {
            let _ = response.respond(answer);
            self.force_failure(TurnFailure::Internal);
            drop(cancel_slot);
            return Ok(());
        }
        let response_result = response.respond(answer);
        drop(cancel_slot);
        if response_result.is_err() {
            self.force_failure(TurnFailure::Internal);
        }
        Ok(())
    }

    fn force_failure(&mut self, failure: TurnFailure) {
        self.forced_failure.get_or_insert(failure);
        if let Some(active) = self.active.as_ref() {
            active.cancellation.cancel();
        }
    }

    async fn handle_runner_event(&mut self, event: RunnerEvent) -> Result<(), SessionError> {
        let turn_id = self.active.as_ref().ok_or(SessionError::Internal)?.turn_id;
        let event = match event {
            RunnerEvent::Model(ModelEvent::TextDelta { delta }) => {
                SessionEvent::TextDelta { turn_id, delta }
            }
            RunnerEvent::Model(ModelEvent::ReasoningDelta { delta }) => {
                SessionEvent::ReasoningDelta { turn_id, delta }
            }
            RunnerEvent::ToolStarted(call) => {
                self.refresh_projection().await;
                SessionEvent::ToolStarted { turn_id, call }
            }
            RunnerEvent::ToolFinished(result) => {
                self.refresh_projection().await;
                SessionEvent::ToolFinished { turn_id, result }
            }
        };
        self.publish_event(event)
    }

    async fn drain_runner_events(&mut self) -> Result<(), SessionError> {
        loop {
            let events = {
                let Some(active) = self.active.as_mut() else {
                    return Err(SessionError::Internal);
                };
                let mut events = Vec::new();
                while let Ok(event) = active.events.try_recv() {
                    events.push(event);
                }
                events
            };
            if events.is_empty() {
                return Ok(());
            }
            for event in events {
                self.handle_runner_event(event).await?;
            }
        }
    }

    async fn finish_active(
        &mut self,
        result: Option<TurnTaskResult>,
        aborted: bool,
    ) -> Result<(), SessionError> {
        let turn_id = self.active.as_ref().ok_or(SessionError::Internal)?.turn_id;
        if self.close_requested.is_cancelled() && self.status != SessionStatus::Closing {
            self.mark_closing()?;
        }
        let forced = self.forced_failure.take();
        let (mut outcome, stored) = if let Some(failure) = forced {
            failure_outcome(failure)
        } else if aborted {
            (TurnOutcome::Cancelled, StoredTurnOutcome::Cancelled)
        } else {
            match result {
                Some(result) => task_outcome(result),
                None => failure_outcome(TurnFailure::Internal),
            }
        };
        let terminal_result = self.append_terminal(turn_id, stored).await;
        if terminal_result.is_err() {
            self.unavailable = true;
            outcome = TurnOutcome::Failed {
                error: PublicErrorSummary::with_retryable(PublicErrorCode::Internal, false),
            };
        }
        self.active.take();
        self.cancel_slot.clear(turn_id);
        self.reject_pending();
        self.refresh_projection().await;
        self.last_terminal = Some(TurnTerminal::new(turn_id, outcome.clone()));
        if let TurnOutcome::Failed { ref error } = outcome {
            self.last_error = Some(error.clone());
        }
        if self.status != SessionStatus::Closing {
            self.status = SessionStatus::Idle;
        }
        self.publish_event(SessionEvent::TurnFinished { turn_id, outcome })?;
        Ok(())
    }

    async fn append_terminal(&self, turn_id: TurnId, outcome: StoredTurnOutcome) -> Result<(), ()> {
        let timestamp = (self.dependencies.timestamp_source)().map_err(|_| ())?;
        self.conversation
            .append(NewConversationEntry::TurnTerminal {
                turn_id,
                timestamp,
                outcome,
            })
            .await
            .map(|_| ())
            .map_err(|_| ())
    }

    async fn append_cancelled_before_spawn(&mut self, turn_id: TurnId) {
        let appended = self
            .append_terminal(turn_id, StoredTurnOutcome::Cancelled)
            .await
            .is_ok();
        self.refresh_projection().await;
        if appended {
            self.last_terminal = Some(TurnTerminal::cancelled(turn_id));
        } else {
            self.unavailable = true;
            let error = PublicErrorSummary::with_retryable(PublicErrorCode::Internal, false);
            self.last_error = Some(error.clone());
            self.last_terminal = Some(TurnTerminal::new(turn_id, TurnOutcome::Failed { error }));
        }
    }

    fn reject_pending_request(&mut self, error: ToolError) {
        if let Some(pending) = self.pending.as_mut() {
            if let Some(request) = pending.request.take() {
                let _ = request.reject(error);
            }
        }
    }

    fn reject_pending(&mut self) {
        self.reject_pending_request(ToolError::Cancelled);
        self.pending.take();
    }

    fn mark_closing(&mut self) -> Result<(), SessionError> {
        if self.status == SessionStatus::Closing {
            return Ok(());
        }
        self.reject_pending();
        if let Some(active) = self.active.as_ref() {
            active.cancellation.cancel();
        }
        self.status = SessionStatus::Closing;
        self.publish_snapshot()
    }

    async fn close_session(
        &mut self,
        prior_error: Option<SessionError>,
    ) -> Result<(), SessionError> {
        let mut close_error = prior_error;
        if let Err(error) = self.mark_closing() {
            if close_error.is_none() {
                close_error = Some(error);
            }
        }
        if self.active.is_some() {
            let waited = tokio::time::timeout(
                self.dependencies.close_timeout,
                self.wait_active_during_close(),
            )
            .await;
            match waited {
                Ok(result) => {
                    if self.drain_runner_events().await.is_err() && close_error.is_none() {
                        close_error = Some(SessionError::Internal);
                        self.force_failure(TurnFailure::Internal);
                    }
                    if self.finish_active(result, false).await.is_err() && close_error.is_none() {
                        close_error = Some(SessionError::Internal);
                    }
                }
                Err(_) => {
                    let result = if let Some(active) = self.active.as_mut() {
                        active.task.abort();
                        (&mut active.task).await.ok()
                    } else {
                        None
                    };
                    self.conversation.wait_idle().await;
                    self.refresh_projection().await;
                    if self.drain_runner_events().await.is_err() && close_error.is_none() {
                        close_error = Some(SessionError::Internal);
                        self.force_failure(TurnFailure::Internal);
                    }
                    if self.finish_active(result, true).await.is_err() && close_error.is_none() {
                        close_error = Some(SessionError::Internal);
                    }
                }
            }
        }
        self.drain_commands();
        if self.conversation.close().await.is_err() && close_error.is_none() {
            close_error = Some(SessionError::Internal);
        }
        if self.workspace.shutdown().await.is_err() && close_error.is_none() {
            close_error = Some(SessionError::Internal);
        }
        match self.snapshot() {
            Ok(snapshot) => {
                if self.observation.close(snapshot).is_err() && close_error.is_none() {
                    close_error = Some(SessionError::Internal);
                }
            }
            Err(error) => {
                if close_error.is_none() {
                    close_error = Some(error);
                }
            }
        }
        let result = close_error.map_or(Ok(()), Err);
        self.close_complete.complete(result.clone());
        result
    }

    async fn wait_active_during_close(&mut self) -> Option<TurnTaskResult> {
        loop {
            let signal = {
                let active = self.active.as_mut()?;
                tokio::select! {
                    biased;
                    result = &mut active.task => return result.ok(),
                    event = recv_runner_event(&mut active.events, active.events_open) => ActiveSignal::Event(event),
                    request = self.interactions.recv(), if self.interactions_open => {
                        ActiveSignal::Interaction(request)
                    }
                }
            };
            match signal {
                ActiveSignal::Event(Some(event)) => {
                    if self.handle_runner_event(event).await.is_err() {
                        self.force_failure(TurnFailure::Internal);
                    }
                }
                ActiveSignal::Event(None) => {
                    if let Some(active) = self.active.as_mut() {
                        active.events_open = false;
                    }
                }
                ActiveSignal::Interaction(Some(request)) => {
                    let _ = request.reject(ToolError::Cancelled);
                }
                ActiveSignal::Interaction(None) => self.interactions_open = false,
                ActiveSignal::Command(_) | ActiveSignal::Close | ActiveSignal::Finished(_) => {}
            }
        }
    }

    fn drain_commands(&mut self) {
        while let Ok(command) = self.commands.try_recv() {
            match command {
                SessionCommand::Submit { reply, .. } => {
                    let _ = reply.send(Err(SessionError::Closing));
                }
                SessionCommand::Answer { reply, .. } => {
                    let _ = reply.send(Err(SessionError::Closing));
                }
            }
        }
    }

    async fn refresh_projection(&mut self) {
        let snapshot = self.conversation.snapshot().await;
        self.conversation_seq = snapshot.max_seq();
        self.usage = snapshot.usage();
        if snapshot.health() == ConversationHealth::Degraded {
            self.unavailable = true;
        }
    }

    fn snapshot(&self) -> Result<SessionSnapshot, SessionError> {
        SessionSnapshot::new(
            self.config.session_id(),
            self.status,
            self.active
                .as_ref()
                .map(|active| TurnSummary::new(active.turn_id)),
            self.pending
                .as_ref()
                .map(|pending| pending.question.clone()),
            self.usage,
            SnapshotHistory::new(self.last_error.clone(), self.last_terminal.clone()),
            self.conversation_seq,
        )
        .map_err(|_| SessionError::Internal)
    }

    fn publish_snapshot(&self) -> Result<(), SessionError> {
        let snapshot = self.snapshot()?;
        self.observation.publish_snapshot(snapshot)
    }

    fn publish_event(&self, event: SessionEvent) -> Result<(), SessionError> {
        let snapshot = self.snapshot()?;
        self.observation.publish(snapshot, Some(event))
    }
}

async fn recv_runner_event(
    events: &mut mpsc::Receiver<RunnerEvent>,
    events_open: bool,
) -> Option<RunnerEvent> {
    if events_open {
        events.recv().await
    } else {
        std::future::pending().await
    }
}

fn validate_descriptor(
    descriptor: &ModelDescriptor,
    selection: &ModelSelection,
) -> Result<(), SessionError> {
    if descriptor.selection() != selection
        || !descriptor.supports_reasoning(ReasoningPreference::Auto)
        || !descriptor.supports_reasoning(ReasoningPreference::Disabled)
    {
        return Err(SessionError::InvalidInput);
    }
    Ok(())
}

fn terminal_from_stored(turn_id: TurnId, outcome: StoredTurnOutcome) -> TurnTerminal {
    let outcome = match outcome {
        StoredTurnOutcome::Completed => TurnOutcome::Completed,
        StoredTurnOutcome::Cancelled | StoredTurnOutcome::CancelledByRestart => {
            TurnOutcome::Cancelled
        }
        StoredTurnOutcome::Failed => TurnOutcome::Failed {
            error: PublicErrorSummary::with_retryable(PublicErrorCode::Internal, false),
        },
    };
    TurnTerminal::new(turn_id, outcome)
}

fn task_outcome(result: TurnTaskResult) -> (TurnOutcome, StoredTurnOutcome) {
    match result {
        TurnTaskResult::Completed { .. } => (TurnOutcome::Completed, StoredTurnOutcome::Completed),
        TurnTaskResult::Cancelled { .. } => (TurnOutcome::Cancelled, StoredTurnOutcome::Cancelled),
        TurnTaskResult::Failed { failure, .. } => failure_outcome(failure),
    }
}

fn failure_outcome(failure: TurnFailure) -> (TurnOutcome, StoredTurnOutcome) {
    let error = match failure {
        TurnFailure::Model => {
            PublicErrorSummary::with_retryable(PublicErrorCode::Unavailable, true)
        }
        TurnFailure::ToolRoundLimit => {
            PublicErrorSummary::with_retryable(PublicErrorCode::InvalidInput, false)
        }
        TurnFailure::Conversation
        | TurnFailure::Compaction
        | TurnFailure::InvalidResponse
        | TurnFailure::Tool
        | TurnFailure::Timestamp
        | TurnFailure::Internal => {
            PublicErrorSummary::with_retryable(PublicErrorCode::Internal, false)
        }
    };
    (TurnOutcome::Failed { error }, StoredTurnOutcome::Failed)
}

fn map_conversation_error(error: ConversationError) -> SessionError {
    match error {
        ConversationError::InvalidEntry | ConversationError::InvalidPage => {
            SessionError::InvalidInput
        }
        ConversationError::Degraded
        | ConversationError::Io
        | ConversationError::WorkerFailed
        | ConversationError::IncompleteToolExchange => SessionError::Unavailable,
        ConversationError::Closing => SessionError::Closing,
        ConversationError::Busy => SessionError::Busy,
        ConversationError::Corrupt
        | ConversationError::CorruptAt { .. }
        | ConversationError::TooLarge
        | ConversationError::NotFound
        | ConversationError::Stale => SessionError::Internal,
    }
}

fn map_conversation_failure(error: ConversationError) -> TurnFailure {
    match error {
        ConversationError::Busy => TurnFailure::Conversation,
        _ => TurnFailure::Internal,
    }
}

const _: () = {
    let _ = std::mem::size_of::<SessionActorDependencies>();
    let _ = std::mem::size_of::<SessionActor>();
    let _ = SessionActor::new;
    let _ = SessionActor::run;
};

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, VecDeque};
    use std::fs;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use std::time::Duration;

    use super::*;
    use crate::agent::RetryPolicy;
    use crate::ids::{SessionId, TurnId};
    use crate::model::{
        AssistantPart, ModelCallContext, ModelDescriptor, ModelError, ModelEvent,
        ModelFinishReason, ModelFuture, ModelGateway, ModelLimits, ModelProvider, ModelRequest,
        ModelResponse, ModelSelection, ProviderId, ProviderRegistry, ReasoningPreference, Usage,
    };
    use crate::session::conversation::{
        ConversationEntry, ConversationLog, NewConversationEntry, StoredTurnOutcome,
    };
    use crate::session::store::{
        SessionStore, StoredCompactionConfig, StoredExecutionConfig, StoredModelConfig,
        StoredSessionConfig,
    };
    use crate::session::time::{Timestamp, TimestampError};
    use crate::tools::{AllowConfiguredTools, ToolRegistry};
    use crate::tools::{Tool, ToolContext, ToolFuture, ToolName, ToolOutput, ToolSpec};
    use crate::workspace::{Workspace, WorkspaceAccess};
    use serde_json::json;

    fn timestamp() -> Timestamp {
        "2026-08-19T12:34:56.789Z".parse().unwrap()
    }

    fn timestamp_source() -> Result<Timestamp, TimestampError> {
        Ok(timestamp())
    }

    #[test]
    fn request_close_cancels_close_and_exact_active_turn() {
        let slot = CancelSlot::new();
        let active = CancellationToken::new();
        let close_requested = CancellationToken::new();
        slot.install(TurnId::new().unwrap(), active.clone());
        slot.request_close(&close_requested);
        assert!(close_requested.is_cancelled());
        assert!(active.is_cancelled());
    }

    #[test]
    fn cancel_current_linearizes_clear_install_and_cancellation() {
        let slot = Arc::new(CancelSlot::new());
        let first_turn = TurnId::new().unwrap();
        let first_cancellation = CancellationToken::new();
        slot.install(first_turn, first_cancellation.clone());
        let guard = slot
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let cancel_slot = Arc::clone(&slot);
        let cancel_task = std::thread::spawn(move || {
            ready_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
            assert!(cancel_slot.cancel_current());
        });
        ready_receiver.recv().unwrap();
        let second_turn = TurnId::new().unwrap();
        let second_cancellation = CancellationToken::new();
        drop(guard);
        slot.clear(first_turn);
        slot.install(second_turn, second_cancellation.clone());
        release_sender.send(()).unwrap();
        cancel_task.join().unwrap();
        assert!(!first_cancellation.is_cancelled());
        assert!(second_cancellation.is_cancelled());
        slot.clear(second_turn);
        assert!(!slot.cancel_current());
    }

    static ANSWER_TIMESTAMP_CALLS: AtomicUsize = AtomicUsize::new(0);
    static ANSWER_STARTED: Mutex<Option<std::sync::mpsc::Sender<()>>> = Mutex::new(None);
    static ANSWER_RELEASE: Mutex<Option<std::sync::mpsc::Receiver<()>>> = Mutex::new(None);

    fn cancel_boundary_timestamp_source() -> Result<Timestamp, TimestampError> {
        if ANSWER_TIMESTAMP_CALLS.fetch_add(1, Ordering::SeqCst) + 1 == 3 {
            if let Some(sender) = ANSWER_STARTED
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
            {
                let _ = sender.send(());
            }
            let receiver = ANSWER_RELEASE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(receiver) = receiver {
                let _ = receiver.recv();
            }
        }
        Ok(timestamp())
    }

    struct NoopProvider {
        id: ProviderId,
        descriptor: ModelDescriptor,
    }

    enum ScriptedStep {
        Response {
            response: ModelResponse,
            event: Option<ModelEvent>,
        },
        Pending,
    }

    struct TestTool {
        name: ToolName,
        order: Arc<Mutex<Vec<String>>>,
    }

    struct AskPolicy;

    impl crate::tools::ToolPolicy for AskPolicy {
        fn decide(
            &self,
            _request: &crate::tools::ToolRequest<'_>,
            _ctx: &crate::tools::ToolContextView<'_>,
        ) -> crate::tools::ToolDecision {
            crate::tools::ToolDecision::ask("allow alpha?", None).unwrap()
        }
    }

    struct BlockingTool {
        name: ToolName,
    }

    impl Tool for BlockingTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(self.name.clone(), "blocking tool", json!({})).unwrap()
        }

        fn execute<'a>(
            &'a self,
            _ctx: ToolContext<'a>,
            _args: serde_json::Value,
        ) -> ToolFuture<'a> {
            Box::pin(async {
                std::future::pending::<Result<ToolOutput, crate::tools::ToolError>>().await
            })
        }
    }

    impl Tool for TestTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new(self.name.clone(), "test tool", json!({})).unwrap()
        }

        fn execute<'a>(
            &'a self,
            _ctx: ToolContext<'a>,
            _args: serde_json::Value,
        ) -> ToolFuture<'a> {
            let order = Arc::clone(&self.order);
            let name = self.name.to_string();
            Box::pin(async move {
                order
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(name);
                ToolOutput::success("ok").map_err(|_| crate::tools::ToolError::Internal)
            })
        }
    }

    struct ScriptedProvider {
        id: ProviderId,
        descriptor: ModelDescriptor,
        steps: Arc<Mutex<VecDeque<ScriptedStep>>>,
    }

    impl ModelProvider for ScriptedProvider {
        fn id(&self) -> &ProviderId {
            &self.id
        }

        fn models(&self) -> &[ModelDescriptor] {
            std::slice::from_ref(&self.descriptor)
        }

        fn generate(&self, _request: ModelRequest, ctx: ModelCallContext) -> ModelFuture<'_> {
            let step = self
                .steps
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop_front();
            Box::pin(async move {
                match step {
                    Some(ScriptedStep::Response { response, event }) => {
                        if let Some(event) = event {
                            let _ = ctx.publish(event);
                        }
                        Ok(response)
                    }
                    Some(ScriptedStep::Pending) => {
                        std::future::pending::<Result<ModelResponse, ModelError>>().await
                    }
                    None => Err(ModelError::Internal),
                }
            })
        }
    }

    impl ModelProvider for NoopProvider {
        fn id(&self) -> &ProviderId {
            &self.id
        }

        fn models(&self) -> &[ModelDescriptor] {
            std::slice::from_ref(&self.descriptor)
        }

        fn generate(&self, _request: ModelRequest, _ctx: ModelCallContext) -> ModelFuture<'_> {
            let future: Pin<Box<dyn Future<Output = Result<ModelResponse, ModelError>> + Send>> =
                Box::pin(async { Err(ModelError::Internal) });
            future
        }
    }

    fn dependencies(gateway: ModelGateway) -> SessionActorDependencies {
        SessionActorDependencies {
            model_gateway: gateway,
            tool_registry: ToolRegistry::builder().build(),
            tool_policy: Arc::new(AllowConfiguredTools::new()),
            coding_instructions: Arc::from("coding"),
            retry_policy: RetryPolicy::new(1, Duration::ZERO).unwrap(),
            timestamp_source,
            runtime: tokio::runtime::Handle::current(),
            close_timeout: Duration::from_secs(30),
            command_capacity: crate::config::DEFAULT_COMMAND_CAPACITY,
            event_capacity: 8,
            runner_event_capacity: 8,
        }
    }

    async fn opened() -> (
        SessionStore,
        Arc<ConversationLog>,
        Arc<Workspace>,
        StoredSessionConfig,
        std::path::PathBuf,
    ) {
        opened_with(BTreeSet::new()).await
    }

    async fn opened_with(
        enabled_tools: BTreeSet<ToolName>,
    ) -> (
        SessionStore,
        Arc<ConversationLog>,
        Arc<Workspace>,
        StoredSessionConfig,
        std::path::PathBuf,
    ) {
        let id = SessionId::new().unwrap();
        let root = std::env::temp_dir().join(format!("minicore-p6-session-{id}"));
        let workspace_root = root.join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let store = SessionStore::open(root.clone()).await.unwrap();
        let selection = ModelSelection::new("noop".parse().unwrap(), "model".parse().unwrap());
        let config = StoredSessionConfig::new(
            id,
            timestamp(),
            timestamp(),
            workspace_root.clone(),
            StoredModelConfig::new(selection.clone()),
            "system".to_owned(),
            StoredExecutionConfig::new(
                enabled_tools,
                StoredCompactionConfig::new(1_000, 999).unwrap(),
                4,
            )
            .unwrap(),
        )
        .unwrap();
        store.create(&config).await.unwrap();
        let log = Arc::new(ConversationLog::open(&store, id).await.unwrap());
        let workspace =
            Arc::new(Workspace::open(&workspace_root, WorkspaceAccess::ReadWrite).unwrap());
        (store, log, workspace, config, root)
    }

    fn gateway(selection: &ModelSelection) -> ModelGateway {
        gateway_with_reasoning(
            selection,
            BTreeSet::from([ReasoningPreference::Auto, ReasoningPreference::Disabled]),
        )
    }

    fn gateway_with_reasoning(
        selection: &ModelSelection,
        supported_reasoning: BTreeSet<ReasoningPreference>,
    ) -> ModelGateway {
        let descriptor = ModelDescriptor::new(
            selection.clone(),
            "noop-model",
            ModelLimits::default(),
            supported_reasoning,
        )
        .unwrap();
        let provider = NoopProvider {
            id: selection.provider_id().clone(),
            descriptor,
        };
        let mut providers = ProviderRegistry::builder();
        providers.register(provider).unwrap();
        ModelGateway::new(providers.build())
    }

    fn scripted_gateway(selection: &ModelSelection, steps: Vec<ScriptedStep>) -> ModelGateway {
        let descriptor = ModelDescriptor::new(
            selection.clone(),
            "scripted-model",
            ModelLimits::default(),
            BTreeSet::from([ReasoningPreference::Auto, ReasoningPreference::Disabled]),
        )
        .unwrap();
        let provider = ScriptedProvider {
            id: selection.provider_id().clone(),
            descriptor,
            steps: Arc::new(Mutex::new(steps.into_iter().collect())),
        };
        let mut providers = ProviderRegistry::builder();
        providers.register(provider).unwrap();
        ModelGateway::new(providers.build())
    }

    fn text_response(text: &str) -> ModelResponse {
        ModelResponse::new(
            vec![AssistantPart::Text(text.to_owned())],
            ModelFinishReason::Stop,
            Some(Usage::new(1, 2, 3)),
        )
        .unwrap()
    }

    fn tool_response(names: &[&str]) -> ModelResponse {
        let parts = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                AssistantPart::ToolCall(
                    crate::model::ToolCall::new(
                        crate::ids::ToolCallId::new(format!("call-{index}")).unwrap(),
                        (*name).parse().unwrap(),
                        json!({}),
                        index as u32,
                    )
                    .unwrap(),
                )
            })
            .collect();
        ModelResponse::new(parts, ModelFinishReason::ToolCalls, None).unwrap()
    }

    fn dependencies_with(
        gateway: ModelGateway,
        tool_registry: ToolRegistry,
        tool_policy: Arc<dyn crate::tools::ToolPolicy>,
    ) -> SessionActorDependencies {
        SessionActorDependencies {
            model_gateway: gateway,
            tool_registry,
            tool_policy,
            coding_instructions: Arc::from("coding"),
            retry_policy: RetryPolicy::new(1, Duration::ZERO).unwrap(),
            timestamp_source,
            runtime: tokio::runtime::Handle::current(),
            close_timeout: Duration::from_secs(30),
            command_capacity: crate::config::DEFAULT_COMMAND_CAPACITY,
            event_capacity: 8,
            runner_event_capacity: 8,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn new_rehydrates_usage_max_seq_and_latest_terminal() {
        let (store, log, workspace, config, root) = opened().await;
        let turn_id = TurnId::new().unwrap();
        log.append(NewConversationEntry::User {
            turn_id,
            timestamp: timestamp(),
            text: "question".to_owned(),
        })
        .await
        .unwrap();
        log.append(NewConversationEntry::Assistant {
            turn_id,
            timestamp: timestamp(),
            text: Some("answer".to_owned()),
            reasoning: None,
            tool_calls: Vec::new(),
            usage: Some(Usage::new(1, 2, 3)),
        })
        .await
        .unwrap();
        log.append(NewConversationEntry::TurnTerminal {
            turn_id,
            timestamp: timestamp(),
            outcome: StoredTurnOutcome::Completed,
        })
        .await
        .unwrap();
        let (handle, actor) = SessionActor::new(
            config.clone(),
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies(gateway(config.model().selection())),
        )
        .await
        .unwrap();
        let snapshot = handle.snapshot();
        assert_eq!(
            snapshot.status(),
            crate::session::state::SessionStatus::Idle
        );
        assert_eq!(snapshot.conversation_seq(), 3);
        assert_eq!(snapshot.usage(), &Usage::new(1, 2, 3));
        assert_eq!(snapshot.last_terminal().unwrap().turn_id, turn_id);
        assert_eq!(
            snapshot.last_terminal().unwrap().outcome,
            crate::session::snapshot::TurnOutcome::Completed
        );
        drop(actor);
        log.close().await.unwrap();
        workspace.shutdown().await.unwrap();
        store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn new_rejects_model_without_disabled_reasoning_support() {
        let (store, log, workspace, config, root) = opened().await;
        let result = SessionActor::new(
            config.clone(),
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies(gateway_with_reasoning(
                config.model().selection(),
                BTreeSet::from([ReasoningPreference::Auto]),
            )),
        )
        .await;
        assert!(matches!(
            result,
            Err(crate::error::SessionError::InvalidInput)
        ));
        log.close().await.unwrap();
        workspace.shutdown().await.unwrap();
        store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn model_only_submit_publishes_ordered_events_and_persists_terminal() {
        let (store, log, workspace, config, root) = opened().await;
        let gateway = scripted_gateway(
            config.model().selection(),
            vec![ScriptedStep::Response {
                response: text_response("done"),
                event: Some(ModelEvent::TextDelta {
                    delta: "delta".to_owned(),
                }),
            }],
        );
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies(gateway),
        )
        .await
        .unwrap();
        let mut stream = handle.subscribe().unwrap();
        assert!(
            matches!(stream.recv().await, Some(SessionEvent::Snapshot(snapshot)) if snapshot.status() == SessionStatus::Idle)
        );
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let turn_id = handle.submit("question".to_owned()).await.unwrap();
        assert_eq!(
            stream.recv().await,
            Some(SessionEvent::TurnStarted { turn_id })
        );
        assert_eq!(stream.snapshot().conversation_seq(), 1);
        assert_eq!(
            stream.recv().await,
            Some(SessionEvent::TextDelta {
                turn_id,
                delta: "delta".to_owned(),
            })
        );
        let finished = stream.recv().await.unwrap();
        assert!(
            matches!(finished, SessionEvent::TurnFinished { turn_id: finished_turn, outcome: TurnOutcome::Completed } if finished_turn == turn_id)
        );
        assert_eq!(handle.snapshot().status(), SessionStatus::Idle);
        assert_eq!(handle.snapshot().conversation_seq(), 3);
        assert_eq!(handle.snapshot().usage(), &Usage::new(1, 2, 3));
        assert_eq!(log.snapshot().await.entries().len(), 3);
        handle.close().await.unwrap();
        actor_task.await.unwrap().unwrap();
        assert_eq!(stream.recv().await, Some(SessionEvent::Closed));
        assert_eq!(stream.recv().await, None);
        store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_round_publishes_lifecycle_in_call_order_and_refreshes_sequence() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut registry_builder = ToolRegistry::builder();
        registry_builder
            .register(TestTool {
                name: "alpha".parse().unwrap(),
                order: Arc::clone(&order),
            })
            .unwrap();
        registry_builder
            .register(TestTool {
                name: "beta".parse().unwrap(),
                order: Arc::clone(&order),
            })
            .unwrap();
        let registry = registry_builder.build();
        let enabled = BTreeSet::from(["alpha".parse().unwrap(), "beta".parse().unwrap()]);
        let (store, log, workspace, config, root) = opened_with(enabled.clone()).await;
        let gateway = scripted_gateway(
            config.model().selection(),
            vec![
                ScriptedStep::Response {
                    response: tool_response(&["alpha", "beta"]),
                    event: None,
                },
                ScriptedStep::Response {
                    response: text_response("finished"),
                    event: None,
                },
            ],
        );
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies_with(gateway, registry, Arc::new(AllowConfiguredTools::new())),
        )
        .await
        .unwrap();
        let mut stream = handle.subscribe().unwrap();
        let _ = stream.recv().await;
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let turn_id = handle.submit("question".to_owned()).await.unwrap();
        assert_eq!(
            stream.recv().await,
            Some(SessionEvent::TurnStarted { turn_id })
        );
        let mut lifecycle = Vec::new();
        loop {
            match stream.recv().await.unwrap() {
                SessionEvent::ToolStarted { call, .. } => {
                    lifecycle.push(format!("started:{}", call.tool_name()))
                }
                SessionEvent::ToolFinished { result, .. } => {
                    lifecycle.push(format!("finished:{:?}", result.status()));
                    assert!(handle.snapshot().conversation_seq() >= 3);
                }
                SessionEvent::TurnFinished { outcome, .. } => {
                    assert_eq!(outcome, TurnOutcome::Completed);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(
            *order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec!["alpha", "beta"]
        );
        assert_eq!(
            lifecycle,
            vec![
                "started:alpha",
                "finished:Succeeded",
                "started:beta",
                "finished:Succeeded",
            ]
        );
        assert_eq!(log.snapshot().await.entries().len(), 6);
        assert_eq!(handle.snapshot().conversation_seq(), 6);
        handle.close().await.unwrap();
        actor_task.await.unwrap().unwrap();
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ask_user_waits_persists_interaction_before_resume_and_rejects_stale_answers() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut registry_builder = ToolRegistry::builder();
        registry_builder
            .register(TestTool {
                name: "alpha".parse().unwrap(),
                order: Arc::clone(&order),
            })
            .unwrap();
        let registry = registry_builder.build();
        let enabled = BTreeSet::from(["alpha".parse().unwrap()]);
        let (store, log, workspace, config, root) = opened_with(enabled).await;
        let gateway = scripted_gateway(
            config.model().selection(),
            vec![
                ScriptedStep::Response {
                    response: tool_response(&["alpha"]),
                    event: None,
                },
                ScriptedStep::Response {
                    response: text_response("approved"),
                    event: None,
                },
            ],
        );
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies_with(gateway, registry, Arc::new(AskPolicy)),
        )
        .await
        .unwrap();
        let mut stream = handle.subscribe().unwrap();
        let _ = stream.recv().await;
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let turn_id = handle.submit("question".to_owned()).await.unwrap();
        assert_eq!(
            stream.recv().await,
            Some(SessionEvent::TurnStarted { turn_id })
        );
        let question = loop {
            match stream.recv().await.unwrap() {
                SessionEvent::InputRequested {
                    turn_id: event_turn,
                    question,
                } if event_turn == turn_id => break question,
                _ => {}
            }
        };
        let interaction_id = question.interaction_id();
        assert_eq!(stream.snapshot().conversation_seq(), 2);
        assert!(matches!(
            handle.snapshot().status(),
            SessionStatus::WaitingForInput {
                turn_id: waiting_turn,
                interaction_id: waiting_interaction,
            } if waiting_turn == turn_id && waiting_interaction == interaction_id
        ));
        let wrong = crate::ids::InteractionId::new().unwrap();
        let wrong_answer = crate::tools::UserAnswer::new("allow").unwrap();
        assert_eq!(
            handle.answer(wrong, wrong_answer).await,
            Err(crate::error::SessionError::InteractionMismatch)
        );
        assert!(matches!(
            handle.snapshot().status(),
            SessionStatus::WaitingForInput { interaction_id: current, .. } if current == interaction_id
        ));
        handle
            .answer(
                interaction_id,
                crate::tools::UserAnswer::new("allow").unwrap(),
            )
            .await
            .unwrap();
        assert!(handle.snapshot().conversation_seq() >= 3);
        loop {
            match stream.recv().await.unwrap() {
                SessionEvent::ToolStarted { .. } => {
                    assert!(handle.snapshot().conversation_seq() >= 3);
                    match handle.snapshot().status() {
                        SessionStatus::Idle | SessionStatus::Closing => {}
                        SessionStatus::Running { turn_id: current } => {
                            assert_eq!(current, turn_id)
                        }
                        SessionStatus::WaitingForInput {
                            interaction_id: current,
                            ..
                        } => assert_eq!(current, interaction_id),
                    }
                }
                SessionEvent::TurnFinished { outcome, .. } => {
                    assert_eq!(outcome, TurnOutcome::Completed);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(
            *order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec!["alpha"]
        );
        let snapshot = log.snapshot().await;
        assert_eq!(snapshot.entries().len(), 6);
        let interaction = serde_json::to_value(&*snapshot.entries()[2]).unwrap();
        assert_eq!(interaction["type"], "interaction");
        assert_eq!(interaction["interaction_id"], interaction_id.to_string());
        assert_eq!(interaction["answer"]["text"], "allow");
        handle.close().await.unwrap();
        actor_task.await.unwrap().unwrap();
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_before_answer_rejects_without_persisting_interaction() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut registry_builder = ToolRegistry::builder();
        registry_builder
            .register(TestTool {
                name: "alpha".parse().unwrap(),
                order: Arc::clone(&order),
            })
            .unwrap();
        let (store, log, workspace, config, root) =
            opened_with(BTreeSet::from(["alpha".parse().unwrap()])).await;
        let gateway = scripted_gateway(
            config.model().selection(),
            vec![ScriptedStep::Response {
                response: tool_response(&["alpha"]),
                event: None,
            }],
        );
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies_with(gateway, registry_builder.build(), Arc::new(AskPolicy)),
        )
        .await
        .unwrap();
        let mut stream = handle.subscribe().unwrap();
        let _ = stream.recv().await;
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let turn_id = handle.submit("question".to_owned()).await.unwrap();
        let interaction_id = loop {
            if let SessionEvent::InputRequested { question, .. } = stream.recv().await.unwrap() {
                break question.interaction_id();
            }
        };
        handle.cancel().unwrap();
        assert_eq!(
            handle
                .answer(
                    interaction_id,
                    crate::tools::UserAnswer::new("allow").unwrap(),
                )
                .await,
            Err(crate::error::SessionError::InteractionMismatch)
        );
        assert!(matches!(
            handle.snapshot().status(),
            SessionStatus::WaitingForInput { turn_id: current, interaction_id: current_id }
                if current == turn_id && current_id == interaction_id
        ));
        let before_terminal = log.snapshot().await;
        assert!(before_terminal.entries().len() >= 2);
        assert!(
            !before_terminal
                .entries()
                .iter()
                .any(|entry| matches!(entry.as_ref(), ConversationEntry::Interaction { .. }))
        );
        assert!(matches!(
            before_terminal.entries()[0].as_ref(),
            ConversationEntry::User { turn_id: current, .. } if *current == turn_id
        ));
        assert!(matches!(
            before_terminal.entries()[1].as_ref(),
            ConversationEntry::Assistant { turn_id: current, tool_calls, .. }
                if *current == turn_id && tool_calls.len() == 1
        ));
        for entry in before_terminal.entries().iter().skip(2) {
            assert!(
                matches!(
                    entry.as_ref(),
                    ConversationEntry::ToolResult { turn_id: current, result, .. }
                        if *current == turn_id && result.text() == "cancelled" && result.is_error()
                ) || matches!(
                    entry.as_ref(),
                    ConversationEntry::TurnTerminal {
                        turn_id: current,
                        outcome: StoredTurnOutcome::Cancelled,
                        ..
                    } if *current == turn_id
                )
            );
        }
        loop {
            if let SessionEvent::TurnFinished {
                outcome: TurnOutcome::Cancelled,
                ..
            } = stream.recv().await.unwrap()
            {
                break;
            }
        }
        let final_snapshot = log.snapshot().await;
        assert_eq!(final_snapshot.entries().len(), 4);
        assert!(
            !final_snapshot
                .entries()
                .iter()
                .any(|entry| matches!(entry.as_ref(), ConversationEntry::Interaction { .. }))
        );
        assert!(matches!(
            final_snapshot.entries()[0].as_ref(),
            ConversationEntry::User { turn_id: current, .. } if *current == turn_id
        ));
        assert!(matches!(
            final_snapshot.entries()[1].as_ref(),
            ConversationEntry::Assistant { turn_id: current, tool_calls, .. }
                if *current == turn_id && tool_calls.len() == 1
        ));
        assert!(matches!(
            final_snapshot.entries()[2].as_ref(),
            ConversationEntry::ToolResult { turn_id: current, result, .. }
                if *current == turn_id && result.text() == "cancelled" && result.is_error()
        ));
        assert!(matches!(
            final_snapshot.entries()[3].as_ref(),
            ConversationEntry::TurnTerminal {
                turn_id: current,
                outcome: StoredTurnOutcome::Cancelled,
                ..
            } if *current == turn_id
        ));
        handle.close().await.unwrap();
        actor_task.await.unwrap().unwrap();
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_first_rejects_answer_without_resuming_tool_work() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut registry_builder = ToolRegistry::builder();
        registry_builder
            .register(TestTool {
                name: "alpha".parse().unwrap(),
                order: Arc::clone(&order),
            })
            .unwrap();
        let (store, log, workspace, config, root) =
            opened_with(BTreeSet::from(["alpha".parse().unwrap()])).await;
        let gateway = scripted_gateway(
            config.model().selection(),
            vec![ScriptedStep::Response {
                response: tool_response(&["alpha"]),
                event: None,
            }],
        );
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies_with(gateway, registry_builder.build(), Arc::new(AskPolicy)),
        )
        .await
        .unwrap();
        let mut stream = handle.subscribe().unwrap();
        let _ = stream.recv().await;
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let turn_id = handle.submit("question".to_owned()).await.unwrap();
        let interaction_id = loop {
            if let SessionEvent::InputRequested { question, .. } = stream.recv().await.unwrap() {
                break question.interaction_id();
            }
        };
        let waker = futures_util::task::noop_waker();
        let mut close = Box::pin(handle.close());
        let mut context = Context::from_waker(&waker);
        assert!(matches!(close.as_mut().poll(&mut context), Poll::Pending));
        assert_eq!(
            handle
                .answer(
                    interaction_id,
                    crate::tools::UserAnswer::new("allow").unwrap(),
                )
                .await,
            Err(crate::error::SessionError::Closing)
        );
        close.await.unwrap();
        assert!(
            order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
        );
        assert_eq!(
            log.snapshot()
                .await
                .entries()
                .iter()
                .filter(|entry| matches!(entry.as_ref(), ConversationEntry::Interaction { .. }))
                .count(),
            0
        );
        assert_eq!(handle.snapshot().last_terminal().unwrap().turn_id, turn_id);
        actor_task.await.unwrap().unwrap();
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn answer_first_resumes_then_close_cancels_exact_runner() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut registry_builder = ToolRegistry::builder();
        registry_builder
            .register(TestTool {
                name: "alpha".parse().unwrap(),
                order: Arc::clone(&order),
            })
            .unwrap();
        let (store, log, workspace, config, root) =
            opened_with(BTreeSet::from(["alpha".parse().unwrap()])).await;
        let gateway = scripted_gateway(
            config.model().selection(),
            vec![
                ScriptedStep::Response {
                    response: tool_response(&["alpha"]),
                    event: None,
                },
                ScriptedStep::Pending,
            ],
        );
        let mut dependencies =
            dependencies_with(gateway, registry_builder.build(), Arc::new(AskPolicy));
        dependencies.close_timeout = Duration::from_secs(1);
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies,
        )
        .await
        .unwrap();
        let mut stream = handle.subscribe().unwrap();
        let _ = stream.recv().await;
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let turn_id = handle.submit("question".to_owned()).await.unwrap();
        let interaction_id = loop {
            if let SessionEvent::InputRequested { question, .. } = stream.recv().await.unwrap() {
                break question.interaction_id();
            }
        };
        handle
            .answer(
                interaction_id,
                crate::tools::UserAnswer::new("allow").unwrap(),
            )
            .await
            .unwrap();
        loop {
            if matches!(
                stream.recv().await.unwrap(),
                SessionEvent::ToolFinished { .. }
            ) {
                break;
            }
        }
        let close_task = tokio::runtime::Handle::current().spawn({
            let handle = handle.clone();
            async move { handle.close().await }
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(close_task.await.unwrap(), Ok(()));
        assert!(matches!(
            stream.recv().await,
            Some(SessionEvent::TurnFinished {
                turn_id: finished,
                outcome: TurnOutcome::Cancelled,
            }) if finished == turn_id
        ));
        assert_eq!(
            order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_slice(),
            ["alpha"]
        );
        let snapshot = log.snapshot().await;
        assert_eq!(snapshot.entries().len(), 5);
        assert!(
            snapshot
                .entries()
                .iter()
                .any(|entry| { matches!(entry.as_ref(), ConversationEntry::TurnTerminal { .. }) })
        );
        actor_task.await.unwrap().unwrap();
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_during_claimed_interaction_append_preserves_answer() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut registry_builder = ToolRegistry::builder();
        registry_builder
            .register(TestTool {
                name: "alpha".parse().unwrap(),
                order: Arc::clone(&order),
            })
            .unwrap();
        let (store, log, workspace, config, root) =
            opened_with(BTreeSet::from(["alpha".parse().unwrap()])).await;
        let gateway = scripted_gateway(
            config.model().selection(),
            vec![ScriptedStep::Response {
                response: tool_response(&["alpha"]),
                event: None,
            }],
        );
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies_with(gateway, registry_builder.build(), Arc::new(AskPolicy)),
        )
        .await
        .unwrap();
        let mut stream = handle.subscribe().unwrap();
        let _ = stream.recv().await;
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let turn_id = handle.submit("question".to_owned()).await.unwrap();
        let interaction_id = loop {
            if let SessionEvent::InputRequested { question, .. } = stream.recv().await.unwrap() {
                break question.interaction_id();
            }
        };
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let blocker = store.run_io(move || {
            started_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
            Ok::<_, crate::session::store::StoreError>(())
        });
        started_receiver.recv().unwrap();
        let answer_task = tokio::runtime::Handle::current().spawn({
            let handle = handle.clone();
            async move {
                handle
                    .answer(
                        interaction_id,
                        crate::tools::UserAnswer::new("allow").unwrap(),
                    )
                    .await
            }
        });
        crate::session::conversation::wait_until_busy_for_test(&log).await;
        assert_eq!(handle.cancel(), Ok(()));
        release_sender.send(()).unwrap();
        SessionStore::await_io(blocker).await.unwrap();
        assert_eq!(answer_task.await.unwrap(), Ok(()));
        loop {
            if matches!(
                stream.recv().await.unwrap(),
                SessionEvent::ToolFinished { .. }
            ) {
                break;
            }
        }
        assert!(matches!(
            stream.recv().await,
            Some(SessionEvent::TurnFinished {
                turn_id: finished,
                outcome: TurnOutcome::Cancelled,
            }) if finished == turn_id
        ));
        assert!(
            order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
        );
        let snapshot = log.snapshot().await;
        assert_eq!(snapshot.entries().len(), 5);
        assert_eq!(
            snapshot
                .entries()
                .iter()
                .filter(|entry| matches!(entry.as_ref(), ConversationEntry::Interaction { .. }))
                .count(),
            1
        );
        handle.close().await.unwrap();
        actor_task.await.unwrap().unwrap();
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_during_claimed_interaction_append_preserves_answer() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut registry_builder = ToolRegistry::builder();
        registry_builder
            .register(TestTool {
                name: "alpha".parse().unwrap(),
                order: Arc::clone(&order),
            })
            .unwrap();
        let (store, log, workspace, config, root) =
            opened_with(BTreeSet::from(["alpha".parse().unwrap()])).await;
        let gateway = scripted_gateway(
            config.model().selection(),
            vec![ScriptedStep::Response {
                response: tool_response(&["alpha"]),
                event: None,
            }],
        );
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies_with(gateway, registry_builder.build(), Arc::new(AskPolicy)),
        )
        .await
        .unwrap();
        let mut stream = handle.subscribe().unwrap();
        let _ = stream.recv().await;
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let turn_id = handle.submit("question".to_owned()).await.unwrap();
        let interaction_id = loop {
            if let SessionEvent::InputRequested { question, .. } = stream.recv().await.unwrap() {
                break question.interaction_id();
            }
        };
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let blocker = store.run_io(move || {
            started_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
            Ok::<_, crate::session::store::StoreError>(())
        });
        started_receiver.recv().unwrap();
        let answer_task = tokio::runtime::Handle::current().spawn({
            let handle = handle.clone();
            async move {
                handle
                    .answer(
                        interaction_id,
                        crate::tools::UserAnswer::new("allow").unwrap(),
                    )
                    .await
            }
        });
        crate::session::conversation::wait_until_busy_for_test(&log).await;
        let close_task = tokio::runtime::Handle::current().spawn({
            let handle = handle.clone();
            async move { handle.close().await }
        });
        tokio::task::yield_now().await;
        release_sender.send(()).unwrap();
        SessionStore::await_io(blocker).await.unwrap();
        assert_eq!(answer_task.await.unwrap(), Ok(()));
        assert_eq!(close_task.await.unwrap(), Ok(()));
        assert!(
            order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
        );
        let snapshot = log.snapshot().await;
        assert_eq!(snapshot.entries().len(), 5);
        assert_eq!(
            snapshot
                .entries()
                .iter()
                .filter(|entry| matches!(entry.as_ref(), ConversationEntry::Interaction { .. }))
                .count(),
            1
        );
        assert!(matches!(
            snapshot.entries().last().unwrap().as_ref(),
            ConversationEntry::TurnTerminal {
                turn_id: finished,
                outcome: StoredTurnOutcome::Cancelled,
                ..
            } if *finished == turn_id
        ));
        actor_task.await.unwrap().unwrap();
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_interaction_append_rejects_claimed_response_without_hanging() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut registry_builder = ToolRegistry::builder();
        registry_builder
            .register(TestTool {
                name: "alpha".parse().unwrap(),
                order: Arc::clone(&order),
            })
            .unwrap();
        let (store, log, workspace, config, root) =
            opened_with(BTreeSet::from(["alpha".parse().unwrap()])).await;
        let gateway = scripted_gateway(
            config.model().selection(),
            vec![ScriptedStep::Response {
                response: tool_response(&["alpha"]),
                event: None,
            }],
        );
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies_with(gateway, registry_builder.build(), Arc::new(AskPolicy)),
        )
        .await
        .unwrap();
        let mut stream = handle.subscribe().unwrap();
        let _ = stream.recv().await;
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let _turn_id = handle.submit("question".to_owned()).await.unwrap();
        let interaction_id = loop {
            if let SessionEvent::InputRequested { question, .. } = stream.recv().await.unwrap() {
                break question.interaction_id();
            }
        };
        log.close().await.unwrap();
        assert_eq!(
            handle
                .answer(
                    interaction_id,
                    crate::tools::UserAnswer::new("allow").unwrap(),
                )
                .await,
            Err(crate::error::SessionError::Internal)
        );
        assert!(matches!(
            stream.recv().await,
            Some(SessionEvent::TurnFinished {
                outcome: TurnOutcome::Failed { error },
                ..
            }) if error.code() == PublicErrorCode::Internal
        ));
        assert!(
            order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
        );
        assert_eq!(log.snapshot().await.entries().len(), 2);
        handle.close().await.unwrap();
        actor_task.await.unwrap().unwrap();
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_after_claim_before_append_preserves_accepted_answer() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut registry_builder = ToolRegistry::builder();
        registry_builder
            .register(TestTool {
                name: "alpha".parse().unwrap(),
                order: Arc::clone(&order),
            })
            .unwrap();
        let (store, log, workspace, config, root) =
            opened_with(BTreeSet::from(["alpha".parse().unwrap()])).await;
        let gateway = scripted_gateway(
            config.model().selection(),
            vec![ScriptedStep::Response {
                response: tool_response(&["alpha"]),
                event: None,
            }],
        );
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        ANSWER_TIMESTAMP_CALLS.store(0, Ordering::SeqCst);
        *ANSWER_STARTED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(started_sender);
        *ANSWER_RELEASE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(release_receiver);
        let mut dependencies =
            dependencies_with(gateway, registry_builder.build(), Arc::new(AskPolicy));
        dependencies.timestamp_source = cancel_boundary_timestamp_source;
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies,
        )
        .await
        .unwrap();
        let mut stream = handle.subscribe().unwrap();
        let _ = stream.recv().await;
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let _turn_id = handle.submit("question".to_owned()).await.unwrap();
        let interaction_id = loop {
            if let SessionEvent::InputRequested { question, .. } = stream.recv().await.unwrap() {
                break question.interaction_id();
            }
        };
        let answer_handle = handle.clone();
        let answer_task = tokio::runtime::Handle::current().spawn(async move {
            answer_handle
                .answer(
                    interaction_id,
                    crate::tools::UserAnswer::new("allow").unwrap(),
                )
                .await
        });
        tokio::task::yield_now().await;
        started_receiver.recv().unwrap();
        handle.cancel().unwrap();
        release_sender.send(()).unwrap();
        *ANSWER_STARTED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        assert_eq!(answer_task.await.unwrap(), Ok(()));
        loop {
            if matches!(
                stream.recv().await.unwrap(),
                SessionEvent::TurnFinished { .. }
            ) {
                break;
            }
        }
        assert_eq!(
            *order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            Vec::<String>::new()
        );
        let snapshot = log.snapshot().await;
        assert_eq!(snapshot.entries().len(), 5);
        assert!(
            snapshot
                .entries()
                .iter()
                .any(|entry| matches!(entry.as_ref(), ConversationEntry::Interaction { .. }))
        );
        handle.close().await.unwrap();
        actor_task.await.unwrap().unwrap();
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dequeued_cancelled_question_does_not_force_failed_terminal() {
        let (store, log, workspace, config, root) = opened().await;
        let (handle, mut actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies(gateway(&ModelSelection::new(
                "noop".parse().unwrap(),
                "model".parse().unwrap(),
            ))),
        )
        .await
        .unwrap();
        let turn_id = TurnId::new().unwrap();
        log.append(NewConversationEntry::User {
            turn_id,
            timestamp: timestamp(),
            text: "question".to_owned(),
        })
        .await
        .unwrap();
        let cancellation = CancellationToken::new();
        let (_events, runner_events) = RunnerEventSink::channel(8).unwrap();
        let runner_task = tokio::runtime::Handle::current()
            .spawn(async { std::future::pending::<TurnTaskResult>().await });
        actor.active = Some(ActiveTurn {
            turn_id,
            cancellation: cancellation.clone(),
            task: runner_task,
            events: runner_events,
            events_open: true,
        });
        actor.cancel_slot.install(turn_id, cancellation.clone());
        let interaction_client = actor.interaction_client.clone();
        let ask_cancellation = cancellation.clone();
        let ask_task = tokio::runtime::Handle::current().spawn(async move {
            interaction_client
                .ask_user(turn_id, "question", None, ask_cancellation)
                .await
        });
        let request = actor.interactions.recv().await.unwrap();
        cancellation.cancel();
        assert_eq!(ask_task.await.unwrap(), Err(ToolError::Cancelled));
        assert_eq!(actor.handle_interaction(request).await, Ok(()));
        assert!(actor.forced_failure.is_none());
        if let Some(active) = actor.active.as_mut() {
            active.task.abort();
            let _ = (&mut active.task).await;
        }
        let result = actor
            .finish_active(
                Some(TurnTaskResult::Cancelled {
                    usage: Usage::default(),
                }),
                false,
            )
            .await;
        assert_eq!(result, Ok(()));
        assert!(matches!(
            log.snapshot().await.entries().last().unwrap().as_ref(),
            ConversationEntry::TurnTerminal {
                turn_id: terminal_turn,
                outcome: StoredTurnOutcome::Cancelled,
                ..
            } if *terminal_turn == turn_id
        ));
        drop(handle);
        log.close().await.unwrap();
        workspace.shutdown().await.unwrap();
        store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn submit_validates_user_text_before_mailbox_full() {
        let (store, log, workspace, config, root) = opened().await;
        let gateway = scripted_gateway(config.model().selection(), Vec::new());
        let mut dependencies = dependencies(gateway);
        dependencies.command_capacity = 1;
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies,
        )
        .await
        .unwrap();
        let mut queued = Box::pin(handle.submit("queued".to_owned()));
        let waker = futures_util::task::noop_waker_ref();
        let mut context = Context::from_waker(waker);
        assert!(matches!(queued.as_mut().poll(&mut context), Poll::Pending));
        for invalid in [String::new(), "bad\u{0001}".to_owned(), "x".repeat(262_145)] {
            assert_eq!(
                handle.submit(invalid).await,
                Err(crate::error::SessionError::InvalidInput)
            );
        }
        drop(queued);
        let exact = "x".repeat(262_144);
        let mut accepted = Box::pin(handle.submit(exact));
        assert!(matches!(
            accepted.as_mut().poll(&mut context),
            Poll::Ready(Err(crate::error::SessionError::Busy))
        ));
        drop(accepted);
        drop(actor);
        log.close().await.unwrap();
        workspace.shutdown().await.unwrap();
        store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exact_limit_user_text_is_admitted_before_actor_runs() {
        let (store, log, workspace, config, root) = opened().await;
        let gateway = scripted_gateway(config.model().selection(), Vec::new());
        let mut dependencies = dependencies(gateway);
        dependencies.command_capacity = 1;
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies,
        )
        .await
        .unwrap();
        let mut exact = Box::pin(handle.submit("x".repeat(262_144)));
        let waker = futures_util::task::noop_waker_ref();
        let mut context = Context::from_waker(waker);
        assert!(matches!(exact.as_mut().poll(&mut context), Poll::Pending));
        drop(exact);
        drop(actor);
        log.close().await.unwrap();
        workspace.shutdown().await.unwrap();
        store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_is_out_of_band_and_status_waits_for_model_settlement() {
        let (store, log, workspace, config, root) = opened().await;
        let gateway = scripted_gateway(config.model().selection(), vec![ScriptedStep::Pending]);
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies(gateway),
        )
        .await
        .unwrap();
        let mut stream = handle.subscribe().unwrap();
        let _ = stream.recv().await;
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let turn_id = handle.submit("question".to_owned()).await.unwrap();
        assert_eq!(
            stream.recv().await,
            Some(SessionEvent::TurnStarted { turn_id })
        );
        assert_eq!(handle.cancel(), Ok(()));
        assert_eq!(
            handle.snapshot().status(),
            SessionStatus::Running { turn_id }
        );
        assert!(matches!(
            stream.recv().await,
            Some(SessionEvent::TurnFinished {
                turn_id: finished_turn,
                outcome: TurnOutcome::Cancelled,
            }) if finished_turn == turn_id
        ));
        assert_eq!(handle.snapshot().status(), SessionStatus::Idle);
        assert_eq!(log.snapshot().await.entries().len(), 2);
        handle.close().await.unwrap();
        actor_task.await.unwrap().unwrap();
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_waiting_input_preserves_waiting_until_cancelled_terminal() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut registry_builder = ToolRegistry::builder();
        registry_builder
            .register(TestTool {
                name: "alpha".parse().unwrap(),
                order: Arc::clone(&order),
            })
            .unwrap();
        let (store, log, workspace, config, root) =
            opened_with(BTreeSet::from(["alpha".parse().unwrap()])).await;
        let gateway = scripted_gateway(
            config.model().selection(),
            vec![ScriptedStep::Response {
                response: tool_response(&["alpha"]),
                event: None,
            }],
        );
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies_with(gateway, registry_builder.build(), Arc::new(AskPolicy)),
        )
        .await
        .unwrap();
        let mut stream = handle.subscribe().unwrap();
        let _ = stream.recv().await;
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let turn_id = handle.submit("question".to_owned()).await.unwrap();
        assert_eq!(
            stream.recv().await,
            Some(SessionEvent::TurnStarted { turn_id })
        );
        let interaction_id = loop {
            if let SessionEvent::InputRequested { question, .. } = stream.recv().await.unwrap() {
                break question.interaction_id();
            }
        };
        assert_eq!(handle.cancel(), Ok(()));
        assert!(matches!(
            handle.snapshot().status(),
            SessionStatus::WaitingForInput { interaction_id: current, .. } if current == interaction_id
        ));
        loop {
            if let SessionEvent::ToolFinished { result, .. } = stream.recv().await.unwrap() {
                assert_eq!(result.status(), crate::tools::ToolResultStatus::Cancelled);
                break;
            }
        }
        loop {
            if let SessionEvent::TurnFinished {
                outcome: TurnOutcome::Cancelled,
                ..
            } = stream.recv().await.unwrap()
            {
                break;
            }
        }
        assert_eq!(handle.snapshot().status(), SessionStatus::Idle);
        assert_eq!(log.snapshot().await.entries().len(), 4);
        handle.close().await.unwrap();
        actor_task.await.unwrap().unwrap();
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn second_submit_is_busy_without_an_implicit_queue() {
        let (store, log, workspace, config, root) = opened().await;
        let gateway = scripted_gateway(config.model().selection(), vec![ScriptedStep::Pending]);
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies(gateway),
        )
        .await
        .unwrap();
        let mut stream = handle.subscribe().unwrap();
        let _ = stream.recv().await;
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let _turn_id = handle.submit("first".to_owned()).await.unwrap();
        assert_eq!(
            handle.submit("second".to_owned()).await,
            Err(crate::error::SessionError::Busy)
        );
        handle.cancel().unwrap();
        let _ = stream.recv().await;
        handle.close().await.unwrap();
        actor_task.await.unwrap().unwrap();
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn full_mailbox_is_busy_before_actor_runs_and_dropped_waiter_does_not_cancel_turn() {
        let (store, log, workspace, config, root) = opened().await;
        let gateway = scripted_gateway(
            config.model().selection(),
            vec![ScriptedStep::Response {
                response: text_response("done"),
                event: None,
            }],
        );
        let mut actor_dependencies = dependencies(gateway);
        actor_dependencies.command_capacity = 1;
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            actor_dependencies,
        )
        .await
        .unwrap();
        let mut first = Box::pin(handle.submit("first".to_owned()));
        let waker = futures_util::task::noop_waker_ref();
        let mut context = Context::from_waker(waker);
        assert!(matches!(first.as_mut().poll(&mut context), Poll::Pending));
        assert_eq!(
            handle.submit("second".to_owned()).await,
            Err(crate::error::SessionError::Busy)
        );
        drop(first);
        let mut stream = handle.subscribe().unwrap();
        let _ = stream.recv().await;
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        loop {
            if matches!(stream.recv().await, Some(SessionEvent::TurnFinished { .. })) {
                break;
            }
        }
        assert_eq!(handle.snapshot().status(), SessionStatus::Idle);
        assert_eq!(log.snapshot().await.entries().len(), 3);
        handle.close().await.unwrap();
        actor_task.await.unwrap().unwrap();
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_idle_publishes_closing_then_closed_and_eof() {
        let (store, log, workspace, config, root) = opened().await;
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies(gateway(&ModelSelection::new(
                "noop".parse().unwrap(),
                "model".parse().unwrap(),
            ))),
        )
        .await
        .unwrap();
        let mut stream = handle.subscribe().unwrap();
        let _ = stream.recv().await;
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        handle.close().await.unwrap();
        assert_eq!(handle.snapshot().status(), SessionStatus::Closing);
        assert_eq!(
            handle.submit("after close".to_owned()).await,
            Err(crate::error::SessionError::Closing)
        );
        assert_eq!(
            handle
                .answer(
                    crate::ids::InteractionId::new().unwrap(),
                    crate::tools::UserAnswer::new("late").unwrap(),
                )
                .await,
            Err(crate::error::SessionError::Closing)
        );
        assert_eq!(stream.recv().await, Some(SessionEvent::Closed));
        assert_eq!(stream.recv().await, None);
        actor_task.await.unwrap().unwrap();
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn actor_step_failure_is_preserved_for_close_completion() {
        let (store, log, workspace, config, root) = opened().await;
        let gateway = gateway(config.model().selection());
        let (handle, mut actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies(gateway),
        )
        .await
        .unwrap();
        let turn_id = TurnId::new().unwrap();
        actor.status = SessionStatus::Running { turn_id };
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        assert_eq!(handle.close().await, Err(SessionError::Internal));
        assert_eq!(actor_task.await.unwrap(), Err(SessionError::Internal));
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_close_callers_receive_the_same_completion_result() {
        let (store, log, workspace, config, root) = opened().await;
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies(gateway(&ModelSelection::new(
                "noop".parse().unwrap(),
                "model".parse().unwrap(),
            ))),
        )
        .await
        .unwrap();
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let first = handle.clone();
        let second = handle.clone();
        let (first_result, second_result) = tokio::join!(first.close(), second.close());
        assert_eq!(first_result, Ok(()));
        assert_eq!(second_result, Ok(()));
        assert_eq!(handle.close().await, Ok(()));
        actor_task.await.unwrap().unwrap();
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_completion_propagates_existing_observation_failure_to_all_callers() {
        let (store, log, workspace, config, root) = opened().await;
        let (handle, mut actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies(gateway(&ModelSelection::new(
                "noop".parse().unwrap(),
                "model".parse().unwrap(),
            ))),
        )
        .await
        .unwrap();
        actor.mark_closing().unwrap();
        actor.observation.close(actor.snapshot().unwrap()).unwrap();
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let first = handle.clone();
        let second = handle.clone();
        let (first_result, second_result) = tokio::join!(first.close(), second.close());
        assert_eq!(first_result, Err(crate::error::SessionError::Internal));
        assert_eq!(second_result, Err(crate::error::SessionError::Internal));
        assert_eq!(
            actor_task.await.unwrap(),
            Err(crate::error::SessionError::Internal)
        );
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cooperative_active_close_finishes_cancelled_turn_before_closed() {
        let (store, log, workspace, config, root) = opened().await;
        let gateway = scripted_gateway(config.model().selection(), vec![ScriptedStep::Pending]);
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies(gateway),
        )
        .await
        .unwrap();
        let mut stream = handle.subscribe().unwrap();
        let _ = stream.recv().await;
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let turn_id = handle.submit("question".to_owned()).await.unwrap();
        assert_eq!(
            stream.recv().await,
            Some(SessionEvent::TurnStarted { turn_id })
        );
        let close_handle = handle.clone();
        let close_task =
            tokio::runtime::Handle::current().spawn(async move { close_handle.close().await });
        loop {
            if let SessionEvent::TurnFinished {
                turn_id: finished_turn,
                outcome: TurnOutcome::Cancelled,
            } = stream.recv().await.unwrap()
            {
                assert_eq!(finished_turn, turn_id);
                break;
            }
        }
        close_task.await.unwrap().unwrap();
        assert_eq!(stream.recv().await, Some(SessionEvent::Closed));
        assert_eq!(stream.recv().await, None);
        actor_task.await.unwrap().unwrap();
        assert_eq!(log.snapshot().await.entries().len(), 2);
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn model_failure_maps_to_retryable_unavailable_and_persists_failed_terminal() {
        let (store, log, workspace, config, root) = opened().await;
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies(gateway(&ModelSelection::new(
                "noop".parse().unwrap(),
                "model".parse().unwrap(),
            ))),
        )
        .await
        .unwrap();
        let mut stream = handle.subscribe().unwrap();
        let _ = stream.recv().await;
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let turn_id = handle.submit("question".to_owned()).await.unwrap();
        let outcome = loop {
            if let SessionEvent::TurnFinished { outcome, .. } = stream.recv().await.unwrap() {
                break outcome;
            }
        };
        assert_eq!(
            outcome,
            TurnOutcome::Failed {
                error: PublicErrorSummary::with_retryable(PublicErrorCode::Unavailable, true),
            }
        );
        assert_eq!(handle.snapshot().last_terminal().unwrap().turn_id, turn_id);
        assert_eq!(
            handle.snapshot().last_error().unwrap().code(),
            PublicErrorCode::Unavailable
        );
        assert!(handle.snapshot().last_error().unwrap().retryable());
        assert_eq!(log.snapshot().await.entries().len(), 2);
        handle.close().await.unwrap();
        actor_task.await.unwrap().unwrap();
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn timeout_abort_waits_for_runner_append_before_terminal() {
        let (store, log, workspace, config, root) = opened().await;
        let session_id = config.session_id();
        let gateway = scripted_gateway(
            config.model().selection(),
            vec![ScriptedStep::Response {
                response: text_response("done"),
                event: None,
            }],
        );
        let mut dependencies = dependencies(gateway);
        dependencies.close_timeout = Duration::from_secs(1);
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies,
        )
        .await
        .unwrap();
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());

        let (first_started_sender, first_started_receiver) = std::sync::mpsc::channel();
        let (first_release_sender, first_release_receiver) = std::sync::mpsc::channel();
        let first_blocker = store.run_io(move || {
            first_started_sender.send(()).unwrap();
            first_release_receiver.recv().unwrap();
            Ok::<_, crate::session::store::StoreError>(())
        });
        first_started_receiver.recv().unwrap();

        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        let mut submit = Box::pin(handle.submit("question".to_owned()));
        assert!(matches!(submit.as_mut().poll(&mut context), Poll::Pending));
        crate::session::conversation::wait_until_busy_for_test(&log).await;

        let (second_started_sender, second_started_receiver) = std::sync::mpsc::channel();
        let (second_release_sender, second_release_receiver) = std::sync::mpsc::channel();
        let second_blocker = store.run_io(move || {
            second_started_sender.send(()).unwrap();
            second_release_receiver.recv().unwrap();
            Ok::<_, crate::session::store::StoreError>(())
        });
        first_release_sender.send(()).unwrap();
        SessionStore::await_io(first_blocker).await.unwrap();
        second_started_receiver.recv().unwrap();
        crate::session::conversation::wait_until_busy_for_test(&log).await;
        let turn_id = submit.await.unwrap();

        let close_task = tokio::runtime::Handle::current().spawn({
            let handle = handle.clone();
            async move { handle.close().await }
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(!close_task.is_finished());

        second_release_sender.send(()).unwrap();
        SessionStore::await_io(second_blocker).await.unwrap();
        assert_eq!(close_task.await.unwrap(), Ok(()));
        assert_eq!(actor_task.await.unwrap(), Ok(()));
        let snapshot = log.snapshot().await;
        assert_eq!(snapshot.health(), ConversationHealth::Healthy);
        assert_eq!(snapshot.entries().len(), 3);
        assert!(matches!(
            snapshot.entries().last().unwrap().as_ref(),
            ConversationEntry::TurnTerminal {
                turn_id: terminal_turn,
                outcome: StoredTurnOutcome::Cancelled,
                ..
            } if *terminal_turn == turn_id
        ));
        assert!(matches!(
            snapshot.entries()[1].as_ref(),
            ConversationEntry::Assistant {
                turn_id: assistant_turn,
                text: Some(text),
                tool_calls,
                ..
            } if *assistant_turn == turn_id && text == "done" && tool_calls.is_empty()
        ));
        let persisted = fs::read_to_string(
            root.join("sessions")
                .join(session_id.to_string())
                .join("conversation.jsonl"),
        )
        .unwrap();
        let persisted_entries = persisted
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(
            persisted_entries.iter().any(|entry| {
                entry["type"] == "turn_terminal" && entry["outcome"] == "cancelled"
            })
        );
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn uncooperative_tool_close_aborts_and_awaits_before_closed() {
        let mut registry_builder = ToolRegistry::builder();
        registry_builder
            .register(BlockingTool {
                name: "block".parse().unwrap(),
            })
            .unwrap();
        let (store, log, workspace, config, root) =
            opened_with(BTreeSet::from(["block".parse().unwrap()])).await;
        let gateway = scripted_gateway(
            config.model().selection(),
            vec![ScriptedStep::Response {
                response: tool_response(&["block"]),
                event: None,
            }],
        );
        let mut dependencies = dependencies_with(
            gateway,
            registry_builder.build(),
            Arc::new(AllowConfiguredTools::new()),
        );
        dependencies.close_timeout = Duration::from_secs(30);
        let (handle, actor) = SessionActor::new(
            config,
            Arc::clone(&log),
            Arc::clone(&workspace),
            dependencies,
        )
        .await
        .unwrap();
        let mut stream = handle.subscribe().unwrap();
        let _ = stream.recv().await;
        let actor_task = tokio::runtime::Handle::current().spawn(actor.run());
        let turn_id = handle.submit("question".to_owned()).await.unwrap();
        assert_eq!(
            stream.recv().await,
            Some(SessionEvent::TurnStarted { turn_id })
        );
        assert!(matches!(
            stream.recv().await,
            Some(SessionEvent::ToolStarted { .. })
        ));
        let close_handle = handle.clone();
        let close_task =
            tokio::runtime::Handle::current().spawn(async move { close_handle.close().await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(matches!(
            stream.recv().await,
            Some(SessionEvent::TurnFinished {
                outcome: TurnOutcome::Failed { error },
                ..
            }) if error.code() == PublicErrorCode::Internal
        ));
        close_task.await.unwrap().unwrap();
        assert_eq!(stream.recv().await, Some(SessionEvent::Closed));
        assert_eq!(stream.recv().await, None);
        actor_task.await.unwrap().unwrap();
        assert_eq!(log.snapshot().await.entries().len(), 2);
        store.shutdown().await.unwrap();
        workspace.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
