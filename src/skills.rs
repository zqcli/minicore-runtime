use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use thiserror::Error;

use crate::wire::lexical::{LexicalError, validate_stable_symbolic_key};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SkillIdError {
    #[error("skill ID must be 1..=128 bytes")]
    InvalidLength,
    #[error("skill ID violates the stable symbolic key grammar")]
    InvalidGrammar,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SkillId(Box<str>);

impl SkillId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for SkillId {
    type Err = SkillIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_stable_symbolic_key(value, 128, false).map_err(|error| match error {
            LexicalError::Empty | LexicalError::TooLong => SkillIdError::InvalidLength,
            LexicalError::InvalidGrammar | LexicalError::UnsafeText => SkillIdError::InvalidGrammar,
        })?;
        Ok(Self(value.into()))
    }
}

impl fmt::Display for SkillId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for SkillId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

/// The immutable Skill view captured for one Turn.
///
/// M6.1 intentionally supports only the valid empty view. The shared inner value gives the
/// Prompt projection an exact parent object without introducing a generation or binding ID.
#[allow(
    dead_code,
    reason = "the empty captured Skill view is consumed by the pending TurnExecutionContext"
)]
pub(crate) struct SkillView {
    inner: Arc<SkillViewInner>,
}

struct SkillViewInner {
    entry_count: usize,
}

/// The model-safe projection of one exact captured [`SkillView`].
#[derive(Clone)]
#[allow(
    dead_code,
    reason = "the PromptSet captures this owner-bound empty projection in M6.1"
)]
pub(crate) struct SkillPromptView {
    inner: Arc<SkillViewInner>,
}

#[allow(
    dead_code,
    reason = "the empty captured Skill view is consumed by the pending TurnExecutionContext"
)]
impl SkillView {
    pub(crate) fn empty() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(SkillViewInner { entry_count: 0 }),
        })
    }

    pub(crate) fn prompt_view(&self) -> SkillPromptView {
        SkillPromptView {
            inner: Arc::clone(&self.inner),
        }
    }

    pub(crate) fn owns_prompt_view(&self, view: &SkillPromptView) -> bool {
        Arc::ptr_eq(&self.inner, &view.inner)
    }
}

#[allow(
    dead_code,
    reason = "the PromptSet captures this owner-bound empty projection in M6.1"
)]
impl SkillPromptView {
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.entry_count == 0
    }
}

impl fmt::Debug for SkillView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SkillView")
            .field("entry_count", &self.inner.entry_count)
            .finish()
    }
}

impl fmt::Debug for SkillPromptView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SkillPromptView")
            .field("entry_count", &self.inner.entry_count)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_prompt_view_retains_its_exact_parent_without_an_identity_value() {
        let first = SkillView::empty();
        let second = SkillView::empty();
        let view = first.prompt_view();
        let clone = view.clone();

        assert!(view.is_empty());
        assert!(clone.is_empty());
        assert!(first.owns_prompt_view(&view));
        assert!(first.owns_prompt_view(&clone));
        assert!(!second.owns_prompt_view(&view));
        assert_eq!(format!("{first:?}"), "SkillView { entry_count: 0 }");
        assert_eq!(format!("{view:?}"), "SkillPromptView { entry_count: 0 }");
    }
}
