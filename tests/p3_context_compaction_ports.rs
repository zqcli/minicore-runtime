use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use minicore_runtime::compaction::{
    CompactionCandidate, CompactionFuture, CompactionProposal, CompactionRequest,
    CompactionStrategy,
};
use minicore_runtime::config::SemanticLimits;
use minicore_runtime::context::{
    ContextBlock, ContextBundle, ContextError, ContextFuture, ContextProvider, ContextRequest,
    ContextSlot,
};
use minicore_runtime::conversation::ConversationView;
use minicore_runtime::ids::{
    ContextSourceId, ContextSourceIdError, SessionId, SessionInstanceId, TurnId,
};
use minicore_runtime::value::BoundedText;
use tokio_util::sync::CancellationToken;

fn session_id() -> SessionId {
    "ses_00000000000000000000000000000001".parse().unwrap()
}

fn instance_id() -> SessionInstanceId {
    "ins_00000000000000000000000000000001".parse().unwrap()
}

fn turn_id() -> TurnId {
    "trn_00000000000000000000000000000001".parse().unwrap()
}

fn source(value: &str) -> ContextSourceId {
    value.parse().unwrap()
}

fn block(source_id: &str, slot: ContextSlot, priority: i16, content: &str) -> ContextBlock {
    ContextBlock {
        source: source(source_id),
        slot,
        priority,
        content: BoundedText::new(content).unwrap(),
    }
}

#[test]
fn context_source_id_is_checked_and_serde_round_trips() {
    let valid: ContextSourceId = "project:/AGENTS.md-1.2".parse().unwrap();
    assert_eq!(valid.as_str(), "project:/AGENTS.md-1.2");
    assert_eq!(valid.to_string(), "project:/AGENTS.md-1.2");
    assert_eq!(format!("{valid:?}"), "project:/AGENTS.md-1.2");
    assert_eq!(
        serde_json::to_string(&valid).unwrap(),
        "\"project:/AGENTS.md-1.2\""
    );
    assert_eq!(
        serde_json::from_str::<ContextSourceId>("\"project:/AGENTS.md-1.2\"").unwrap(),
        valid
    );

    assert_eq!(
        "".parse::<ContextSourceId>(),
        Err(ContextSourceIdError::InvalidLength)
    );
    assert_eq!(
        "a".repeat(129).parse::<ContextSourceId>(),
        Err(ContextSourceIdError::InvalidLength)
    );
    for invalid in ["has space", "has+plus", "has\nnewline", "非ascii"] {
        assert_eq!(
            invalid.parse::<ContextSourceId>(),
            Err(ContextSourceIdError::InvalidGrammar)
        );
    }
    assert!("a".repeat(128).parse::<ContextSourceId>().is_ok());
}

#[test]
fn context_bundle_checks_limits_duplicates_and_deterministic_order() {
    let content_debug = format!(
        "{:?}",
        block("debug", ContextSlot::TurnContext, 0, "secret context")
    );
    assert!(!content_debug.contains("secret context"));

    let bundle = ContextBundle {
        blocks: vec![
            block("zeta", ContextSlot::TurnContext, 1, "z"),
            block("beta", ContextSlot::ProjectInstructions, 1, "b"),
            block("alpha", ContextSlot::RetrievedKnowledge, 2, "a"),
            block("gamma", ContextSlot::RetrievedKnowledge, 2, "g"),
            block("delta", ContextSlot::ProjectInstructions, 5, "d"),
        ],
    };
    let checked = bundle
        .validate_and_sort(&SemanticLimits::default())
        .unwrap();
    assert_eq!(checked.blocks.len(), 5);
    assert_eq!(
        checked
            .blocks
            .iter()
            .map(|value| value.source.as_str())
            .collect::<Vec<_>>(),
        vec!["delta", "beta", "alpha", "gamma", "zeta"]
    );

    let limits = SemanticLimits {
        max_context_blocks: 2,
        ..SemanticLimits::default()
    };
    assert_eq!(
        (ContextBundle {
            blocks: vec![
                block("a", ContextSlot::TurnContext, 0, ""),
                block("b", ContextSlot::TurnContext, 0, ""),
                block("c", ContextSlot::TurnContext, 0, ""),
            ],
        })
        .validate_and_sort(&limits),
        Err(ContextError::TooManyBlocks)
    );
    assert!(
        (ContextBundle {
            blocks: vec![
                block("a", ContextSlot::TurnContext, 0, ""),
                block("b", ContextSlot::TurnContext, 0, ""),
            ],
        })
        .validate_and_sort(&limits)
        .is_ok()
    );

    let byte_limits = SemanticLimits {
        max_context_bytes: 3,
        ..SemanticLimits::default()
    };
    assert!(
        (ContextBundle {
            blocks: vec![block("a", ContextSlot::TurnContext, 0, "ab")],
        })
        .validate_and_sort(&byte_limits)
        .is_ok()
    );
    assert_eq!(
        (ContextBundle {
            blocks: vec![block("a", ContextSlot::TurnContext, 0, "ab")],
        })
        .validate_and_sort(&SemanticLimits {
            max_context_bytes: 1,
            ..SemanticLimits::default()
        }),
        Err(ContextError::BlockTooLarge)
    );
    assert!(
        (ContextBundle {
            blocks: vec![
                block("a", ContextSlot::TurnContext, 0, "ab"),
                block("b", ContextSlot::TurnContext, 0, "c"),
            ],
        })
        .validate_and_sort(&byte_limits)
        .is_ok()
    );
    assert_eq!(
        (ContextBundle {
            blocks: vec![
                block("a", ContextSlot::TurnContext, 0, "ab"),
                block("b", ContextSlot::TurnContext, 0, "cd"),
            ],
        })
        .validate_and_sort(&byte_limits),
        Err(ContextError::TotalTooLarge)
    );

    assert_eq!(
        (ContextBundle {
            blocks: vec![
                block("same", ContextSlot::TurnContext, 1, "a"),
                block("same", ContextSlot::ProjectInstructions, 1, "b"),
            ],
        })
        .validate_and_sort(&SemanticLimits::default()),
        Err(ContextError::DuplicateSource)
    );
}

