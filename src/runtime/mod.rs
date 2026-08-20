mod runtime_impl;
mod session_manager;

pub use runtime_impl::{Runtime, SessionSummary};

const _: () = {
    let _ = std::mem::size_of::<Runtime>();
    let _ = std::mem::size_of::<SessionSummary>();
};
