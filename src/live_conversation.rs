use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use crate::wire::EntryId;

const MAX_ENTRY_ID_ALLOCATION_ATTEMPTS: usize = 32;

#[derive(Clone, Copy, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ConversationRevision(u64);

impl ConversationRevision {
    pub(crate) fn checked_next(self) -> Result<Self, LiveConversationError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(LiveConversationError::new(
                LiveConversationErrorReason::RevisionOverflow,
            ))
    }
}

impl fmt::Debug for ConversationRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ConversationRevision(<process-local>)")
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum LiveConversationErrorReason {
    RevisionOverflow,
    EntryIdAllocation,
    InvalidRelation,
    InvalidTurn,
    InvalidPromptProjection,
    InvalidCompactionSource,
    StaleCompactionSource,
    InvalidCompactionCut,
    CompactionMarkerMismatch,
    PendingToolExchange,
    InteractionConflict,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct LiveConversationError {
    reason: LiveConversationErrorReason,
}

impl LiveConversationError {
    const fn new(reason: LiveConversationErrorReason) -> Self {
        Self { reason }
    }

    pub(crate) const fn reason(&self) -> LiveConversationErrorReason {
        self.reason
    }
}

impl fmt::Debug for LiveConversationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveConversationError")
            .field("reason", &self.reason)
            .finish()
    }
}

impl fmt::Display for LiveConversationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("live conversation operation failed")
    }
}

impl Error for LiveConversationError {}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum EntryIdAllocationError {
    EntropyUnavailable,
    CollisionAttemptsExhausted,
}

impl fmt::Display for EntryIdAllocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("entry identifier allocation failed")
    }
}

impl Error for EntryIdAllocationError {}

pub(crate) struct EntryIdGenerator {
    reserved: BTreeSet<EntryId>,
}

impl EntryIdGenerator {
    pub(crate) fn new(reserved: impl IntoIterator<Item = EntryId>) -> Self {
        Self {
            reserved: reserved.into_iter().collect(),
        }
    }

    pub(crate) fn allocate(&mut self) -> Result<EntryId, EntryIdAllocationError> {
        self.allocate_candidates(EntryId::generate)
    }

    fn allocate_candidates<F, E>(
        &mut self,
        mut next_candidate: F,
    ) -> Result<EntryId, EntryIdAllocationError>
    where
        F: FnMut() -> Result<EntryId, E>,
    {
        for _ in 0..MAX_ENTRY_ID_ALLOCATION_ATTEMPTS {
            let candidate =
                next_candidate().map_err(|_| EntryIdAllocationError::EntropyUnavailable)?;
            if self.reserved.insert(candidate) {
                return Ok(candidate);
            }
        }
        Err(EntryIdAllocationError::CollisionAttemptsExhausted)
    }

    #[cfg(test)]
    fn allocate_with<F, E>(&mut self, next_candidate: F) -> Result<EntryId, EntryIdAllocationError>
    where
        F: FnMut() -> Result<EntryId, E>,
    {
        self.allocate_candidates(next_candidate)
    }
}

