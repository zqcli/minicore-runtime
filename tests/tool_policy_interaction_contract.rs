use std::time::{Duration, Instant};

use minicore_runtime::tools::{
    ApprovalDecision, ApprovalRequest, ApprovalRisk, MAX_TOOL_POLICY_TEXT_BYTES, ToolDecision,
    ToolInputAnswer, ToolInputAnswerKind, ToolInputRequest, ToolInvocation, ToolPolicy,
    ToolPolicyError, ToolPolicyFuture, ToolPolicyRequest, ToolSpec, ToolValueError,
};
use minicore_runtime::value::BoundedText;
use minicore_runtime::{
    InteractionAnswer, InteractionId, InteractionKind, PendingInteraction, ToolCallId,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn interaction_id() -> InteractionId {
    "int_00000000000000000000000000000001".parse().unwrap()
}

fn tool_call_id() -> ToolCallId {
    "call_00000000000000000000000000000001".parse().unwrap()
}

fn invocation(arguments: serde_json::Value) -> ToolInvocation {
    ToolInvocation::new(tool_call_id(), "deploy".parse().unwrap(), arguments).unwrap()
}

fn spec(description: &str) -> ToolSpec {
    ToolSpec::new(
        "deploy".parse().unwrap(),
        description,
        json!({"type": "object"}),
    )
    .unwrap()
}

fn decision_kind(decision: &ToolDecision) -> &'static str {
    match decision {
        ToolDecision::Allow => "allow",
        ToolDecision::Deny { .. } => "deny",
        ToolDecision::RequireApproval { .. } => "require_approval",
    }
}

fn approval_decision_kind(decision: ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::AllowOnce => "allow_once",
        ApprovalDecision::Deny => "deny",
    }
}

fn approval_risk_kind(risk: ApprovalRisk) -> &'static str {
    match risk {
        ApprovalRisk::Low => "low",
        ApprovalRisk::Medium => "medium",
        ApprovalRisk::High => "high",
    }
}

#[derive(Clone)]
struct FixedPolicy {
    decision: ToolDecision,
}

impl ToolPolicy for FixedPolicy {
    fn decide<'a>(&'a self, _request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        let decision = self.decision.clone();
        Box::pin(async move { Ok(decision) })
    }
}

struct InspectPolicy {
    invocation: ToolInvocation,
    spec: ToolSpec,
    deadline: Instant,
}

impl ToolPolicy for InspectPolicy {
    fn decide<'a>(&'a self, request: ToolPolicyRequest) -> ToolPolicyFuture<'a> {
        assert_eq!(request.invocation, self.invocation);
        assert_eq!(request.spec, self.spec);
        assert!(request.cancellation.is_cancelled());
        assert_eq!(request.deadline, self.deadline);
        Box::pin(async { Ok(ToolDecision::Allow) })
    }
}

fn policy_request(cancellation: CancellationToken, deadline: Instant) -> ToolPolicyRequest {
    ToolPolicyRequest {
        invocation: invocation(json!({"target": "production"})),
        spec: spec("deploy one release"),
        cancellation,
        deadline,
    }
}

#[tokio::test]
async fn policy_port_is_async_send_sync_and_returns_only_typed_decisions() {
    fn assert_policy<T: ToolPolicy + Send + Sync + 'static>() {}
    fn assert_future_send<'a>(future: ToolPolicyFuture<'a>) -> ToolPolicyFuture<'a> {
        future
    }

    assert_policy::<FixedPolicy>();
    let deadline = Instant::now() + Duration::from_secs(30);
    let approval = ApprovalRequest::new("approve deployment", ApprovalRisk::High).unwrap();
    let decisions = [
        ToolDecision::Allow,
        ToolDecision::deny("blocked by policy").unwrap(),
        ToolDecision::require_approval(approval.clone()).unwrap(),
    ];

    for expected in decisions {
        let policy = FixedPolicy {
            decision: expected.clone(),
        };
        let future =
            assert_future_send(policy.decide(policy_request(CancellationToken::new(), deadline)));
        assert_eq!(future.await.unwrap(), expected);
    }

    assert_eq!(decision_kind(&ToolDecision::Allow), "allow");
    assert_eq!(
        approval_decision_kind(ApprovalDecision::AllowOnce),
        "allow_once"
    );
    assert_eq!(approval_decision_kind(ApprovalDecision::Deny), "deny");
    assert_eq!(approval_risk_kind(ApprovalRisk::Low), "low");
    assert_eq!(approval_risk_kind(ApprovalRisk::Medium), "medium");
    assert_eq!(approval_risk_kind(ApprovalRisk::High), "high");

    assert!(matches!(
        ToolDecision::RequireApproval { request: approval },
        ToolDecision::RequireApproval {
            request: ApprovalRequest {
                risk: ApprovalRisk::High,
                ..
            }
        }
    ));
}

