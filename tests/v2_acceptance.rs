//! P0 acceptance inventory for the v0.2 Core Reset.
//!
//! These names are the stable acceptance seams. The ignored bodies deliberately fail when
//! explicitly selected before the corresponding P1+ implementation exists. They are not a
//! substitute for behavior and must be replaced with public typed-API tests during P1–P7.

const ACCEPTANCE_CASES: [&str; 20] = [
    "AT-01 Model-only Turn",
    "AT-02 Read file",
    "AT-03 Edit file",
    "AT-04 Run tests",
    "AT-05 Multi-round tools",
    "AT-06 Ask user",
    "AT-07 Cancel model",
    "AT-08 Cancel process",
    "AT-09 Runtime restart",
    "AT-10 Partial JSONL",
    "AT-11 Compaction",
    "AT-12 Workspace security",
    "AT-13 Provider conformance",
    "AT-14 Session isolation",
    "AT-15 Event lag",
    "AT-16 Busy rule",
    "AT-17 Close",
    "AT-18 Custom Tool",
    "AT-19 Secret env",
    "AT-20 No legacy coupling",
];

#[test]
fn acceptance_inventory_is_complete() {
    assert_eq!(ACCEPTANCE_CASES.len(), 20);
    for (index, case) in ACCEPTANCE_CASES.iter().enumerate() {
        let expected = format!("AT-{:02}", index + 1);
        assert!(
            case.starts_with(&expected),
            "missing acceptance case: {expected}"
        );
    }
}

#[test]
#[ignore = "P1+ implementation pending: typed submit, streaming events, terminal recovery"]
fn at_01_model_only_turn() {
    panic!("AT-01 is not implemented before P1+");
}

#[test]
#[ignore = "P2/P6 implementation pending: public read_file Tool round-trip"]
fn at_02_read_file() {
    panic!("AT-02 is not implemented before P1+");
}

#[test]
#[ignore = "P2/P6 implementation pending: root-relative write_file security"]
fn at_03_edit_file() {
    panic!("AT-03 is not implemented before P1+");
}

#[test]
#[ignore = "P7 implementation pending: structured run_command and ProcessPolicy"]
fn at_04_run_tests() {
    panic!("AT-04 is not implemented before P1+");
}

#[test]
#[ignore = "P6 implementation pending: ordered multi-round tool execution"]
fn at_05_multi_round_tools() {
    panic!("AT-05 is not implemented before P1+");
}

#[test]
#[ignore = "P6 implementation pending: WaitingForInput and typed answer routing"]
fn at_06_ask_user() {
    panic!("AT-06 is not implemented before P1+");
}

#[test]
#[ignore = "P6 implementation pending: provider cancellation and cancelled terminal outcome"]
fn at_07_cancel_model() {
    panic!("AT-07 is not implemented before P1+");
}

#[test]
#[ignore = "P7 implementation pending: direct child cancellation and cleanup"]
fn at_08_cancel_process() {
    panic!("AT-08 is not implemented before P1+");
}

#[test]
#[ignore = "P4/P6/P7 implementation pending: restartable session persistence"]
fn at_09_runtime_restart() {
    panic!("AT-09 is not implemented before P1+");
}

#[test]
#[ignore = "P4 implementation pending: JSONL tail repair and middle corruption"]
fn at_10_partial_jsonl() {
    panic!("AT-10 is not implemented before P1+");
}

#[test]
#[ignore = "P5/P6 implementation pending: summary append and prompt projection"]
fn at_11_compaction() {
    panic!("AT-11 is not implemented before P1+");
}

#[test]
#[ignore = "P2 implementation pending: single-root capability and symlink safety"]
fn at_12_workspace_security() {
    panic!("AT-12 is not implemented before P1+");
}

#[test]
#[ignore = "P3 implementation pending: provider-neutral OpenAI/Anthropic conformance"]
fn at_13_provider_conformance() {
    panic!("AT-13 is not implemented before P1+");
}

#[test]
#[ignore = "P6/P7 implementation pending: independent Session actors and streams"]
fn at_14_session_isolation() {
    panic!("AT-14 is not implemented before P1+");
}

#[test]
#[ignore = "P6 implementation pending: lag-to-ResyncRequired snapshot recovery"]
fn at_15_event_lag() {
    panic!("AT-15 is not implemented before P1+");
}

#[test]
#[ignore = "P6 implementation pending: Busy without implicit queueing"]
fn at_16_busy_rule() {
    panic!("AT-16 is not implemented before P1+");
}

#[test]
#[ignore = "P7 implementation pending: public close cancellation, bounded abort, and reloadability"]
fn at_17_close() {
    panic!("AT-17 is not implemented before P1+");
}

#[test]
#[ignore = "P2/P6 implementation pending: host Tool trait and registry extension seam"]
fn at_18_custom_tool() {
    panic!("AT-18 is not implemented before P1+");
}

#[test]
#[ignore = "P7 implementation pending: env_clear and ProcessPolicy allowlist"]
fn at_19_secret_env() {
    panic!("AT-19 is not implemented before P1+");
}

#[test]
#[ignore = "P8/P9 static gate pending: remove legacy wire/store/residency coupling"]
fn at_20_no_legacy_coupling() {
    panic!("AT-20 is a static gate and is not implemented before P8/P9");
}