impl fmt::Debug for EntryIdGenerator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EntryIdGenerator")
            .field("reserved_count", &self.reserved.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_id(value: &str) -> EntryId {
        value.parse().expect("test entry IDs are valid")
    }

    #[test]
    fn revision_starts_at_zero_and_advances_checked() {
        let initial = ConversationRevision::default();

        assert_eq!(initial.0, 0);
        assert_eq!(initial.checked_next().unwrap().0, 1);
        assert_eq!(
            format!("{initial:?}"),
            "ConversationRevision(<process-local>)"
        );
    }

    #[test]
    fn revision_overflow_is_a_redacted_live_conversation_error() {
        let error = ConversationRevision(u64::MAX).checked_next().unwrap_err();

        assert_eq!(
            error.reason(),
            LiveConversationErrorReason::RevisionOverflow
        );
        assert_eq!(error.to_string(), "live conversation operation failed");
        assert_eq!(
            format!("{error:?}"),
            "LiveConversationError { reason: RevisionOverflow }"
        );
    }

    #[test]
    fn seeded_ids_are_reserved() {
        let seeded = entry_id("ent_11111111111111111111111111111111");
        let fresh = entry_id("ent_22222222222222222222222222222222");
        let mut generator = EntryIdGenerator::new([seeded]);
        let mut candidates = [Ok::<_, ()>(seeded), Ok(fresh)].into_iter();

        assert_eq!(
            generator.allocate_with(|| candidates.next().unwrap()),
            Ok(fresh)
        );
    }

    #[test]
    fn immediate_unique_allocation_reserves_before_returning() {
        let candidate = entry_id("ent_33333333333333333333333333333333");
        let mut generator = EntryIdGenerator::new([]);

        assert_eq!(
            generator.allocate_with(|| Ok::<_, ()>(candidate)),
            Ok(candidate)
        );
        assert_eq!(
            generator.allocate_with(|| Ok::<_, ()>(candidate)),
            Err(EntryIdAllocationError::CollisionAttemptsExhausted)
        );
    }

    #[test]
    fn collision_then_unique_candidate_allocates_the_unique_id() {
        let collision = entry_id("ent_44444444444444444444444444444444");
        let unique = entry_id("ent_55555555555555555555555555555555");
        let mut generator = EntryIdGenerator::new([collision]);
        let mut candidates = [Ok::<_, ()>(collision), Ok(unique)].into_iter();

        assert_eq!(
            generator.allocate_with(|| candidates.next().unwrap()),
            Ok(unique)
        );
    }

    #[test]
    fn thirty_two_collisions_exhaust_allocation_attempts() {
        let collision = entry_id("ent_66666666666666666666666666666666");
        let mut generator = EntryIdGenerator::new([collision]);
        let mut attempts = 0;

        assert_eq!(
            generator.allocate_with(|| {
                attempts += 1;
                Ok::<_, ()>(collision)
            }),
            Err(EntryIdAllocationError::CollisionAttemptsExhausted)
        );
        assert_eq!(attempts, MAX_ENTRY_ID_ALLOCATION_ATTEMPTS);
    }

    #[test]
    fn entropy_failure_is_redacted_and_does_not_reserve_the_sentinel() {
        let raw_entropy = "raw entropy detail";
        let sentinel = entry_id("ent_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let mut generator = EntryIdGenerator::new([]);
        let reserved_before = generator.reserved.clone();

        let error = generator
            .allocate_with(|| Err::<EntryId, _>(raw_entropy))
            .unwrap_err();

        assert_eq!(error, EntryIdAllocationError::EntropyUnavailable);
        assert_eq!(error.to_string(), "entry identifier allocation failed");
        assert!(!format!("{error:?} {error}").contains(raw_entropy));
        assert_eq!(generator.reserved, reserved_before);
        assert_eq!(
            generator.allocate_with(|| Ok::<_, ()>(sentinel)),
            Ok(sentinel)
        );
    }

    #[test]
    fn allocation_errors_preserve_reservations_and_the_next_sentinel() {
        let collision = entry_id("ent_77777777777777777777777777777777");
        let sentinel = entry_id("ent_88888888888888888888888888888888");
        let mut generator = EntryIdGenerator::new([collision]);
        let reserved_before = generator.reserved.clone();
        let mut candidates =
            std::iter::repeat_n(Ok::<_, ()>(collision), 32).chain(std::iter::once(Ok(sentinel)));

        assert_eq!(
            generator.allocate_with(|| candidates.next().unwrap()),
            Err(EntryIdAllocationError::CollisionAttemptsExhausted)
        );
        assert_eq!(generator.reserved, reserved_before);
        assert_eq!(
            generator.allocate_with(|| candidates.next().unwrap()),
            Ok(sentinel)
        );
    }

    #[test]
    fn debug_and_errors_never_disclose_ids_or_entropy_details() {
        let id = entry_id("ent_99999999999999999999999999999999");
        let id_text = id.to_string();
        let raw_entropy = "raw entropy detail";
        let mut generator = EntryIdGenerator::new([id]);
        let error = generator
            .allocate_with(|| Err::<EntryId, _>(raw_entropy))
            .unwrap_err();

        assert_eq!(
            format!("{generator:?}"),
            "EntryIdGenerator { reserved_count: 1 }"
        );
        for output in [
            format!("{generator:?}"),
            format!("{error:?}"),
            error.to_string(),
        ] {
            assert!(!output.contains(&id_text));
            assert!(!output.contains(raw_entropy));
        }
    }
}
