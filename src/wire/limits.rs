use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "consumed by M1.2 bounded codecs")
)]
pub enum LimitError {
    #[error("wire limit exceeded: maximum {maximum}, actual {actual}")]
    Exceeded { maximum: usize, actual: usize },
    #[error("wire counter overflow")]
    CounterOverflow,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "consumed by M1.2 bounded codecs")
)]
pub struct WireLimit(usize);

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "consumed by M1.2 bounded codecs")
)]
impl WireLimit {
    pub const fn new(maximum: usize) -> Self {
        Self(maximum)
    }

    pub const fn maximum(self) -> usize {
        self.0
    }

    pub fn validate(self, actual: usize) -> Result<(), LimitError> {
        if actual <= self.0 {
            Ok(())
        } else {
            Err(LimitError::Exceeded {
                maximum: self.0,
                actual,
            })
        }
    }

    pub fn validate_bytes(self, value: &[u8]) -> Result<(), LimitError> {
        self.validate(value.len())
    }

    pub fn validate_str(self, value: &str) -> Result<(), LimitError> {
        self.validate(value.len())
    }

    pub fn validate_items<T>(self, value: &[T]) -> Result<(), LimitError> {
        self.validate(value.len())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "consumed by M1.2 bounded codecs")
)]
pub struct CheckedLimitCounter {
    limit: WireLimit,
    value: usize,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "consumed by M1.2 bounded codecs")
)]
impl CheckedLimitCounter {
    pub const fn new(limit: WireLimit) -> Self {
        Self { limit, value: 0 }
    }

    pub const fn value(&self) -> usize {
        self.value
    }

    pub const fn limit(&self) -> WireLimit {
        self.limit
    }

