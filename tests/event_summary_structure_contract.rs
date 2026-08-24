fn compact(source: &str) -> String {
    source.split_whitespace().collect()
}

#[test]
fn public_event_summaries_exclude_payloads_arguments_answers_and_raw_errors() {
    let source = include_str!("../src/session/event.rs");
    let compact = compact(source);
    assert!(compact.contains(
        "pubstructToolResultSummary{puboutcome:ToolResultOutcome,pubcontent_bytes:usize,}"
    ));
    assert!(
        compact.contains("pubenumInteractionResolutionSummary{Approved,Denied,InputProvided,}")
    );
    let summary = source
        .split_once("pub struct ToolResultSummary")
        .and_then(|(_, tail)| tail.split_once('}'))
        .map(|(body, _)| body)
        .unwrap();
    let resolution = source
        .split_once("pub enum InteractionResolutionSummary")
        .and_then(|(_, tail)| tail.split_once('}'))
        .map(|(body, _)| body)
        .unwrap();
    for forbidden in [
        "ToolOutput",
        "content: BoundedText",
        "arguments",
        "InteractionAnswer",
        "ModelError",
        "ToolError",
        "SessionLogError",
        "source:",
    ] {
        assert!(!summary.contains(forbidden));
        assert!(!resolution.contains(forbidden));
    }
}
