use std::collections::BTreeMap;
use std::str::FromStr;

use thiserror::Error;

use crate::agent_session_lifecycle::{AgentDefinition, AgentMetadata, AgentStatus};
use crate::durable_state::{DurableAgentHead, StorageGeneration};
use crate::prompt::{AgentPromptSelection, PromptId};
use crate::wire::AgentRevision;
use crate::wire::bounded_json::{BoundedJsonError, JsonNode, JsonParseLimits, parse_node};

pub(crate) const MAX_DURABLE_DOCUMENT_BYTES: usize = 1_048_576;
pub(crate) const MAX_DURABLE_DOCUMENT_BODY_BYTES: usize = MAX_DURABLE_DOCUMENT_BYTES - 1;

/// The closed, redacted failure taxonomy for Store V1 Agent document bytes.
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

/// The concrete Store V1 codec for the physical Agent documents. It intentionally has no
/// registry or public representation: DurableState is the only future production caller.
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
        encode_metadata(&mut writer, head.metadata())?;
        writer.comma()?;
        writer.key("status")?;
        writer.string(status_name(head.status()))?;
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
        decode_current_definition(required(object, "currentDefinition")?)?;
    let metadata = decode_metadata(required(object, "metadata")?)?;
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

fn decode_current_definition(
    node: &JsonNode,
) -> Result<(AgentRevision, StorageGeneration), DurableStoreCodecError> {
    let object = object(node)?;
    exact_fields(object, &["revision", "storageGeneration"])?;
    Ok((
        scalar(required(object, "revision")?)?,
        generation(required(object, "storageGeneration")?)?,
    ))
}

fn decode_metadata(node: &JsonNode) -> Result<AgentMetadata, DurableStoreCodecError> {
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

fn encode_metadata(
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

fn status_name(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Enabled => "enabled",
        AgentStatus::Disabled => "disabled",
        AgentStatus::Deleted => "deleted",
    }
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
