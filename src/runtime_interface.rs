use std::collections::BTreeSet;

use thiserror::Error;

use crate::wire::{CommandId, SessionId};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum RuntimeCapability {
    StateEvents,
    ProgressEvents,
    RuntimeSnapshot,
    SessionSnapshot,
    PagedQueries,
    CommandCatalog,
    InteractionResolution,
    SessionFork,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeCapabilities {
    values: Vec<RuntimeCapability>,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RuntimeCapabilitiesError {
    #[error("runtime capability set contains a duplicate value")]
    DuplicateCapability,
    #[error("runtime capability is not declared by protocol v1.0")]
    UnsupportedCapability,
}

impl RuntimeCapabilities {
    pub fn empty() -> Self {
        Self { values: Vec::new() }
    }

    pub fn all_v1() -> Self {
        Self {
            values: v1_runtime_capabilities().to_vec(),
        }
    }

    pub fn for_v1(values: Vec<RuntimeCapability>) -> Result<Self, RuntimeCapabilitiesError> {
        let selected = values.iter().copied().collect::<BTreeSet<_>>();
        if selected.len() != values.len() {
            return Err(RuntimeCapabilitiesError::DuplicateCapability);
        }
        if selected
            .iter()
            .any(|capability| !v1_runtime_capabilities().contains(capability))
        {
            return Err(RuntimeCapabilitiesError::UnsupportedCapability);
        }
        Ok(Self {
            values: v1_runtime_capabilities()
                .iter()
                .copied()
                .filter(|capability| selected.contains(capability))
                .collect(),
        })
    }

    pub fn values(&self) -> &[RuntimeCapability] {
        &self.values
    }
}

const fn v1_runtime_capabilities() -> &'static [RuntimeCapability; 8] {
    &[
        RuntimeCapability::StateEvents,
        RuntimeCapability::ProgressEvents,
        RuntimeCapability::RuntimeSnapshot,
        RuntimeCapability::SessionSnapshot,
        RuntimeCapability::PagedQueries,
        RuntimeCapability::CommandCatalog,
        RuntimeCapability::InteractionResolution,
        RuntimeCapability::SessionFork,
    ]
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RuntimeLifecycleCommand {
    ReloadSharedResources,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RuntimeCommand {
    Runtime(RuntimeLifecycleCommand),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandRequest {
    command_id: CommandId,
    command: RuntimeCommand,
}

impl CommandRequest {
    pub const fn new(command_id: CommandId, command: RuntimeCommand) -> Self {
        Self {
            command_id,
            command,
        }
    }

    pub const fn command_id(&self) -> CommandId {
        self.command_id
    }

    pub const fn command(&self) -> &RuntimeCommand {
        &self.command
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum RuntimeReadQuery {
    GetCapabilities,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RuntimeQuery {
    Runtime(RuntimeReadQuery),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryResponse {
    data: QueryResult,
}

impl QueryResponse {
    pub const fn new(data: QueryResult) -> Self {
        Self { data }
    }

    pub const fn data(&self) -> &QueryResult {
        &self.data
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum QueryResult {
    Runtime(RuntimeQueryResult),
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RuntimeQueryResult {
    Capabilities(RuntimeCapabilities),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SnapshotRequest {
    Runtime,
    Session { session_id: SessionId },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SubscriptionScope {
    Runtime,
    Session { session_id: SessionId },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SubscriptionRequest {
    scope: SubscriptionScope,
    include_progress: bool,
}

impl SubscriptionRequest {
    pub const fn new(scope: SubscriptionScope, include_progress: bool) -> Self {
        Self {
            scope,
            include_progress,
        }
    }

    pub const fn scope(&self) -> SubscriptionScope {
        self.scope
    }

    pub const fn include_progress(&self) -> bool {
        self.include_progress
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RuntimeRequest {
    Dispatch(CommandRequest),
    Query(RuntimeQuery),
    Snapshot(SnapshotRequest),
    Subscribe(SubscriptionRequest),
}

#[derive(Clone, Copy, Debug, Eq, Error, Hash, PartialEq)]
pub enum RuntimeDispatchError {
    #[error("runtime dispatch envelope is invalid")]
    InvalidEnvelope,
    #[error("runtime dispatch request exceeds the selected limit")]
    RequestTooLarge,
    #[error("runtime is closed")]
    RuntimeClosed,
    #[error("runtime dispatch owner is unavailable")]
    InternalDispatchUnavailable,
}
