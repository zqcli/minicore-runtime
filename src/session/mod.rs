pub(crate) mod actor;
pub(crate) mod command;
#[cfg(test)]
mod compaction_visibility;
pub(crate) mod conversation;
mod event;
pub(crate) mod event_stream;
mod snapshot;
mod state;
pub(crate) mod store;
pub(crate) mod time;

pub(crate) use actor::{SessionActor, SessionActorDependencies};
pub(crate) use command::SessionHandle;

const _: () = {
    let _ = SessionActor::new;
    let _ = SessionActor::run;
    let _ = std::mem::size_of::<SessionActorDependencies>();
    let _ = std::mem::size_of::<SessionHandle>();
};

pub use event::SessionEvent;
pub use event_stream::SessionEventStream;
pub use snapshot::{
    SessionSnapshot, SnapshotHistory, SnapshotShapeError, TerminalOutcome, TurnOutcome,
    TurnSummary, TurnTerminal, TurnTerminalSummary,
};
pub use state::SessionStatus;