#[tokio::test]
async fn policy_request_carries_exact_owned_invocation_spec_cancellation_and_deadline() {
    let cancellation = CancellationToken::new();
    let deadline = Instant::now() + Duration::from_secs(17);
    let request = policy_request(cancellation.clone(), deadline);
    let expected_invocation = request.invocation.clone();
    let expected_spec = request.spec.clone();
    cancellation.cancel();

    let policy = InspectPolicy {
        invocation: expected_invocation,
        spec: expected_spec,
        deadline,
    };
    assert_eq!(policy.decide(request).await, Ok(ToolDecision::Allow));
}

#[test]
fn policy_text_is_checked_at_the_exact_boundary_and_debug_is_redacted() {
    let exact = "x".repeat(MAX_TOOL_POLICY_TEXT_BYTES);
    let oversized = "x".repeat(MAX_TOOL_POLICY_TEXT_BYTES + 1);
    assert!(ToolDecision::deny(&exact).is_ok());
    assert_eq!(
        ToolDecision::deny(&oversized),
        Err(ToolPolicyError::InvalidDecision)
    );
    assert_eq!(
        ToolDecision::deny(""),
        Err(ToolPolicyError::InvalidDecision)
    );
    assert!(ApprovalRequest::new(&exact, ApprovalRisk::Low).is_ok());
    assert_eq!(
        ApprovalRequest::new(&oversized, ApprovalRisk::Medium),
        Err(ToolPolicyError::InvalidDecision)
    );
    assert_eq!(
        ApprovalRequest::new("", ApprovalRisk::High),
        Err(ToolPolicyError::InvalidDecision)
    );

    let invalid_approval = ApprovalRequest {
        prompt: BoundedText::new(oversized).unwrap(),
        risk: ApprovalRisk::High,
    };
    assert_eq!(
        invalid_approval.validate(),
        Err(ToolPolicyError::InvalidDecision)
    );
    let invalid_denial = ToolDecision::Deny {
        reason: BoundedText::new("").unwrap(),
    };
    assert_eq!(
        invalid_denial.validate(),
        Err(ToolPolicyError::InvalidDecision)
    );

    let denial = ToolDecision::deny("private denial reason").unwrap();
    let approval = ApprovalRequest::new("private approval prompt", ApprovalRisk::Medium).unwrap();
    let request = policy_request(
        CancellationToken::new(),
        Instant::now() + Duration::from_secs(30),
    );
    for (debug, secret) in [
        (format!("{denial:?}"), "private denial reason"),
        (format!("{approval:?}"), "private approval prompt"),
        (format!("{request:?}"), "production"),
    ] {
        assert!(!debug.contains(secret));
    }
}

fn pending(kind: InteractionKind) -> PendingInteraction {
    PendingInteraction {
        interaction_id: interaction_id(),
        tool_call_id: tool_call_id(),
        tool_name: "deploy".parse().unwrap(),
        kind,
    }
}

