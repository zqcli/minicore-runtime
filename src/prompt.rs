use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use thiserror::Error;

use crate::model_gateway::ReasoningContent;
use crate::skills::SkillId;
use crate::tools::{ToolCallId, ToolName, ToolResultContent};
use crate::wire::lexical::{
    LexicalError, normalize_newlines, validate_safe_text, validate_stable_symbolic_key,
};
use crate::wire::{BoundedJsonObject, ProtocolLimits, WorkspaceRelativePath};
use crate::workspace::WorkspaceRootKey;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PromptValueError {
    #[error("prompt text must be non-empty")]
    EmptyText,
    #[error("prompt text or aggregate exceeds its byte limit")]
    TextTooLong,
    #[error("prompt text contains unsafe control characters")]
    UnsafeText,
    #[error("prompt intent has too many skills")]
    TooManySkills,
    #[error("prompt intent contains a duplicate skill")]
    DuplicateSkill,
    #[error("message part count is outside the supported range")]
    InvalidPartCount,
    #[error("prompt contribution stamp is invalid")]
    InvalidContributionStamp,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PromptIdError {
    #[error("prompt id must be 1..=128 bytes")]
    InvalidLength,
    #[error("prompt id violates the stable symbolic key grammar")]
    InvalidGrammar,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PromptId(Box<str>);

impl PromptId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for PromptId {
    type Err = PromptIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_stable_symbolic_key(value, 128, false).map_err(|error| match error {
            LexicalError::Empty | LexicalError::TooLong => PromptIdError::InvalidLength,
            LexicalError::InvalidGrammar | LexicalError::UnsafeText => {
                PromptIdError::InvalidGrammar
            }
        })?;
        Ok(Self(value.into()))
    }
}

impl fmt::Display for PromptId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for PromptId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SessionPromptSelectionError {
    #[error("session prompt selection has too many entries")]
    TooManyPrompts,
    #[error("session prompt selection contains a duplicate prompt")]
    DuplicatePrompt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionPromptSelection {
    enabled: BTreeSet<PromptId>,
}

impl SessionPromptSelection {
    pub fn new(enabled: Vec<PromptId>) -> Result<Self, SessionPromptSelectionError> {
        Self::new_with_maximum(
            enabled,
            usize::try_from(ProtocolLimits::v1_0().transport.max_array_items).unwrap_or(usize::MAX),
        )
    }

    pub(crate) fn new_with_maximum(
        enabled: Vec<PromptId>,
        maximum: usize,
    ) -> Result<Self, SessionPromptSelectionError> {
        if enabled.len() > maximum {
            return Err(SessionPromptSelectionError::TooManyPrompts);
        }
        let original_len = enabled.len();
        let enabled = enabled.into_iter().collect::<BTreeSet<_>>();
        if enabled.len() != original_len {
            return Err(SessionPromptSelectionError::DuplicatePrompt);
        }
        Ok(Self { enabled })
    }

    pub fn enabled(&self) -> &BTreeSet<PromptId> {
        &self.enabled
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct TextIntent(Box<str>);

impl TextIntent {
    pub fn new(text: impl AsRef<str>) -> Result<Self, PromptValueError> {
        let maximum = ProtocolLimits::v1_0().text.max_text_intent_bytes as usize;
        Self::new_with_maximum(text, maximum)
    }

    pub(crate) fn new_with_maximum(
        text: impl AsRef<str>,
        maximum: usize,
    ) -> Result<Self, PromptValueError> {
        let text = normalize_text_intent(text.as_ref(), maximum)?;
        Ok(Self(text.into()))
    }

    pub fn text(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillIntent {
    skill_id: SkillId,
}

impl SkillIntent {
    pub fn new(skill_id: SkillId) -> Self {
        Self { skill_id }
    }

    pub const fn skill_id(&self) -> &SkillId {
        &self.skill_id
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum PromptBodyIntent {
    Empty,
    Text(TextIntent),
}

#[derive(Clone, Eq, PartialEq)]
pub struct PromptIntent {
    body: PromptBodyIntent,
    skills: Arc<[SkillIntent]>,
}

impl PromptIntent {
    pub fn new(body: PromptBodyIntent, skills: Vec<SkillIntent>) -> Result<Self, PromptValueError> {
        let maximum = ProtocolLimits::v1_0().prompt.max_skills_per_intent as usize;
        Self::new_with_maximum_skills(body, skills, maximum)
    }

    pub(crate) fn new_with_maximum_skills(
        body: PromptBodyIntent,
        skills: Vec<SkillIntent>,
        maximum: usize,
    ) -> Result<Self, PromptValueError> {
        validate_skill_intent_count(skills.len(), maximum)?;
        let unique = skills
            .iter()
            .map(SkillIntent::skill_id)
            .collect::<BTreeSet<_>>();
        if unique.len() != skills.len() {
            return Err(PromptValueError::DuplicateSkill);
        }
        Ok(Self {
            body,
            skills: skills.into(),
        })
    }

    pub const fn body(&self) -> &PromptBodyIntent {
        &self.body
    }

    pub fn skills(&self) -> &[SkillIntent] {
        &self.skills
    }
}

pub(crate) fn validate_skill_intent_count(
    count: usize,
    maximum: usize,
) -> Result<(), PromptValueError> {
    if count > maximum {
        return Err(PromptValueError::TooManySkills);
    }
    Ok(())
}

pub(crate) fn normalize_text_intent(
    text: &str,
    maximum: usize,
) -> Result<String, PromptValueError> {
    let text = normalize_newlines(text);
    validate_prompt_text(&text, maximum, false)?;
    Ok(text)
}

#[derive(Clone, Eq, PartialEq)]
pub enum MessageContent {
    Text(MessageText),
}

#[derive(Clone, Eq, PartialEq)]
pub struct MessageText(Arc<str>);

impl MessageText {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl MessageContent {
    #[allow(dead_code, reason = "consumed by PromptSet composition in M7")]
    fn text(text: impl AsRef<str>) -> Result<Self, PromptValueError> {
        let text = normalize_newlines(text.as_ref());
        let maximum = ProtocolLimits::v1_0().prompt.max_message_part_bytes as usize;
        validate_prompt_text(&text, maximum, false)?;
        Ok(Self::Text(MessageText(text.into())))
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct_text(text: impl AsRef<str>) -> Result<Self, PromptValueError> {
        Self::text(text)
    }

    pub fn as_text(&self) -> &str {
        match self {
            Self::Text(text) => text.as_str(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct MessageRecord {
    content: Arc<[MessageContent]>,
}

impl MessageRecord {
    #[allow(dead_code, reason = "consumed by PromptSet composition and replay")]
    fn new(content: Vec<MessageContent>) -> Result<Self, PromptValueError> {
        let limits = ProtocolLimits::v1_0().prompt;
        if content.is_empty() || content.len() > limits.max_user_message_parts as usize {
            return Err(PromptValueError::InvalidPartCount);
        }
        let mut aggregate = 0_usize;
        for part in &content {
            let text = part.as_text();
            validate_prompt_text(text, limits.max_message_part_bytes as usize, false)?;
            aggregate = aggregate
                .checked_add(text.len())
                .ok_or(PromptValueError::TextTooLong)?;
            if aggregate > limits.max_user_message_bytes as usize {
                return Err(PromptValueError::TextTooLong);
            }
        }
        Ok(Self {
            content: content.into(),
        })
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct(content: Vec<MessageContent>) -> Result<Self, PromptValueError> {
        Self::new(content)
    }

    pub fn content(&self) -> &[MessageContent] {
        &self.content
    }
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PromptContributionOrigin {
    Skill {
        skill_id: SkillId,
    },
    Workspace {
        root_key: WorkspaceRootKey,
        relative_location: WorkspaceRelativePath,
    },
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PromptContributionStamp {
    content_part_index: u32,
    origin: PromptContributionOrigin,
}

impl PromptContributionStamp {
    #[allow(dead_code, reason = "consumed by PromptSet composition and replay")]
    fn new(
        content_part_index: u32,
        origin: PromptContributionOrigin,
    ) -> Result<Self, PromptValueError> {
        if let PromptContributionOrigin::Workspace {
            relative_location, ..
        } = &origin
        {
            validate_prompt_text(
                relative_location.as_str(),
                ProtocolLimits::v1_0().workspace.max_relative_path_bytes as usize,
                true,
            )?;
        }
        Ok(Self {
            content_part_index,
            origin,
        })
    }

    #[allow(dead_code, reason = "consumed by Conversation codec in M3")]
    pub(crate) fn reconstruct(
        content_part_index: u32,
        origin: PromptContributionOrigin,
    ) -> Result<Self, PromptValueError> {
        Self::new(content_part_index, origin)
    }

    pub const fn content_part_index(&self) -> u32 {
        self.content_part_index
    }

    pub const fn origin(&self) -> &PromptContributionOrigin {
        &self.origin
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct CanonicalUserMessage {
    message: MessageRecord,
    contribution_stamps: Arc<[PromptContributionStamp]>,
}

impl CanonicalUserMessage {
    #[allow(dead_code, reason = "consumed by PromptSet composition and replay")]
    fn new(
        message: MessageRecord,
        contribution_stamps: Vec<PromptContributionStamp>,
    ) -> Result<Self, PromptValueError> {
        validate_contribution_stamps(&message, &contribution_stamps, true)?;
        Ok(Self {
            message,
            contribution_stamps: contribution_stamps.into(),
        })
    }

    #[allow(
        dead_code,
        reason = "consumed by tolerant Conversation replay in M3/M5"
    )]
    pub(crate) fn reconstruct(
        message: MessageRecord,
        contribution_stamps: Vec<PromptContributionStamp>,
    ) -> Result<Self, PromptValueError> {
        validate_contribution_stamps(&message, &contribution_stamps, false)?;
        Ok(Self {
            message,
            contribution_stamps: contribution_stamps.into(),
        })
    }

    pub const fn message(&self) -> &MessageRecord {
        &self.message
    }

    pub fn contribution_stamps(&self) -> &[PromptContributionStamp] {
        &self.contribution_stamps
    }

    pub(crate) fn validate_for_wire(&self) -> Result<(), PromptValueError> {
        validate_contribution_stamps(&self.message, &self.contribution_stamps, true)
    }
}

fn validate_contribution_stamps(
    message: &MessageRecord,
    contribution_stamps: &[PromptContributionStamp],
    require_complete_provenance: bool,
) -> Result<(), PromptValueError> {
    if contribution_stamps.len() > message.content().len() {
        return Err(PromptValueError::InvalidContributionStamp);
    }
    let mut indices = BTreeSet::new();
    let mut origins = BTreeSet::new();
    let mut previous_index = None;
    for stamp in contribution_stamps {
        let index = stamp.content_part_index() as usize;
        if index >= message.content().len()
            || previous_index.is_some_and(|previous| index <= previous)
            || !indices.insert(index)
            || !origins.insert(stamp.origin())
        {
            return Err(PromptValueError::InvalidContributionStamp);
        }
        previous_index = Some(index);
    }
    if require_complete_provenance {
        let unstamped = (0..message.content().len())
            .filter(|index| !indices.contains(index))
            .collect::<Vec<_>>();
        if unstamped.len() > 1 || unstamped.first().is_some_and(|index| *index != 0) {
            return Err(PromptValueError::InvalidContributionStamp);
        }
    }
    Ok(())
}

impl fmt::Debug for CanonicalUserMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CanonicalUserMessage")
            .field("parts", &self.message.content().len())
            .field("contribution_stamps", &self.contribution_stamps.len())
            .finish()
    }
}

#[allow(dead_code, reason = "consumed by M4 transcript construction")]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct ModelMessageError {
    reason: ModelMessageErrorReason,
}

#[allow(dead_code, reason = "consumed by M4 transcript construction")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModelMessageErrorReason {
    EmptyText,
    UnsafeText,
    TextTooLong,
    EmptyAssistantContent,
    DuplicateToolCallId,
}

impl fmt::Debug for ModelMessageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelMessageError")
            .field("reason", &self.reason)
            .finish()
    }
}

impl fmt::Display for ModelMessageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid model message")
    }
}

impl std::error::Error for ModelMessageError {}

#[allow(dead_code, reason = "consumed by M4 transcript construction")]
#[derive(Clone)]
pub(crate) struct ModelMessage {
    kind: ModelMessageKind,
}

#[allow(
    dead_code,
    reason = "owned exclusively by Prompt transcript construction"
)]
#[derive(Clone)]
enum ModelMessageKind {
    User {
        message: CanonicalUserMessage,
    },
    Assistant {
        content: Arc<[ModelAssistantContent]>,
    },
    Tool {
        tool_call_id: ToolCallId,
        content: ToolResultContent,
    },
}

#[allow(dead_code, reason = "consumed by M4 transcript construction")]
#[derive(Clone)]
pub(crate) struct ModelAssistantContent {
    kind: ModelAssistantContentKind,
}

#[allow(
    dead_code,
    reason = "owned exclusively by Prompt transcript construction"
)]
#[derive(Clone)]
enum ModelAssistantContentKind {
    Reasoning(ReasoningContent),
    Text(Arc<str>),
    ToolCall {
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: BoundedJsonObject,
    },
}

#[allow(dead_code, reason = "consumed by authorized M4 transcript readers")]
#[derive(Clone, Copy)]
pub(crate) enum ModelMessageRef<'a> {
    User {
        content: &'a [MessageContent],
    },
    Assistant {
        content: &'a [ModelAssistantContent],
    },
    Tool {
        tool_call_id: &'a ToolCallId,
        content: &'a ToolResultContent,
    },
}

#[allow(dead_code, reason = "consumed by authorized M4 transcript readers")]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ModelAssistantContentRef<'a> {
    Reasoning(&'a ReasoningContent),
    Text(&'a str),
    ToolCall {
        tool_call_id: &'a ToolCallId,
        name: &'a ToolName,
        arguments: &'a BoundedJsonObject,
    },
}

impl PartialEq for ModelMessageRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::User { content: left }, Self::User { content: right }) => left == right,
            (Self::Assistant { content: left }, Self::Assistant { content: right }) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right.iter())
                        .all(|(left, right)| left.as_ref() == right.as_ref())
            }
            (
                Self::Tool {
                    tool_call_id: left_id,
                    content: left_content,
                },
                Self::Tool {
                    tool_call_id: right_id,
                    content: right_content,
                },
            ) => left_id == right_id && left_content == right_content,
            _ => false,
        }
    }
}

