mod strategy;

pub use strategy::{
    CompactionCandidate, CompactionError, CompactionFuture, CompactionProposal, CompactionRequest,
    CompactionStrategy,
};

const _: () = {
    let _ = strategy::CompactionCandidate::from_confirmed;
};
