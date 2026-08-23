use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use super::session_log::SessionLog;
use crate::config::{KernelConfig, SemanticLimits, SessionManifest};
use crate::ids::SessionId;
use futures_util::FutureExt;

use super::log::{
    ConversationCloseOutcome, ConversationCommitError, ConversationCommitErrorKind,
    ConversationLog, FAILED_OPEN_CLOSE_TIMEOUT, OperationOutcome, TimestampSource, commit_error,
    map_log_error, run_log_operation,
};
use super::state::ConversationState;
use super::transcript::valid_page_contract;

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct LoadCompatibilityValidated(Arc<()>);

impl LoadCompatibilityValidated {
    // P4 load flow contract, in order:
    // bindings.validate(&pending.manifest().spec, limits),
    // LoadCompatibilityValidated::after_session_bindings_validation(&pending),
    // pending.finish(proof). OpenGuard owns cancellation/drop cleanup there;
    // this proof does not perform cleanup.
    pub(crate) fn after_session_bindings_validation(pending: &PendingConversationLoad) -> Self {
        Self(Arc::clone(&pending.proof_key))
    }
}

#[must_use = "finish after bindings validation or abort"]
pub(crate) struct PendingConversationLoad {
    inner: Box<dyn SessionLog>,
    manifest: SessionManifest,
    limits: SemanticLimits,
    log_operation_timeout: Duration,
    timestamp_source: TimestampSource,
    proof_key: Arc<()>,
}

impl PendingConversationLoad {
    pub(crate) async fn begin_load(
        expected_session_id: SessionId,
        mut inner: Box<dyn SessionLog>,
        kernel: KernelConfig,
        timestamp_source: TimestampSource,
    ) -> Result<Self, ConversationCommitError> {
        let timeout = if kernel.validate().is_err() {
            let primary = commit_error(ConversationCommitErrorKind::InvalidConfiguration);
            return Err(close_after_error(inner, FAILED_OPEN_CLOSE_TIMEOUT, primary).await);
        } else {
            kernel.log_operation_timeout
        };
        let manifest = match run_log_operation(timeout, || inner.load_manifest()).await {
            OperationOutcome::Success(manifest) => manifest,
            OperationOutcome::Known(error) => {
                return Err(close_after_error(inner, timeout, map_log_error(error)).await);
            }
            OperationOutcome::Timeout | OperationOutcome::Panic => {
                return Err(close_after_error(
                    inner,
                    timeout,
                    commit_error(ConversationCommitErrorKind::DurabilityUnknown),
                )
                .await);
            }
        };
        if manifest.session_id != expected_session_id {
            let primary = ConversationCommitError::with_session_id_mismatch(
                expected_session_id,
                manifest.session_id,
            );
            return Err(close_after_error(inner, timeout, primary).await);
        }
        if manifest.validate(&kernel.limits).is_err() {
            let primary = commit_error(ConversationCommitErrorKind::InvalidManifest);
            return Err(close_after_error(inner, timeout, primary).await);
        }
        Ok(Self {
            inner,
            manifest,
            limits: kernel.limits,
            log_operation_timeout: timeout,
            timestamp_source,
            proof_key: Arc::new(()),
        })
    }

    pub(crate) fn manifest(&self) -> &SessionManifest {
        &self.manifest
    }