impl Eq for ModelMessageRef<'_> {}

#[allow(dead_code, reason = "consumed by M4 transcript construction")]
impl ModelMessage {
    pub(crate) fn canonical_user(message: CanonicalUserMessage) -> Self {
        Self {
            kind: ModelMessageKind::User { message },
        }
    }

    pub(crate) fn unstamped_user_text(text: Arc<str>) -> Result<Self, ModelMessageError> {
        let text = normalize_newlines(&text);
        let maximum = ProtocolLimits::v1_0().prompt.max_message_part_bytes as usize;
        validate_model_message_text(&text, maximum)?;
        Ok(Self::canonical_user(CanonicalUserMessage {
            message: MessageRecord {
                content: Arc::from([MessageContent::Text(MessageText(text.into()))]),
            },
            contribution_stamps: Arc::from([]),
        }))
    }

    pub(crate) fn rolling_summary(summary: Arc<str>) -> Result<Self, ModelMessageError> {
        validate_model_message_text(&summary, 65_536)?;
        Ok(Self::canonical_user(CanonicalUserMessage {
            message: MessageRecord {
                content: Arc::from([MessageContent::Text(MessageText(summary))]),
            },
            contribution_stamps: Arc::from([]),
        }))
    }

    pub(crate) fn assistant(
        content: Arc<[ModelAssistantContent]>,
    ) -> Result<Self, ModelMessageError> {
        if content.is_empty() {
            return Err(ModelMessageError {
                reason: ModelMessageErrorReason::EmptyAssistantContent,
            });
        }
        let mut tool_call_ids = BTreeSet::new();
        for block in &*content {
            if let ModelAssistantContentKind::ToolCall { tool_call_id, .. } = &block.kind {
                if !tool_call_ids.insert(tool_call_id) {
                    return Err(ModelMessageError {
                        reason: ModelMessageErrorReason::DuplicateToolCallId,
                    });
                }
            }
        }
        Ok(Self {
            kind: ModelMessageKind::Assistant { content },
        })
    }

