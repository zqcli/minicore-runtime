use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::config::{SessionManifest, SessionSpec};
use crate::conversation::{
    ConversationCommitError, ConversationCommitErrorKind, ConversationLog,
    LoadCompatibilityValidated,
};
use crate::error::{SessionLogErrorKind, SessionOpenError};
use crate::ids::{SessionId, SessionInstanceId};
use crate::storage::SessionLog;

use super::actor::{ActorReady, SessionActor, SessionActorExit, run_session_actor};
use super::runtime::{SessionRuntimeOptions, SessionRuntimeParts};
use super::runtime_log::{
    cancellable_log, close_raw, synthetic_log_error, timestamp_source, with_secondary,
};
use crate::bindings::SessionBindingError;

const FAILED_OPEN_CLOSE_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) enum OpenRequest {
    Create {
        session_id: SessionId,
        spec: SessionSpec,
    },
    Load {
        expected_session_id: SessionId,
    },
}

pub(super) struct OpenPayload {
    request: OpenRequest,
    log: Box<dyn SessionLog>,
    options: SessionRuntimeOptions,
}

pub(super) type SharedOpenPayload = Arc<Mutex<Option<OpenPayload>>>;

impl OpenPayload {
    pub(super) fn shared(
        request: OpenRequest,
        log: Box<dyn SessionLog>,
        options: SessionRuntimeOptions,
    ) -> SharedOpenPayload {
        Arc::new(Mutex::new(Some(Self {
            request,
            log,
            options,
        })))
    }

    pub(super) async fn close_unstarted(self) -> SessionOpenError {
        let timeout = if self.options.kernel().validate().is_ok() {
            self.options.kernel().log_operation_timeout
        } else {
            FAILED_OPEN_CLOSE_TIMEOUT
        };
        close_raw(self.log, timeout, SessionOpenError::actor_start_failed()).await
    }
}

pub(super) struct OpenReady {
    pub(super) session_id: SessionId,
    pub(super) instance_id: SessionInstanceId,
    pub(super) handle: super::SessionHandle,
    pub(super) events: super::SessionEventStream,
    pub(super) runner_lifecycle: super::actor::RunnerLifecycle,
}

struct PreparedOwner {
    session_id: SessionId,
    instance_id: SessionInstanceId,
    actor: SessionActor,
    ready: ActorReady,
}

pub(super) async fn run_open(
    payload: SharedOpenPayload,
    owner_cancel: CancellationToken,
    payload_claimed: CancellationToken,
    ready: oneshot::Sender<Result<OpenReady, SessionOpenError>>,
) -> SessionActorExit {
    let payload = {
        let mut payload = payload
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        payload.take()
    };
    let payload = match payload {
        Some(payload) => {
            payload_claimed.cancel();
            payload
        }
        None => {
            let _ = ready.send(Err(SessionOpenError::actor_start_failed()));
            return SessionActorExit::OpenFailed;
        }
    };
    let request = payload.request;
    let log = payload.log;
    let options = payload.options;
    let prepared = match prepare(request, log, options, &owner_cancel).await {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = ready.send(Err(error));
            return SessionActorExit::OpenFailed;
        }
    };
    let PreparedOwner {
        session_id,
        instance_id,
        mut actor,
        ready: actor_ready,
    } = prepared;
    if owner_cancel.is_cancelled() {
        let _ = actor.close_before_ready().await;
        return SessionActorExit::OpenFailed;
    }
    let ActorReady {
        handle,
        events,
        runner_lifecycle,
    } = actor_ready;
    if ready
        .send(Ok(OpenReady {
            session_id,
            instance_id,
            handle,
            events,
            runner_lifecycle,
        }))
        .is_err()
    {
        owner_cancel.cancel();
        let _ = actor.close_before_ready().await;
        return SessionActorExit::OpenFailed;
    }
    run_session_actor(&mut actor).await
}