#[test]
fn pending_interactions_keep_checked_ids_kinds_and_redacted_debug() {
    let approval = ApprovalRequest::new("approve secret deploy", ApprovalRisk::High).unwrap();
    let pending_approval = pending(InteractionKind::Approval(approval));
    assert_eq!(pending_approval.interaction_id, interaction_id());
    assert_eq!(pending_approval.tool_call_id, tool_call_id());
    assert_eq!(pending_approval.tool_name.as_str(), "deploy");
    assert!(!format!("{pending_approval:?}").contains("approve secret deploy"));

    let input = ToolInputRequest::new(
        "choose secret target",
        vec![
            BoundedText::new("secret-a").unwrap(),
            BoundedText::new("secret-b").unwrap(),
        ],
        ToolInputAnswerKind::SingleChoice,
    )
    .unwrap();
    let pending_input = pending(InteractionKind::ToolInput(input));
    let debug = format!("{pending_input:?}");
    assert!(!debug.contains("choose secret target"));
    assert!(!debug.contains("secret-a"));
    assert!(!debug.contains("secret-b"));
}

#[test]
fn interaction_answers_validate_kind_and_delegate_exact_tool_input_rules() {
    let pending_approval = pending(InteractionKind::Approval(
        ApprovalRequest::new("approve", ApprovalRisk::Low).unwrap(),
    ));
    let allow_once = InteractionAnswer::Approval(ApprovalDecision::AllowOnce);
    let deny = InteractionAnswer::Approval(ApprovalDecision::Deny);
    assert!(pending_approval.validate_answer(&allow_once).is_ok());
    assert!(pending_approval.validate_answer(&deny).is_ok());
    assert!(pending_approval.validate_answer(&allow_once).is_ok());

    let choice_request = ToolInputRequest::new(
        "choose",
        vec![
            BoundedText::new("alpha").unwrap(),
            BoundedText::new("beta").unwrap(),
        ],
        ToolInputAnswerKind::SingleChoice,
    )
    .unwrap();
    let pending_choice = pending(InteractionKind::ToolInput(choice_request));
    let valid_choice = InteractionAnswer::ToolInput(ToolInputAnswer::Choice { index: 1 });
    assert!(pending_choice.validate_answer(&valid_choice).is_ok());
    assert!(pending_choice.validate_answer(&valid_choice).is_ok());
    assert_eq!(
        pending_choice.validate_answer(&InteractionAnswer::ToolInput(ToolInputAnswer::Choice {
            index: 2
        })),
        Err(ToolValueError::InvalidAnswer)
    );
    assert_eq!(
        pending_choice.validate_answer(&InteractionAnswer::ToolInput(ToolInputAnswer::Text(
            BoundedText::new("alpha").unwrap(),
        ))),
        Err(ToolValueError::InvalidAnswer)
    );
    assert_eq!(
        pending_choice.validate_answer(&allow_once),
        Err(ToolValueError::InvalidAnswer)
    );

    let text_request =
        ToolInputRequest::new("answer", Vec::new(), ToolInputAnswerKind::Text).unwrap();
    let pending_text = pending(InteractionKind::ToolInput(text_request));
    let text_answer = InteractionAnswer::ToolInput(ToolInputAnswer::Text(
        BoundedText::new("private answer").unwrap(),
    ));
    assert!(pending_text.validate_answer(&text_answer).is_ok());
    assert_eq!(
        pending_text.validate_answer(&InteractionAnswer::ToolInput(ToolInputAnswer::Choice {
            index: 0
        })),
        Err(ToolValueError::InvalidAnswer)
    );
    assert_eq!(
        pending_text.validate_answer(&InteractionAnswer::ToolInput(ToolInputAnswer::Text(
            BoundedText::new("x".repeat(8_193)).unwrap(),
        ))),
        Err(ToolValueError::InvalidAnswer)
    );
    assert!(!format!("{text_answer:?}").contains("private answer"));
}

