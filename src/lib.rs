pub mod agent_session_lifecycle;
#[allow(
    dead_code,
    reason = "M4/M5 foundations and M10 planning are consumed by adjacent summary/orchestration slices"
)]
pub(crate) mod compaction;
pub use compaction::CompactionSettings;
pub(crate) mod conversation_storage;
pub(crate) mod durable_state;
// Crate-private protocol-neutral shared locked-down HTTP transport owner: the one deep
// primitive shared by the direct provider adapters and the fetch_url builtin's pinned
// per-origin clients (see `http_transport`).
pub(crate) mod http_transport;
// Preserve the established crate path while making the reducer a descendant of its storage owner.
pub(crate) use conversation_storage::live_conversation;
pub mod model_gateway;
pub mod prompt;
pub mod runtime;
pub mod runtime_interface;
pub(crate) mod runtime_task;
pub(crate) mod session_execution;
pub(crate) mod session_ingress;
pub(crate) mod session_residency;
pub(crate) mod turn_execution_context;
pub use runtime::{MiniCoreRuntime, MiniCoreRuntimeConfig, RuntimeInitializationError};
pub mod skills;
pub mod tools;
pub mod turn_item_interaction;
pub mod wire;
pub mod workspace;
