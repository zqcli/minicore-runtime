pub(crate) mod actor;
pub(crate) mod command;
mod event;
pub(crate) mod event_stream;
pub(crate) mod snapshot;
mod state;
pub(crate) mod transcript;

pub(crate) use actor::{SessionActor, SessionActorDependencies};
pub(crate) use command::SessionHandle;

const _: () = {
    let _ = SessionActor::new;
    let _ = SessionActor::run;
    let _ = std::mem::size_of::<SessionActorDependencies>();
    let _ = std::mem::size_of::<SessionHandle>();
};