    pub fn try_add(&mut self, amount: usize) -> Result<usize, LimitError> {
        let next = self
            .value
            .checked_add(amount)
            .ok_or(LimitError::CounterOverflow)?;
        self.limit.validate(next)?;
        self.value = next;
        Ok(next)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProtocolLimits {
    pub transport: TransportLimits,
    pub text: TextLimits,
    pub catalog: CatalogLimits,
    pub paging: PagingLimits,
    pub prompt: PromptWireLimits,
    pub workspace: WorkspaceWireLimits,
    pub queues: QueueLimits,
    pub interaction: InteractionLimits,
    pub observation: ObservationLimits,
    pub embedded_json: EmbeddedJsonLimits,
}

impl ProtocolLimits {
    pub const fn v1_0() -> Self {
        Self {
            transport: TransportLimits {
                max_request_bytes: 1_048_576,
                max_response_bytes: 8_388_608,
                max_runtime_snapshot_bytes: 8_388_608,
                max_session_snapshot_bytes: 8_388_608,
                max_state_event_bytes: 8_388_608,
                max_progress_event_bytes: 65_536,
                max_json_depth: 64,
                max_object_members: 256,
                max_array_items: 4_096,
                max_string_bytes: 262_144,
            },
            text: TextLimits {
                max_text_intent_bytes: 131_072,
                max_command_input_bytes: 32_768,
                max_command_output_bytes: 65_536,
                max_display_name_bytes: 256,
                max_description_bytes: 8_192,
                max_public_summary_bytes: 8_192,
                max_diagnostic_code_bytes: 64,
                max_diagnostic_message_bytes: 2_048,
            },
            catalog: CatalogLimits {
                max_command_path_segments: 16,
                max_command_arguments: 64,
                max_command_catalog_entries: 1_024,
            },
            paging: PagingLimits {
                max_page_size: 200,
                max_page_cursor_bytes: 256,
            },
            prompt: PromptWireLimits {
                max_skills_per_intent: 32,
                max_user_message_parts: 64,
                max_message_part_bytes: 131_072,
                max_user_message_bytes: 524_288,
            },
            workspace: WorkspaceWireLimits {
                max_workspace_roots: 16,
                max_absolute_path_uri_bytes: 8_192,
                max_relative_path_bytes: 4_096,
                max_relative_path_segments: 256,
            },
            queues: QueueLimits {
                max_submit_admissions: 16,
                max_steers: 32,
                max_follow_ups: 32,
            },
            interaction: InteractionLimits {
                max_tool_approval_options: 16,
                max_interaction_questions: 32,
                max_choices_per_question: 64,
                max_answer_text_bytes: 16_384,
                max_interaction_answer_bytes: 65_536,
                max_interaction_view_bytes: 131_072,
            },
            observation: ObservationLimits {
                max_active_items: 64,
                max_item_view_bytes: 65_536,
                max_pending_interactions: 16,
                max_snapshot_diagnostics: 50,
                max_query_diagnostics_per_scope: 100,
            },
            embedded_json: EmbeddedJsonLimits {
                value: JsonValueLimits {
                    max_encoded_bytes: 65_536,
                    max_depth: 32,
                    max_array_items: 256,
                    max_object_members: 256,
                    max_string_bytes: 16_384,
                    max_number_literal_bytes: 64,
                },
                schema: JsonSchemaLimits {
                    max_encoded_bytes: 65_536,
                    max_depth: 32,
                    max_nodes: 4_096,
                    max_properties_required_or_enum_items: 256,
                    max_regex_bytes: 1_024,
                },
            },
        }
    }

    pub(crate) const fn is_within_v1_hard_maxima(self) -> bool {
        let maximum = Self::v1_0();
        self.transport.is_within(maximum.transport)
            && self.text.is_within(maximum.text)
            && self.catalog.is_within(maximum.catalog)
            && self.paging.is_within(maximum.paging)
            && self.prompt.is_within(maximum.prompt)
            && self.workspace.is_within(maximum.workspace)
            && self.queues.is_within(maximum.queues)
            && self.interaction.is_within(maximum.interaction)
            && self.observation.is_within(maximum.observation)
            && self
                .embedded_json
                .value
                .is_within(maximum.embedded_json.value)
            && self
                .embedded_json
                .schema
                .is_within(maximum.embedded_json.schema)
    }
}

macro_rules! limit_struct {
    ($name:ident { $($field:ident: $type:ty),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        pub struct $name {
            $(pub $field: $type),+
        }


        impl $name {
            const fn is_within(self, maximum: Self) -> bool {
                true $(&& self.$field <= maximum.$field)+
            }
        }
    };
}

limit_struct!(TransportLimits {
    max_request_bytes: u32,
    max_response_bytes: u32,
    max_runtime_snapshot_bytes: u32,
    max_session_snapshot_bytes: u32,
    max_state_event_bytes: u32,
    max_progress_event_bytes: u32,
    max_json_depth: u16,
    max_object_members: u16,
    max_array_items: u32,
    max_string_bytes: u32,
});

limit_struct!(TextLimits {
    max_text_intent_bytes: u32,
    max_command_input_bytes: u32,
    max_command_output_bytes: u32,
    max_display_name_bytes: u16,
    max_description_bytes: u32,
    max_public_summary_bytes: u32,
    max_diagnostic_code_bytes: u16,
    max_diagnostic_message_bytes: u16,
});

limit_struct!(CatalogLimits {
    max_command_path_segments: u16,
    max_command_arguments: u16,
    max_command_catalog_entries: u16,
});

limit_struct!(PagingLimits {
    max_page_size: u16,
    max_page_cursor_bytes: u16,
});

limit_struct!(PromptWireLimits {
    max_skills_per_intent: u16,
    max_user_message_parts: u16,
    max_message_part_bytes: u32,
    max_user_message_bytes: u32,
});

limit_struct!(WorkspaceWireLimits {
    max_workspace_roots: u16,
    max_absolute_path_uri_bytes: u32,
    max_relative_path_bytes: u32,
    max_relative_path_segments: u16,
});

limit_struct!(QueueLimits {
    max_submit_admissions: u16,
    max_steers: u16,
    max_follow_ups: u16,
});

limit_struct!(InteractionLimits {
    max_tool_approval_options: u16,
    max_interaction_questions: u16,
    max_choices_per_question: u16,
    max_answer_text_bytes: u32,
    max_interaction_answer_bytes: u32,
    max_interaction_view_bytes: u32,
});

limit_struct!(ObservationLimits {
    max_active_items: u16,
    max_item_view_bytes: u32,
    max_pending_interactions: u16,
    max_snapshot_diagnostics: u16,
    max_query_diagnostics_per_scope: u16,
});

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EmbeddedJsonLimits {
    pub value: JsonValueLimits,
    pub schema: JsonSchemaLimits,
}

limit_struct!(JsonValueLimits {
    max_encoded_bytes: u32,
    max_depth: u16,
    max_array_items: u16,
    max_object_members: u16,
    max_string_bytes: u32,
    max_number_literal_bytes: u16,
});

limit_struct!(JsonSchemaLimits {
    max_encoded_bytes: u32,
    max_depth: u16,
    max_nodes: u32,
    max_properties_required_or_enum_items: u16,
    max_regex_bytes: u16,
});

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LimitValidator {
    EncodedFramePreflight,
    PublicJsonStructuralDecoder,
    OwningSafeTextConstructor,
    CommandCatalogConstructor,
    PageRequestValidator,
    CursorAllocationPreflight,
    PromptIntentOrMessageConstructor,
    WorkspaceLexicalConstructor,
    QueueCollectionConstructor,
    InteractionConstructor,
    ObservationViewOrSnapshotPreflight,
    BoundedJsonValueConstructor,
    BoundedJsonSchemaConstructor,
    RawAndCanonicalOutputByteGates,
}

#[cfg(test)]
impl LimitValidator {
    const fn name(self) -> &'static str {
        match self {
            Self::EncodedFramePreflight => "encoded_frame_preflight",
            Self::PublicJsonStructuralDecoder => "public_json_structural_decoder",
            Self::OwningSafeTextConstructor => "owning_safe_text_constructor",
            Self::CommandCatalogConstructor => "command_catalog_constructor",
            Self::PageRequestValidator => "page_request_validator",
            Self::CursorAllocationPreflight => "cursor_allocation_preflight",
            Self::PromptIntentOrMessageConstructor => "prompt_intent_or_message_constructor",
            Self::WorkspaceLexicalConstructor => "workspace_lexical_constructor",
            Self::QueueCollectionConstructor => "queue_collection_constructor",
            Self::InteractionConstructor => "interaction_constructor",
            Self::ObservationViewOrSnapshotPreflight => "observation_view_or_snapshot_preflight",
            Self::BoundedJsonValueConstructor => "bounded_json_value_constructor",
            Self::BoundedJsonSchemaConstructor => "bounded_json_schema_constructor",
            Self::RawAndCanonicalOutputByteGates => "raw_and_canonical_output_byte_gates",
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LimitProbe {
    path: &'static str,
    limit: WireLimit,
    validator: LimitValidator,
}

#[cfg(test)]
impl LimitProbe {
    fn validate_metric(self, actual: usize) -> Result<(), LimitError> {
        self.limit.validate(actual)
    }
}

#[cfg(test)]
fn limit_probes(limits: &ProtocolLimits) -> Vec<LimitProbe> {
    macro_rules! probe {
        ($path:literal, $value:expr, $validator:ident) => {
            LimitProbe {
                path: $path,
                limit: WireLimit::new($value as usize),
                validator: LimitValidator::$validator,
            }
        };
    }

    vec![
        probe!(
            "transport.maxRequestBytes",
            limits.transport.max_request_bytes,
            EncodedFramePreflight
        ),
        probe!(
            "transport.maxResponseBytes",
            limits.transport.max_response_bytes,
            EncodedFramePreflight
        ),
        probe!(
            "transport.maxRuntimeSnapshotBytes",
            limits.transport.max_runtime_snapshot_bytes,
            EncodedFramePreflight
        ),
        probe!(
            "transport.maxSessionSnapshotBytes",
            limits.transport.max_session_snapshot_bytes,
            EncodedFramePreflight
        ),
        probe!(
            "transport.maxStateEventBytes",
            limits.transport.max_state_event_bytes,
            EncodedFramePreflight
        ),
        probe!(
            "transport.maxProgressEventBytes",
            limits.transport.max_progress_event_bytes,
            EncodedFramePreflight
        ),
        probe!(
            "transport.maxJsonDepth",
            limits.transport.max_json_depth,
            PublicJsonStructuralDecoder
        ),
        probe!(
            "transport.maxObjectMembers",
            limits.transport.max_object_members,
            PublicJsonStructuralDecoder
        ),
        probe!(
            "transport.maxArrayItems",
            limits.transport.max_array_items,
            PublicJsonStructuralDecoder
        ),
        probe!(
            "transport.maxStringBytes",
            limits.transport.max_string_bytes,
            PublicJsonStructuralDecoder
        ),
        probe!(
            "text.maxTextIntentBytes",
            limits.text.max_text_intent_bytes,
            OwningSafeTextConstructor
        ),
        probe!(
            "text.maxCommandInputBytes",
            limits.text.max_command_input_bytes,
            OwningSafeTextConstructor
        ),
        probe!(
            "text.maxCommandOutputBytes",
            limits.text.max_command_output_bytes,
            OwningSafeTextConstructor
        ),
        probe!(
            "text.maxDisplayNameBytes",
            limits.text.max_display_name_bytes,
            OwningSafeTextConstructor
        ),
        probe!(
            "text.maxDescriptionBytes",
            limits.text.max_description_bytes,
            OwningSafeTextConstructor
        ),
        probe!(
            "text.maxPublicSummaryBytes",
            limits.text.max_public_summary_bytes,
            OwningSafeTextConstructor
        ),
        probe!(
            "text.maxDiagnosticCodeBytes",
            limits.text.max_diagnostic_code_bytes,
            OwningSafeTextConstructor
        ),
        probe!(
            "text.maxDiagnosticMessageBytes",
            limits.text.max_diagnostic_message_bytes,
            OwningSafeTextConstructor
        ),
        probe!(
            "catalog.maxCommandPathSegments",
            limits.catalog.max_command_path_segments,
            CommandCatalogConstructor
        ),
        probe!(
            "catalog.maxCommandArguments",
            limits.catalog.max_command_arguments,
            CommandCatalogConstructor
        ),
        probe!(
            "catalog.maxCommandCatalogEntries",
            limits.catalog.max_command_catalog_entries,
            CommandCatalogConstructor
        ),
        probe!(
            "paging.maxPageSize",
            limits.paging.max_page_size,
            PageRequestValidator
        ),
        probe!(
            "paging.maxPageCursorBytes",
            limits.paging.max_page_cursor_bytes,
            CursorAllocationPreflight
        ),
        probe!(
            "prompt.maxSkillsPerIntent",
            limits.prompt.max_skills_per_intent,
            PromptIntentOrMessageConstructor
        ),
        probe!(
            "prompt.maxUserMessageParts",
            limits.prompt.max_user_message_parts,
            PromptIntentOrMessageConstructor
        ),
        probe!(
            "prompt.maxMessagePartBytes",
            limits.prompt.max_message_part_bytes,
            PromptIntentOrMessageConstructor
        ),
        probe!(
            "prompt.maxUserMessageBytes",
            limits.prompt.max_user_message_bytes,
            PromptIntentOrMessageConstructor
        ),
        probe!(
            "workspace.maxWorkspaceRoots",
            limits.workspace.max_workspace_roots,
            WorkspaceLexicalConstructor
        ),
        probe!(
            "workspace.maxAbsolutePathUriBytes",
            limits.workspace.max_absolute_path_uri_bytes,
            WorkspaceLexicalConstructor
        ),
        probe!(
            "workspace.maxRelativePathBytes",
            limits.workspace.max_relative_path_bytes,
            WorkspaceLexicalConstructor
        ),
        probe!(
            "workspace.maxRelativePathSegments",
            limits.workspace.max_relative_path_segments,
            WorkspaceLexicalConstructor
        ),
        probe!(
            "queues.maxSubmitAdmissions",
            limits.queues.max_submit_admissions,
            QueueCollectionConstructor
        ),
        probe!(
            "queues.maxSteers",
            limits.queues.max_steers,
            QueueCollectionConstructor
        ),
        probe!(
            "queues.maxFollowUps",
            limits.queues.max_follow_ups,
            QueueCollectionConstructor
        ),
        probe!(
            "interaction.maxToolApprovalOptions",
            limits.interaction.max_tool_approval_options,
            InteractionConstructor
        ),
        probe!(
            "interaction.maxInteractionQuestions",
            limits.interaction.max_interaction_questions,
            InteractionConstructor
        ),
        probe!(
            "interaction.maxChoicesPerQuestion",
            limits.interaction.max_choices_per_question,
            InteractionConstructor
        ),
        probe!(
            "interaction.maxAnswerTextBytes",
            limits.interaction.max_answer_text_bytes,
            InteractionConstructor
        ),
        probe!(
            "interaction.maxInteractionAnswerBytes",
            limits.interaction.max_interaction_answer_bytes,
            InteractionConstructor
        ),
        probe!(
            "interaction.maxInteractionViewBytes",
            limits.interaction.max_interaction_view_bytes,
            InteractionConstructor
        ),
        probe!(
            "observation.maxActiveItems",
            limits.observation.max_active_items,
            ObservationViewOrSnapshotPreflight
        ),
        probe!(
            "observation.maxItemViewBytes",
            limits.observation.max_item_view_bytes,
            ObservationViewOrSnapshotPreflight
        ),
        probe!(
            "observation.maxPendingInteractions",
            limits.observation.max_pending_interactions,
            ObservationViewOrSnapshotPreflight
        ),
        probe!(
            "observation.maxSnapshotDiagnostics",
            limits.observation.max_snapshot_diagnostics,
            ObservationViewOrSnapshotPreflight
        ),
        probe!(
            "observation.maxQueryDiagnosticsPerScope",
            limits.observation.max_query_diagnostics_per_scope,
            ObservationViewOrSnapshotPreflight
        ),
        probe!(
            "embeddedJson.value.maxEncodedBytes",
            limits.embedded_json.value.max_encoded_bytes,
            RawAndCanonicalOutputByteGates
        ),
        probe!(
            "embeddedJson.value.maxDepth",
            limits.embedded_json.value.max_depth,
            BoundedJsonValueConstructor
        ),
        probe!(
            "embeddedJson.value.maxArrayItems",
            limits.embedded_json.value.max_array_items,
            BoundedJsonValueConstructor
        ),
        probe!(
            "embeddedJson.value.maxObjectMembers",
            limits.embedded_json.value.max_object_members,
            BoundedJsonValueConstructor
        ),
        probe!(
            "embeddedJson.value.maxStringBytes",
            limits.embedded_json.value.max_string_bytes,
            BoundedJsonValueConstructor
        ),
        probe!(
            "embeddedJson.value.maxNumberLiteralBytes",
            limits.embedded_json.value.max_number_literal_bytes,
            BoundedJsonValueConstructor
        ),
        probe!(
            "embeddedJson.schema.maxEncodedBytes",
            limits.embedded_json.schema.max_encoded_bytes,
            RawAndCanonicalOutputByteGates
        ),
        probe!(
            "embeddedJson.schema.maxDepth",
            limits.embedded_json.schema.max_depth,
            BoundedJsonSchemaConstructor
        ),
        probe!(
            "embeddedJson.schema.maxNodes",
            limits.embedded_json.schema.max_nodes,
            BoundedJsonSchemaConstructor
        ),
        probe!(
            "embeddedJson.schema.maxPropertiesRequiredOrEnumItems",
            limits
                .embedded_json
                .schema
                .max_properties_required_or_enum_items,
            BoundedJsonSchemaConstructor
        ),
        probe!(
            "embeddedJson.schema.maxRegexBytes",
            limits.embedded_json.schema.max_regex_bytes,
            BoundedJsonSchemaConstructor
        ),
    ]
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub const V1_0: Self = Self { major: 1, minor: 0 };

    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CapabilityTokenError {
    #[error("capability token must be 1..=64 bytes")]
    InvalidLength,
    #[error("capability token must use lowercase ASCII token grammar")]
    InvalidGrammar,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CapabilityToken(Box<str>);

impl CapabilityToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for CapabilityToken {
    type Err = CapabilityTokenError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() || value.len() > 64 {
            return Err(CapabilityTokenError::InvalidLength);
        }
        let mut bytes = value.bytes();
        if !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
            || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(CapabilityTokenError::InvalidGrammar);
        }
        Ok(Self(value.into()))
    }
}

impl fmt::Display for CapabilityToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for CapabilityToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl Serialize for CapabilityToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CapabilityToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientInfo {
    name: Box<str>,
    version: Box<str>,
}

impl ClientInfo {
    pub fn new(name: impl Into<Box<str>>, version: impl Into<Box<str>>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn version(&self) -> &str {
        &self.version
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "consumed by M2 bootstrap routing")
)]
fn validate_client_text(value: &str) -> Result<(), HelloValidationError> {
    if value.len() > 128 {
        return Err(HelloValidationError::InvalidClientInfo);
    }
    if value.chars().any(|character| {
        matches!(
            u32::from(character),
            0x00..=0x08 | 0x0b..=0x1f | 0x7f..=0x9f
        )
    }) {
        return Err(HelloValidationError::InvalidClientInfo);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "consumed by M2 bootstrap routing")
)]
enum HelloValidationError {
    #[error("protocol hello has more than 16 versions")]
    TooManyVersions,
    #[error("protocol hello has duplicate versions")]
    DuplicateVersion,
    #[error("protocol hello has more than 64 capabilities")]
    TooManyCapabilities,
    #[error("protocol hello has duplicate capabilities")]
    DuplicateCapability,
    #[error("invalid client info")]
    InvalidClientInfo,
    #[error("invalid capability token")]
    InvalidCapability(#[source] CapabilityTokenError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtocolHello {
    supported_versions: Vec<ProtocolVersion>,
    client: ClientInfo,
    capabilities: Vec<Box<str>>,
}

impl ProtocolHello {
    pub fn new(
        supported_versions: Vec<ProtocolVersion>,
        client: ClientInfo,
        capabilities: Vec<String>,
    ) -> Self {
        Self {
            supported_versions,
            client,
            capabilities: capabilities.into_iter().map(Into::into).collect(),
        }
    }

    pub fn supported_versions(&self) -> &[ProtocolVersion] {
        &self.supported_versions
    }

    pub const fn client(&self) -> &ClientInfo {
        &self.client
    }

    pub fn capabilities(&self) -> &[Box<str>] {
        &self.capabilities
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "consumed by M2 bootstrap routing")
)]
struct ValidatedHello {
    capabilities: Vec<CapabilityToken>,
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "consumed by M2 bootstrap routing")
)]
fn validate_hello(hello: &ProtocolHello) -> Result<ValidatedHello, HelloValidationError> {
    if hello.supported_versions.len() > 16 {
        return Err(HelloValidationError::TooManyVersions);
    }
    if hello
        .supported_versions
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .len()
        != hello.supported_versions.len()
    {
        return Err(HelloValidationError::DuplicateVersion);
    }
    if hello.capabilities.len() > 64 {
        return Err(HelloValidationError::TooManyCapabilities);
    }
    validate_client_text(hello.client.name())?;
    validate_client_text(hello.client.version())?;

    let capabilities = hello
        .capabilities
        .iter()
        .map(|value| value.parse::<CapabilityToken>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(HelloValidationError::InvalidCapability)?;
    if capabilities.iter().cloned().collect::<BTreeSet<_>>().len() != capabilities.len() {
        return Err(HelloValidationError::DuplicateCapability);
    }
    Ok(ValidatedHello { capabilities })
}

pub(crate) fn protocol_hello_is_valid(hello: &ProtocolHello) -> bool {
    validate_hello(hello).is_ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolRejectReason {
    UnsupportedProtocolVersion,
    InvalidHello,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeInfo {
    protocol_version: ProtocolVersion,
    implementation: Box<str>,
    implementation_version: Box<str>,
}

impl RuntimeInfo {
    pub fn new(
        protocol_version: ProtocolVersion,
        implementation: impl Into<Box<str>>,
        implementation_version: impl Into<Box<str>>,
    ) -> Self {
        Self {
            protocol_version,
            implementation: implementation.into(),
            implementation_version: implementation_version.into(),
        }
    }

    pub const fn protocol_version(&self) -> ProtocolVersion {
        self.protocol_version
    }

    pub fn implementation(&self) -> &str {
        &self.implementation
    }

    pub fn implementation_version(&self) -> &str {
        &self.implementation_version
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RuntimeCapabilities {
    values: Vec<CapabilityToken>,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum RuntimeCapabilitiesError {
    #[error("runtime capability is not declared by protocol v1.0")]
    UnknownCapability,
    #[error("runtime capability set contains a duplicate token")]
    DuplicateCapability,
}

impl RuntimeCapabilities {
    pub fn empty() -> Self {
        Self { values: Vec::new() }
    }

    pub(crate) fn from_v1_negotiated(
        values: Vec<CapabilityToken>,
    ) -> Result<Self, RuntimeCapabilitiesError> {
        let selected = values.iter().cloned().collect::<BTreeSet<_>>();
        if selected.len() != values.len() {
            return Err(RuntimeCapabilitiesError::DuplicateCapability);
        }
        if selected
            .iter()
            .any(|capability| !is_v1_runtime_capability(capability))
        {
            return Err(RuntimeCapabilitiesError::UnknownCapability);
        }
        let values = v1_runtime_capabilities()
            .into_iter()
            .filter(|capability| selected.contains(capability))
            .collect();
        Ok(Self { values })
    }

    pub fn values(&self) -> &[CapabilityToken] {
        &self.values
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolWelcome {
    selected_version: ProtocolVersion,
    runtime: RuntimeInfo,
    capabilities: RuntimeCapabilities,
    limits: Box<ProtocolLimits>,
}

impl ProtocolWelcome {
    pub fn new(
        selected_version: ProtocolVersion,
        runtime: RuntimeInfo,
        capabilities: RuntimeCapabilities,
        limits: ProtocolLimits,
    ) -> Self {
        Self {
            selected_version,
            runtime,
            capabilities,
            limits: Box::new(limits),
        }
    }

    pub const fn selected_version(&self) -> ProtocolVersion {
        self.selected_version
    }

    pub const fn runtime(&self) -> &RuntimeInfo {
        &self.runtime
    }

    pub const fn capabilities(&self) -> &RuntimeCapabilities {
        &self.capabilities
    }

    pub fn limits(&self) -> ProtocolLimits {
        *self.limits
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolReject {
    reason: ProtocolRejectReason,
    supported_versions: Vec<ProtocolVersion>,
}

impl ProtocolReject {
    pub fn new(reason: ProtocolRejectReason, supported_versions: Vec<ProtocolVersion>) -> Self {
        Self {
            reason,
            supported_versions,
        }
    }

    pub const fn reason(&self) -> ProtocolRejectReason {
        self.reason
    }

    pub fn supported_versions(&self) -> &[ProtocolVersion] {
        &self.supported_versions
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ProtocolBootstrapResponse {
    Welcome(ProtocolWelcome),
    Reject(ProtocolReject),
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "consumed by M2 bootstrap routing")
)]
pub enum ProtocolNegotiation {
    Selected {
        version: ProtocolVersion,
        capabilities: RuntimeCapabilities,
    },
    Rejected {
        reason: ProtocolRejectReason,
    },
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "consumed by M2 bootstrap routing")
)]
pub fn negotiate_protocol(hello: &ProtocolHello) -> ProtocolNegotiation {
    let Ok(validated) = validate_hello(hello) else {
        return ProtocolNegotiation::Rejected {
            reason: ProtocolRejectReason::InvalidHello,
        };
    };
    let selected = hello
        .supported_versions()
        .iter()
        .copied()
        .any(|version| version == ProtocolVersion::V1_0);
    if !selected {
        return ProtocolNegotiation::Rejected {
            reason: ProtocolRejectReason::UnsupportedProtocolVersion,
        };
    }

    let client_capabilities = validated.capabilities.iter().collect::<BTreeSet<_>>();
    let capabilities = v1_runtime_capabilities()
        .into_iter()
        .filter(|capability| client_capabilities.contains(capability))
        .collect::<Vec<_>>();
    let capabilities = RuntimeCapabilities::from_v1_negotiated(capabilities)
        .expect("built-in V1 capability intersection must be valid");
    ProtocolNegotiation::Selected {
        version: ProtocolVersion::V1_0,
        capabilities,
    }
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "consumed by M2 bootstrap routing")
)]
pub fn v1_runtime_capabilities() -> Vec<CapabilityToken> {
    V1_RUNTIME_CAPABILITY_TOKENS
        .into_iter()
        .map(|value| {
            value
                .parse()
                .expect("built-in capability token must be valid")
        })
        .collect()
}

pub(crate) fn is_v1_runtime_capability(capability: &CapabilityToken) -> bool {
    V1_RUNTIME_CAPABILITY_TOKENS.contains(&capability.as_str())
}

const V1_RUNTIME_CAPABILITY_TOKENS: [&str; 8] = [
    "state_events",
    "progress_events",
    "runtime_snapshot",
    "session_snapshot",
    "paged_queries",
    "command_catalog",
    "interaction_resolution",
    "session_fork",
];

#[cfg(test)]
#[path = "limits_tests.rs"]
mod tests;