async fn prepare(
    request: OpenRequest,
    log: Box<dyn SessionLog>,
    options: SessionRuntimeOptions,
    owner_cancel: &CancellationToken,
) -> Result<PreparedOwner, SessionOpenError> {
    let parts = options.into_parts();
    if parts.kernel.validate().is_err() {
        return Err(close_raw(
            log,
            FAILED_OPEN_CLOSE_TIMEOUT,
            SessionOpenError::invalid_configuration(),
        )
        .await);
    }
    if owner_cancel.is_cancelled() {
        return Err(close_raw(
            log,
            parts.kernel.log_operation_timeout,
            SessionOpenError::actor_start_failed(),
        )
        .await);
    }
    match request {
        OpenRequest::Create { session_id, spec } => {
            prepare_create(session_id, spec, log, parts, owner_cancel).await
        }
        OpenRequest::Load {
            expected_session_id,
        } => prepare_load(expected_session_id, log, parts, owner_cancel).await,
    }
}

async fn prepare_create(
    session_id: SessionId,
    spec: SessionSpec,
    log: Box<dyn SessionLog>,
    parts: SessionRuntimeParts,
    owner_cancel: &CancellationToken,
) -> Result<PreparedOwner, SessionOpenError> {
    if spec.validate(&parts.kernel.limits).is_err() {
        return Err(close_raw(
            log,
            parts.kernel.log_operation_timeout,
            SessionOpenError::invalid_manifest(),
        )
        .await);
    }
    if let Err(error) = parts.bindings.validate(&spec, &parts.kernel.limits) {
        return Err(close_raw(
            log,
            parts.kernel.log_operation_timeout,
            binding_error(error),
        )
        .await);
    }
    if owner_cancel.is_cancelled() {
        return Err(close_raw(
            log,
            parts.kernel.log_operation_timeout,
            SessionOpenError::actor_start_failed(),
        )
        .await);
    }
    let manifest = match SessionManifest::new(session_id, spec.clone()) {
        Ok(manifest) if manifest.validate(&parts.kernel.limits).is_ok() => manifest,
        Ok(_) | Err(_) => {
            return Err(close_raw(
                log,
                parts.kernel.log_operation_timeout,
                SessionOpenError::invalid_manifest(),
            )
            .await);
        }
    };
    let mut conversation = ConversationLog::initialize(
        cancellable_log(log, owner_cancel.clone()),
        manifest,
        parts.kernel.clone(),
        timestamp_source(),
    )
    .await
    .map_err(map_conversation_error)?;
    if owner_cancel.is_cancelled() {
        let secondary = conversation.close_after_open_failure().await;
        return Err(with_secondary(
            SessionOpenError::actor_start_failed(),
            secondary,
        ));
    }
    build_owner(session_id, spec, conversation, parts, owner_cancel).await
}

async fn prepare_load(
    expected_session_id: SessionId,
    log: Box<dyn SessionLog>,
    parts: SessionRuntimeParts,
    owner_cancel: &CancellationToken,
) -> Result<PreparedOwner, SessionOpenError> {
    let pending = ConversationLog::begin_load(
        expected_session_id,
        cancellable_log(log, owner_cancel.clone()),
        parts.kernel.clone(),
        timestamp_source(),
    )
    .await
    .map_err(map_conversation_error)?;
    if owner_cancel.is_cancelled() {
        let secondary = pending.abort().await;
        return Err(with_secondary(
            SessionOpenError::actor_start_failed(),
            secondary,
        ));
    }
    if let Err(error) = parts
        .bindings
        .validate(&pending.manifest().spec, &parts.kernel.limits)
    {
        let secondary = pending.abort().await;
        return Err(with_secondary(binding_error(error), secondary));
    }
    if owner_cancel.is_cancelled() {
        let secondary = pending.abort().await;
        return Err(with_secondary(
            SessionOpenError::actor_start_failed(),
            secondary,
        ));
    }
    let session_id = pending.manifest().session_id;
    let spec = pending.manifest().spec.clone();
    let proof = LoadCompatibilityValidated::after_session_bindings_validation(&pending);
    let mut conversation = pending
        .finish(proof)
        .await
        .map_err(map_conversation_error)?;
    if owner_cancel.is_cancelled() {
        let secondary = conversation.close_after_open_failure().await;
        return Err(with_secondary(
            SessionOpenError::actor_start_failed(),
            secondary,
        ));
    }
    build_owner(session_id, spec, conversation, parts, owner_cancel).await
}