#[test]
fn policy_and_interaction_sources_are_process_local_and_owner_neutral() {
    let policy = include_str!("../src/tools/policy.rs");
    let interaction = include_str!("../src/interaction.rs");
    let tools = include_str!("../src/tools/mod.rs");

    for source in [policy, interaction] {
        for forbidden in [
            "serde",
            "Serialize",
            "Deserialize",
            "AllowForSession",
            "ToolDecision::Ask",
            "\"yes\"",
            "\"allow\"",
            "resume",
            "continuation",
            "continuation_token",
            "resume_token",
            "oneshot",
            "Sender",
            "callback",
            "closure",
            "Any",
            "SessionHandle",
            "Runtime",
            "Workspace",
            "Store",
            "Service",
        ] {
            assert!(
                !source.contains(forbidden),
                "process-local public DTO leaked forbidden token: {forbidden}"
            );
        }
    }

    let approval = policy
        .split_once("pub enum ApprovalDecision")
        .and_then(|(_, tail)| tail.split_once('}'))
        .map(|(body, _)| body)
        .unwrap();
    assert!(approval.contains("AllowOnce"));
    assert!(approval.contains("Deny"));
    assert!(!approval.contains("AllowForSession"));
    assert!(tools.contains("pub use policy::{"));
    assert!(!tools.contains("legacy_"));
}

/// FIX-05-T06: Valid Text and SingleChoice constructors and interactions are preserved.
#[test]
fn valid_tool_input_constructors_and_interactions_regression() {
    let text_req = ToolInputRequest::new("enter text", vec![], ToolInputAnswerKind::Text);
    assert!(text_req.is_ok());

    let single_choice_min = ToolInputRequest::new(
        "choose one",
        vec![BoundedText::new("only_choice").unwrap()],
        ToolInputAnswerKind::SingleChoice,
    );
    assert!(single_choice_min.is_ok());

    let boundary_choices: Vec<_> = (0..32)
        .map(|i| BoundedText::new(format!("choice_{i}")).unwrap())
        .collect();
    let single_choice_max = ToolInputRequest::new(
        "choose one",
        boundary_choices,
        ToolInputAnswerKind::SingleChoice,
    );
    assert!(single_choice_max.is_ok());

    let req = single_choice_min.unwrap();
    let valid_answer = ToolInputAnswer::Choice { index: 0 };
    assert!(valid_answer.validate(&req).is_ok());

    let text_req = text_req.unwrap();
    let valid_text_answer = ToolInputAnswer::Text(BoundedText::new("some answer").unwrap());
    assert!(valid_text_answer.validate(&text_req).is_ok());
}

/// FIX-05-T01..T04: Direct struct literal ToolInputRequest validation rejects invalid shapes.
#[test]
fn invalid_tool_input_requests_rejected_by_validate() {
    // FIX-05-T01: SingleChoice with empty choices
    let empty_choices_single_choice = ToolInputRequest {
        prompt: BoundedText::new("choose").unwrap(),
        choices: vec![],
        answer_kind: ToolInputAnswerKind::SingleChoice,
    };
    assert_eq!(
        empty_choices_single_choice.validate(),
        Err(ToolValueError::InvalidText),
        "SingleChoice with empty choices must be rejected"
    );

    // FIX-05-T02: Empty prompt
    let empty_prompt = ToolInputRequest {
        prompt: BoundedText::new("").unwrap(),
        choices: vec![BoundedText::new("choice_1").unwrap()],
        answer_kind: ToolInputAnswerKind::SingleChoice,
    };
    assert_eq!(
        empty_prompt.validate(),
        Err(ToolValueError::InvalidText),
        "Empty prompt must be rejected"
    );

    // FIX-05-T03: Too many choices (33 choices > 32 max)
    let too_many_choices = ToolInputRequest {
        prompt: BoundedText::new("choose").unwrap(),
        choices: (0..33)
            .map(|i| BoundedText::new(format!("choice_{i}")).unwrap())
            .collect(),
        answer_kind: ToolInputAnswerKind::SingleChoice,
    };
    assert_eq!(
        too_many_choices.validate(),
        Err(ToolValueError::InvalidText),
        "More than 32 choices must be rejected"
    );

    // FIX-05-T04: Oversized choice (1025 bytes > 1024 max)
    let oversized_choice = ToolInputRequest {
        prompt: BoundedText::new("choose").unwrap(),
        choices: vec![BoundedText::new("x".repeat(1025)).unwrap()],
        answer_kind: ToolInputAnswerKind::SingleChoice,
    };
    assert_eq!(
        oversized_choice.validate(),
        Err(ToolValueError::InvalidText),
        "Choice exceeding 1024 bytes must be rejected"
    );
}
