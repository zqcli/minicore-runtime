use std::fmt;

use thiserror::Error;

use crate::tools::{
    ToolApprovalDecision, ToolApprovalDecisionInput, ToolApprovalRequest, ToolApprovalRequestView,
    ToolApprovalResolution, ToolCallId, UserQuestionAnswer, UserQuestionRequest,
};
use crate::wire::{ItemId, TurnId};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ItemStatus {
    Started,
    Completed,
    Abandoned,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum UserMessageSource {
    Input,
    Steer,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AssistantDisposition {
    Intermediate,
    Final,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ItemContentFamily {
    UserMessage,
    AgentMessage,
    Reasoning,
    ToolInvocation,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ItemRelation {
    item_id: ItemId,
    turn_id: TurnId,
    kind: ItemRelationKind,
}

#[derive(Clone, Eq, PartialEq)]
enum ItemRelationKind {
    UserMessage,
    AgentMessage,
    Reasoning,
    ToolInvocation { tool_call_id: ToolCallId },
}

impl ItemRelation {
    #[allow(
        dead_code,
        reason = "constructed by LiveConversation and replay in M3/M4"
    )]
    pub(crate) const fn user_message(item_id: ItemId, turn_id: TurnId) -> Self {
        Self {
            item_id,
            turn_id,
            kind: ItemRelationKind::UserMessage,
        }
    }

    #[allow(
        dead_code,
        reason = "constructed by LiveConversation and replay in M3/M4"
    )]
    pub(crate) const fn agent_message(item_id: ItemId, turn_id: TurnId) -> Self {
        Self {
            item_id,
            turn_id,
            kind: ItemRelationKind::AgentMessage,
        }
    }

    #[allow(
        dead_code,
        reason = "constructed by LiveConversation and replay in M3/M4"
    )]
    pub(crate) const fn reasoning(item_id: ItemId, turn_id: TurnId) -> Self {
        Self {
            item_id,
            turn_id,
            kind: ItemRelationKind::Reasoning,
        }
    }

    #[allow(
        dead_code,
        reason = "constructed by LiveConversation and replay in M3/M4"
    )]
    pub(crate) fn tool_invocation(
        item_id: ItemId,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
    ) -> Self {
        Self {
            item_id,
            turn_id,
            kind: ItemRelationKind::ToolInvocation { tool_call_id },
        }
    }

    pub const fn item_id(&self) -> ItemId {
        self.item_id
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub const fn family(&self) -> ItemContentFamily {
        match &self.kind {
            ItemRelationKind::UserMessage => ItemContentFamily::UserMessage,
            ItemRelationKind::AgentMessage => ItemContentFamily::AgentMessage,
            ItemRelationKind::Reasoning => ItemContentFamily::Reasoning,
            ItemRelationKind::ToolInvocation { .. } => ItemContentFamily::ToolInvocation,
        }
    }

    pub const fn tool_call_id(&self) -> Option<&ToolCallId> {
        match &self.kind {
            ItemRelationKind::ToolInvocation { tool_call_id } => Some(tool_call_id),
            ItemRelationKind::UserMessage
            | ItemRelationKind::AgentMessage
            | ItemRelationKind::Reasoning => None,
        }
    }
}

impl fmt::Debug for ItemRelation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ItemRelation")
            .field("item_id", &self.item_id)
            .field("turn_id", &self.turn_id)
            .field("family", &self.family())
            .field("tool_call_id", &self.tool_call_id())
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum InteractionRequestView {
    ToolApproval(ToolApprovalRequestView),
    UserQuestion(UserQuestionRequest),
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) enum InteractionRequest {
    ToolApproval(ToolApprovalRequest),
    UserQuestion(UserQuestionRequest),
}

impl InteractionRequest {
    #[allow(dead_code, reason = "constructed by Tool execution control in M8")]
    pub(crate) fn tool_approval(request: ToolApprovalRequest) -> Self {
        Self::ToolApproval(request)
    }

    #[allow(dead_code, reason = "constructed by ask-user Tool execution in M8")]
    pub(crate) fn user_question(request: UserQuestionRequest) -> Self {
        Self::UserQuestion(request)
    }