async fn build_owner(
    session_id: SessionId,
    spec: SessionSpec,
    mut conversation: ConversationLog,
    parts: SessionRuntimeParts,
    owner_cancel: &CancellationToken,
) -> Result<PreparedOwner, SessionOpenError> {
    let instance_id = match SessionInstanceId::new() {
        Ok(instance_id) => instance_id,
        Err(_) => {
            let secondary = conversation.close_after_open_failure().await;
            return Err(with_secondary(
                SessionOpenError::actor_start_failed(),
                secondary,
            ));
        }
    };
    let (actor, ready) = match SessionActor::new(
        conversation,
        parts.kernel,
        parts.bindings,
        spec,
        session_id,
        instance_id,
        owner_cancel.clone(),
    ) {
        Ok(owner) => owner,
        Err(mut failure) => {
            let secondary = failure.log.close_after_open_failure().await;
            return Err(with_secondary(
                SessionOpenError::actor_start_failed(),
                secondary,
            ));
        }
    };
    Ok(PreparedOwner {
        session_id,
        instance_id,
        actor,
        ready,
    })
}

fn binding_error(error: SessionBindingError) -> SessionOpenError {
    SessionOpenError::binding_mismatch(matches!(error, SessionBindingError::ModelMismatch))
}

fn map_conversation_error(error: ConversationCommitError) -> SessionOpenError {
    let primary = match error.kind() {
        ConversationCommitErrorKind::InvalidConfiguration => {
            SessionOpenError::invalid_configuration()
        }
        ConversationCommitErrorKind::InvalidManifest
        | ConversationCommitErrorKind::ReplayInvalid
        | ConversationCommitErrorKind::Validation => SessionOpenError::invalid_manifest(),
        ConversationCommitErrorKind::SessionIdMismatch => match error.session_id_mismatch() {
            Some((expected, actual)) => SessionOpenError::for_session_id_mismatch(expected, actual),
            None => SessionOpenError::invalid_manifest(),
        },
        ConversationCommitErrorKind::RecoveryUncertain => {
            SessionOpenError::recovery_uncertain(error.primary_log_error().cloned())
        }
        ConversationCommitErrorKind::Log(kind) => SessionOpenError::log(
            error
                .primary_log_error()
                .cloned()
                .unwrap_or_else(|| synthetic_log_error(kind)),
        ),
        ConversationCommitErrorKind::DurabilityUnknown => SessionOpenError::log(
            error
                .primary_log_error()
                .cloned()
                .unwrap_or_else(|| synthetic_log_error(SessionLogErrorKind::UnknownOutcome)),
        ),
        ConversationCommitErrorKind::ContractViolation => {
            SessionOpenError::log(synthetic_log_error(SessionLogErrorKind::Internal))
        }
        ConversationCommitErrorKind::CompatibilityProofMismatch
        | ConversationCommitErrorKind::Closed
        | ConversationCommitErrorKind::EmptyBatch
        | ConversationCommitErrorKind::TranscriptLimit
        | ConversationCommitErrorKind::TranscriptInvalid
        | ConversationCommitErrorKind::SequenceOverflow
        | ConversationCommitErrorKind::Timestamp => SessionOpenError::actor_start_failed(),
    };
    with_secondary(primary, error.secondary_close_outcome().cloned())
}

#[cfg(test)]
mod tests;
