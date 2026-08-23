pub(crate) mod actor;
mod bindings;
pub(crate) mod command;
mod event;
pub(crate) mod event_stream;
mod interaction;
pub(crate) mod snapshot;
mod state;
pub(crate) mod transcript;

pub(crate) use actor::{SessionActor, SessionActorDependencies};
pub use bindings::{SessionBindingError, SessionBindings};
pub(crate) use command::SessionHandle;
pub use interaction::{InteractionAnswer, InteractionKind, PendingInteraction};

const _: () = {
    let _ = SessionActor::new;
    let _ = SessionActor::run;
    let _ = std::mem::size_of::<SessionActorDependencies>();
    let _ = std::mem::size_of::<SessionHandle>();
};