    pub(crate) fn tool_result(tool_call_id: ToolCallId, content: ToolResultContent) -> Self {
        Self {
            kind: ModelMessageKind::Tool {
                tool_call_id,
                content,
            },
        }
    }

    pub(crate) fn as_ref(&self) -> ModelMessageRef<'_> {
        match &self.kind {
            ModelMessageKind::User { message } => ModelMessageRef::User {
                content: message.message().content(),
            },
            ModelMessageKind::Assistant { content } => ModelMessageRef::Assistant { content },
            ModelMessageKind::Tool {
                tool_call_id,
                content,
            } => ModelMessageRef::Tool {
                tool_call_id,
                content,
            },
        }
    }
}

impl fmt::Debug for ModelMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ModelMessageKind::User { message } => formatter
                .debug_struct("ModelMessage")
                .field("role", &"user")
                .field("content_parts", &message.message().content().len())
                .finish(),
            ModelMessageKind::Assistant { content } => formatter
                .debug_struct("ModelMessage")
                .field("role", &"assistant")
                .field("content_blocks", &content.len())
                .finish(),
            ModelMessageKind::Tool { .. } => formatter
                .debug_struct("ModelMessage")
                .field("role", &"tool")
                .field("content", &"redacted")
                .finish(),
        }
    }
}

