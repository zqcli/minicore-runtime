pub mod agent_session_lifecycle;
#[allow(
    dead_code,
    reason = "the completed M4 compaction owner is consumed by the pending M7 and M5.2 slices"
)]
pub(crate) mod compaction;
pub(crate) mod conversation_storage;
pub(crate) mod durable_state;
// Preserve the established crate path while making the reducer a descendant of its storage owner.
pub(crate) use conversation_storage::live_conversation;
pub mod model_gateway;
pub mod prompt;
pub mod runtime;
pub mod runtime_interface;
pub(crate) mod runtime_task;
pub(crate) mod session_execution;
pub use runtime::{MiniCoreRuntime, MiniCoreRuntimeConfig, RuntimeInitializationError};
pub mod skills;
pub mod tools;
pub mod turn_item_interaction;
pub mod wire;
pub mod workspace;
