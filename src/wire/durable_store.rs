use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::str::FromStr;

use thiserror::Error;

use crate::agent_session_lifecycle::{
    AgentDefinition, AgentMetadata, AgentRevisionRef, AgentStatus, ForkAnchor, ForkSourceKind,
    SessionDefinition, SessionForkProvenance, SessionLifecycle, SessionMetadata,
    SessionModelConfig,
};
use crate::durable_state::{DurableAgentHead, DurableSessionHead, StorageGeneration};
use crate::model_gateway::{ModelSelection, ReasoningPreference};
use crate::prompt::{AgentPromptSelection, PromptId, SessionPromptSelection};
use crate::wire::bounded_json::{BoundedJsonError, JsonNode, JsonParseLimits, parse_node};
use crate::wire::{AgentRevision, ItemId, SessionDefinitionRevision, WorkspaceRevision};
use crate::workspace::{
    RequestedFilesystemAccess, WorkspaceCwdSpec, WorkspaceDefinitionInput, WorkspacePathTarget,
    WorkspaceRootInput, WorkspaceSourcePolicy, lower_workspace, uri_from_spec,
};

pub(crate) const MAX_DURABLE_DOCUMENT_BYTES: usize = 1_048_576;
pub(crate) const MAX_DURABLE_DOCUMENT_BODY_BYTES: usize = MAX_DURABLE_DOCUMENT_BYTES - 1;

/// The closed, redacted failure taxonomy for Store V1 entity document bytes.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum DurableStoreCodecError {
    #[error("durable document exceeds the Store V1 size limit")]
    DocumentTooLarge,
    #[error("durable document has invalid JSON")]
    InvalidDocument,
    #[error("durable document exceeds a JSON structural limit")]
    JsonStructure,
    #[error("durable document does not have the required Store V1 shape")]
    InvalidShape,
    #[error("durable document has an invalid typed scalar")]
    InvalidScalar,
    #[error("durable document violates a Store V1 value invariant")]
    InvalidSemantic,
    #[error("durable document is not canonically encoded")]
    Noncanonical,
}

/// The concrete Store V1 codec for physical Agent and Session documents. It intentionally has
/// no registry or public representation: DurableState is the only future production caller.
pub(crate) struct DurableStoreV1Codec;

impl DurableStoreV1Codec {
    pub(crate) fn decode_agent_definition(
        input: &[u8],
    ) -> Result<AgentDefinition, DurableStoreCodecError> {
        let node = parse_document(input)?;
        let definition = decode_agent_definition(&node)?;
        require_canonical(input, Self::encode_agent_definition(&definition)?)?;
        Ok(definition)
    }

    pub(crate) fn encode_agent_definition(
        definition: &AgentDefinition,
    ) -> Result<Vec<u8>, DurableStoreCodecError> {
        let mut writer = DurableDocumentWriter::new();
        writer.object_start()?;
        writer.key("agentId")?;
        writer.string(&definition.agent_id().to_string())?;
        writer.comma()?;
        writer.key("revision")?;
        writer.string(&definition.revision().to_string())?;
        writer.comma()?;
        writer.key("promptIds")?;
        writer.array_start()?;
        for (index, prompt_id) in definition.prompts().enabled().iter().enumerate() {
            if index != 0 {
                writer.comma()?;
            }
            writer.string(prompt_id.as_str())?;
        }
        writer.array_end()?;
        writer.comma()?;
        writer.key("createdAt")?;
        writer.string(&definition.created_at().to_string())?;
        writer.object_end()?;
        writer.finish()
    }

    pub(crate) fn decode_agent_head(
        input: &[u8],
    ) -> Result<DurableAgentHead, DurableStoreCodecError> {
        let node = parse_document(input)?;
        let head = decode_agent_head(&node)?;
        require_canonical(input, Self::encode_agent_head(&head)?)?;
        Ok(head)
    }

    pub(crate) fn encode_agent_head(
        head: &DurableAgentHead,
    ) -> Result<Vec<u8>, DurableStoreCodecError> {
        let mut writer = DurableDocumentWriter::new();
        writer.object_start()?;
        writer.key("entity")?;
        writer.string("agent")?;
        writer.comma()?;
        writer.key("agentId")?;
        writer.string(&head.agent_id().to_string())?;
        writer.comma()?;
        writer.key("storageGeneration")?;
        writer.u32(head.storage_generation().get())?;
        writer.comma()?;
        writer.key("previousStorageGeneration")?;
        optional_generation(&mut writer, head.previous_storage_generation())?;
        writer.comma()?;
        writer.key("currentDefinition")?;
        writer.object_start()?;
        writer.key("revision")?;
        writer.string(&head.current_definition_revision().to_string())?;
        writer.comma()?;
        writer.key("storageGeneration")?;
        writer.u32(head.current_definition_storage_generation().get())?;
        writer.object_end()?;
        writer.comma()?;
        writer.key("metadata")?;
        encode_agent_metadata(&mut writer, head.metadata())?;
        writer.comma()?;
        writer.key("status")?;
        writer.string(status_name(head.status()))?;
        writer.comma()?;
        writer.key("createdAt")?;
        writer.string(&head.created_at().to_string())?;
        writer.object_end()?;
        writer.finish()
    }

    pub(crate) fn decode_session_definition(
        input: &[u8],
    ) -> Result<SessionDefinition, DurableStoreCodecError> {
        Self::decode_session_definition_for_target(input, WorkspacePathTarget::current())
    }

    pub(crate) fn encode_session_definition(
        definition: &SessionDefinition,
    ) -> Result<Vec<u8>, DurableStoreCodecError> {
        Self::encode_session_definition_for_target(definition, WorkspacePathTarget::current())
    }

    fn decode_session_definition_for_target(
        input: &[u8],
        target: WorkspacePathTarget,
    ) -> Result<SessionDefinition, DurableStoreCodecError> {
        let node = parse_document(input)?;
        let definition = decode_session_definition(&node, target)?;
        require_canonical(
            input,
            Self::encode_session_definition_for_target(&definition, target)?,
        )?;
        Ok(definition)
    }

    fn encode_session_definition_for_target(
        definition: &SessionDefinition,
        target: WorkspacePathTarget,
    ) -> Result<Vec<u8>, DurableStoreCodecError> {
        let mut writer = DurableDocumentWriter::new();
        writer.object_start()?;
        writer.key("sessionId")?;
        writer.string(&definition.session_id().to_string())?;
        writer.comma()?;
        writer.key("revision")?;
        writer.string(&definition.revision().to_string())?;
        writer.comma()?;
        writer.key("agent")?;
        encode_agent_revision_ref(&mut writer, definition.agent())?;
        writer.comma()?;
        writer.key("workspace")?;
        encode_workspace(&mut writer, definition.workspace(), target)?;
        writer.comma()?;
        writer.key("model")?;
        encode_session_model(&mut writer, definition.model())?;
        writer.comma()?;
        writer.key("promptIds")?;
        encode_session_prompt_ids(&mut writer, definition.prompts())?;
        writer.comma()?;
        writer.key("createdAt")?;
        writer.string(&definition.created_at().to_string())?;
        writer.object_end()?;
        writer.finish()
    }

    pub(crate) fn decode_session_head(
        input: &[u8],
    ) -> Result<DurableSessionHead, DurableStoreCodecError> {
        let node = parse_document(input)?;
        let head = decode_session_head(&node)?;
        require_canonical(input, Self::encode_session_head(&head)?)?;
        Ok(head)
    }

    pub(crate) fn encode_session_head(
        head: &DurableSessionHead,
    ) -> Result<Vec<u8>, DurableStoreCodecError> {
        let mut writer = DurableDocumentWriter::new();
        writer.object_start()?;
        writer.key("entity")?;
        writer.string("session")?;
        writer.comma()?;
        writer.key("sessionId")?;
        writer.string(&head.session_id().to_string())?;
        writer.comma()?;
        writer.key("storageGeneration")?;
        writer.u32(head.storage_generation().get())?;
        writer.comma()?;
        writer.key("previousStorageGeneration")?;
        optional_generation(&mut writer, head.previous_storage_generation())?;
        writer.comma()?;
        writer.key("currentDefinition")?;
        writer.object_start()?;
        writer.key("revision")?;
        writer.string(&head.current_definition_revision().to_string())?;
        writer.comma()?;
        writer.key("storageGeneration")?;
        writer.u32(head.current_definition_storage_generation().get())?;
        writer.object_end()?;
        writer.comma()?;
        writer.key("metadata")?;
        encode_session_metadata(&mut writer, head.metadata())?;
        writer.comma()?;
        writer.key("lifecycle")?;
        writer.string(lifecycle_name(head.lifecycle()))?;
        writer.comma()?;
        writer.key("forkProvenance")?;
        encode_fork_provenance(&mut writer, head.fork_provenance())?;
        writer.comma()?;
        writer.key("createdAt")?;
        writer.string(&head.created_at().to_string())?;
        writer.object_end()?;
        writer.finish()
    }
}

fn parse_document(input: &[u8]) -> Result<JsonNode, DurableStoreCodecError> {
    if input.len() > MAX_DURABLE_DOCUMENT_BYTES {
        return Err(DurableStoreCodecError::DocumentTooLarge);
    }
    let Some(body) = input.strip_suffix(b"\n") else {
        return Err(DurableStoreCodecError::Noncanonical);
    };
    parse_node(
        body,
        JsonParseLimits::durable_store(MAX_DURABLE_DOCUMENT_BODY_BYTES),
    )
    .map_err(map_json_error)
}

fn require_canonical(input: &[u8], canonical: Vec<u8>) -> Result<(), DurableStoreCodecError> {
    if input == canonical {
        Ok(())
    } else {
        Err(DurableStoreCodecError::Noncanonical)
    }
}

