use minicore_runtime::conversation::{ConversationEntry, ConversationSeq};
use minicore_runtime::storage::{AppendReceipt, SessionLogErrorKind};

use super::{FakeSessionLogInitError, State};

pub(super) fn current_head(state: &State) -> ConversationSeq {
    state
        .entries
        .last()
        .map(ConversationEntry::seq)
        .unwrap_or(ConversationSeq::ZERO)
}

pub(super) fn validate_contiguous(
    entries: &[ConversationEntry],
) -> Result<(), FakeSessionLogInitError> {
    if let Some(first) = entries.first() {
        let expected = ConversationSeq::new(1);
        if first.seq() != expected {
            return Err(FakeSessionLogInitError::NonContiguous {
                expected,
                actual: first.seq(),
            });
        }
    }
    for pair in entries.windows(2) {
        let expected = pair[0]
            .seq()
            .next()
            .ok_or(FakeSessionLogInitError::SequenceOverflow)?;
        if pair[1].seq() != expected {
            return Err(FakeSessionLogInitError::NonContiguous {
                expected,
                actual: pair[1].seq(),
            });
        }
    }
    Ok(())
}

pub(super) fn append_batch(
    state: &mut State,
    expected_head: ConversationSeq,
    entries: Vec<ConversationEntry>,
) -> Result<AppendReceipt, SessionLogErrorKind> {
    if state.closed {
        return Err(SessionLogErrorKind::Closed);
    }
    if state.corrupt {
        return Err(SessionLogErrorKind::Corrupt);
    }
    if state.manifest.is_none() {
        return Err(SessionLogErrorKind::NotInitialized);
    }
    if expected_head != current_head(state) {
        return Err(SessionLogErrorKind::Conflict);
    }
    if entries.is_empty() {
        return Err(SessionLogErrorKind::Internal);
    }
    let first_expected = expected_head.next().ok_or(SessionLogErrorKind::Internal)?;
    let contiguous = entries.first().is_some_and(|entry| {
        entry.seq() == first_expected
            && entries
                .windows(2)
                .all(|pair| Some(pair[1].seq()) == pair[0].seq().next())
    });
    if !contiguous {
        return Err(SessionLogErrorKind::Conflict);
    }
    let appended = entries.len();
    let new_head = entries.last().map(ConversationEntry::seq).unwrap();
    state.entries.extend(entries);
    Ok(AppendReceipt {
        previous_head: expected_head,
        new_head,
        appended,
    })
}