#[allow(dead_code, reason = "consumed by M4 transcript construction")]
impl ModelAssistantContent {
    pub(crate) fn reasoning(content: ReasoningContent) -> Self {
        Self {
            kind: ModelAssistantContentKind::Reasoning(content),
        }
    }

    pub(crate) fn text(text: Arc<str>) -> Result<Self, ModelMessageError> {
        validate_model_message_text(&text, 65_536)?;
        Ok(Self {
            kind: ModelAssistantContentKind::Text(text),
        })
    }

    pub(crate) fn tool_call(
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: BoundedJsonObject,
    ) -> Self {
        Self {
            kind: ModelAssistantContentKind::ToolCall {
                tool_call_id,
                name,
                arguments,
            },
        }
    }

    pub(crate) fn as_ref(&self) -> ModelAssistantContentRef<'_> {
        match &self.kind {
            ModelAssistantContentKind::Reasoning(content) => {
                ModelAssistantContentRef::Reasoning(content)
            }
            ModelAssistantContentKind::Text(text) => ModelAssistantContentRef::Text(text),
            ModelAssistantContentKind::ToolCall {
                tool_call_id,
                name,
                arguments,
            } => ModelAssistantContentRef::ToolCall {
                tool_call_id,
                name,
                arguments,
            },
        }
    }
}

impl fmt::Debug for ModelAssistantContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ModelAssistantContentKind::Reasoning(_) => formatter
                .debug_tuple("ModelAssistantContent::Reasoning")
                .field(&"redacted")
                .finish(),
            ModelAssistantContentKind::Text(_) => formatter
                .debug_tuple("ModelAssistantContent::Text")
                .field(&"redacted")
                .finish(),
            ModelAssistantContentKind::ToolCall { .. } => formatter
                .debug_tuple("ModelAssistantContent::ToolCall")
                .field(&"redacted")
                .finish(),
        }
    }
}