fn map_json_error(error: BoundedJsonError) -> DurableStoreCodecError {
    match error {
        BoundedJsonError::RawInputTooLarge | BoundedJsonError::CanonicalOutputTooLarge => {
            DurableStoreCodecError::DocumentTooLarge
        }
        BoundedJsonError::InvalidUtf8
        | BoundedJsonError::InvalidSyntax
        | BoundedJsonError::DuplicateKey => DurableStoreCodecError::InvalidDocument,
        BoundedJsonError::DepthLimit
        | BoundedJsonError::ArrayItemsLimit
        | BoundedJsonError::ObjectMembersLimit
        | BoundedJsonError::StringBytesLimit
        | BoundedJsonError::NumberLiteralLimit
        | BoundedJsonError::NumberExponentLimit
        | BoundedJsonError::NodeLimit => DurableStoreCodecError::JsonStructure,
        BoundedJsonError::RootObjectRequired => DurableStoreCodecError::InvalidShape,
    }
}

fn decode_agent_definition(node: &JsonNode) -> Result<AgentDefinition, DurableStoreCodecError> {
    let object = object(node)?;
    exact_fields(object, &["agentId", "revision", "promptIds", "createdAt"])?;
    let agent_id = scalar(required(object, "agentId")?)?;
    let revision = scalar(required(object, "revision")?)?;
    let prompt_ids = array(required(object, "promptIds")?)?
        .iter()
        .map(scalar::<PromptId>)
        .collect::<Result<Vec<_>, _>>()?;
    let prompts = AgentPromptSelection::new(prompt_ids.clone())
        .map_err(|_| DurableStoreCodecError::InvalidSemantic)?;
    if prompts.enabled().iter().ne(prompt_ids.iter()) {
        return Err(DurableStoreCodecError::InvalidSemantic);
    }
    let created_at = scalar(required(object, "createdAt")?)?;
    Ok(AgentDefinition::new(
        agent_id, revision, prompts, created_at,
    ))
}

fn decode_session_definition(
    node: &JsonNode,
    target: WorkspacePathTarget,
) -> Result<SessionDefinition, DurableStoreCodecError> {
    let object = object(node)?;
    exact_fields(
        object,
        &[
            "sessionId",
            "revision",
            "agent",
            "workspace",
            "model",
            "promptIds",
            "createdAt",
        ],
    )?;
    let session_id = scalar(required(object, "sessionId")?)?;
    let revision = scalar(required(object, "revision")?)?;
    let agent = decode_agent_revision_ref(required(object, "agent")?)?;
    let workspace = decode_workspace(required(object, "workspace")?, target)?;
    let model = decode_session_model(required(object, "model")?)?;
    let prompt_ids = array(required(object, "promptIds")?)?
        .iter()
        .map(scalar::<PromptId>)
        .collect::<Result<Vec<_>, _>>()?;
    let prompts = SessionPromptSelection::new(prompt_ids.clone())
        .map_err(|_| DurableStoreCodecError::InvalidSemantic)?;
    if prompts.enabled().iter().ne(prompt_ids.iter()) {
        return Err(DurableStoreCodecError::InvalidSemantic);
    }
    let created_at = scalar(required(object, "createdAt")?)?;
    Ok(SessionDefinition::new(
        session_id, revision, agent, workspace, model, prompts, created_at,
    ))
}

fn decode_agent_revision_ref(node: &JsonNode) -> Result<AgentRevisionRef, DurableStoreCodecError> {
    let object = object(node)?;
    exact_fields(object, &["agentId", "revision"])?;
    Ok(AgentRevisionRef::new(
        scalar(required(object, "agentId")?)?,
        scalar(required(object, "revision")?)?,
    ))
}

fn decode_workspace(
    node: &JsonNode,
    target: WorkspacePathTarget,
) -> Result<crate::workspace::Workspace, DurableStoreCodecError> {
    let object = object(node)?;
    exact_fields(
        object,
        &["revision", "primaryRoot", "additionalRoots", "cwd"],
    )?;
    let revision: WorkspaceRevision = scalar(required(object, "revision")?)?;
    let primary_root = decode_workspace_root(required(object, "primaryRoot")?)?;
    let additional_roots = array(required(object, "additionalRoots")?)?
        .iter()
        .map(decode_workspace_root)
        .collect::<Result<Vec<_>, _>>()?;
    let cwd = decode_workspace_cwd(required(object, "cwd")?)?;
    let input = WorkspaceDefinitionInput::new(primary_root, additional_roots, cwd)
        .map_err(|_| DurableStoreCodecError::InvalidSemantic)?;
    lower_workspace(input, revision, target).map_err(|_| DurableStoreCodecError::InvalidSemantic)
}

fn decode_workspace_root(node: &JsonNode) -> Result<WorkspaceRootInput, DurableStoreCodecError> {
    let object = object(node)?;
    exact_fields(object, &["key", "path", "requestedAccess", "sources"])?;
    let key = scalar(required(object, "key")?)?;
    let path = scalar(required(object, "path")?)?;
    let requested_access = decode_requested_access(required(object, "requestedAccess")?)?;
    let sources = decode_workspace_sources(required(object, "sources")?)?;
    Ok(WorkspaceRootInput::new(
        key,
        path,
        requested_access,
        sources,
    ))
}

fn decode_requested_access(
    node: &JsonNode,
) -> Result<RequestedFilesystemAccess, DurableStoreCodecError> {
    match string(node)? {
        "read_only" => Ok(RequestedFilesystemAccess::ReadOnly),
        "read_write" => Ok(RequestedFilesystemAccess::ReadWrite),
        _ => Err(DurableStoreCodecError::InvalidSemantic),
    }
}

fn decode_workspace_sources(
    node: &JsonNode,
) -> Result<WorkspaceSourcePolicy, DurableStoreCodecError> {
    let object = object(node)?;
    exact_fields(object, &["prompt", "skill"])?;
    Ok(WorkspaceSourcePolicy::new(
        boolean(required(object, "prompt")?)?,
        boolean(required(object, "skill")?)?,
    ))
}

fn decode_workspace_cwd(node: &JsonNode) -> Result<WorkspaceCwdSpec, DurableStoreCodecError> {
    let object = object(node)?;
    exact_fields(object, &["root", "relativePath"])?;
    Ok(WorkspaceCwdSpec::new(
        scalar(required(object, "root")?)?,
        scalar(required(object, "relativePath")?)?,
    ))
}

fn decode_session_model(node: &JsonNode) -> Result<SessionModelConfig, DurableStoreCodecError> {
    let object = object(node)?;
    exact_fields(object, &["selection", "reasoning", "maxOutputTokens"])?;
    let selection = decode_model_selection(required(object, "selection")?)?;
    let reasoning = decode_reasoning_preference(required(object, "reasoning")?)?;
    let max_output_tokens = nullable_nonzero_u32(required(object, "maxOutputTokens")?)?;
    Ok(SessionModelConfig::new(
        selection,
        reasoning,
        max_output_tokens,
    ))
}

fn decode_model_selection(node: &JsonNode) -> Result<ModelSelection, DurableStoreCodecError> {
    let object = object(node)?;
    exact_fields(object, &["providerId", "modelId"])?;
    Ok(ModelSelection::new(
        scalar(required(object, "providerId")?)?,
        scalar(required(object, "modelId")?)?,
    ))
}

fn decode_reasoning_preference(
    node: &JsonNode,
) -> Result<ReasoningPreference, DurableStoreCodecError> {
    match string(node)? {
        "auto" => Ok(ReasoningPreference::Auto),
        "disabled" => Ok(ReasoningPreference::Disabled),
        "low" => Ok(ReasoningPreference::Low),
        "medium" => Ok(ReasoningPreference::Medium),
        "high" => Ok(ReasoningPreference::High),
        _ => Err(DurableStoreCodecError::InvalidSemantic),
    }
}

fn nullable_nonzero_u32(node: &JsonNode) -> Result<Option<NonZeroU32>, DurableStoreCodecError> {
    if matches!(node, JsonNode::Null) {
        return Ok(None);
    }
    let raw = node
        .as_number()
        .map(|number| number.raw())
        .ok_or(DurableStoreCodecError::InvalidScalar)?;
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DurableStoreCodecError::InvalidScalar);
    }
    raw.parse::<u32>()
        .ok()
        .and_then(NonZeroU32::new)
        .map(Some)
        .ok_or(DurableStoreCodecError::InvalidSemantic)
}

fn decode_agent_head(node: &JsonNode) -> Result<DurableAgentHead, DurableStoreCodecError> {
    let object = object(node)?;
    exact_fields(
        object,
        &[
            "entity",
            "agentId",
            "storageGeneration",
            "previousStorageGeneration",
            "currentDefinition",
            "metadata",
            "status",
            "createdAt",
        ],
    )?;
    if string(required(object, "entity")?)? != "agent" {
        return Err(DurableStoreCodecError::InvalidSemantic);
    }
    let agent_id = scalar(required(object, "agentId")?)?;
    let storage_generation = generation(required(object, "storageGeneration")?)?;
    let previous_storage_generation =
        nullable_generation(required(object, "previousStorageGeneration")?)?;
    let (current_definition_revision, current_definition_storage_generation) =
        decode_agent_current_definition(required(object, "currentDefinition")?)?;
    let metadata = decode_agent_metadata(required(object, "metadata")?)?;
    let status = decode_status(required(object, "status")?)?;
    let created_at = scalar(required(object, "createdAt")?)?;
    DurableAgentHead::new(
        agent_id,
        storage_generation,
        previous_storage_generation,
        current_definition_revision,
        current_definition_storage_generation,
        metadata,
        status,
        created_at,
    )
    .map_err(|_| DurableStoreCodecError::InvalidSemantic)
}

