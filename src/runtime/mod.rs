mod runtime_impl;
mod session_manager;

pub use crate::session::transcript::{TranscriptEntry, TranscriptPage, TranscriptToolCall};
pub use runtime_impl::{Runtime, SessionSummary};

const _: () = {
    let _ = std::mem::size_of::<Runtime>();
    let _ = std::mem::size_of::<SessionSummary>();
    let _ = std::mem::size_of::<TranscriptEntry>();
    let _ = std::mem::size_of::<TranscriptPage>();
    let _ = std::mem::size_of::<TranscriptToolCall>();
};
