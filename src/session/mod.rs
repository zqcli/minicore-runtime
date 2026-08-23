pub(crate) mod actor;
mod bindings;
pub(crate) mod command;
mod event;
mod event_stream;
mod interaction;
pub(crate) mod legacy_event;
pub(crate) mod legacy_event_stream;
pub(crate) mod legacy_snapshot;
pub(crate) mod legacy_state;
mod state;
pub(crate) mod transcript;
mod turn_handle;

pub(crate) use actor::{SessionActor, SessionActorDependencies};
pub use bindings::{SessionBindingError, SessionBindings};
pub(crate) use command::SessionHandle;
pub use event::{
    InteractionResolutionSummary, OutputChannel, SessionEvent, SessionEventEnvelope,
    ToolResultSummary,
};
pub use event_stream::SessionEventStream;
pub use interaction::{InteractionAnswer, InteractionKind, PendingInteraction};
pub use state::{SessionHealth, SessionState, SessionStateError, SessionStatus};
pub use turn_handle::{TurnHandle, TurnOutcome};

const _: () = {
    let _ = SessionActor::new;
    let _ = SessionActor::run;
    let _ = std::mem::size_of::<SessionActorDependencies>();
    let _ = std::mem::size_of::<SessionHandle>();
};