fn decode_session_head(node: &JsonNode) -> Result<DurableSessionHead, DurableStoreCodecError> {
    let object = object(node)?;
    exact_fields(
        object,
        &[
            "entity",
            "sessionId",
            "storageGeneration",
            "previousStorageGeneration",
            "currentDefinition",
            "metadata",
            "lifecycle",
            "forkProvenance",
            "createdAt",
        ],
    )?;
    if string(required(object, "entity")?)? != "session" {
        return Err(DurableStoreCodecError::InvalidSemantic);
    }
    let session_id = scalar(required(object, "sessionId")?)?;
    let storage_generation = generation(required(object, "storageGeneration")?)?;
    let previous_storage_generation =
        nullable_generation(required(object, "previousStorageGeneration")?)?;
    let (current_definition_revision, current_definition_storage_generation) =
        decode_session_current_definition(required(object, "currentDefinition")?)?;
    let metadata = decode_session_metadata(required(object, "metadata")?)?;
    let lifecycle = decode_lifecycle(required(object, "lifecycle")?)?;
    let fork_provenance = decode_fork_provenance(required(object, "forkProvenance")?)?;
    let created_at = scalar(required(object, "createdAt")?)?;
    DurableSessionHead::new(
        session_id,
        storage_generation,
        previous_storage_generation,
        current_definition_revision,
        current_definition_storage_generation,
        metadata,
        lifecycle,
        fork_provenance,
        created_at,
    )
    .map_err(|_| DurableStoreCodecError::InvalidSemantic)
}

fn decode_session_current_definition(
    node: &JsonNode,
) -> Result<(SessionDefinitionRevision, StorageGeneration), DurableStoreCodecError> {
    let object = object(node)?;
    exact_fields(object, &["revision", "storageGeneration"])?;
    Ok((
        scalar(required(object, "revision")?)?,
        generation(required(object, "storageGeneration")?)?,
    ))
}

fn decode_session_metadata(node: &JsonNode) -> Result<SessionMetadata, DurableStoreCodecError> {
    let object = object(node)?;
    exact_fields(object, &["revision", "name", "description", "updatedAt"])?;
    let revision = scalar(required(object, "revision")?)?;
    let name = nullable_string(required(object, "name")?)?;
    let description = nullable_string(required(object, "description")?)?;
    let updated_at = scalar(required(object, "updatedAt")?)?;
    SessionMetadata::new(revision, name, description, updated_at)
        .map_err(|_| DurableStoreCodecError::InvalidSemantic)
}

fn decode_lifecycle(node: &JsonNode) -> Result<SessionLifecycle, DurableStoreCodecError> {
    match string(node)? {
        "open" => Ok(SessionLifecycle::Open),
        "archived" => Ok(SessionLifecycle::Archived),
        "deleted" => Ok(SessionLifecycle::Deleted),
        _ => Err(DurableStoreCodecError::InvalidSemantic),
    }
}

fn decode_fork_provenance(
    node: &JsonNode,
) -> Result<Option<SessionForkProvenance>, DurableStoreCodecError> {
    if matches!(node, JsonNode::Null) {
        return Ok(None);
    }
    let object = object(node)?;
    exact_fields(object, &["sourceSessionId", "source", "anchor"])?;
    let source_session_id = scalar(required(object, "sourceSessionId")?)?;
    let source = decode_fork_source_kind(required(object, "source")?)?;
    let anchor = decode_fork_anchor(required(object, "anchor")?)?;
    Ok(Some(SessionForkProvenance::new(
        source_session_id,
        source,
        anchor,
    )))
}

fn decode_fork_source_kind(node: &JsonNode) -> Result<ForkSourceKind, DurableStoreCodecError> {
    match string(node)? {
        "live_snapshot" => Ok(ForkSourceKind::LiveSnapshot),
        "recorded_history" => Ok(ForkSourceKind::RecordedHistory),
        _ => Err(DurableStoreCodecError::InvalidSemantic),
    }
}

fn decode_fork_anchor(node: &JsonNode) -> Result<ForkAnchor, DurableStoreCodecError> {
    let object = object(node)?;
    let tag = string(required(object, "type")?)?;
    if tag == "genesis" {
        exact_fields(object, &["type"])?;
        return Ok(ForkAnchor::Genesis);
    }
    exact_fields(object, &["type", "data"])?;
    let data = required(object, "data")?
        .as_object()
        .ok_or(DurableStoreCodecError::InvalidShape)?;
    exact_fields(data, &["itemId"])?;
    let item_id: ItemId = scalar(required(data, "itemId")?)?;
    match tag {
        "before_user_message" => Ok(ForkAnchor::BeforeUserMessage { item_id }),
        "after_user_message" => Ok(ForkAnchor::AfterUserMessage { item_id }),
        "before_final_agent_message" => Ok(ForkAnchor::BeforeFinalAgentMessage { item_id }),
        "after_final_agent_message" => Ok(ForkAnchor::AfterFinalAgentMessage { item_id }),
        _ => Err(DurableStoreCodecError::InvalidSemantic),
    }
}

fn decode_agent_current_definition(
    node: &JsonNode,
) -> Result<(AgentRevision, StorageGeneration), DurableStoreCodecError> {
    let object = object(node)?;
    exact_fields(object, &["revision", "storageGeneration"])?;
    Ok((
        scalar(required(object, "revision")?)?,
        generation(required(object, "storageGeneration")?)?,
    ))
}

fn decode_agent_metadata(node: &JsonNode) -> Result<AgentMetadata, DurableStoreCodecError> {
    let object = object(node)?;
    exact_fields(object, &["revision", "name", "description", "updatedAt"])?;
    let revision = scalar(required(object, "revision")?)?;
    let name = string(required(object, "name")?)?;
    let description = match required(object, "description")? {
        JsonNode::Null => None,
        node => Some(string(node)?),
    };
    let updated_at = scalar(required(object, "updatedAt")?)?;
    AgentMetadata::new(revision, name, description, updated_at)
        .map_err(|_| DurableStoreCodecError::InvalidSemantic)
}

fn decode_status(node: &JsonNode) -> Result<AgentStatus, DurableStoreCodecError> {
    match string(node)? {
        "enabled" => Ok(AgentStatus::Enabled),
        "disabled" => Ok(AgentStatus::Disabled),
        "deleted" => Ok(AgentStatus::Deleted),
        _ => Err(DurableStoreCodecError::InvalidSemantic),
    }
}

fn object(node: &JsonNode) -> Result<&BTreeMap<Box<str>, JsonNode>, DurableStoreCodecError> {
    node.as_object().ok_or(DurableStoreCodecError::InvalidShape)
}

fn array(node: &JsonNode) -> Result<&[JsonNode], DurableStoreCodecError> {
    node.as_array().ok_or(DurableStoreCodecError::InvalidShape)
}

fn string(node: &JsonNode) -> Result<&str, DurableStoreCodecError> {
    node.as_str().ok_or(DurableStoreCodecError::InvalidScalar)
}

fn boolean(node: &JsonNode) -> Result<bool, DurableStoreCodecError> {
    match node {
        JsonNode::Bool(value) => Ok(*value),
        _ => Err(DurableStoreCodecError::InvalidScalar),
    }
}

fn nullable_string(node: &JsonNode) -> Result<Option<&str>, DurableStoreCodecError> {
    if matches!(node, JsonNode::Null) {
        Ok(None)
    } else {
        string(node).map(Some)
    }
}

fn required<'a>(
    object: &'a BTreeMap<Box<str>, JsonNode>,
    field: &str,
) -> Result<&'a JsonNode, DurableStoreCodecError> {
    object
        .get(field)
        .ok_or(DurableStoreCodecError::InvalidShape)
}

fn exact_fields(
    object: &BTreeMap<Box<str>, JsonNode>,
    fields: &[&str],
) -> Result<(), DurableStoreCodecError> {
    if object.len() != fields.len() || object.keys().any(|key| !fields.contains(&key.as_ref())) {
        return Err(DurableStoreCodecError::InvalidShape);
    }
    Ok(())
}

fn scalar<T: FromStr>(node: &JsonNode) -> Result<T, DurableStoreCodecError> {
    string(node)?
        .parse()
        .map_err(|_| DurableStoreCodecError::InvalidScalar)
}

fn generation(node: &JsonNode) -> Result<StorageGeneration, DurableStoreCodecError> {
    let raw = node
        .as_number()
        .map(|number| number.raw())
        .ok_or(DurableStoreCodecError::InvalidScalar)?;
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DurableStoreCodecError::InvalidScalar);
    }
    raw.parse::<u32>()
        .ok()
        .and_then(StorageGeneration::new)
        .ok_or(DurableStoreCodecError::InvalidSemantic)
}

fn nullable_generation(
    node: &JsonNode,
) -> Result<Option<StorageGeneration>, DurableStoreCodecError> {
    if matches!(node, JsonNode::Null) {
        Ok(None)
    } else {
        generation(node).map(Some)
    }
}

fn optional_generation(
    writer: &mut DurableDocumentWriter,
    value: Option<StorageGeneration>,
) -> Result<(), DurableStoreCodecError> {
    match value {
        Some(value) => writer.u32(value.get()),
        None => writer.null(),
    }
}

fn encode_agent_revision_ref(
    writer: &mut DurableDocumentWriter,
    agent: AgentRevisionRef,
) -> Result<(), DurableStoreCodecError> {
    writer.object_start()?;
    writer.key("agentId")?;
    writer.string(&agent.agent_id().to_string())?;
    writer.comma()?;
    writer.key("revision")?;
    writer.string(&agent.revision().to_string())?;
    writer.object_end()
}

fn encode_workspace(
    writer: &mut DurableDocumentWriter,
    workspace: &crate::workspace::Workspace,
    target: WorkspacePathTarget,
) -> Result<(), DurableStoreCodecError> {
    writer.object_start()?;
    writer.key("revision")?;
    writer.string(&workspace.revision().to_string())?;
    writer.comma()?;
    writer.key("primaryRoot")?;
    encode_workspace_root(writer, workspace.primary_root(), target)?;
    writer.comma()?;
    writer.key("additionalRoots")?;
    writer.array_start()?;
    for (index, root) in workspace.additional_roots().iter().enumerate() {
        if index != 0 {
            writer.comma()?;
        }
        encode_workspace_root(writer, root, target)?;
    }
    writer.array_end()?;
    writer.comma()?;
    writer.key("cwd")?;
    writer.object_start()?;
    writer.key("root")?;
    writer.string(workspace.cwd().root().as_str())?;
    writer.comma()?;
    writer.key("relativePath")?;
    writer.string(workspace.cwd().relative_path().as_str())?;
    writer.object_end()?;
    writer.object_end()
}

