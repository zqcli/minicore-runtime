use std::fmt;
use std::sync::Arc;

use crate::conversation_storage::{StoredAssistantContent, StoredEntryBody, StoredSessionEntry};
use crate::live_conversation::LiveSessionState;
use crate::turn_item_interaction::UserMessageSource;
use crate::wire::{ItemId, Timestamp, TurnId};

/// Which side of the conversation produced a transcript item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTranscriptItemRole {
    User,
    Assistant,
}

/// One user or assistant message in the session transcript.
///
/// Crate owners construct instances from already-validated bodies; the body is
/// never printed by `Debug` (only its byte length plus identity and role).
#[derive(Clone, Eq, PartialEq)]
pub struct SessionTranscriptItem {
    item_id: ItemId,
    turn_id: TurnId,
    content: SessionTranscriptItemContent,
    created_at: Timestamp,
}

#[derive(Clone, Eq, PartialEq)]
enum SessionTranscriptItemContent {
    UserMessage {
        source: UserMessageSource,
        body: Box<str>,
    },
    AssistantMessage {
        body: Box<str>,
    },
}

impl SessionTranscriptItem {
    /// Constructs a user-message item from a caller-validated body.
    pub(crate) fn user_message(
        item_id: ItemId,
        turn_id: TurnId,
        source: UserMessageSource,
        body: Box<str>,
        created_at: Timestamp,
    ) -> Self {
        Self {
            item_id,
            turn_id,
            content: SessionTranscriptItemContent::UserMessage { source, body },
            created_at,
        }
    }

    /// Constructs an assistant-message item from a caller-validated body.
    pub(crate) fn assistant_message(
        item_id: ItemId,
        turn_id: TurnId,
        body: Box<str>,
        created_at: Timestamp,
    ) -> Self {
        Self {
            item_id,
            turn_id,
            content: SessionTranscriptItemContent::AssistantMessage { body },
            created_at,
        }
    }

    pub const fn item_id(&self) -> ItemId {
        self.item_id
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub const fn created_at(&self) -> Timestamp {
        self.created_at
    }

    pub fn body(&self) -> &str {
        match &self.content {
            SessionTranscriptItemContent::UserMessage { body, .. }
            | SessionTranscriptItemContent::AssistantMessage { body } => body,
        }
    }

    pub const fn user_source(&self) -> Option<UserMessageSource> {
        match &self.content {
            SessionTranscriptItemContent::UserMessage { source, .. } => Some(*source),
            SessionTranscriptItemContent::AssistantMessage { .. } => None,
        }
    }

    pub const fn role(&self) -> SessionTranscriptItemRole {
        match &self.content {
            SessionTranscriptItemContent::UserMessage { .. } => SessionTranscriptItemRole::User,
            SessionTranscriptItemContent::AssistantMessage { .. } => {
                SessionTranscriptItemRole::Assistant
            }
        }
    }
}

impl fmt::Debug for SessionTranscriptItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionTranscriptItem")
            .field("item_id", &self.item_id)
            .field("turn_id", &self.turn_id)
            .field("role", &self.role())
            .field("user_source", &self.user_source())
            .field("body_bytes", &self.body().len())
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// Immutable snapshot of a live session's stable transcript history.
///
/// Owns only `Arc` clones of the stored selected-path entries; page bodies are
/// cloned on demand by [`SessionTranscriptCapture::page`]. `Debug` never prints
/// message bodies.
#[derive(Clone)]
pub(crate) struct SessionTranscriptCapture {
    entries: Arc<[Arc<StoredSessionEntry>]>,
}

impl SessionTranscriptCapture {
    /// Captures `state`'s selected path, excluding the running turn (if any):
    /// in-flight content is surfaced by `SessionSnapshot::active_items` instead
    /// of the transcript.
    pub(crate) fn from_live_state(state: &LiveSessionState) -> Self {
        let entries = match state.current_turn() {
            Some(turn_id) => state
                .selected_entries()
                .iter()
                .filter(|entry| entry.turn_id() != turn_id)
                .cloned()
                .collect(),
            None => state.selected_entries().to_vec(),
        };
        Self {
            entries: Arc::from(entries),
        }
    }

