use std::num::NonZeroU32;

use minicore_runtime::agent_session_lifecycle::SessionModelConfig;
use minicore_runtime::model_gateway::{ModelId, ModelSelection, ProviderId, ReasoningPreference};
use minicore_runtime::prompt::{PromptId, SessionPromptSelection};
use minicore_runtime::runtime_interface::{NewSessionDefinition, NewSessionMetadata};
use minicore_runtime::wire::{CanonicalFileUri, WorkspaceRelativePath};
use minicore_runtime::workspace::{
    RequestedFilesystemAccess, WorkspaceCwdSpec, WorkspaceDefinitionInput, WorkspaceInputError,
    WorkspaceRootInput, WorkspaceRootKey, WorkspaceSourcePolicy,
};

#[test]
fn create_session_owner_values_are_valid_by_construction_and_debug_safe() {
    let root = root("repo", "file:///Users/alice/project");
    let cwd = WorkspaceCwdSpec::new("repo".parse().unwrap(), "private/source".parse().unwrap());
    let workspace = WorkspaceDefinitionInput::new(root, Vec::new(), cwd).unwrap();
    let model = SessionModelConfig::new(
        ModelSelection::new(
            "openai".parse::<ProviderId>().unwrap(),
            "gpt-5".parse::<ModelId>().unwrap(),
        ),
        ReasoningPreference::Auto,
        None,
    );
    let prompts = SessionPromptSelection::new(Vec::new()).unwrap();
    let definition = NewSessionDefinition::new(workspace, model, prompts);
    let metadata = NewSessionMetadata::new(None::<String>, None::<String>).unwrap();

    assert_eq!(definition.workspace().primary_root().key().as_str(), "repo");
    assert_eq!(
        definition.workspace().cwd().relative_path().as_str(),
        "private/source"
    );
    assert_eq!(
        definition.model().selection().provider_id().as_str(),
        "openai"
    );
    assert!(definition.prompts().enabled().is_empty());
    assert!(metadata.name().is_none());
    let debug = format!("{definition:?} {metadata:?}");
    assert!(!debug.contains("/Users/alice/project"));
    let path = definition.workspace().primary_root().path();
    assert!(!format!("{path}").contains("/Users/alice/project"));
    assert!(!format!("{path:?}").contains("/Users/alice/project"));
    assert!(!debug.contains("private/source"));
}

#[test]
fn workspace_input_rejects_duplicate_roots_unknown_cwd_and_selected_limit_overflow() {
    let primary = root("repo", "file:///repo");
    let duplicate_key = root("repo", "file:///other");
    let cwd = WorkspaceCwdSpec::new("repo".parse().unwrap(), WorkspaceRelativePath::default());
    assert_eq!(
        WorkspaceDefinitionInput::new(primary.clone(), vec![duplicate_key], cwd.clone()),
        Err(WorkspaceInputError::DuplicateRootKey),
    );

    let duplicate_uri = WorkspaceRootInput::new(
        "other".parse().unwrap(),
        "file:///repo".parse().unwrap(),
        RequestedFilesystemAccess::ReadOnly,
        WorkspaceSourcePolicy::new(false, false),
    );
    assert_eq!(
        WorkspaceDefinitionInput::new(primary.clone(), vec![duplicate_uri], cwd),
        Err(WorkspaceInputError::DuplicateRootUri),
    );

    let unknown_cwd =
        WorkspaceCwdSpec::new("missing".parse().unwrap(), WorkspaceRelativePath::default());
    assert_eq!(
        WorkspaceDefinitionInput::new(primary.clone(), Vec::new(), unknown_cwd),
        Err(WorkspaceInputError::UnknownCwdRoot),
    );

    let additional = (0..16)
        .map(|index| root(&format!("extra-{index}"), &format!("file:///extra-{index}")))
        .collect();
    assert_eq!(
        WorkspaceDefinitionInput::new(
            primary,
            additional,
            WorkspaceCwdSpec::new("repo".parse().unwrap(), WorkspaceRelativePath::default()),
        ),
        Err(WorkspaceInputError::TooManyRoots),
    );
}

#[test]
fn prompt_model_and_metadata_values_enforce_their_owner_rules() {
    let prompt = "session-context".parse::<PromptId>().unwrap();
    assert!(SessionPromptSelection::new(vec![prompt.clone()]).is_ok());
    assert!(SessionPromptSelection::new(vec![prompt.clone(), prompt]).is_err());

    let config = SessionModelConfig::new(
        ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
        ReasoningPreference::High,
        NonZeroU32::new(1024),
    );
    assert_eq!(config.max_output_tokens(), NonZeroU32::new(1024));

    let metadata =
        NewSessionMetadata::new(Some("name\r\nline"), Some("description\rline")).unwrap();
    assert_eq!(metadata.name(), Some("name\nline"));
    assert_eq!(metadata.description(), Some("description\nline"));
    assert!(NewSessionMetadata::new(Some(""), None::<String>).is_err());
}

fn root(key: &str, uri: &str) -> WorkspaceRootInput {
    WorkspaceRootInput::new(
        key.parse::<WorkspaceRootKey>().unwrap(),
        uri.parse::<CanonicalFileUri>().unwrap(),
        RequestedFilesystemAccess::ReadWrite,
        WorkspaceSourcePolicy::new(true, true),
    )
}