fn encode_workspace_root(
    writer: &mut DurableDocumentWriter,
    root: &crate::workspace::WorkspaceRootSpec,
    target: WorkspacePathTarget,
) -> Result<(), DurableStoreCodecError> {
    let uri = uri_from_spec(root, target).map_err(|_| DurableStoreCodecError::InvalidSemantic)?;
    writer.object_start()?;
    writer.key("key")?;
    writer.string(root.key().as_str())?;
    writer.comma()?;
    writer.key("path")?;
    writer.string(uri.as_str())?;
    writer.comma()?;
    writer.key("requestedAccess")?;
    writer.string(requested_access_name(root.requested_access()))?;
    writer.comma()?;
    writer.key("sources")?;
    writer.object_start()?;
    writer.key("prompt")?;
    writer.boolean(root.sources().prompt())?;
    writer.comma()?;
    writer.key("skill")?;
    writer.boolean(root.sources().skill())?;
    writer.object_end()?;
    writer.object_end()
}

fn requested_access_name(access: RequestedFilesystemAccess) -> &'static str {
    match access {
        RequestedFilesystemAccess::ReadOnly => "read_only",
        RequestedFilesystemAccess::ReadWrite => "read_write",
    }
}

fn encode_session_model(
    writer: &mut DurableDocumentWriter,
    model: &SessionModelConfig,
) -> Result<(), DurableStoreCodecError> {
    writer.object_start()?;
    writer.key("selection")?;
    writer.object_start()?;
    writer.key("providerId")?;
    writer.string(model.selection().provider_id().as_str())?;
    writer.comma()?;
    writer.key("modelId")?;
    writer.string(model.selection().model_id().as_str())?;
    writer.object_end()?;
    writer.comma()?;
    writer.key("reasoning")?;
    writer.string(reasoning_name(model.reasoning()))?;
    writer.comma()?;
    writer.key("maxOutputTokens")?;
    match model.max_output_tokens() {
        Some(value) => writer.u32(value.get())?,
        None => writer.null()?,
    }
    writer.object_end()
}

fn reasoning_name(reasoning: ReasoningPreference) -> &'static str {
    match reasoning {
        ReasoningPreference::Auto => "auto",
        ReasoningPreference::Disabled => "disabled",
        ReasoningPreference::Low => "low",
        ReasoningPreference::Medium => "medium",
        ReasoningPreference::High => "high",
    }
}

fn encode_session_prompt_ids(
    writer: &mut DurableDocumentWriter,
    prompts: &SessionPromptSelection,
) -> Result<(), DurableStoreCodecError> {
    writer.array_start()?;
    for (index, prompt_id) in prompts.enabled().iter().enumerate() {
        if index != 0 {
            writer.comma()?;
        }
        writer.string(prompt_id.as_str())?;
    }
    writer.array_end()
}

fn encode_agent_metadata(
    writer: &mut DurableDocumentWriter,
    metadata: &AgentMetadata,
) -> Result<(), DurableStoreCodecError> {
    writer.object_start()?;
    writer.key("revision")?;
    writer.string(&metadata.revision().to_string())?;
    writer.comma()?;
    writer.key("name")?;
    writer.string(metadata.name())?;
    writer.comma()?;
    writer.key("description")?;
    match metadata.description() {
        Some(value) => writer.string(value)?,
        None => writer.null()?,
    }
    writer.comma()?;
    writer.key("updatedAt")?;
    writer.string(&metadata.updated_at().to_string())?;
    writer.object_end()
}

fn encode_session_metadata(
    writer: &mut DurableDocumentWriter,
    metadata: &SessionMetadata,
) -> Result<(), DurableStoreCodecError> {
    writer.object_start()?;
    writer.key("revision")?;
    writer.string(&metadata.revision().to_string())?;
    writer.comma()?;
    writer.key("name")?;
    match metadata.name() {
        Some(value) => writer.string(value)?,
        None => writer.null()?,
    }
    writer.comma()?;
    writer.key("description")?;
    match metadata.description() {
        Some(value) => writer.string(value)?,
        None => writer.null()?,
    }
    writer.comma()?;
    writer.key("updatedAt")?;
    writer.string(&metadata.updated_at().to_string())?;
    writer.object_end()
}

fn status_name(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Enabled => "enabled",
        AgentStatus::Disabled => "disabled",
        AgentStatus::Deleted => "deleted",
    }
}

fn lifecycle_name(lifecycle: SessionLifecycle) -> &'static str {
    match lifecycle {
        SessionLifecycle::Open => "open",
        SessionLifecycle::Archived => "archived",
        SessionLifecycle::Deleted => "deleted",
    }
}

fn encode_fork_provenance(
    writer: &mut DurableDocumentWriter,
    provenance: Option<&SessionForkProvenance>,
) -> Result<(), DurableStoreCodecError> {
    let Some(provenance) = provenance else {
        return writer.null();
    };
    writer.object_start()?;
    writer.key("sourceSessionId")?;
    writer.string(&provenance.source_session_id().to_string())?;
    writer.comma()?;
    writer.key("source")?;
    writer.string(fork_source_kind_name(provenance.source()))?;
    writer.comma()?;
    writer.key("anchor")?;
    encode_fork_anchor(writer, provenance.anchor())?;
    writer.object_end()
}

fn fork_source_kind_name(source: ForkSourceKind) -> &'static str {
    match source {
        ForkSourceKind::LiveSnapshot => "live_snapshot",
        ForkSourceKind::RecordedHistory => "recorded_history",
    }
}

fn encode_fork_anchor(
    writer: &mut DurableDocumentWriter,
    anchor: &ForkAnchor,
) -> Result<(), DurableStoreCodecError> {
    writer.object_start()?;
    writer.key("type")?;
    match anchor {
        ForkAnchor::Genesis => {
            writer.string("genesis")?;
        }
        ForkAnchor::BeforeUserMessage { item_id } => {
            encode_item_anchor_data(writer, "before_user_message", *item_id)?;
        }
        ForkAnchor::AfterUserMessage { item_id } => {
            encode_item_anchor_data(writer, "after_user_message", *item_id)?;
        }
        ForkAnchor::BeforeFinalAgentMessage { item_id } => {
            encode_item_anchor_data(writer, "before_final_agent_message", *item_id)?;
        }
        ForkAnchor::AfterFinalAgentMessage { item_id } => {
            encode_item_anchor_data(writer, "after_final_agent_message", *item_id)?;
        }
    }
    writer.object_end()
}

fn encode_item_anchor_data(
    writer: &mut DurableDocumentWriter,
    tag: &str,
    item_id: ItemId,
) -> Result<(), DurableStoreCodecError> {
    writer.string(tag)?;
    writer.comma()?;
    writer.key("data")?;
    writer.object_start()?;
    writer.key("itemId")?;
    writer.string(&item_id.to_string())?;
    writer.object_end()
}

struct DurableDocumentWriter {
    bytes: Vec<u8>,
}

impl DurableDocumentWriter {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn finish(mut self) -> Result<Vec<u8>, DurableStoreCodecError> {
        self.push(b"\n")?;
        Ok(self.bytes)
    }

    fn object_start(&mut self) -> Result<(), DurableStoreCodecError> {
        self.push(b"{")
    }

    fn object_end(&mut self) -> Result<(), DurableStoreCodecError> {
        self.push(b"}")
    }

    fn array_start(&mut self) -> Result<(), DurableStoreCodecError> {
        self.push(b"[")
    }

    fn array_end(&mut self) -> Result<(), DurableStoreCodecError> {
        self.push(b"]")
    }

    fn comma(&mut self) -> Result<(), DurableStoreCodecError> {
        self.push(b",")
    }

    fn key(&mut self, value: &str) -> Result<(), DurableStoreCodecError> {
        self.string(value)?;
        self.push(b":")
    }

    fn null(&mut self) -> Result<(), DurableStoreCodecError> {
        self.push(b"null")
    }

    fn boolean(&mut self, value: bool) -> Result<(), DurableStoreCodecError> {
        self.push(if value { b"true" } else { b"false" })
    }

    fn u32(&mut self, value: u32) -> Result<(), DurableStoreCodecError> {
        self.push(value.to_string().as_bytes())
    }

