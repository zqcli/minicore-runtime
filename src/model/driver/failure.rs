use crate::time::DeadlineSource;

use super::ModelError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ModelDriverFailure {
    pub(super) error: ModelError,
    pub(super) deadline_source: Option<DeadlineSource>,
}

impl ModelDriverFailure {
    pub(crate) const fn error(self) -> ModelError {
        self.error
    }

    pub(crate) const fn deadline_source(self) -> Option<DeadlineSource> {
        self.deadline_source
    }

    pub(super) const fn ordinary(error: ModelError) -> Self {
        Self {
            error,
            deadline_source: None,
        }
    }

    pub(super) const fn deadline(error: ModelError, source: DeadlineSource) -> Self {
        Self {
            error,
            deadline_source: Some(source),
        }
    }
}