    /// Projects up to `limit` items starting at displayable-item `offset` (not
    /// an entry offset), in selected-path physical order. Returns `None` past
    /// the last item; `offset == total` yields an empty page with no next
    /// offset.
    pub(crate) fn page(&self, offset: usize, limit: usize) -> Option<SessionTranscriptSlice> {
        debug_assert!(limit != 0, "page limit must be non-zero");
        let total = self.item_count();
        if offset > total {
            return None;
        }
        let mut items = Vec::with_capacity(limit.min(total - offset));
        let mut item_index = 0usize;
        let mut remaining = limit;
        for entry in self.entries.iter() {
            if remaining == 0 {
                break;
            }
            let entry_items = Self::entry_item_count(entry);
            if item_index + entry_items <= offset {
                item_index += entry_items;
                continue;
            }
            match entry.body() {
                StoredEntryBody::UserMessage(message) => {
                    // A user entry is a single item, so its body is joined
                    // and cloned only when it lands in the page.
                    if item_index >= offset {
                        let body = message
                            .content()
                            .message()
                            .content()
                            .iter()
                            .map(|part| part.as_text())
                            .collect::<Vec<_>>()
                            .join("\n");
                        items.push(SessionTranscriptItem::user_message(
                            message.item_id(),
                            entry.turn_id(),
                            message.source(),
                            body.into_boxed_str(),
                            entry.timestamp(),
                        ));
                        remaining -= 1;
                    }
                    item_index += 1;
                }
                StoredEntryBody::AssistantMessage(message) => {
                    for content in message.content() {
                        let StoredAssistantContent::Text { item_id, text } = content else {
                            continue;
                        };
                        // Text is cloned only when it lands in the page;
                        // reasoning and tool calls count no item.
                        if item_index >= offset && remaining > 0 {
                            items.push(SessionTranscriptItem::assistant_message(
                                *item_id,
                                entry.turn_id(),
                                Box::from(text.as_ref()),
                                entry.timestamp(),
                            ));
                            remaining -= 1;
                        }
                        item_index += 1;
                    }
                }
                StoredEntryBody::ToolMessage(_)
                | StoredEntryBody::InteractionRequested(_)
                | StoredEntryBody::InteractionResolved(_)
                | StoredEntryBody::Compaction(_) => {}
            }
        }
        let next_offset = offset + items.len();
        Some(SessionTranscriptSlice {
            items,
            next_offset: (next_offset < total).then_some(next_offset),
        })
    }

    /// Total projected transcript items across all captured entries.
    fn item_count(&self) -> usize {
        self.entries
            .iter()
            .map(|entry| Self::entry_item_count(entry))
            .sum()
    }

    /// Number of transcript items one stored entry projects.
    fn entry_item_count(entry: &StoredSessionEntry) -> usize {
        match entry.body() {
            StoredEntryBody::UserMessage(_) => 1,
            StoredEntryBody::AssistantMessage(message) => message
                .content()
                .iter()
                .filter(|content| matches!(content, StoredAssistantContent::Text { .. }))
                .count(),
            StoredEntryBody::ToolMessage(_)
            | StoredEntryBody::InteractionRequested(_)
            | StoredEntryBody::InteractionResolved(_)
            | StoredEntryBody::Compaction(_) => 0,
        }
    }
}

/// One transcript page: projected items plus the offset of the next page.
pub(crate) struct SessionTranscriptSlice {
    items: Vec<SessionTranscriptItem>,
    next_offset: Option<usize>,
}

impl SessionTranscriptSlice {
    pub(crate) fn into_parts(self) -> (Vec<SessionTranscriptItem>, Option<usize>) {
        (self.items, self.next_offset)
    }
}

impl fmt::Debug for SessionTranscriptCapture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionTranscriptCapture")
            .field("entries", &self.entries.len())
            .field("items", &self.item_count())
            .finish()
    }
}

