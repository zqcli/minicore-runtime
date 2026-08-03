use std::num::NonZeroU64;
use std::str::FromStr;

use minicore_runtime::agent_session_lifecycle::{
    AgentDefinition, AgentMetadata, AgentMetadataError, AgentStatus,
};
use minicore_runtime::prompt::{
    AgentPromptSelection, AgentPromptSelectionError, PromptId, SessionPromptSelection,
    SessionPromptSelectionError,
};
use minicore_runtime::wire::{
    AgentId, AgentMetadataRevision, AgentRevision, ProtocolLimits, Timestamp,
};

#[test]
fn agent_prompt_selection_is_sorted_unique_bounded_and_keeps_session_selection_unchanged() {
    let base = PromptId::from_str("base").unwrap();
    let safety = PromptId::from_str("safety").unwrap();
    let selection = AgentPromptSelection::new(vec![safety.clone(), base.clone()]).unwrap();
    assert_eq!(
        selection
            .enabled()
            .iter()
            .map(PromptId::as_str)
            .collect::<Vec<_>>(),
        ["base", "safety"]
    );
    assert!(
        AgentPromptSelection::new(Vec::new())
            .unwrap()
            .enabled()
            .is_empty()
    );
    assert_eq!(
        AgentPromptSelection::new(vec![base.clone(), base.clone()]),
        Err(AgentPromptSelectionError::DuplicatePrompt)
    );

    let maximum =
        usize::try_from(ProtocolLimits::v1_0().transport.max_array_items).unwrap_or(usize::MAX);
    let full = (0..maximum)
        .map(|index| PromptId::from_str(&format!("prompt-{index}")))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(AgentPromptSelection::new(full.clone()).is_ok());
    let mut too_many = full;
    too_many.push(PromptId::from_str("prompt-overflow").unwrap());
    assert_eq!(
        AgentPromptSelection::new(too_many),
        Err(AgentPromptSelectionError::TooManyPrompts)
    );

    let selection_debug = format!("{selection:?}");
    assert!(!selection_debug.contains("base"));
    assert!(!selection_debug.contains("safety"));

    // Session ownership continues to have its original sorted/unique behavior and error type.
    let session = SessionPromptSelection::new(vec![safety, base.clone()]).unwrap();
    assert_eq!(
        session
            .enabled()
            .iter()
            .map(PromptId::as_str)
            .collect::<Vec<_>>(),
        ["base", "safety"]
    );
    assert_eq!(
        SessionPromptSelection::new(vec![base.clone(), base]),
        Err(SessionPromptSelectionError::DuplicatePrompt)
    );
}

#[test]
fn agent_definition_and_metadata_are_valid_by_construction_and_redacted() {
    let agent_id = AgentId::from_str("agt_11111111111111111111111111111111").unwrap();
    let definition_revision = AgentRevision::new(NonZeroU64::new(1).unwrap());
    let metadata_revision = AgentMetadataRevision::new(NonZeroU64::new(1).unwrap());
    let timestamp = Timestamp::from_str("2026-08-03T10:00:00.123Z").unwrap();
    let private_selection =
        AgentPromptSelection::new(vec![PromptId::from_str("secret-prompt").unwrap()]).unwrap();
    let definition = AgentDefinition::new(
        agent_id,
        definition_revision,
        private_selection.clone(),
        timestamp,
    );
    assert_eq!(definition.agent_id(), agent_id);
    assert_eq!(definition.revision(), definition_revision);
    assert_eq!(definition.prompts().enabled().len(), 1);
    assert_eq!(definition.created_at(), timestamp);

    let metadata = AgentMetadata::new(
        metadata_revision,
        "Planner\r\nName",
        Some("private description\rtext"),
        timestamp,
    )
    .unwrap();
    assert_eq!(metadata.revision(), metadata_revision);
    assert_eq!(metadata.name(), "Planner\nName");
    assert_eq!(metadata.description(), Some("private description\ntext"));
    assert_eq!(metadata.updated_at(), timestamp);
    assert_ne!(AgentStatus::Enabled, AgentStatus::Disabled);
    assert_ne!(AgentStatus::Disabled, AgentStatus::Deleted);

    let limits = ProtocolLimits::v1_0().text;
    let maximum_name = usize::from(limits.max_display_name_bytes);
    let maximum_description = usize::try_from(limits.max_description_bytes).unwrap_or(usize::MAX);
    let exact_name = "é".repeat(maximum_name / "é".len());
    assert_eq!(exact_name.len(), maximum_name);
    assert!(AgentMetadata::new(metadata_revision, &exact_name, None::<String>, timestamp).is_ok());
    let oversized_name = format!("{exact_name}x");
    assert_eq!(oversized_name.len(), maximum_name + 1);
    assert_eq!(
        AgentMetadata::new(
            metadata_revision,
            &oversized_name,
            None::<String>,
            timestamp
        ),
        Err(AgentMetadataError::TextTooLong)
    );

    let exact_description = "é".repeat(maximum_description / "é".len());
    assert_eq!(exact_description.len(), maximum_description);
    assert!(
        AgentMetadata::new(
            metadata_revision,
            "safe",
            Some(&exact_description),
            timestamp,
        )
        .is_ok()
    );
    let oversized_description = format!("{exact_description}x");
    assert_eq!(oversized_description.len(), maximum_description + 1);
    assert_eq!(
        AgentMetadata::new(
            metadata_revision,
            "safe",
            Some(&oversized_description),
            timestamp,
        ),
        Err(AgentMetadataError::TextTooLong)
    );
    assert_eq!(
        AgentMetadata::new(metadata_revision, "", None::<String>, timestamp),
        Err(AgentMetadataError::EmptyName)
    );
    assert_eq!(
        AgentMetadata::new(
            metadata_revision,
            "unsafe\u{001b}",
            None::<String>,
            timestamp
        ),
        Err(AgentMetadataError::UnsafeText)
    );
    assert_eq!(
        AgentMetadata::new(metadata_revision, "safe", Some("unsafe\u{001b}"), timestamp,),
        Err(AgentMetadataError::UnsafeText)
    );
    assert_eq!(
        AgentMetadata::new(metadata_revision, "safe", Some(""), timestamp)
            .unwrap()
            .description(),
        Some("")
    );

    let debug = format!("{private_selection:?} {definition:?} {metadata:?}");
    for private_value in [
        "secret-prompt",
        "agt_11111111111111111111111111111111",
        "Planner",
        "private description",
    ] {
        assert!(
            !debug.contains(private_value),
            "debug leaked {private_value:?}"
        );
    }
    assert!(!format!("{:?}", AgentMetadataError::UnsafeText).contains("unsafe\u{001b}"));
}