impl fmt::Debug for ModelMessageRef<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User { content } => formatter
                .debug_struct("ModelMessageRef::User")
                .field("content_parts", &content.len())
                .finish(),
            Self::Assistant { content } => formatter
                .debug_struct("ModelMessageRef::Assistant")
                .field("content_blocks", &content.len())
                .finish(),
            Self::Tool { .. } => formatter
                .debug_struct("ModelMessageRef::Tool")
                .field("content", &"redacted")
                .finish(),
        }
    }
}

impl fmt::Debug for ModelAssistantContentRef<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reasoning(_) => formatter
                .debug_tuple("ModelAssistantContentRef::Reasoning")
                .field(&"redacted")
                .finish(),
            Self::Text(_) => formatter
                .debug_tuple("ModelAssistantContentRef::Text")
                .field(&"redacted")
                .finish(),
            Self::ToolCall { .. } => formatter
                .debug_tuple("ModelAssistantContentRef::ToolCall")
                .field(&"redacted")
                .finish(),
        }
    }
}

#[allow(dead_code, reason = "consumed by M4 transcript construction")]
fn validate_model_message_text(text: &str, maximum: usize) -> Result<(), ModelMessageError> {
    validate_safe_text(text, maximum, false).map_err(|error| ModelMessageError {
        reason: match error {
            LexicalError::Empty => ModelMessageErrorReason::EmptyText,
            LexicalError::TooLong => ModelMessageErrorReason::TextTooLong,
            LexicalError::InvalidGrammar | LexicalError::UnsafeText => {
                ModelMessageErrorReason::UnsafeText
            }
        },
    })
}

