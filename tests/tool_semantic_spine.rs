use std::str::FromStr;

use minicore_runtime::tools::{ToolCallId, ToolName, ToolResultContent};

#[test]
fn tool_name_and_call_id_use_distinct_owner_grammars() {
    for valid in ["read_file", "Write-File2", "_private", "-prefixed"] {
        assert_eq!(ToolName::from_str(valid).unwrap().as_str(), valid);
    }
    for invalid in ["", "has/slash", "has space", "punctuation?", "é"] {
        assert!(ToolName::from_str(invalid).is_err(), "accepted {invalid:?}");
    }
    assert!(ToolName::from_str(&"x".repeat(64)).is_ok());
    assert!(ToolName::from_str(&"x".repeat(65)).is_err());

    assert_eq!(
        ToolCallId::from_str("provider/call:1").unwrap().as_str(),
        "provider/call:1"
    );
    for invalid in ["", "has space", "has\"quote", "has\\slash", "é"] {
        assert!(
            ToolCallId::from_str(invalid).is_err(),
            "accepted {invalid:?}"
        );
    }
    assert!(ToolCallId::from_str(&"x".repeat(256)).is_ok());
    assert!(ToolCallId::from_str(&"x".repeat(257)).is_err());
}

#[test]
fn tool_result_content_is_bounded_normalized_and_debug_redacted() {
    let content =
        ToolResultContent::from_text_parts(vec!["SECRET-MARKER\r\nnext".to_owned()]).unwrap();
    assert_eq!(content.parts()[0].as_text(), "SECRET-MARKER\nnext");
    assert!(!format!("{content:?}").contains("SECRET-MARKER"));

    assert!(ToolResultContent::from_text_parts(Vec::new()).is_err());
    assert!(ToolResultContent::from_text_parts(vec![String::new()]).is_ok());
    assert!(ToolResultContent::from_text_parts(vec!["x".repeat(65_537)]).is_err());
    assert!(ToolResultContent::from_text_parts(vec!["bad\u{001b}".to_owned()]).is_err());
}
