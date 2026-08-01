use std::str::FromStr;

use minicore_runtime::prompt::{PromptBodyIntent, PromptIntent, SkillIntent, TextIntent};
use minicore_runtime::skills::SkillId;
use minicore_runtime::workspace::WorkspaceRootKey;

#[test]
fn stable_prompt_owner_ids_apply_the_wire_floor() {
    assert_eq!(
        SkillId::from_str("code-review").unwrap().as_str(),
        "code-review"
    );
    assert_eq!(WorkspaceRootKey::from_str("repo").unwrap().as_str(), "repo");

    for invalid in ["", "has space", "has/slash", "has\\slash", "é"] {
        assert!(SkillId::from_str(invalid).is_err(), "accepted {invalid:?}");
        assert!(
            WorkspaceRootKey::from_str(invalid).is_err(),
            "accepted {invalid:?}"
        );
    }
    assert!(SkillId::from_str(&"x".repeat(128)).is_ok());
    assert!(SkillId::from_str(&"x".repeat(129)).is_err());
    assert!(WorkspaceRootKey::from_str(&"x".repeat(64)).is_ok());
    assert!(WorkspaceRootKey::from_str(&"x".repeat(65)).is_err());
    assert!(SkillId::from_str("owner's-skill").is_ok());
    assert!(SkillId::from_str("has\"quote").is_err());
}

#[test]
fn text_intent_is_non_empty_safe_normalized_text() {
    assert_eq!(
        TextIntent::new("hello\nworld").unwrap().text(),
        "hello\nworld"
    );
    assert_eq!(TextIntent::new("a\r\nb\rc").unwrap().text(), "a\nb\nc");
    assert!(TextIntent::new("tab\tis allowed").is_ok());
    for invalid in [
        "",
        "has\0nul",
        "has\u{001b}escape",
        "has\u{007f}del",
        "has\u{0085}c1",
    ] {
        assert!(TextIntent::new(invalid).is_err(), "accepted {invalid:?}");
    }
    assert!(TextIntent::new("x".repeat(131_072)).is_ok());
    assert!(TextIntent::new("x".repeat(131_073)).is_err());
}

#[test]
fn prompt_intent_preserves_order_and_rejects_duplicate_skills() {
    let first = SkillIntent::new("review".parse().unwrap());
    let second = SkillIntent::new("tests".parse().unwrap());
    let intent = PromptIntent::new(
        PromptBodyIntent::Text(TextIntent::new("inspect").unwrap()),
        vec![first.clone(), second.clone()],
    )
    .unwrap();
    assert_eq!(intent.skills(), &[first, second]);

    let duplicate = SkillIntent::new("review".parse().unwrap());
    assert!(
        PromptIntent::new(PromptBodyIntent::Empty, vec![duplicate.clone(), duplicate]).is_err()
    );

    let boundary = (0..32)
        .map(|index| SkillIntent::new(format!("skill-{index}").parse().unwrap()))
        .collect::<Vec<_>>();
    assert!(PromptIntent::new(PromptBodyIntent::Empty, boundary.clone()).is_ok());
    let mut oversized = boundary;
    oversized.push(SkillIntent::new("skill-32".parse().unwrap()));
    assert!(PromptIntent::new(PromptBodyIntent::Empty, oversized).is_err());
}