fn validate_prompt_text(
    text: &str,
    maximum: usize,
    allow_empty: bool,
) -> Result<(), PromptValueError> {
    validate_safe_text(text, maximum, allow_empty).map_err(|error| match error {
        crate::wire::lexical::LexicalError::Empty => PromptValueError::EmptyText,
        crate::wire::lexical::LexicalError::TooLong => PromptValueError::TextTooLong,
        crate::wire::lexical::LexicalError::InvalidGrammar
        | crate::wire::lexical::LexicalError::UnsafeText => PromptValueError::UnsafeText,
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::Arc;

    use super::*;
    use crate::model_gateway::ProviderItemId;
    use crate::tools::{ToolCallId, ToolName, ToolResultContent};
    use crate::wire::BoundedJsonObject;

    fn assert_model_message_error<T>(
        result: Result<T, ModelMessageError>,
        expected: ModelMessageErrorReason,
    ) {
        match result {
            Err(error) => assert_eq!(error.reason, expected),
            Ok(_) => panic!("model message construction unexpectedly succeeded"),
        }
    }

    fn reasoning_content() -> ReasoningContent {
        ReasoningContent::reconstruct(
            Some("reasoning artifact".to_owned()),
            Some("reasoning summary".to_owned()),
            Some("encrypted artifact".to_owned()),
            Some("reasoning signature".to_owned()),
            Some(ProviderItemId::from_str("provider-item").unwrap()),
        )
        .unwrap()
    }

    fn tool_call_id(value: &str) -> ToolCallId {
        ToolCallId::from_str(value).unwrap()
    }

    fn tool_name(value: &str) -> ToolName {
        ToolName::from_str(value).unwrap()
    }

    fn tool_arguments() -> BoundedJsonObject {
        BoundedJsonObject::from_slice(br#"{"query":"argument secret"}"#).unwrap()
    }

    fn tool_result() -> ToolResultContent {
        ToolResultContent::from_text_parts(vec!["tool result secret".to_owned()]).unwrap()
    }

    fn canonical_user_with_stamp() -> CanonicalUserMessage {
        let message =
            MessageRecord::new(vec![MessageContent::text("user secret").unwrap()]).unwrap();
        let stamp = PromptContributionStamp::new(
            0,
            PromptContributionOrigin::Skill {
                skill_id: SkillId::from_str("review").unwrap(),
            },
        )
        .unwrap();
        CanonicalUserMessage::new(message, vec![stamp]).unwrap()
    }

    #[test]
    fn model_message_user_constructors_project_only_content() {
        let canonical = ModelMessage::canonical_user(canonical_user_with_stamp());
        let ModelMessageRef::User { content } = canonical.as_ref() else {
            panic!("canonical user did not retain the user role");
        };
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].as_text(), "user secret");

        let unstamped = ModelMessage::unstamped_user_text(Arc::from("a\r\nb\rc")).unwrap();
        let ModelMessageRef::User { content } = unstamped.as_ref() else {
            panic!("unstamped user text did not construct a user message");
        };
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].as_text(), "a\nb\nc");
        let ModelMessageKind::User { message } = &unstamped.kind else {
            panic!("unstamped user text did not retain the user role");
        };
        assert!(message.contribution_stamps().is_empty());

        assert_model_message_error(
            ModelMessage::unstamped_user_text(Arc::from("")),
            ModelMessageErrorReason::EmptyText,
        );
        assert_model_message_error(
            ModelMessage::unstamped_user_text(Arc::from("x".repeat(131_073))),
            ModelMessageErrorReason::TextTooLong,
        );
        assert_model_message_error(
            ModelMessage::unstamped_user_text(Arc::from("unsafe\u{001b}")),
            ModelMessageErrorReason::UnsafeText,
        );
    }

    #[test]
    fn rolling_summary_is_verbatim_one_unstamped_user_text_and_rejects_invalid_text() {
        let summary = ModelMessage::rolling_summary(Arc::from("first line\nsecond line")).unwrap();
        let ModelMessageRef::User { content } = summary.as_ref() else {
            panic!("rolling summary did not construct a user message");
        };
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].as_text(), "first line\nsecond line");
        let ModelMessageKind::User { message } = &summary.kind else {
            panic!("rolling summary did not retain the user role");
        };
        assert!(message.contribution_stamps().is_empty());

        assert_model_message_error(
            ModelMessage::rolling_summary(Arc::from("")),
            ModelMessageErrorReason::EmptyText,
        );
        assert_model_message_error(
            ModelMessage::rolling_summary(Arc::from("x".repeat(65_537))),
            ModelMessageErrorReason::TextTooLong,
        );
        assert_model_message_error(
            ModelMessage::rolling_summary(Arc::from("first\rsecond")),
            ModelMessageErrorReason::UnsafeText,
        );
        assert_model_message_error(
            ModelMessage::rolling_summary(Arc::from("first\r\nsecond")),
            ModelMessageErrorReason::UnsafeText,
        );
        assert_model_message_error(
            ModelMessage::rolling_summary(Arc::from("unsafe\u{001b}")),
            ModelMessageErrorReason::UnsafeText,
        );
    }

    #[test]
    fn assistant_content_and_message_preserve_source_order() {
        let reasoning = reasoning_content();
        let text = ModelAssistantContent::text(Arc::from("assistant\ntext")).unwrap();
        let call_id = tool_call_id("call-1");
        let name = tool_name("read_file");
        let arguments = tool_arguments();
        let call =
            ModelAssistantContent::tool_call(call_id.clone(), name.clone(), arguments.clone());
        let content: Arc<[ModelAssistantContent]> = Arc::from([
            ModelAssistantContent::reasoning(reasoning.clone()),
            text.clone(),
            call.clone(),
        ]);
        let assistant = ModelMessage::assistant(content).unwrap();

        let ModelMessageRef::Assistant { content } = assistant.as_ref() else {
            panic!("assistant content did not construct an assistant message");
        };
        assert_eq!(content.len(), 3);
        assert!(
            matches!(content[0].as_ref(), ModelAssistantContentRef::Reasoning(actual) if actual == &reasoning)
        );
        assert!(matches!(
            content[1].as_ref(),
            ModelAssistantContentRef::Text("assistant\ntext")
        ));
        assert!(matches!(
            content[2].as_ref(),
            ModelAssistantContentRef::ToolCall {
                tool_call_id,
                name: actual_name,
                arguments: actual_arguments,
            } if tool_call_id == &call_id
                && actual_name == &name
                && actual_arguments == &arguments
        ));

        assert!(matches!(
            text.as_ref(),
            ModelAssistantContentRef::Text("assistant\ntext")
        ));
        assert!(matches!(
            call.as_ref(),
            ModelAssistantContentRef::ToolCall { .. }
        ));
    }

    #[test]
    fn assistant_content_text_uses_external_text_rules() {
        let text = ModelAssistantContent::text(Arc::from("line one\nline two")).unwrap();
        assert!(matches!(
            text.as_ref(),
            ModelAssistantContentRef::Text("line one\nline two")
        ));
        assert_model_message_error(
            ModelAssistantContent::text(Arc::from("")),
            ModelMessageErrorReason::EmptyText,
        );
        assert_model_message_error(
            ModelAssistantContent::text(Arc::from("x".repeat(65_537))),
            ModelMessageErrorReason::TextTooLong,
        );
        assert_model_message_error(
            ModelAssistantContent::text(Arc::from("line one\r\nline two")),
            ModelMessageErrorReason::UnsafeText,
        );
    }

    #[test]
    fn assistant_rejects_empty_and_duplicate_tool_calls() {
        assert_model_message_error(
            ModelMessage::assistant(Arc::from([])),
            ModelMessageErrorReason::EmptyAssistantContent,
        );

        let duplicate = tool_call_id("call-duplicate");
        let content: Arc<[ModelAssistantContent]> = Arc::from([
            ModelAssistantContent::tool_call(
                duplicate.clone(),
                tool_name("read_file"),
                tool_arguments(),
            ),
            ModelAssistantContent::text(Arc::from("between calls")).unwrap(),
            ModelAssistantContent::tool_call(duplicate, tool_name("write_file"), tool_arguments()),
        ]);
        assert_model_message_error(
            ModelMessage::assistant(content),
            ModelMessageErrorReason::DuplicateToolCallId,
        );
    }

    #[test]
    fn tool_result_projection_exposes_only_tools_owned_values() {
        let call_id = tool_call_id("call-tool-result");
        let result = tool_result();
        let message = ModelMessage::tool_result(call_id.clone(), result.clone());
        let ModelMessageRef::Tool {
            tool_call_id,
            content,
        } = message.as_ref()
        else {
            panic!("tool result did not construct a tool message");
        };
        assert_eq!(tool_call_id, &call_id);
        assert_eq!(content, &result);
    }

    #[test]
    fn model_messages_and_content_clone_preserve_read_projections() {
        let content = ModelAssistantContent::text(Arc::from("assistant text")).unwrap();
        let content_clone = content.clone();
        assert_eq!(content.as_ref(), content_clone.as_ref());
        assert!(matches!(
            content_clone.as_ref(),
            ModelAssistantContentRef::Text("assistant text")
        ));

        let messages = [
            ModelMessage::unstamped_user_text(Arc::from("user text")).unwrap(),
            ModelMessage::assistant(Arc::from([content])).unwrap(),
            ModelMessage::tool_result(tool_call_id("call-clone"), tool_result()),
        ];
        for message in messages {
            let clone = message.clone();
            assert_eq!(message.as_ref(), clone.as_ref());
            match (message.as_ref(), clone.as_ref()) {
                (
                    ModelMessageRef::User {
                        content: original_content,
                    },
                    ModelMessageRef::User {
                        content: cloned_content,
                    },
                ) => {
                    assert_eq!(original_content.len(), cloned_content.len());
                    for (original, cloned) in original_content.iter().zip(cloned_content) {
                        assert_eq!(original.as_text(), cloned.as_text());
                    }
                }
                (
                    ModelMessageRef::Assistant {
                        content: original_content,
                    },
                    ModelMessageRef::Assistant {
                        content: cloned_content,
                    },
                ) => {
                    assert_eq!(original_content.len(), cloned_content.len());
                    for (original, cloned) in original_content.iter().zip(cloned_content) {
                        assert_eq!(original.as_ref(), cloned.as_ref());
                    }
                }
                (
                    ModelMessageRef::Tool {
                        tool_call_id: original_id,
                        content: original_content,
                    },
                    ModelMessageRef::Tool {
                        tool_call_id: cloned_id,
                        content: cloned_content,
                    },
                ) => {
                    assert_eq!(original_id, cloned_id);
                    assert_eq!(original_content, cloned_content);
                }
                _ => panic!("cloning changed the message role"),
            }
        }
    }

    #[test]
    fn model_transcript_debug_is_redacted() {
        let user = ModelMessage::canonical_user(canonical_user_with_stamp());
        let reasoning = ModelAssistantContent::reasoning(reasoning_content());
        let text = ModelAssistantContent::text(Arc::from("assistant secret")).unwrap();
        let call = ModelAssistantContent::tool_call(
            tool_call_id("call-secret"),
            tool_name("secret_tool"),
            tool_arguments(),
        );
        let assistant =
            ModelMessage::assistant(Arc::from([reasoning.clone(), text.clone(), call.clone()]))
                .unwrap();
        let tool = ModelMessage::tool_result(tool_call_id("tool-call-secret"), tool_result());
        let error = ModelAssistantContent::text(Arc::from("error secret\r")).unwrap_err();

        let debug = [
            format!("{user:?}"),
            format!("{:?}", user.as_ref()),
            format!("{assistant:?}"),
            format!("{:?}", assistant.as_ref()),
            format!("{tool:?}"),
            format!("{:?}", tool.as_ref()),
            format!("{reasoning:?}"),
            format!("{:?}", reasoning.as_ref()),
            format!("{text:?}"),
            format!("{:?}", text.as_ref()),
            format!("{call:?}"),
            format!("{:?}", call.as_ref()),
            format!("{error:?}"),
        ];
        for value in debug {
            for secret in [
                "user secret",
                "review",
                "reasoning artifact",
                "reasoning summary",
                "encrypted artifact",
                "reasoning signature",
                "provider-item",
                "assistant secret",
                "argument secret",
                "call-secret",
                "secret_tool",
                "tool-call-secret",
                "tool result secret",
                "error secret",
            ] {
                assert!(!value.contains(secret));
            }
        }
    }

    #[test]
    fn canonical_message_enforces_parts_aggregate_and_stamp_indices() {
        let body = MessageContent::text("body").unwrap();
        let contribution = MessageContent::text("contribution").unwrap();
        let message = MessageRecord::new(vec![body, contribution]).unwrap();
        let stamp = PromptContributionStamp::new(
            1,
            PromptContributionOrigin::Skill {
                skill_id: SkillId::from_str("review").unwrap(),
            },
        )
        .unwrap();
        let canonical = CanonicalUserMessage::new(message, vec![stamp]).unwrap();
        assert_eq!(canonical.message().content().len(), 2);
        assert_eq!(canonical.contribution_stamps()[0].content_part_index(), 1);

        let duplicate = canonical.contribution_stamps()[0].clone();
        assert_eq!(
            CanonicalUserMessage::new(
                canonical.message.clone(),
                vec![duplicate.clone(), duplicate]
            ),
            Err(PromptValueError::InvalidContributionStamp)
        );

        let out_of_range = PromptContributionStamp::new(
            2,
            PromptContributionOrigin::Skill {
                skill_id: SkillId::from_str("other").unwrap(),
            },
        )
        .unwrap();
        assert_eq!(
            CanonicalUserMessage::new(canonical.message.clone(), vec![out_of_range]),
            Err(PromptValueError::InvalidContributionStamp)
        );

        let same_origin =
            PromptContributionStamp::new(0, canonical.contribution_stamps()[0].origin().clone())
                .unwrap();
        assert_eq!(
            CanonicalUserMessage::new(
                canonical.message.clone(),
                vec![same_origin, canonical.contribution_stamps()[0].clone()]
            ),
            Err(PromptValueError::InvalidContributionStamp)
        );
    }

    #[test]
    fn replay_reconstruction_preserves_text_after_stamp_degradation() {
        let message = MessageRecord::reconstruct(vec![
            MessageContent::reconstruct_text("body").unwrap(),
            MessageContent::reconstruct_text("unstamped contribution").unwrap(),
            MessageContent::reconstruct_text("valid contribution").unwrap(),
        ])
        .unwrap();
        let surviving_stamp = PromptContributionStamp::reconstruct(
            2,
            PromptContributionOrigin::Skill {
                skill_id: SkillId::from_str("review").unwrap(),
            },
        )
        .unwrap();

        assert_eq!(
            CanonicalUserMessage::new(message.clone(), vec![surviving_stamp.clone()]),
            Err(PromptValueError::InvalidContributionStamp)
        );
        let out_of_range = PromptContributionStamp::reconstruct(
            3,
            PromptContributionOrigin::Skill {
                skill_id: SkillId::from_str("other").unwrap(),
            },
        )
        .unwrap();
        assert_eq!(
            CanonicalUserMessage::reconstruct(message.clone(), vec![out_of_range]),
            Err(PromptValueError::InvalidContributionStamp)
        );
        let earlier_stamp = PromptContributionStamp::reconstruct(
            1,
            PromptContributionOrigin::Skill {
                skill_id: SkillId::from_str("other").unwrap(),
            },
        )
        .unwrap();
        assert_eq!(
            CanonicalUserMessage::reconstruct(
                message.clone(),
                vec![surviving_stamp.clone(), earlier_stamp]
            ),
            Err(PromptValueError::InvalidContributionStamp)
        );
        let duplicate_origin =
            PromptContributionStamp::reconstruct(1, surviving_stamp.origin().clone()).unwrap();
        assert_eq!(
            CanonicalUserMessage::reconstruct(
                message.clone(),
                vec![duplicate_origin, surviving_stamp.clone()]
            ),
            Err(PromptValueError::InvalidContributionStamp)
        );

        let reconstructed =
            CanonicalUserMessage::reconstruct(message, vec![surviving_stamp]).unwrap();
        assert_eq!(reconstructed.message().content().len(), 3);
        assert_eq!(
            reconstructed.message().content()[1].as_text(),
            "unstamped contribution"
        );
        assert_eq!(reconstructed.contribution_stamps().len(), 1);
    }

    #[test]
    fn message_record_enforces_part_and_aggregate_limits() {
        let limits = ProtocolLimits::v1_0().prompt;
        assert!(MessageRecord::new(Vec::new()).is_err());
        assert!(
            MessageContent::text("x".repeat(limits.max_message_part_bytes as usize + 1)).is_err()
        );
        assert!(
            MessageRecord::new(
                (0..5)
                    .map(|_| MessageContent::text("x".repeat(120_000)).unwrap())
                    .collect()
            )
            .is_err()
        );

        let normalized = MessageContent::text("a\r\nb\rc").unwrap();
        assert_eq!(normalized.as_text(), "a\nb\nc");

        let boundary_parts = (0..limits.max_user_message_parts)
            .map(|_| MessageContent::text("x").unwrap())
            .collect::<Vec<_>>();
        assert!(MessageRecord::new(boundary_parts.clone()).is_ok());
        let mut oversized_parts = boundary_parts;
        oversized_parts.push(MessageContent::text("x").unwrap());
        assert!(MessageRecord::new(oversized_parts).is_err());

        let aggregate_boundary = (0..4)
            .map(|_| MessageContent::text("x".repeat(131_072)).unwrap())
            .collect::<Vec<_>>();
        assert!(MessageRecord::new(aggregate_boundary.clone()).is_ok());
        let mut aggregate_oversized = aggregate_boundary;
        aggregate_oversized.push(MessageContent::text("x").unwrap());
        assert!(MessageRecord::new(aggregate_oversized).is_err());
    }

    #[test]
    fn workspace_relative_path_rejects_unsafe_location_before_prompt_stamping() {
        assert!(WorkspaceRelativePath::from_str("src/\u{001b}[31m").is_err());
    }
}
