mod runtime;
mod session_manager;

pub use crate::session_v2::transcript::{TranscriptEntry, TranscriptPage, TranscriptToolCall};
pub use runtime::{Runtime, SessionSummary};

const _: () = {
    let _ = std::mem::size_of::<Runtime>();
    let _ = std::mem::size_of::<SessionSummary>();
    let _ = std::mem::size_of::<TranscriptEntry>();
    let _ = std::mem::size_of::<TranscriptPage>();
    let _ = std::mem::size_of::<TranscriptToolCall>();
};
