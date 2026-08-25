use super::super::support::{CriticalFailure, validate_assistant_ack, validate_tool_ack};
use super::*;
use crate::agent::turn_context::TurnRunnerContext;
use crate::conversation::{TurnTerminal, TurnTerminalEntry};

fn validation_fixture() -> (
    TurnRunnerContext,
    SessionSpec,
    ConversationView,
    AssistantMessageDraft,
    CommittedUpdate,
) {
    let spec = session_spec(&[], 4);
    let initial = initial_conversation(&spec, 4);
    let model = ScriptModel::new(4_096, Vec::new());
    let (request, _critical_rx, _progress_rx) = runner_request(
        spec.clone(),
        4,
        session_bindings(model, None, Vec::new(), None),
        initial.clone(),
    );
    let context = TurnRunnerContext::from_request(request);
    let draft = AssistantMessageDraft {
        turn_id: turn_id(),
        model: spec.model.clone(),
        text: Some(BoundedText::new("answer").unwrap()),
        reasoning: None,
        tool_calls: Vec::new(),
        usage: Usage::default(),
        finish_reason: ModelFinishReason::Stop,
    };
    let update = ack_assistant(&initial, &draft, &spec);
    (context, spec, initial, draft, update)
}

fn assert_invalid(
    context: &TurnRunnerContext,
    previous_head: ConversationSeq,
    draft: &AssistantMessageDraft,
    update: CommittedUpdate,
) {
    assert_eq!(
        validate_assistant_ack(context, previous_head, draft, update),
        Err(CriticalFailure::InvalidAck)
    );
}

fn replace_update_entry(
    update: &mut CommittedUpdate,
    entry: ConversationEntry,
    spec: &SessionSpec,
) {
    let mut entries = update.conversation.entries().to_vec();
    *entries.last_mut().unwrap() = entry.clone();
    update.conversation =
        ConversationView::from_validated_entries(spec, &SemanticLimits::default(), entries.into())
            .unwrap();
    update.entry = entry;
}

fn ten_thousand_entry_history(spec: &SessionSpec) -> ConversationView {
    let mut entries = Vec::with_capacity(10_000);
    let mut seq = 1_u64;
    for index in 0..3_333_u64 {
        let prior_turn: TurnId = format!("trn_{:032x}", index + 1_000).parse().unwrap();
        entries.push(ConversationEntry::UserMessage(UserMessageEntry {
            seq: ConversationSeq::new(seq),
            turn_id: prior_turn,
            input: UserInputRecord::new(BoundedText::new("history").unwrap()).unwrap(),
            execution: TurnExecutionRecord::new(spec.model.clone(), spec.reasoning, 4).unwrap(),
            created_at: timestamp(),
        }));
        seq += 1;
        entries.push(ConversationEntry::AssistantMessage(AssistantMessageEntry {
            seq: ConversationSeq::new(seq),
            turn_id: prior_turn,
            model: spec.model.clone(),
            text: Some(BoundedText::new("history").unwrap()),
            reasoning: None,
            tool_calls: Vec::new(),
            usage: Usage::default(),
            finish_reason: ModelFinishReason::Stop,
            created_at: timestamp(),
        }));
        seq += 1;
        entries.push(ConversationEntry::TurnTerminal(TurnTerminalEntry {
            seq: ConversationSeq::new(seq),
            turn_id: prior_turn,
            terminal: TurnTerminal::Completed,
            usage: Usage::default(),
            created_at: timestamp(),
        }));
        seq += 1;
    }
    entries.push(ConversationEntry::UserMessage(UserMessageEntry {
        seq: ConversationSeq::new(seq),
        turn_id: turn_id(),
        input: UserInputRecord::new(BoundedText::new("question").unwrap()).unwrap(),
        execution: TurnExecutionRecord::new(spec.model.clone(), spec.reasoning, 4).unwrap(),
        created_at: timestamp(),
    }));
    ConversationView::from_validated_entries(spec, &SemanticLimits::default(), entries.into())
        .unwrap()
}

#[test]
fn assistant_delta_validation_rejects_stale_sequence_view_and_draft_mismatches() {
    let (context, spec, initial, draft, valid) = validation_fixture();
    let previous_head = initial.head();
    assert_eq!(
        validate_assistant_ack(&context, previous_head, &draft, valid.clone()).unwrap(),
        valid.conversation
    );

    let mut stale = valid.clone();
    stale.previous_head = ConversationSeq::ZERO;
    assert_invalid(&context, previous_head, &draft, stale);

    let mut sequence_mismatch = valid.clone();
    if let ConversationEntry::AssistantMessage(mut entry) = sequence_mismatch.entry.clone() {
        entry.seq = ConversationSeq::new(3);
        sequence_mismatch.entry = ConversationEntry::AssistantMessage(entry);
    }
    assert_invalid(&context, previous_head, &draft, sequence_mismatch);

    let mut wrong_head = valid.clone();
    wrong_head.conversation = ConversationView::from_confirmed(
        ConversationSeq::new(99),
        wrong_head.conversation.entries().to_vec().into(),
    );
    assert_invalid(&context, previous_head, &draft, wrong_head);

    let mut wrong_tail = valid.clone();
    let mut entries = wrong_tail.conversation.entries().to_vec();
    match entries.last_mut().unwrap() {
        ConversationEntry::AssistantMessage(entry) => {
            entry.text = Some(BoundedText::new("forged tail").unwrap());
        }
        entry => panic!("unexpected entry: {entry:?}"),
    }
    wrong_tail.conversation =
        ConversationView::from_validated_entries(&spec, &SemanticLimits::default(), entries.into())
            .unwrap();
    assert_invalid(&context, previous_head, &draft, wrong_tail);

    let mut wrong_draft = valid.clone();
    let forged = match wrong_draft.entry.clone() {
        ConversationEntry::AssistantMessage(mut entry) => {
            entry.text = Some(BoundedText::new("forged").unwrap());
            ConversationEntry::AssistantMessage(entry)
        }
        entry => entry,
    };
    replace_update_entry(&mut wrong_draft, forged, &spec);
    assert_invalid(&context, previous_head, &draft, wrong_draft);
}

