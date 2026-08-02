use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use thiserror::Error;

use crate::skills::SkillId;
use crate::wire::lexical::{normalize_newlines, validate_safe_text};
use crate::wire::{ProtocolLimits, WorkspaceRelativePath};
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

    use super::*;

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
    fn workspace_stamp_requires_a_model_safe_relative_location() {
        let unsafe_path = WorkspaceRelativePath::from_str("src/\u{001b}[31m").unwrap();
        assert!(matches!(
            PromptContributionStamp::new(
                0,
                PromptContributionOrigin::Workspace {
                    root_key: WorkspaceRootKey::from_str("repo").unwrap(),
                    relative_location: unsafe_path,
                },
            ),
            Err(PromptValueError::UnsafeText)
        ));
    }
}
