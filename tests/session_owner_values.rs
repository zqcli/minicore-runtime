use std::num::NonZeroU64;
use std::str::FromStr;

use minicore_runtime::agent_session_lifecycle::{
    ForkAnchor, ForkSourceKind, SessionForkProvenance, SessionLifecycle, SessionMetadata,
    SessionMetadataError, SessionModelConfig,
};
use minicore_runtime::model_gateway::{ModelSelection, ReasoningPreference};
use minicore_runtime::prompt::{PromptId, SessionPromptSelection};
use minicore_runtime::wire::{
    ItemId, ProtocolLimits, SessionId, SessionMetadataRevision, Timestamp,
};

#[test]
fn session_metadata_is_valid_by_construction_normalized_and_redacted() {
    let revision = SessionMetadataRevision::new(NonZeroU64::new(1).unwrap());
    let timestamp = Timestamp::from_str("2026-08-03T10:01:00.456Z").unwrap();
    let metadata = SessionMetadata::new(
        revision,
        Some("Project\r\nsession"),
        Some("private description\rtext"),
        timestamp,
    )
    .unwrap();

    assert_eq!(metadata.revision(), revision);
    assert_eq!(metadata.name(), Some("Project\nsession"));
    assert_eq!(metadata.description(), Some("private description\ntext"));
    assert_eq!(metadata.updated_at(), timestamp);
    assert!(SessionMetadata::new(revision, None::<String>, None::<String>, timestamp).is_ok());
    assert_eq!(
        SessionMetadata::new(revision, Some(""), None::<String>, timestamp),
        Err(SessionMetadataError::EmptyName)
    );
    assert_eq!(
        SessionMetadata::new(revision, None::<String>, Some(""), timestamp)
            .unwrap()
            .description(),
        Some("")
    );

    let limits = ProtocolLimits::v1_0().text;
    let maximum_name = usize::from(limits.max_display_name_bytes);
    let maximum_description = usize::try_from(limits.max_description_bytes).unwrap_or(usize::MAX);
    let exact_name = "é".repeat(maximum_name / "é".len());
    let exact_description = "é".repeat(maximum_description / "é".len());
    assert!(SessionMetadata::new(revision, Some(&exact_name), None::<String>, timestamp).is_ok());
    assert!(
        SessionMetadata::new(
            revision,
            None::<String>,
            Some(&exact_description),
            timestamp,
        )
        .is_ok()
    );
    assert_eq!(
        SessionMetadata::new(
            revision,
            Some(format!("{exact_name}x")),
            None::<String>,
            timestamp,
        ),
        Err(SessionMetadataError::TextTooLong)
    );
    assert_eq!(
        SessionMetadata::new(
            revision,
            None::<String>,
            Some(format!("{exact_description}x")),
            timestamp,
        ),
        Err(SessionMetadataError::TextTooLong)
    );
    assert_eq!(
        SessionMetadata::new(revision, Some("unsafe\u{001b}"), None::<String>, timestamp),
        Err(SessionMetadataError::UnsafeText)
    );

    let debug = format!("{metadata:?} {:?}", SessionMetadataError::UnsafeText);
    for secret in ["Project", "private description", "unsafe\u{001b}"] {
        assert!(!debug.contains(secret), "debug leaked {secret:?}");
    }
}

#[test]
fn session_fork_values_expose_semantics_without_identity_debug_leaks() {
    let source_session_id = SessionId::from_str("ses_33333333333333333333333333333333").unwrap();
    let item_id = ItemId::from_str("itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
    let anchor = ForkAnchor::AfterUserMessage { item_id };
    let provenance = SessionForkProvenance::new(
        source_session_id,
        ForkSourceKind::RecordedHistory,
        anchor.clone(),
    );

    assert_eq!(provenance.source_session_id(), source_session_id);
    assert_eq!(provenance.source(), ForkSourceKind::RecordedHistory);
    assert_eq!(provenance.anchor(), &anchor);
    assert_ne!(SessionLifecycle::Open, SessionLifecycle::Archived);
    assert_ne!(SessionLifecycle::Archived, SessionLifecycle::Deleted);
    assert_ne!(
        ForkSourceKind::LiveSnapshot,
        ForkSourceKind::RecordedHistory
    );
    assert_eq!(format!("{:?}", ForkAnchor::Genesis), "Genesis");
    let before_user = ForkAnchor::BeforeUserMessage { item_id };
    assert!(matches!(
        before_user,
        ForkAnchor::BeforeUserMessage { item_id: actual } if actual == item_id
    ));
    let before_agent = ForkAnchor::BeforeFinalAgentMessage { item_id };
    assert!(matches!(
        before_agent,
        ForkAnchor::BeforeFinalAgentMessage { item_id: actual } if actual == item_id
    ));
    let after_agent = ForkAnchor::AfterFinalAgentMessage { item_id };
    assert!(matches!(
        after_agent,
        ForkAnchor::AfterFinalAgentMessage { item_id: actual } if actual == item_id
    ));

    let model = SessionModelConfig::new(
        ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
        ReasoningPreference::High,
        std::num::NonZeroU32::new(8192),
    );
    let prompts = SessionPromptSelection::new(vec![
        PromptId::from_str("session-secret").unwrap(),
        PromptId::from_str("base").unwrap(),
    ])
    .unwrap();
    assert_eq!(model.selection().provider_id().as_str(), "openai");
    assert_eq!(model.selection().model_id().as_str(), "gpt-5");
    assert_eq!(model.reasoning(), ReasoningPreference::High);
    assert_eq!(model.max_output_tokens().unwrap().get(), 8192);
    assert_eq!(
        prompts
            .enabled()
            .iter()
            .map(PromptId::as_str)
            .collect::<Vec<_>>(),
        ["base", "session-secret"]
    );

    let debug = format!("{anchor:?} {provenance:?} {prompts:?}");
    assert!(debug.contains("AfterUserMessage"));
    for secret in [
        "ses_33333333333333333333333333333333",
        "itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "session-secret",
    ] {
        assert!(!debug.contains(secret), "debug leaked {secret:?}");
    }
}