    #[allow(dead_code, reason = "consumed by Interaction execution control in M8")]
    pub(crate) fn view(&self) -> InteractionRequestView {
        match self {
            Self::ToolApproval(request) => {
                InteractionRequestView::ToolApproval(request.view().clone())
            }
            Self::UserQuestion(request) => InteractionRequestView::UserQuestion(request.clone()),
        }
    }

    #[allow(dead_code, reason = "consumed by Interaction execution control in M8")]
    pub(crate) fn resolve_host(
        &self,
        input: InteractionHostResolutionInput,
    ) -> Result<ResolvedInteraction, InteractionValueError> {
        match (self, input) {
            (Self::ToolApproval(request), InteractionHostResolutionInput::ToolApproval(input)) => {
                let (decision, resolution) = request
                    .resolve(input)
                    .map_err(|_| InteractionValueError::InvalidResolution)?
                    .into_parts();
                Ok(ResolvedInteraction {
                    live: InteractionResolution::ToolApproval(decision),
                    view: InteractionResolutionView::tool_approval(resolution),
                })
            }
            (Self::UserQuestion(request), InteractionHostResolutionInput::UserAnswer(answer)) => {
                let answer = request
                    .validate_answer(answer)
                    .map_err(|_| InteractionValueError::InvalidResolution)?;
                Ok(ResolvedInteraction {
                    live: InteractionResolution::UserAnswer(answer.clone()),
                    view: InteractionResolutionView::user_answer(answer),
                })
            }
            (_, InteractionHostResolutionInput::Cancelled) => Ok(ResolvedInteraction::cancelled(
                InteractionCancelReason::HostCancelled,
            )),
            _ => Err(InteractionValueError::FamilyMismatch),
        }
    }

    /// Validates an opaque terminal value against this exact pending request.
    ///
    /// `ResolvedInteraction` carries the private execution decision beside its safe view. The
    /// reducer must use this owner check rather than trusting or rebuilding a value from the
    /// safe projection, which cannot prove the approval option's private mapping.
    pub(crate) fn validate_exact_resolution(
        &self,
        resolution: &ResolvedInteraction,
    ) -> Result<(), InteractionValueError> {
        match self {
            Self::ToolApproval(request) => match resolution.live() {
                InteractionResolution::ToolApproval(decision) => {
                    let InteractionResolutionViewRef::ToolApproval(view) =
                        resolution.view().as_ref()
                    else {
                        return Err(InteractionValueError::InvalidResolution);
                    };
                    request
                        .validate_exact_resolution(decision, view)
                        .map_err(|_| InteractionValueError::InvalidResolution)
                }
                InteractionResolution::Cancelled(reason) => {
                    Self::validate_cancelled_resolution(*reason, resolution.view())
                }
                InteractionResolution::UserAnswer(_) => Err(InteractionValueError::FamilyMismatch),
            },
            Self::UserQuestion(request) => match resolution.live() {
                InteractionResolution::UserAnswer(answer) => {
                    let InteractionResolutionViewRef::UserAnswer(view) = resolution.view().as_ref()
                    else {
                        return Err(InteractionValueError::InvalidResolution);
                    };
                    if answer != view {
                        return Err(InteractionValueError::InvalidResolution);
                    }
                    request
                        .validate_answer(answer.clone())
                        .map(|_| ())
                        .map_err(|_| InteractionValueError::InvalidResolution)
                }
                InteractionResolution::Cancelled(reason) => {
                    Self::validate_cancelled_resolution(*reason, resolution.view())
                }
                InteractionResolution::ToolApproval(_) => {
                    Err(InteractionValueError::FamilyMismatch)
                }
            },
        }
    }

