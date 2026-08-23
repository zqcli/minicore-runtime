use std::time::Instant;

use tokio_util::sync::CancellationToken;

use super::progress::ToolProgressSink;

#[derive(Clone)]
pub struct ToolContext {
    pub cancellation: CancellationToken,
    pub deadline: Instant,
    pub progress: ToolProgressSink,
}
