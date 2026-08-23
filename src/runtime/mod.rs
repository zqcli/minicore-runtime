mod runtime_impl;
mod session_manager;

pub(crate) use runtime_impl::{Runtime, SessionSummary};

const _: () = {
    // P7 deletion target: remove when SessionRuntime replaces the legacy owner.
    let _ = std::mem::size_of::<Runtime>();
    let _ = std::mem::size_of::<SessionSummary>();
};
