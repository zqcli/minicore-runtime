pub mod agent_session_lifecycle;
pub(crate) mod compaction;
#[allow(
    dead_code,
    reason = "M4 live conversation foundation is consumed by the upcoming M7 runtime slice"
)]
pub(crate) mod conversation_storage;
pub(crate) mod durable_state;
// Preserve the established crate path while making the reducer a descendant of its storage owner.
pub(crate) use conversation_storage::live_conversation;
pub mod model_gateway;
pub mod prompt;
pub mod runtime;
pub mod runtime_interface;
#[allow(
    dead_code,
    reason = "owner-tracked clocks and tasks are consumed incrementally by M5 and M7"
)]
pub(crate) mod runtime_task;
pub use runtime::{MiniCoreRuntime, MiniCoreRuntimeConfig, RuntimeInitializationError};
pub mod skills;
pub mod tools;
pub mod turn_item_interaction;
pub mod wire;
pub mod workspace;