    pub(crate) async fn finish(
        self,
        proof: LoadCompatibilityValidated,
    ) -> Result<ConversationLog, ConversationCommitError> {
        if !Arc::ptr_eq(&self.proof_key, &proof.0) {
            let Self {
                inner,
                log_operation_timeout,
                ..
            } = self;
            let primary = commit_error(ConversationCommitErrorKind::CompatibilityProofMismatch);
            return Err(close_after_error(inner, log_operation_timeout, primary).await);
        }
        let Self {
            inner,
            manifest,
            limits,
            log_operation_timeout,
            timestamp_source,
            proof_key: _,
        } = self;
        let mut inner = inner;
        let mut state = match ConversationState::new(manifest.spec.clone(), limits.clone()) {
            Ok(state) => state,
            Err(_) => {
                return Err(close_after_error(
                    inner,
                    log_operation_timeout,
                    commit_error(ConversationCommitErrorKind::ReplayInvalid),
                )
                .await);
            }
        };
        let mut after = None;
        let mut observed_head = None;
        loop {
            let page = match run_log_operation(log_operation_timeout, || {
                inner.read_page(after, limits.max_replay_page_size)
            })
            .await
            {
                OperationOutcome::Success(page) => page,
                OperationOutcome::Known(error) => {
                    return Err(close_after_error(
                        inner,
                        log_operation_timeout,
                        map_log_error(error),
                    )
                    .await);
                }
                OperationOutcome::Timeout | OperationOutcome::Panic => {
                    return Err(close_after_error(
                        inner,
                        log_operation_timeout,
                        commit_error(ConversationCommitErrorKind::DurabilityUnknown),
                    )
                    .await);
                }
            };
            if !valid_page_contract(&page, after, limits.max_replay_page_size)
                || page.observed_head < state.head()
                || page
                    .entries
                    .last()
                    .is_some_and(|entry| page.observed_head < entry.seq())
            {
                return Err(close_after_error(
                    inner,
                    log_operation_timeout,
                    commit_error(ConversationCommitErrorKind::ReplayInvalid),
                )
                .await);
            }
            if observed_head.is_some_and(|head| head != page.observed_head) {
                return Err(close_after_error(
                    inner,
                    log_operation_timeout,
                    commit_error(ConversationCommitErrorKind::ReplayInvalid),
                )
                .await);
            }
            observed_head = Some(page.observed_head);
            let candidate = match state.candidate(&page.entries) {
                Ok(candidate) => candidate,
                Err(_) => {
                    return Err(close_after_error(
                        inner,
                        log_operation_timeout,
                        commit_error(ConversationCommitErrorKind::ReplayInvalid),
                    )
                    .await);
                }
            };
            match page.next_after {
                Some(next_after) => {
                    after = Some(next_after);
                    state.commit(candidate);
                }
                None => {
                    if candidate.head() != page.observed_head {
                        return Err(close_after_error(
                            inner,
                            log_operation_timeout,
                            commit_error(ConversationCommitErrorKind::ReplayInvalid),
                        )
                        .await);
                    }
                    state.commit(candidate);
                    break;
                }
            }
        }
        let mut log = ConversationLog::from_loaded_parts(
            inner,
            state,
            limits,
            log_operation_timeout,
            timestamp_source,
        );
        if let Some(plan) = log.recovery_plan() {
            let append = AssertUnwindSafe(log.append_validated(plan.drafts()))
                .catch_unwind()
                .await;
            let primary = match append {
                Ok(Ok(_)) => None,
                Ok(Err(error))
                    if error.kind() == ConversationCommitErrorKind::DurabilityUnknown =>
                {
                    Some(error.with_kind(ConversationCommitErrorKind::RecoveryUncertain))
                }
                Ok(Err(error)) => Some(error),
                Err(_) => Some(commit_error(ConversationCommitErrorKind::RecoveryUncertain)),
            };
            if let Some(primary) = primary {
                let secondary = close_log(&mut log).await;
                return Err(primary.with_secondary_close(secondary));
            }
        }
        Ok(log)
    }

    pub(crate) async fn abort(self) -> Option<ConversationCloseOutcome> {
        // P4 OpenGuard owns cancellation/drop cleanup. P2 deliberately does
        // not attempt asynchronous cleanup from Drop or use a detached executor.
        close_owned(self.inner, self.log_operation_timeout).await
    }
}

async fn close_log(log: &mut ConversationLog) -> Option<ConversationCloseOutcome> {
    log.close_for_load().await
}

pub(super) async fn close_after_error(
    inner: Box<dyn SessionLog>,
    timeout: Duration,
    primary: ConversationCommitError,
) -> ConversationCommitError {
    primary.with_secondary_close(close_owned(inner, timeout).await)
}

pub(super) async fn close_owned(
    mut inner: Box<dyn SessionLog>,
    timeout: Duration,
) -> Option<ConversationCloseOutcome> {
    match run_log_operation(timeout, || inner.close()).await {
        OperationOutcome::Success(()) => None,
        OperationOutcome::Known(error) => Some(ConversationCloseOutcome::Known(error)),
        OperationOutcome::Timeout => Some(ConversationCloseOutcome::Timeout),
        OperationOutcome::Panic => Some(ConversationCloseOutcome::Panic),
    }
}

pub(crate) async fn close_unopened_log(
    inner: Box<dyn SessionLog>,
    timeout: Duration,
) -> Option<ConversationCloseOutcome> {
    close_owned(inner, timeout).await
}