struct RecordingContextProvider {
    observed_cancel: Arc<AtomicBool>,
}

impl ContextProvider for RecordingContextProvider {
    fn provide<'a>(&'a self, request: ContextRequest) -> ContextFuture<'a> {
        let observed_cancel = Arc::clone(&self.observed_cancel);
        Box::pin(async move {
            assert_eq!(request.session_id, session_id());
            assert_eq!(request.instance_id, instance_id());
            assert_eq!(request.turn_id, turn_id());
            assert_eq!(request.model_round, 3);
            assert_eq!(request.remaining_context_budget, 1_024);
            assert_eq!(request.conversation, ConversationView::empty());
            assert!(request.deadline > Instant::now() - std::time::Duration::from_secs(1));
            observed_cancel.store(request.cancellation.is_cancelled(), Ordering::SeqCst);
            Err(ContextError::Cancelled)
        })
    }
}

#[tokio::test]
async fn context_provider_is_send_sync_and_observes_cancellation() {
    fn assert_send_sync<T: ?Sized + Send + Sync + 'static>() {}
    assert_send_sync::<dyn ContextProvider>();
    fn assert_context_future_send<'a>(future: ContextFuture<'a>) -> ContextFuture<'a> {
        future
    }

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let observed_cancel = Arc::new(AtomicBool::new(false));
    let provider: Arc<dyn ContextProvider> = Arc::new(RecordingContextProvider {
        observed_cancel: Arc::clone(&observed_cancel),
    });
    let request = ContextRequest {
        session_id: session_id(),
        instance_id: instance_id(),
        turn_id: turn_id(),
        model_round: 3,
        conversation: ConversationView::empty(),
        remaining_context_budget: 1_024,
        cancellation,
        deadline: Instant::now() + std::time::Duration::from_secs(60),
    };
    let result = assert_context_future_send(provider.provide(request)).await;
    assert_eq!(result, Err(ContextError::Cancelled));
    assert!(observed_cancel.load(Ordering::SeqCst));
}

struct FakeCompactionStrategy;

impl CompactionStrategy for FakeCompactionStrategy {
    fn compact<'a>(&'a self, request: CompactionRequest) -> CompactionFuture<'a> {
        Box::pin(async move {
            assert_eq!(request.session_id, session_id());
            assert_eq!(request.turn_id, turn_id());
            assert_eq!(request.target_tokens, 256);
            assert!(request.cancellation.is_cancelled());
            assert!(request.deadline > Instant::now() - std::time::Duration::from_secs(1));
            assert_eq!(request.candidate, CompactionCandidate::empty());
            Ok(CompactionProposal {
                through_seq: minicore_runtime::conversation::ConversationSeq::new(7),
                summary: BoundedText::new("secret summary").unwrap(),
            })
        })
    }
}

#[tokio::test]
async fn compaction_port_is_send_sync_and_redacts_summary_debug() {
    fn assert_send_sync<T: ?Sized + Send + Sync + 'static>() {}
    assert_send_sync::<dyn CompactionStrategy>();
    fn assert_compaction_future_send<'a>(future: CompactionFuture<'a>) -> CompactionFuture<'a> {
        future
    }

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let result = assert_compaction_future_send(FakeCompactionStrategy.compact(CompactionRequest {
        session_id: session_id(),
        turn_id: turn_id(),
        candidate: CompactionCandidate::empty(),
        target_tokens: 256,
        cancellation,
        deadline: Instant::now() + std::time::Duration::from_secs(60),
    }))
    .await
    .unwrap();
    assert_eq!(result.through_seq.get(), 7);
    assert!(!format!("{result:?}").contains("secret summary"));
    assert!(!format!("{:?}", CompactionCandidate::empty()).contains("secret"));
}

#[test]
fn p3_ports_are_module_qualified_and_do_not_pull_owner_handles_or_adapters() {
    let lib = include_str!("../src/lib.rs");
    assert!(lib.contains("pub mod context;"));
    assert!(lib.contains("pub mod compaction;"));
    assert!(!lib.contains("pub use context::"));
    assert!(!lib.contains("pub use compaction::"));
    assert!(!lib.contains("ContextSourceId"));

    for source in [
        include_str!("../src/context/provider.rs"),
        include_str!("../src/conversation/compaction_candidate.rs"),
        include_str!("../src/compaction/strategy.rs"),
    ] {
        for forbidden in [
            "Workspace",
            "SessionHandle",
            "SessionRuntime",
            "SessionLog",
            "ModelRequest",
            "HashMap",
            "Any",
            "tokio::spawn",
            "block_on",
            "join_all",
            "FuturesUnordered",
        ] {
            assert!(
                !source.contains(forbidden),
                "forbidden P3 port dependency: {forbidden}"
            );
        }
    }
}