    fn string(&mut self, value: &str) -> Result<(), DurableStoreCodecError> {
        self.push(b"\"")?;
        for character in value.chars() {
            match character {
                '"' => self.push(b"\\\"")?,
                '\\' => self.push(b"\\\\")?,
                '\u{0008}' => self.push(b"\\b")?,
                '\t' => self.push(b"\\t")?,
                '\n' => self.push(b"\\n")?,
                '\u{000c}' => self.push(b"\\f")?,
                '\r' => self.push(b"\\r")?,
                '\u{0000}'..='\u{001f}' => {
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    let value = u32::from(character) as u8;
                    self.push(&[
                        b'\\',
                        b'u',
                        b'0',
                        b'0',
                        HEX[usize::from(value >> 4)],
                        HEX[usize::from(value & 0x0f)],
                    ])?;
                }
                _ => {
                    let mut buffer = [0; 4];
                    self.push(character.encode_utf8(&mut buffer).as_bytes())?;
                }
            }
        }
        self.push(b"\"")
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), DurableStoreCodecError> {
        let next = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or(DurableStoreCodecError::DocumentTooLarge)?;
        if next > MAX_DURABLE_DOCUMENT_BYTES {
            return Err(DurableStoreCodecError::DocumentTooLarge);
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DurableStoreCodecError, DurableStoreV1Codec, MAX_DURABLE_DOCUMENT_BODY_BYTES,
        MAX_DURABLE_DOCUMENT_BYTES, parse_document,
    };
    use crate::agent_session_lifecycle::AgentStatus;
    use crate::durable_state::{DurableAgentHead, StorageGeneration};
    use std::error::Error as _;

    fn fixture(name: &str) -> Vec<u8> {
        match name {
            "agent-definition.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "agent-definition.json"
            ))
            .to_vec(),
            "agent-head.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "agent-head.json"
            ))
            .to_vec(),
            "agent-definition-2.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "agent-definition-2.json"
            ))
            .to_vec(),
            "agent-head-2-definition.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "agent-head-2-definition.json"
            ))
            .to_vec(),
            "agent-head-2-metadata.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "agent-head-2-metadata.json"
            ))
            .to_vec(),
            "agent-head-2-status.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "agent-head-2-status.json"
            ))
            .to_vec(),
            "session-definition.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "session-definition.json"
            ))
            .to_vec(),
            "fork-session-definition.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "fork-session-definition.json"
            ))
            .to_vec(),
            "genesis-fork-session-definition.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "genesis-fork-session-definition.json"
            ))
            .to_vec(),
            "session-definition-2.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "session-definition-2.json"
            ))
            .to_vec(),
            "session-definition-2-workspace.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "session-definition-2-workspace.json"
            ))
            .to_vec(),
            "session-head.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "session-head.json"
            ))
            .to_vec(),
            "fork-session-head.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "fork-session-head.json"
            ))
            .to_vec(),
            "genesis-fork-session-head.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "genesis-fork-session-head.json"
            ))
            .to_vec(),
            "session-head-2-definition.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "session-head-2-definition.json"
            ))
            .to_vec(),
            "session-head-2-workspace-definition.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "session-head-2-workspace-definition.json"
            ))
            .to_vec(),
            "session-head-2-metadata.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "session-head-2-metadata.json"
            ))
            .to_vec(),
            "session-head-2-lifecycle.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "session-head-2-lifecycle.json"
            ))
            .to_vec(),
            "session-head-3-unarchive.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "session-head-3-unarchive.json"
            ))
            .to_vec(),
            "session-head-3-deleted.json" => include_bytes!(concat!(
                "../../docs/fixtures/durable-store-v1/",
                "session-head-3-deleted.json"
            ))
            .to_vec(),
            _ => panic!("unknown durable fixture"),
        }
    }

    fn replace(input: &[u8], from: &str, to: &str) -> Vec<u8> {
        let input = std::str::from_utf8(input).unwrap();
        assert_eq!(
            input.matches(from).count(),
            1,
            "fixture replacement must be unique"
        );
        input.replacen(from, to, 1).into_bytes()
    }

    fn decode_session_definition_posix(
        input: &[u8],
    ) -> Result<crate::agent_session_lifecycle::SessionDefinition, DurableStoreCodecError> {
        DurableStoreV1Codec::decode_session_definition_for_target(
            input,
            crate::workspace::WorkspacePathTarget::Posix,
        )
    }

    #[cfg(not(windows))]
    #[test]
    fn production_session_definition_codec_uses_the_current_posix_target() {
        let bytes = fixture("session-definition.json");
        let definition = DurableStoreV1Codec::decode_session_definition(&bytes).unwrap();
        assert_eq!(
            DurableStoreV1Codec::encode_session_definition(&definition).unwrap(),
            bytes
        );
    }

    #[cfg(windows)]
    #[test]
    fn production_session_definition_codec_uses_the_current_windows_target() {
        let bytes = replace(
            &fixture("session-definition.json"),
            "file:///Users/example/project",
            "file:///C:/work/project",
        );
        let definition = DurableStoreV1Codec::decode_session_definition(&bytes).unwrap();
        assert_eq!(
            DurableStoreV1Codec::encode_session_definition(&definition).unwrap(),
            bytes
        );
    }

    #[test]
    fn agent_documents_byte_round_trip_the_authoritative_literals() {
        for (name, revision, prompts) in [
            ("agent-definition.json", 1, vec!["base", "safety"]),
            (
                "agent-definition-2.json",
                2,
                vec!["base", "code-review", "safety"],
            ),
        ] {
            let bytes = fixture(name);
            let decoded = DurableStoreV1Codec::decode_agent_definition(&bytes).unwrap();
            assert_eq!(decoded.revision().get(), revision);
            assert_eq!(
                decoded
                    .prompts()
                    .enabled()
                    .iter()
                    .map(|prompt| prompt.as_str())
                    .collect::<Vec<_>>(),
                prompts
            );
            assert_eq!(
                DurableStoreV1Codec::encode_agent_definition(&decoded).unwrap(),
                bytes,
                "{name} must byte-round-trip"
            );
        }
        for (
            name,
            generation,
            definition_revision,
            definition_generation,
            metadata_revision,
            status,
        ) in [
            ("agent-head.json", 1, 1, 1, 1, AgentStatus::Enabled),
            (
                "agent-head-2-definition.json",
                2,
                2,
                2,
                1,
                AgentStatus::Enabled,
            ),
            (
                "agent-head-2-metadata.json",
                2,
                1,
                1,
                2,
                AgentStatus::Enabled,
            ),
            (
                "agent-head-2-status.json",
                2,
                1,
                1,
                1,
                AgentStatus::Disabled,
            ),
        ] {
            let bytes = fixture(name);
            let decoded = DurableStoreV1Codec::decode_agent_head(&bytes).unwrap();
            assert_eq!(decoded.storage_generation().get(), generation);
            assert_eq!(
                decoded.current_definition_revision().get(),
                definition_revision
            );
            assert_eq!(
                decoded.current_definition_storage_generation().get(),
                definition_generation
            );
            assert_eq!(decoded.metadata().revision().get(), metadata_revision);
            assert_eq!(decoded.status(), status);
            assert_eq!(
                DurableStoreV1Codec::encode_agent_head(&decoded).unwrap(),
                bytes,
                "{name} must byte-round-trip"
            );
        }
    }

    #[test]
    fn session_documents_byte_round_trip_all_authoritative_literals_with_literal_semantics() {
        let target = crate::workspace::WorkspacePathTarget::Posix;
        for (name, session_id, revision, workspace_revision, cwd, max_output_tokens) in [
            (
                "session-definition.json",
                "ses_22222222222222222222222222222222",
                1,
                1,
                "src",
                Some(4096),
            ),
            (
                "fork-session-definition.json",
                "ses_33333333333333333333333333333333",
                1,
                1,
                "src",
                Some(4096),
            ),
            (
                "genesis-fork-session-definition.json",
                "ses_44444444444444444444444444444444",
                1,
                1,
                "src",
                Some(4096),
            ),
            (
                "session-definition-2.json",
                "ses_22222222222222222222222222222222",
                2,
                1,
                "src",
                Some(8192),
            ),
            (
                "session-definition-2-workspace.json",
                "ses_22222222222222222222222222222222",
                2,
                2,
                "tests",
                Some(4096),
            ),
        ] {
            let bytes = fixture(name);
            let definition =
                DurableStoreV1Codec::decode_session_definition_for_target(&bytes, target).unwrap();
            assert_eq!(definition.session_id().to_string(), session_id);
            assert_eq!(definition.revision().get(), revision);
            assert_eq!(
                definition.agent().agent_id().to_string(),
                "agt_11111111111111111111111111111111"
            );
            assert_eq!(definition.agent().revision().get(), 1);
            assert_eq!(definition.workspace().revision().get(), workspace_revision);
            assert_eq!(
                definition.workspace().primary_root().path(),
                std::path::Path::new("/Users/example/project")
            );
            assert_eq!(
                definition.workspace().primary_root().requested_access(),
                crate::workspace::RequestedFilesystemAccess::ReadWrite
            );
            assert!(definition.workspace().primary_root().sources().prompt());
            assert!(definition.workspace().primary_root().sources().skill());
            assert!(definition.workspace().additional_roots().is_empty());
            assert_eq!(definition.workspace().cwd().root().as_str(), "repo");
            assert_eq!(definition.workspace().cwd().relative_path().as_str(), cwd);
            assert_eq!(
                definition
                    .model()
                    .max_output_tokens()
                    .map(std::num::NonZeroU32::get),
                max_output_tokens
            );
            assert_eq!(
                definition.model().selection().provider_id().as_str(),
                "openai"
            );
            assert_eq!(definition.model().selection().model_id().as_str(), "gpt-5");
            assert_eq!(
                definition
                    .prompts()
                    .enabled()
                    .iter()
                    .map(|prompt| prompt.as_str())
                    .collect::<Vec<_>>(),
                ["base", "session-notes"]
            );
            assert_eq!(
                DurableStoreV1Codec::encode_session_definition_for_target(&definition, target)
                    .unwrap(),
                bytes,
                "{name} must byte-round-trip"
            );
        }

        for (
            name,
            generation,
            definition_revision,
            definition_generation,
            metadata_revision,
            lifecycle,
        ) in [
            (
                "session-head.json",
                1,
                1,
                1,
                1,
                crate::agent_session_lifecycle::SessionLifecycle::Open,
            ),
            (
                "fork-session-head.json",
                1,
                1,
                1,
                1,
                crate::agent_session_lifecycle::SessionLifecycle::Open,
            ),
            (
                "genesis-fork-session-head.json",
                1,
                1,
                1,
                1,
                crate::agent_session_lifecycle::SessionLifecycle::Open,
            ),
            (
                "session-head-2-definition.json",
                2,
                2,
                2,
                1,
                crate::agent_session_lifecycle::SessionLifecycle::Open,
            ),
            (
                "session-head-2-workspace-definition.json",
                2,
                2,
                2,
                1,
                crate::agent_session_lifecycle::SessionLifecycle::Open,
            ),
            (
                "session-head-2-metadata.json",
                2,
                1,
                1,
                2,
                crate::agent_session_lifecycle::SessionLifecycle::Open,
            ),
            (
                "session-head-2-lifecycle.json",
                2,
                1,
                1,
                1,
                crate::agent_session_lifecycle::SessionLifecycle::Archived,
            ),
            (
                "session-head-3-unarchive.json",
                3,
                1,
                1,
                1,
                crate::agent_session_lifecycle::SessionLifecycle::Open,
            ),
            (
                "session-head-3-deleted.json",
                3,
                1,
                1,
                1,
                crate::agent_session_lifecycle::SessionLifecycle::Deleted,
            ),
        ] {
            let bytes = fixture(name);
            let head = DurableStoreV1Codec::decode_session_head(&bytes).unwrap();
            let expected_session_id = match name {
                "fork-session-head.json" => "ses_33333333333333333333333333333333",
                "genesis-fork-session-head.json" => "ses_44444444444444444444444444444444",
                _ => "ses_22222222222222222222222222222222",
            };
            assert_eq!(head.session_id().to_string(), expected_session_id);
            assert_eq!(head.storage_generation().get(), generation);
            assert_eq!(
                head.previous_storage_generation().map(|value| value.get()),
                (generation > 1).then_some(generation - 1)
            );
            assert_eq!(
                head.current_definition_revision().get(),
                definition_revision
            );
            assert_eq!(
                head.current_definition_storage_generation().get(),
                definition_generation
            );
            assert_eq!(head.metadata().revision().get(), metadata_revision);
            assert_eq!(head.lifecycle(), lifecycle);
            assert_eq!(
                DurableStoreV1Codec::encode_session_head(&head).unwrap(),
                bytes,
                "{name} must byte-round-trip"
            );
        }

        let metadata_head =
            DurableStoreV1Codec::decode_session_head(&fixture("session-head-2-metadata.json"))
                .unwrap();
        assert_eq!(metadata_head.metadata().name(), Some("Project session"));
        assert_eq!(metadata_head.metadata().description(), None);

        let fork =
            DurableStoreV1Codec::decode_session_head(&fixture("fork-session-head.json")).unwrap();
        let provenance = fork.fork_provenance().unwrap();
        assert_eq!(
            provenance.source_session_id().to_string(),
            "ses_22222222222222222222222222222222"
        );
        assert_eq!(
            provenance.source(),
            crate::agent_session_lifecycle::ForkSourceKind::RecordedHistory
        );
        assert!(matches!(
            provenance.anchor(),
            crate::agent_session_lifecycle::ForkAnchor::AfterUserMessage { item_id }
                if item_id.to_string() == "itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        ));

        let genesis =
            DurableStoreV1Codec::decode_session_head(&fixture("genesis-fork-session-head.json"))
                .unwrap();
        let provenance = genesis.fork_provenance().unwrap();
        assert_eq!(
            provenance.source(),
            crate::agent_session_lifecycle::ForkSourceKind::RecordedHistory
        );
        assert!(matches!(
            provenance.anchor(),
            crate::agent_session_lifecycle::ForkAnchor::Genesis
        ));
    }

    #[test]
    fn session_documents_reject_noncanonical_order_duplicate_and_closed_shapes() {
        let definition = fixture("session-definition.json");
        let head = fixture("session-head.json");
        let wrong_top_order = replace(
            &definition,
            r#"{"sessionId":"ses_22222222222222222222222222222222","revision":"sdr_1""#,
            r#"{"revision":"sdr_1","sessionId":"ses_22222222222222222222222222222222""#,
        );
        let wrong_nested_order = replace(
            &definition,
            r#"{"agentId":"agt_11111111111111111111111111111111","revision":"ar_1"}"#,
            r#"{"revision":"ar_1","agentId":"agt_11111111111111111111111111111111"}"#,
        );
        assert_eq!(
            decode_session_definition_posix(&wrong_top_order),
            Err(DurableStoreCodecError::Noncanonical)
        );
        assert_eq!(
            decode_session_definition_posix(&wrong_nested_order),
            Err(DurableStoreCodecError::Noncanonical)
        );

        let duplicate_top = replace(
            &definition,
            r#""revision":"sdr_1""#,
            r#""revision":"sdr_1","revision":"sdr_1""#,
        );
        let duplicate_nested = replace(
            &definition,
            r#""providerId":"openai""#,
            r#""providerId":"openai","providerId":"openai""#,
        );
        assert_eq!(
            decode_session_definition_posix(&duplicate_top),
            Err(DurableStoreCodecError::InvalidDocument)
        );
        assert_eq!(
            decode_session_definition_posix(&duplicate_nested),
            Err(DurableStoreCodecError::InvalidDocument)
        );

        let unknown_top = replace(
            &definition,
            r#""createdAt":"2026-08-03T10:01:00.456Z"}"#,
            r#""createdAt":"2026-08-03T10:01:00.456Z","future":true}"#,
        );
        let unknown_nested = replace(
            &definition,
            r#""prompt":true,"skill":true}"#,
            r#""prompt":true,"skill":true,"future":true}"#,
        );
        assert_eq!(
            decode_session_definition_posix(&unknown_top),
            Err(DurableStoreCodecError::InvalidShape)
        );
        assert_eq!(
            decode_session_definition_posix(&unknown_nested),
            Err(DurableStoreCodecError::InvalidShape)
        );
        assert_eq!(
            DurableStoreV1Codec::decode_session_head(&replace(
                &head,
                r#""lifecycle":"open""#,
                r#""lifecycle":true"#,
            )),
            Err(DurableStoreCodecError::InvalidScalar)
        );
        assert_eq!(
            decode_session_definition_posix(&replace(
                &definition,
                r#""sessionId":"ses_22222222222222222222222222222222""#,
                r#""sessionId":1"#,
            )),
            Err(DurableStoreCodecError::InvalidScalar)
        );
    }

    #[test]
    fn session_documents_reject_invalid_typed_scalar_text() {
        let definition = fixture("session-definition.json");
        for invalid in [
            replace(
                &definition,
                "ses_22222222222222222222222222222222",
                "ses_invalid",
            ),
            replace(
                &definition,
                r#""revision":"sdr_1""#,
                r#""revision":"sdr_0""#,
            ),
            replace(&definition, r#""revision":"ar_1""#, r#""revision":"ar_0""#),
            replace(&definition, r#""revision":"wr_1""#, r#""revision":"wr_0""#),
            replace(
                &definition,
                "2026-08-03T10:01:00.456Z",
                "2026-08-03T10:01:00Z",
            ),
        ] {
            assert_eq!(
                decode_session_definition_posix(&invalid),
                Err(DurableStoreCodecError::InvalidScalar)
            );
        }

        let head = fixture("session-head.json");
        assert_eq!(
            DurableStoreV1Codec::decode_session_head(&replace(
                &head,
                r#""revision":"smr_1""#,
                r#""revision":"smr_0""#,
            )),
            Err(DurableStoreCodecError::InvalidScalar)
        );

        let fork = fixture("fork-session-head.json");
        for invalid in [
            replace(&fork, "ses_22222222222222222222222222222222", "ses_invalid"),
            replace(&fork, "itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "itm_invalid"),
        ] {
            assert_eq!(
                DurableStoreV1Codec::decode_session_head(&invalid),
                Err(DurableStoreCodecError::InvalidScalar)
            );
        }
    }

    #[test]
    fn session_definition_variants_and_model_token_scalars_are_closed() {
        let definition = fixture("session-definition.json");
        let head = fixture("session-head.json");
        let fork_head = fixture("fork-session-head.json");

        for reasoning in ["auto", "disabled", "low", "medium", "high"] {
            assert!(
                decode_session_definition_posix(&replace(
                    &definition,
                    r#""reasoning":"auto""#,
                    &format!(r#""reasoning":"{reasoning}""#),
                ))
                .is_ok()
            );
        }
        for access in ["read_only", "read_write"] {
            assert!(
                decode_session_definition_posix(&replace(
                    &definition,
                    r#""requestedAccess":"read_write""#,
                    &format!(r#""requestedAccess":"{access}""#),
                ))
                .is_ok()
            );
        }
        for lifecycle in ["open", "archived", "deleted"] {
            assert!(
                DurableStoreV1Codec::decode_session_head(&replace(
                    &head,
                    r#""lifecycle":"open""#,
                    &format!(r#""lifecycle":"{lifecycle}""#),
                ))
                .is_ok()
            );
        }
        for source in ["live_snapshot", "recorded_history"] {
            assert!(
                DurableStoreV1Codec::decode_session_head(&replace(
                    &fork_head,
                    r#""source":"recorded_history""#,
                    &format!(r#""source":"{source}""#),
                ))
                .is_ok()
            );
        }

        for (field, unknown) in [
            (r#""reasoning":"auto""#, r#""reasoning":"future""#),
            (
                r#""requestedAccess":"read_write""#,
                r#""requestedAccess":"write_once""#,
            ),
        ] {
            assert_eq!(
                decode_session_definition_posix(&replace(&definition, field, unknown)),
                Err(DurableStoreCodecError::InvalidSemantic)
            );
        }
        for (field, unknown) in [
            (r#""lifecycle":"open""#, r#""lifecycle":"future""#),
            (r#""source":"recorded_history""#, r#""source":"future""#),
        ] {
            assert_eq!(
                DurableStoreV1Codec::decode_session_head(&replace(
                    if field.contains("lifecycle") {
                        &head
                    } else {
                        &fork_head
                    },
                    field,
                    unknown,
                )),
                Err(DurableStoreCodecError::InvalidSemantic)
            );
        }

        for (literal, expected) in [
            ("null", None),
            ("1", Some(1)),
            ("4294967295", Some(u32::MAX)),
        ] {
            let decoded = decode_session_definition_posix(&replace(
                &definition,
                r#""maxOutputTokens":4096"#,
                &format!(r#""maxOutputTokens":{literal}"#),
            ))
            .unwrap();
            assert_eq!(
                decoded
                    .model()
                    .max_output_tokens()
                    .map(std::num::NonZeroU32::get),
                expected
            );
        }
        for literal in ["0", "-1", "1.0", "1e3", r#""1""#, "4294967296"] {
            assert!(
                decode_session_definition_posix(&replace(
                    &definition,
                    r#""maxOutputTokens":4096"#,
                    &format!(r#""maxOutputTokens":{literal}"#),
                ))
                .is_err()
            );
        }
    }

    #[test]
    fn session_workspace_prompt_and_metadata_validation_are_exact() {
        let definition = fixture("session-definition.json");
        let head = fixture("session-head.json");
        for field in [r#""prompt":true"#, r#""skill":true"#] {
            assert!(
                decode_session_definition_posix(&replace(
                    &definition,
                    field,
                    &field.replace("true", "false"),
                ))
                .is_ok()
            );
            for wrong in ["1", r#""true""#, "null"] {
                let key = field.split(':').next().unwrap();
                assert_eq!(
                    decode_session_definition_posix(&replace(
                        &definition,
                        field,
                        &format!("{key}:{wrong}"),
                    )),
                    Err(DurableStoreCodecError::InvalidScalar)
                );
            }
        }

        let duplicate_key_root = r#"[{"key":"repo","path":"file:///Users/example/other","requestedAccess":"read_write","sources":{"prompt":true,"skill":true}}]"#;
        let duplicate_uri_root = r#"[{"key":"other","path":"file:///Users/example/project","requestedAccess":"read_write","sources":{"prompt":true,"skill":true}}]"#;
        for roots in [duplicate_key_root, duplicate_uri_root] {
            assert_eq!(
                decode_session_definition_posix(&replace(
                    &definition,
                    r#""additionalRoots":[]"#,
                    &format!(r#""additionalRoots":{roots}"#)
                )),
                Err(DurableStoreCodecError::InvalidSemantic)
            );
        }
        for invalid in [
            replace(&definition, r#""root":"repo""#, r#""root":"missing""#),
            replace(
                &definition,
                r#"["base","session-notes"]"#,
                r#"["session-notes","base"]"#,
            ),
            replace(
                &definition,
                r#"["base","session-notes"]"#,
                r#"["base","base"]"#,
            ),
            replace(
                &definition,
                "file:///Users/example/project",
                "file:///Users/../example/project",
            ),
        ] {
            assert!(decode_session_definition_posix(&invalid).is_err());
        }
        assert_eq!(
            decode_session_definition_posix(&replace(
                &definition,
                "file:///Users/example/project",
                "file:///C:/work/project",
            )),
            Err(DurableStoreCodecError::InvalidSemantic)
        );

        assert!(DurableStoreV1Codec::decode_session_head(&head).is_ok());
        assert!(
            DurableStoreV1Codec::decode_session_head(&replace(
                &head,
                r#""description":null"#,
                r#""description":"""#,
            ))
            .is_ok()
        );
        assert_eq!(
            DurableStoreV1Codec::decode_session_head(&replace(
                &head,
                r#""name":null"#,
                r#""name":"""#,
            )),
            Err(DurableStoreCodecError::InvalidSemantic)
        );
        let limits = crate::wire::ProtocolLimits::v1_0().text;
        let max_name = "é".repeat(usize::from(limits.max_display_name_bytes) / "é".len());
        let max_description = "é".repeat(
            usize::try_from(limits.max_description_bytes).unwrap_or(usize::MAX) / "é".len(),
        );
        for (field, value) in [
            ("name", max_name.as_str()),
            ("description", max_description.as_str()),
        ] {
            assert!(
                DurableStoreV1Codec::decode_session_head(&replace(
                    &head,
                    &format!(r#""{field}":null"#),
                    &format!(r#""{field}":"{value}""#),
                ))
                .is_ok()
            );
            assert_eq!(
                DurableStoreV1Codec::decode_session_head(&replace(
                    &head,
                    &format!(r#""{field}":null"#),
                    &format!(r#""{field}":"{value}x""#),
                )),
                Err(DurableStoreCodecError::InvalidSemantic)
            );
        }
        for field in ["name", "description"] {
            assert_eq!(
                DurableStoreV1Codec::decode_session_head(&replace(
                    &head,
                    &format!(r#""{field}":null"#),
                    &format!(r#""{field}":"unsafe\u001b""#),
                )),
                Err(DurableStoreCodecError::InvalidSemantic)
            );
        }
    }

    #[test]
    fn session_head_generation_and_fork_document_invariants_are_local_and_closed() {
        let generation_one = fixture("session-head.json");
        let generation_two = fixture("session-head-2-definition.json");
        let generation_three = fixture("session-head-3-unarchive.json");
        let fork = fixture("fork-session-head.json");
        for invalid in [
            replace(
                &generation_one,
                r#""previousStorageGeneration":null"#,
                r#""previousStorageGeneration":1"#,
            ),
            replace(
                &generation_two,
                r#""previousStorageGeneration":1"#,
                r#""previousStorageGeneration":null"#,
            ),
            replace(
                &generation_two,
                r#""previousStorageGeneration":1"#,
                r#""previousStorageGeneration":2"#,
            ),
            replace(
                &generation_three,
                r#""previousStorageGeneration":2"#,
                r#""previousStorageGeneration":1"#,
            ),
            replace(
                &generation_three,
                r#""previousStorageGeneration":2"#,
                r#""previousStorageGeneration":null"#,
            ),
            replace(
                &generation_one,
                r#""revision":"sdr_1","storageGeneration":1"#,
                r#""revision":"sdr_1","storageGeneration":0"#,
            ),
            replace(
                &generation_one,
                r#""revision":"sdr_1","storageGeneration":1"#,
                r#""revision":"sdr_1","storageGeneration":2"#,
            ),
            replace(
                &fork,
                "ses_22222222222222222222222222222222",
                "ses_33333333333333333333333333333333",
            ),
        ] {
            assert_eq!(
                DurableStoreV1Codec::decode_session_head(&invalid),
                Err(DurableStoreCodecError::InvalidSemantic)
            );
        }
    }

    #[test]
    fn session_fork_anchor_encoding_is_exact_for_genesis_payloads_and_sources() {
        let fork = fixture("fork-session-head.json");
        let genesis = fixture("genesis-fork-session-head.json");
        assert!(
            !std::str::from_utf8(&genesis).unwrap().contains(r#""data""#),
            "Genesis must omit anchor data"
        );
        assert_eq!(
            DurableStoreV1Codec::encode_session_head(
                &DurableStoreV1Codec::decode_session_head(&genesis).unwrap(),
            )
            .unwrap(),
            genesis
        );
        for tag in [
            "before_user_message",
            "after_user_message",
            "before_final_agent_message",
            "after_final_agent_message",
        ] {
            let bytes = replace(
                &fork,
                r#""type":"after_user_message""#,
                &format!(r#""type":"{tag}""#),
            );
            let head = DurableStoreV1Codec::decode_session_head(&bytes).unwrap();
            let anchor = head.fork_provenance().unwrap().anchor();
            match tag {
                "before_user_message" => assert!(matches!(
                    anchor,
                    crate::agent_session_lifecycle::ForkAnchor::BeforeUserMessage { .. }
                )),
                "after_user_message" => assert!(matches!(
                    anchor,
                    crate::agent_session_lifecycle::ForkAnchor::AfterUserMessage { .. }
                )),
                "before_final_agent_message" => assert!(matches!(
                    anchor,
                    crate::agent_session_lifecycle::ForkAnchor::BeforeFinalAgentMessage { .. }
                )),
                "after_final_agent_message" => assert!(matches!(
                    anchor,
                    crate::agent_session_lifecycle::ForkAnchor::AfterFinalAgentMessage { .. }
                )),
                _ => unreachable!("closed anchor tag test matrix"),
            }
            assert_eq!(
                DurableStoreV1Codec::encode_session_head(&head).unwrap(),
                bytes
            );
        }
        for source in ["live_snapshot", "recorded_history"] {
            let bytes = replace(
                &fork,
                r#""source":"recorded_history""#,
                &format!(r#""source":"{source}""#),
            );
            let head = DurableStoreV1Codec::decode_session_head(&bytes).unwrap();
            let actual = head.fork_provenance().unwrap().source();
            let expected = match source {
                "live_snapshot" => crate::agent_session_lifecycle::ForkSourceKind::LiveSnapshot,
                "recorded_history" => {
                    crate::agent_session_lifecycle::ForkSourceKind::RecordedHistory
                }
                _ => unreachable!("closed source test matrix"),
            };
            assert_eq!(actual, expected);
        }
        for invalid in [
            replace(
                &genesis,
                r#""type":"genesis""#,
                r#""type":"genesis","data":null"#,
            ),
            replace(
                &genesis,
                r#""type":"genesis""#,
                r#""type":"genesis","data":{}"#,
            ),
            replace(
                &fork,
                r#","data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#,
                "",
            ),
            replace(
                &fork,
                r#""data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#,
                r#""data":null"#,
            ),
            replace(
                &fork,
                r#""data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#,
                r#""data":{}"#,
            ),
            replace(
                &fork,
                r#""data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#,
                r#""data":{"itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","future":true}"#,
            ),
            replace(
                &fork,
                r#""type":"after_user_message""#,
                r#""type":"future_anchor""#,
            ),
        ] {
            assert!(DurableStoreV1Codec::decode_session_head(&invalid).is_err());
        }
    }

    #[test]
    fn session_definition_head_and_errors_keep_sensitive_values_redacted() {
        let definition =
            decode_session_definition_posix(&fixture("session-definition.json")).unwrap();
        let head =
            DurableStoreV1Codec::decode_session_head(&fixture("fork-session-head.json")).unwrap();
        let metadata_head =
            DurableStoreV1Codec::decode_session_head(&fixture("session-head-2-metadata.json"))
                .unwrap();
        let definition_error = decode_session_definition_posix(&replace(
            &fixture("session-definition.json"),
            r#""providerId":"openai""#,
            r#""providerId":true"#,
        ))
        .unwrap_err();
        let metadata_error = DurableStoreV1Codec::decode_session_head(&replace(
            &fixture("session-head-2-metadata.json"),
            r#""description":null"#,
            r#""description":"private description\u001b""#,
        ))
        .unwrap_err();
        let debug = format!(
            "{definition:?} {head:?} {metadata_head:?} {definition_error:?} {definition_error} {metadata_error:?} {metadata_error}"
        );
        for secret in [
            r#"{"sessionId"#,
            "ses_22222222222222222222222222222222",
            "ses_33333333333333333333333333333333",
            "agt_11111111111111111111111111111111",
            "itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "file:///Users/example/project",
            "/Users/example/project",
            "openai",
            "gpt-5",
            "base",
            "session-notes",
            "Project session",
            "private description",
        ] {
            assert!(!debug.contains(secret), "debug leaked {secret:?}");
        }
        assert!(definition_error.source().is_none());
        assert!(metadata_error.source().is_none());
    }

    #[test]
    fn rejects_noncanonical_transport_and_closed_document_shape() {
        let definition = fixture("agent-definition.json");
        let head = fixture("agent-head.json");
        let wrong_order = replace(
            &definition,
            r#"{"agentId":"agt_11111111111111111111111111111111","revision":"ar_1""#,
            r#"{"revision":"ar_1","agentId":"agt_11111111111111111111111111111111""#,
        );
        assert_eq!(
            DurableStoreV1Codec::decode_agent_definition(&wrong_order),
            Err(DurableStoreCodecError::Noncanonical)
        );
        let duplicate = replace(
            &definition,
            r#""revision":"ar_1""#,
            r#""revision":"ar_1","revision":"ar_1""#,
        );
        assert_eq!(
            DurableStoreV1Codec::decode_agent_definition(&duplicate),
            Err(DurableStoreCodecError::InvalidDocument)
        );
        let unknown = replace(&definition, "}", ",\"future\":true}");
        assert_eq!(
            DurableStoreV1Codec::decode_agent_definition(&unknown),
            Err(DurableStoreCodecError::InvalidShape)
        );
        let mut invalid_utf8 = definition.clone();
        invalid_utf8[1] = 0xff;
        assert_eq!(
            DurableStoreV1Codec::decode_agent_definition(&invalid_utf8),
            Err(DurableStoreCodecError::InvalidDocument)
        );
        for input in [
            [b"\xef\xbb\xbf".as_slice(), definition.as_slice()].concat(),
            definition
                .iter()
                .copied()
                .map(|byte| if byte == b'\n' { b'\r' } else { byte })
                .chain(std::iter::once(b'\n'))
                .collect(),
            definition[..definition.len() - 1].to_vec(),
            [definition.as_slice(), b"\n".as_slice()].concat(),
            [definition[..definition.len() - 1].as_ref(), b" \n".as_ref()].concat(),
        ] {
            assert!(
                DurableStoreV1Codec::decode_agent_definition(&input).is_err(),
                "noncanonical document was accepted"
            );
        }
        for input in [
            replace(&head, r#""entity":"agent""#, r#""entity":"session""#),
            replace(
                &head,
                r#""revision":"ar_1","storageGeneration":1"#,
                r#""revision":"ar_1","storageGeneration":1,"future":true"#,
            ),
        ] {
            assert!(DurableStoreV1Codec::decode_agent_head(&input).is_err());
        }
    }

    #[test]
    fn rejects_invalid_agent_scalars_generations_and_prompt_selection() {
        let definition = fixture("agent-definition.json");
        let head = fixture("agent-head.json");
        let second_head = fixture("agent-head-2-definition.json");
        for input in [
            replace(
                &definition,
                "agt_11111111111111111111111111111111",
                "agt_not-an-id",
            ),
            replace(
                &definition,
                r#""agentId":"agt_11111111111111111111111111111111"#,
                r#""agentId":1"#,
            ),
            replace(&definition, r#""revision":"ar_1""#, r#""revision":"ar_0""#),
            replace(&definition, r#""revision":"ar_1""#, r#""revision":1""#),
            replace(
                &definition,
                "2026-08-03T10:00:00.123Z",
                "2026-08-03T10:00:00Z",
            ),
            replace(
                &definition,
                r#""createdAt":"2026-08-03T10:00:00.123Z"#,
                r#""createdAt":true"#,
            ),
            replace(&definition, r#"["base","safety"]"#, r#"["base","base"]"#),
            replace(&definition, r#"["base","safety"]"#, r#"["safety","base"]"#),
        ] {
            assert!(DurableStoreV1Codec::decode_agent_definition(&input).is_err());
        }
        assert!(
            DurableStoreV1Codec::decode_agent_definition(&replace(
                &definition,
                r#"["base","safety"]"#,
                "[]",
            ))
            .is_ok()
        );

        for input in [
            replace(
                &head,
                r#""agentId":"agt_11111111111111111111111111111111","storageGeneration":1"#,
                r#""agentId":"agt_11111111111111111111111111111111","storageGeneration":"1""#,
            ),
            replace(
                &head,
                r#""agentId":"agt_11111111111111111111111111111111","storageGeneration":1"#,
                r#""agentId":"agt_11111111111111111111111111111111","storageGeneration":0"#,
            ),
            replace(
                &head,
                r#""agentId":"agt_11111111111111111111111111111111","storageGeneration":1"#,
                r#""agentId":"agt_11111111111111111111111111111111","storageGeneration":1000001"#,
            ),
            replace(
                &head,
                r#""previousStorageGeneration":null"#,
                r#""previousStorageGeneration":1"#,
            ),
            replace(
                &head,
                r#""revision":"ar_1","storageGeneration":1"#,
                r#""revision":"ar_1","storageGeneration":0"#,
            ),
            replace(
                &head,
                r#""revision":"ar_1","storageGeneration":1"#,
                r#""revision":"ar_1","storageGeneration":2"#,
            ),
            replace(
                &second_head,
                r#""previousStorageGeneration":1"#,
                r#""previousStorageGeneration":null"#,
            ),
            replace(
                &second_head,
                r#""previousStorageGeneration":1"#,
                r#""previousStorageGeneration":2"#,
            ),
        ] {
            assert!(DurableStoreV1Codec::decode_agent_head(&input).is_err());
        }
    }

    #[test]
    fn rejects_invalid_metadata_and_never_echoes_secrets() {
        let head = fixture("agent-head.json");
        for input in [
            replace(&head, r#""name":"Planner""#, r#""name":"""#),
            replace(&head, r#""name":"Planner""#, r#""name":"unsafe\u001b""#),
            replace(
                &head,
                r#""name":"Planner""#,
                &format!(r#""name":"{}""#, "x".repeat(257)),
            ),
            replace(
                &head,
                r#""description":null"#,
                r#""description":"unsafe\u001b""#,
            ),
            replace(
                &head,
                r#""description":null"#,
                &format!(r#""description":"{}""#, "x".repeat(8_193)),
            ),
        ] {
            let error = DurableStoreV1Codec::decode_agent_head(&input).unwrap_err();
            let debug = format!("{error:?} {error}");
            for secret in [
                "unsafe",
                "Planner",
                "base",
                "agt_11111111111111111111111111111111",
                r#"{"entity"#,
                "/private/path",
            ] {
                assert!(!debug.contains(secret), "error leaked {secret:?}");
            }
            assert!(error.source().is_none());
        }
        assert!(
            DurableStoreV1Codec::decode_agent_head(&replace(
                &head,
                r#""description":null"#,
                r#""description":"""#,
            ))
            .is_ok()
        );
    }

    #[test]
    fn private_generation_and_head_values_enforce_only_single_document_invariants() {
        assert_eq!(StorageGeneration::new(0), None);
        assert_eq!(StorageGeneration::new(1_000_001), None);
        assert_eq!(
            StorageGeneration::new(1).unwrap().directory_name(),
            "00000000000000000001"
        );
        assert_eq!(
            StorageGeneration::new(1_000_000).unwrap().directory_name(),
            "00000000000001000000"
        );

        let head = DurableStoreV1Codec::decode_agent_head(&fixture("agent-head.json")).unwrap();
        assert_eq!(head.storage_generation().get(), 1);
        assert_eq!(head.previous_storage_generation(), None);
        let debug = format!("{head:?}");
        for secret in ["agt_11111111111111111111111111111111", "Planner", "base"] {
            assert!(!debug.contains(secret), "head debug leaked {secret:?}");
        }
        assert!(
            DurableAgentHead::new(
                head.agent_id(),
                StorageGeneration::new(1).unwrap(),
                Some(StorageGeneration::new(1).unwrap()),
                head.current_definition_revision(),
                head.current_definition_storage_generation(),
                head.metadata().clone(),
                head.status(),
                head.created_at(),
            )
            .is_err()
        );
    }

    #[test]
    fn document_limits_have_a_separate_body_and_total_cap() {
        assert_eq!(
            MAX_DURABLE_DOCUMENT_BODY_BYTES + 1,
            MAX_DURABLE_DOCUMENT_BYTES
        );
        let mut exact_body = b"{}".to_vec();
        exact_body.resize(MAX_DURABLE_DOCUMENT_BODY_BYTES, b' ');
        let exact_total = [exact_body.as_slice(), b"\n".as_slice()].concat();
        assert_eq!(exact_total.len(), MAX_DURABLE_DOCUMENT_BYTES);
        assert!(parse_document(&exact_total).is_ok());
        assert_eq!(
            DurableStoreV1Codec::decode_agent_definition(&exact_total),
            Err(DurableStoreCodecError::InvalidShape)
        );
        exact_body.push(b' ');
        let total_plus_one = [exact_body.as_slice(), b"\n".as_slice()].concat();
        assert_eq!(
            DurableStoreV1Codec::decode_agent_definition(&total_plus_one),
            Err(DurableStoreCodecError::DocumentTooLarge)
        );
    }
}