impl fmt::Debug for SessionTranscriptSlice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionTranscriptSlice")
            .field("items", &self.items.len())
            .field("next_offset", &self.next_offset)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::conversation_storage::{StoredAssistantMessage, StoredUserMessage};
    use crate::model_gateway::{
        ModelFinishReason, ModelId, ModelReasoningSummary, ModelResponseSummary, ModelServiceClass,
        ProviderId, ProviderResponseMetadata, ReasoningContent,
    };
    use crate::prompt::{
        CanonicalUserMessage, MessageContent, MessageRecord, PromptContributionOrigin,
        PromptContributionStamp,
    };
    use crate::turn_item_interaction::AssistantDisposition;

    fn item(number: u8) -> ItemId {
        format!("itm_{number:032x}")
            .parse()
            .expect("test item IDs are valid")
    }

    fn turn(number: u8) -> TurnId {
        format!("trn_{number:032x}")
            .parse()
            .expect("test turn IDs are valid")
    }

    fn timestamp() -> Timestamp {
        "2026-07-31T12:00:00.000Z"
            .parse()
            .expect("test timestamps are valid")
    }

    fn stored_user(item_id: ItemId, parts: &[&str]) -> StoredUserMessage {
        let content = CanonicalUserMessage::reconstruct(
            MessageRecord::reconstruct(
                parts
                    .iter()
                    .map(|part| MessageContent::reconstruct_text(part).unwrap())
                    .collect(),
            )
            .unwrap(),
            (1..parts.len())
                .map(|index| {
                    PromptContributionStamp::reconstruct(
                        index as u32,
                        PromptContributionOrigin::Skill {
                            skill_id: "review".parse().unwrap(),
                        },
                    )
                    .unwrap()
                })
                .collect(),
        )
        .unwrap();
        StoredUserMessage::reconstruct(item_id, UserMessageSource::Input, content)
    }

    fn stored_assistant(content: Vec<StoredAssistantContent>) -> StoredAssistantMessage {
        StoredAssistantMessage::reconstruct(
            AssistantDisposition::Final,
            content,
            ModelResponseSummary::reconstruct(
                "fixture".parse::<ProviderId>().unwrap(),
                "scripted".parse::<ModelId>().unwrap(),
                ModelReasoningSummary::Disabled,
                ModelServiceClass::Standard,
            ),
            None,
            ModelFinishReason::Stop,
            NonZeroU32::new(1).unwrap(),
            None,
            0,
            ProviderResponseMetadata::reconstruct(None, None, None),
        )
        .unwrap()
    }

    #[test]
    fn capture_excludes_running_turn_and_user_parts_join_with_newlines() {
        let session_id = "ses_11111111111111111111111111111111"
            .parse()
            .expect("test session IDs are valid");
        let turn_id = turn(1);
        let mut state = LiveSessionState::new(session_id, []);
        state
            .apply_user_message(
                stored_user(item(1), &["first", "second"]),
                turn_id,
                timestamp(),
            )
            .unwrap();

        // The running turn is excluded from the stable transcript.
        let capture = SessionTranscriptCapture::from_live_state(&state);
        let (items, next) = capture.page(0, 10).unwrap().into_parts();
        assert!(items.is_empty());
        assert!(next.is_none());

        state.fail_current_turn(turn_id).unwrap();
        let capture = SessionTranscriptCapture::from_live_state(&state);
        let (items, next) = capture.page(0, 10).unwrap().into_parts();
        assert_eq!(items.len(), 1);
        let transcript_item = &items[0];
        assert_eq!(transcript_item.role(), SessionTranscriptItemRole::User);
        assert_eq!(transcript_item.body(), "first\nsecond");
        assert_eq!(
            transcript_item.user_source(),
            Some(UserMessageSource::Input)
        );
        assert_eq!(transcript_item.item_id(), item(1));
        assert_eq!(transcript_item.turn_id(), turn_id);
        assert_eq!(transcript_item.created_at(), timestamp());
        assert!(next.is_none());
    }

    #[test]
    fn assistant_text_item_offset_skips_reasoning_and_debug_redacts_body() {
        let entry = StoredSessionEntry::reconstruct(
            "ent_11111111111111111111111111111111"
                .parse()
                .expect("test entry IDs are valid"),
            None,
            "ses_11111111111111111111111111111111"
                .parse()
                .expect("test session IDs are valid"),
            turn(1),
            timestamp(),
            StoredEntryBody::AssistantMessage(stored_assistant(vec![
                StoredAssistantContent::Reasoning {
                    item_id: item(1),
                    content: ReasoningContent::reconstruct(
                        None,
                        Some("secret reasoning".to_owned()),
                        None,
                        None,
                        None,
                    )
                    .unwrap(),
                },
                StoredAssistantContent::Text {
                    item_id: item(2),
                    text: Arc::from("first secret text"),
                },
                StoredAssistantContent::Text {
                    item_id: item(3),
                    text: Arc::from("second secret text"),
                },
            ])),
        );
        let capture = SessionTranscriptCapture {
            entries: Arc::from([Arc::new(entry)]),
        };

        // Reasoning is not a displayable item, so offset 1 lands on the second text.
        let (items, next) = capture.page(1, 1).unwrap().into_parts();
        assert_eq!(items.len(), 1);
        let transcript_item = &items[0];
        assert_eq!(transcript_item.body(), "second secret text");
        assert_eq!(transcript_item.item_id(), item(3));
        assert_eq!(transcript_item.turn_id(), turn(1));
        assert_eq!(transcript_item.created_at(), timestamp());
        assert!(next.is_none());

        // Debug output never leaks message bodies.
        assert!(!format!("{transcript_item:?}").contains("second secret text"));
        let debug = format!("{capture:?}");
        assert!(!debug.contains("secret reasoning"));
        assert!(!debug.contains("secret text"));
    }
}
