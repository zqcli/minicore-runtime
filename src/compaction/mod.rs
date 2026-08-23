mod driver;
mod strategy;

pub use crate::conversation::compaction_candidate::CompactionCandidate;
pub(crate) use driver::{CompactionDriver, CompactionDriverFailure, ValidatedCompactionProposal};
pub use strategy::{
    CompactionError, CompactionFuture, CompactionProposal, CompactionRequest, CompactionStrategy,
};

const _: () = {
    let _ = CompactionDriver::new;
    let _ = CompactionDriver::run;
    let _ = CompactionDriver::run_detailed;
    let _ = std::mem::size_of::<CompactionDriverFailure>();
    let _ = std::mem::size_of::<ValidatedCompactionProposal>();
    let _ = ValidatedCompactionProposal::snapshot_head;
    let _ = ValidatedCompactionProposal::through_seq;
    let _ = ValidatedCompactionProposal::summary;
};
