mod driver;
mod provider;

pub(crate) use driver::ContextDriver;

pub use provider::{
    ContextBlock, ContextBundle, ContextError, ContextFuture, ContextProvider, ContextRequest,
    ContextSlot,
};

const _: () = {
    let _ = std::mem::size_of::<ContextDriver>();
    let _ = ContextDriver::new;
    let _ = ContextDriver::provide;
};
