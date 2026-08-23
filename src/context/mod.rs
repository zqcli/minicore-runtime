mod driver;
mod provider;

pub(crate) use driver::{ContextDriver, ContextDriverFailure};

pub use provider::{
    ContextBlock, ContextBundle, ContextError, ContextFuture, ContextProvider, ContextRequest,
    ContextSlot,
};

const _: () = {
    let _ = std::mem::size_of::<ContextDriver>();
    let _ = ContextDriver::new;
    let _ = ContextDriver::provide;
    let _ = ContextDriver::provide_detailed;
    let _ = std::mem::size_of::<ContextDriverFailure>();
    let _ = ContextDriverFailure::error;
    let _ = ContextDriverFailure::deadline_source;
};