    fn validate_cancelled_resolution(
        reason: InteractionCancelReason,
        view: &InteractionResolutionView,
    ) -> Result<(), InteractionValueError> {
        match view.as_ref() {
            InteractionResolutionViewRef::Cancelled {
                reason: view_reason,
            } if reason == view_reason => Ok(()),
            InteractionResolutionViewRef::Cancelled { .. }
            | InteractionResolutionViewRef::ToolApproval(_)
            | InteractionResolutionViewRef::UserAnswer(_) => {
                Err(InteractionValueError::InvalidResolution)
            }
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum InteractionHostResolutionInput {
    ToolApproval(ToolApprovalDecisionInput),
    UserAnswer(UserQuestionAnswer),
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InteractionCancelReason {
    HostCancelled,
    TurnCancelled,
    SecurityRevoked,
    SessionUnloaded,
    RuntimeClosing,
    TurnTerminal,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[allow(dead_code, reason = "consumed by Session/lifecycle owners in M8")]
pub(crate) enum OwnerInteractionCancelReason {
    TurnCancelled,
    SecurityRevoked,
    SessionUnloaded,
    RuntimeClosing,
    TurnTerminal,
}

impl From<OwnerInteractionCancelReason> for InteractionCancelReason {
    fn from(value: OwnerInteractionCancelReason) -> Self {
        match value {
            OwnerInteractionCancelReason::TurnCancelled => Self::TurnCancelled,
            OwnerInteractionCancelReason::SecurityRevoked => Self::SecurityRevoked,
            OwnerInteractionCancelReason::SessionUnloaded => Self::SessionUnloaded,
            OwnerInteractionCancelReason::RuntimeClosing => Self::RuntimeClosing,
            OwnerInteractionCancelReason::TurnTerminal => Self::TurnTerminal,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum InteractionValueError {
    #[error("interaction resolution family does not match the request")]
    FamilyMismatch,
    #[error("interaction resolution is invalid for the exact request")]
    InvalidResolution,
}

#[derive(Clone, Eq, PartialEq)]
pub struct InteractionResolutionView {
    kind: InteractionResolutionViewKind,
}

#[derive(Clone, Eq, PartialEq)]
enum InteractionResolutionViewKind {
    ToolApproval(ToolApprovalResolution),
    UserAnswer(UserQuestionAnswer),
    Cancelled { reason: InteractionCancelReason },
}

pub enum InteractionResolutionViewRef<'a> {
    ToolApproval(&'a ToolApprovalResolution),
    UserAnswer(&'a UserQuestionAnswer),
    Cancelled { reason: InteractionCancelReason },
}

impl InteractionResolutionView {
    pub(crate) const fn tool_approval(resolution: ToolApprovalResolution) -> Self {
        Self {
            kind: InteractionResolutionViewKind::ToolApproval(resolution),
        }
    }

    pub(crate) fn user_answer(answer: UserQuestionAnswer) -> Self {
        Self {
            kind: InteractionResolutionViewKind::UserAnswer(answer),
        }
    }

    pub(crate) const fn cancelled(reason: InteractionCancelReason) -> Self {
        Self {
            kind: InteractionResolutionViewKind::Cancelled { reason },
        }
    }

    pub const fn as_ref(&self) -> InteractionResolutionViewRef<'_> {
        match &self.kind {
            InteractionResolutionViewKind::ToolApproval(resolution) => {
                InteractionResolutionViewRef::ToolApproval(resolution)
            }
            InteractionResolutionViewKind::UserAnswer(answer) => {
                InteractionResolutionViewRef::UserAnswer(answer)
            }
            InteractionResolutionViewKind::Cancelled { reason } => {
                InteractionResolutionViewRef::Cancelled { reason: *reason }
            }
        }
    }
}

impl fmt::Debug for InteractionResolutionView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.as_ref() {
            InteractionResolutionViewRef::ToolApproval(resolution) => formatter
                .debug_tuple("InteractionResolutionView::ToolApproval")
                .field(resolution)
                .finish(),
            InteractionResolutionViewRef::UserAnswer(answer) => formatter
                .debug_struct("InteractionResolutionView::UserAnswer")
                .field("answers", &answer.answers().len())
                .finish(),
            InteractionResolutionViewRef::Cancelled { reason } => formatter
                .debug_struct("InteractionResolutionView::Cancelled")
                .field("reason", &reason)
                .finish(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
#[allow(dead_code, reason = "consumed by Interaction waiter settlement in M8")]
pub(crate) enum InteractionResolution {
    ToolApproval(ToolApprovalDecision),
    UserAnswer(UserQuestionAnswer),
    Cancelled(InteractionCancelReason),
}

#[allow(dead_code, reason = "consumed by Interaction waiter settlement in M8")]
pub(crate) struct ResolvedInteraction {
    live: InteractionResolution,
    view: InteractionResolutionView,
}

impl ResolvedInteraction {
    #[allow(dead_code, reason = "consumed by Interaction waiter settlement in M8")]
    fn cancelled(reason: InteractionCancelReason) -> Self {
        Self {
            live: InteractionResolution::Cancelled(reason),
            view: InteractionResolutionView::cancelled(reason),
        }
    }

    #[allow(dead_code, reason = "consumed by Session/lifecycle owners in M8")]
    pub(crate) fn cancelled_by_owner(reason: OwnerInteractionCancelReason) -> Self {
        Self::cancelled(reason.into())
    }

    #[allow(dead_code, reason = "consumed by Interaction waiter settlement in M8")]
    pub(crate) const fn live(&self) -> &InteractionResolution {
        &self.live
    }

    #[allow(dead_code, reason = "consumed by public/storage projection in M8")]
    pub(crate) const fn view(&self) -> &InteractionResolutionView {
        &self.view
    }

    /// Clones both the opaque execution value and its safe projection for owner tests without
    /// broadening the production construction surface.
    #[cfg(test)]
    pub(crate) fn clone_for_test(&self) -> Self {
        Self {
            live: self.live.clone(),
            view: self.view.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{
        ToolApprovalResolutionRef, UserQuestionField, UserQuestionFieldAnswer, UserQuestionInput,
        live_approval_request_fixture,
    };

    #[test]
    fn item_relation_keeps_tool_correlation_with_exact_turn() {
        let item_id: ItemId = "itm_11111111111111111111111111111111".parse().unwrap();
        let turn_id: TurnId = "trn_22222222222222222222222222222222".parse().unwrap();
        let relation = ItemRelation::tool_invocation(item_id, turn_id, "call_1".parse().unwrap());
        assert_eq!(relation.item_id(), item_id);
        assert_eq!(relation.turn_id(), turn_id);
        assert_eq!(relation.family(), ItemContentFamily::ToolInvocation);
        assert_eq!(relation.tool_call_id().unwrap().as_str(), "call_1");
    }

    #[test]
    fn user_question_resolution_is_request_bound_and_debug_redacted() {
        let request = UserQuestionRequest::reconstruct(
            None,
            vec![
                UserQuestionField::reconstruct(
                    0,
                    "Explain",
                    true,
                    UserQuestionInput::Text { multiline: false },
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let request = InteractionRequest::user_question(request);
        assert!(matches!(
            request.view(),
            InteractionRequestView::UserQuestion(_)
        ));
        let answer = UserQuestionAnswer::new(vec![
            UserQuestionFieldAnswer::text(0, "SECRET-ANSWER").unwrap(),
        ])
        .unwrap();
        let resolved = request
            .resolve_host(InteractionHostResolutionInput::UserAnswer(answer))
            .unwrap();
        assert!(!format!("{:?}", resolved.view()).contains("SECRET-ANSWER"));
        assert!(matches!(
            resolved.live(),
            InteractionResolution::UserAnswer(_)
        ));
        assert!(matches!(
            request.resolve_host(InteractionHostResolutionInput::ToolApproval(
                ToolApprovalDecisionInput::Deny
            )),
            Err(InteractionValueError::FamilyMismatch)
        ));
    }

    #[test]
    fn tool_approval_resolution_preserves_private_decision_and_safe_view() {
        let request = InteractionRequest::tool_approval(live_approval_request_fixture());
        assert!(matches!(
            request.view(),
            InteractionRequestView::ToolApproval(_)
        ));
        let resolved = request
            .resolve_host(InteractionHostResolutionInput::ToolApproval(
                ToolApprovalDecisionInput::Allow { option_index: 0 },
            ))
            .unwrap();
        assert!(matches!(
            resolved.live(),
            InteractionResolution::ToolApproval(ToolApprovalDecision::AllowOnce)
        ));
        assert!(matches!(
            resolved.view().as_ref(),
            InteractionResolutionViewRef::ToolApproval(resolution)
                if matches!(
                    resolution.as_ref(),
                    ToolApprovalResolutionRef::Allowed { option_index: 0, .. }
                )
        ));
        assert!(matches!(
            request.resolve_host(InteractionHostResolutionInput::ToolApproval(
                ToolApprovalDecisionInput::Allow { option_index: 1 },
            )),
            Err(InteractionValueError::InvalidResolution)
        ));
    }

    #[test]
    fn exact_resolution_validation_rechecks_request_family_answer_and_live_view_coherence() {
        let request = InteractionRequest::user_question(
            UserQuestionRequest::reconstruct(
                None,
                vec![
                    UserQuestionField::reconstruct(
                        0,
                        "Continue?",
                        true,
                        UserQuestionInput::Text { multiline: false },
                    )
                    .unwrap(),
                ],
            )
            .unwrap(),
        );
        let valid_answer =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(0, "yes").unwrap()])
                .unwrap();
        let valid = request
            .resolve_host(InteractionHostResolutionInput::UserAnswer(valid_answer))
            .unwrap();
        assert!(request.validate_exact_resolution(&valid).is_ok());

        let different_request = InteractionRequest::user_question(
            UserQuestionRequest::reconstruct(
                None,
                vec![
                    UserQuestionField::reconstruct(
                        1,
                        "A different question",
                        true,
                        UserQuestionInput::Text { multiline: false },
                    )
                    .unwrap(),
                ],
            )
            .unwrap(),
        );
        let different = different_request
            .resolve_host(InteractionHostResolutionInput::UserAnswer(
                UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(1, "answer").unwrap()])
                    .unwrap(),
            ))
            .unwrap();
        assert_eq!(
            request.validate_exact_resolution(&different),
            Err(InteractionValueError::InvalidResolution)
        );

        let incoherent_cancel = ResolvedInteraction {
            live: InteractionResolution::Cancelled(InteractionCancelReason::TurnCancelled),
            view: InteractionResolutionView::cancelled(InteractionCancelReason::SecurityRevoked),
        };
        assert_eq!(
            request.validate_exact_resolution(&incoherent_cancel),
            Err(InteractionValueError::InvalidResolution)
        );
        let approval = InteractionRequest::tool_approval(live_approval_request_fixture());
        assert_eq!(
            approval.validate_exact_resolution(&valid),
            Err(InteractionValueError::FamilyMismatch)
        );
    }

    #[test]
    fn host_and_owner_cancellation_are_distinct_in_release_semantics() {
        let request = InteractionRequest::user_question(
            UserQuestionRequest::reconstruct(
                None,
                vec![
                    UserQuestionField::reconstruct(
                        0,
                        "Continue?",
                        false,
                        UserQuestionInput::Text { multiline: false },
                    )
                    .unwrap(),
                ],
            )
            .unwrap(),
        );
        let host = request
            .resolve_host(InteractionHostResolutionInput::Cancelled)
            .unwrap();
        assert!(matches!(
            host.view().as_ref(),
            InteractionResolutionViewRef::Cancelled {
                reason: InteractionCancelReason::HostCancelled
            }
        ));
        let mappings = [
            (
                OwnerInteractionCancelReason::TurnCancelled,
                InteractionCancelReason::TurnCancelled,
            ),
            (
                OwnerInteractionCancelReason::SecurityRevoked,
                InteractionCancelReason::SecurityRevoked,
            ),
            (
                OwnerInteractionCancelReason::SessionUnloaded,
                InteractionCancelReason::SessionUnloaded,
            ),
            (
                OwnerInteractionCancelReason::RuntimeClosing,
                InteractionCancelReason::RuntimeClosing,
            ),
            (
                OwnerInteractionCancelReason::TurnTerminal,
                InteractionCancelReason::TurnTerminal,
            ),
        ];
        for (owner_reason, public_reason) in mappings {
            assert!(matches!(
                ResolvedInteraction::cancelled_by_owner(owner_reason).view().as_ref(),
                InteractionResolutionViewRef::Cancelled { reason } if reason == public_reason
            ));
        }
    }
}