#[test]
fn assistant_ack_rejects_semantically_valid_untrusted_early_prefix_replacement() {
    let (context, _spec, initial, draft, mut update) = validation_fixture();
    let mut entries = update.conversation.entries().to_vec();
    match &mut entries[0] {
        ConversationEntry::UserMessage(entry) => {
            entry.input =
                UserInputRecord::new(BoundedText::new("forged earlier user input").unwrap())
                    .unwrap();
        }
        entry => panic!("unexpected entry: {entry:?}"),
    }
    let trusted_replacement = ConversationView::from_validated_entries(
        &context.environment.spec,
        &context.environment.limits,
        entries.clone().into(),
    )
    .unwrap();
    assert!(
        trusted_replacement
            .is_validated_for(&context.environment.spec, &context.environment.limits)
    );
    update.conversation =
        ConversationView::from_confirmed(update.conversation.head(), entries.into());
    assert!(
        !update
            .conversation
            .is_validated_for(&context.environment.spec, &context.environment.limits)
    );
    assert_invalid(&context, initial.head(), &draft, update);
}

#[test]
fn tool_result_ack_rejects_each_draft_field_mismatch() {
    let spec = session_spec(&["search"], 4);
    let initial = pending_tool_conversation(&spec, "search", call_id(1));
    let (request, _critical_rx, _progress_rx) = runner_request(
        spec.clone(),
        4,
        session_bindings(
            ScriptModel::new(4_096, Vec::new()),
            None,
            vec![ScriptTool::new(
                "search",
                vec![ToolBehavior::Complete(ToolOutput::new("result").unwrap())],
            )],
            Some(ScriptPolicy::new(Vec::new())),
        ),
        initial.clone(),
    );
    let context = TurnRunnerContext::from_request(request);
    let draft = ToolResultDraft {
        turn_id: turn_id(),
        tool_call_id: call_id(1),
        tool_name: "search".parse().unwrap(),
        outcome: ToolResultOutcome::Success,
        content: BoundedText::new("result").unwrap(),
    };
    let valid = ack_tool(&initial, &draft, &spec);
    assert_eq!(
        validate_tool_ack(&context, initial.head(), &draft, valid.clone()).unwrap(),
        valid.conversation
    );
    let mismatches = [
        ToolResultDraft {
            turn_id: TurnId::new().unwrap(),
            ..draft.clone()
        },
        ToolResultDraft {
            tool_call_id: call_id(2),
            ..draft.clone()
        },
        ToolResultDraft {
            tool_name: "other".parse().unwrap(),
            ..draft.clone()
        },
        ToolResultDraft {
            outcome: ToolResultOutcome::Failed,
            ..draft.clone()
        },
        ToolResultDraft {
            content: BoundedText::new("forged").unwrap(),
            ..draft
        },
    ];
    for mismatch in mismatches {
        assert_eq!(
            validate_tool_ack(&context, initial.head(), &mismatch, valid.clone()),
            Err(CriticalFailure::InvalidAck)
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn committed_update_acknowledges_a_ten_thousand_entry_history() {
    let spec = session_spec(&[], 4);
    let initial = ten_thousand_entry_history(&spec);
    assert_eq!(initial.entries().len(), 10_000);
    assert!(initial.is_validated_for(&spec, &SemanticLimits::default()));
    let model = ScriptModel::new(
        1_000_000,
        vec![ModelBehavior::Events(final_events(
            "answer",
            Usage::default(),
        ))],
    );
    let (request, mut critical_rx, _progress_rx) = runner_request(
        spec.clone(),
        4,
        session_bindings(model, None, Vec::new(), None),
        initial.clone(),
    );
    let task = tokio::spawn(run_turn(request));
    match critical_rx.recv().await.unwrap() {
        RunnerEvent::CommitAssistant { draft, reply } => {
            let update = ack_assistant(&initial, &draft, &spec);
            assert_eq!(update.previous_head, ConversationSeq::new(10_000));
            assert_eq!(update.entry.seq(), ConversationSeq::new(10_001));
            assert_eq!(update.conversation.head(), update.entry.seq());
            assert!(
                update
                    .conversation
                    .is_validated_for(&spec, &SemanticLimits::default())
            );
            reply.send(Ok(update)).unwrap();
        }
        event => panic!("unexpected event: {event:?}"),
    }
    assert!(matches!(
        critical_rx.recv().await,
        Some(RunnerEvent::Finish {
            outcome: RunnerOutcome::Completed { .. }
        })
    ));
    assert_finished(task.await.unwrap());
}
