use minicore_runtime::model::{
    AssistantPart, ModelEvent, ModelFinishReason, ModelMessage, ModelResponse, ProviderReplay,
};
use serde_json::{Value, json};

fn replay() -> ProviderReplay {
    ProviderReplay::new("test/v1", json!({"opaque":"secret-ciphertext"})).unwrap()
}

#[test]
fn envelope_bounds_depth_nodes_and_serde_validation() {
    let overhead = ProviderReplay::new("x", json!("")).unwrap().byte_len();
    let exact =
        ProviderReplay::new("x", json!("a".repeat(ProviderReplay::MAX_BYTES - overhead))).unwrap();
    assert_eq!(exact.byte_len(), ProviderReplay::MAX_BYTES);
    assert_eq!(serde_json::to_vec(&exact).unwrap().len(), exact.byte_len());
    assert!(
        ProviderReplay::new(
            "x",
            json!("a".repeat(ProviderReplay::MAX_BYTES - overhead + 1))
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<ProviderReplay>(
            json!({"format":"x", "payload":"a".repeat(ProviderReplay::MAX_BYTES)})
        )
        .is_err()
    );
    for format in ["", "bad format", "bad\nformat"] {
        assert!(ProviderReplay::new(format, Value::Null).is_err());
    }
    assert!(ProviderReplay::new("x".repeat(129), Value::Null).is_err());
    assert!(
        serde_json::from_value::<ProviderReplay>(json!({"format":"x","payload":null,"bytes":0}))
            .is_err()
    );
    let mut deep = Value::Null;
    for _ in 0..ProviderReplay::MAX_DEPTH {
        deep = json!([deep]);
    }
    assert!(ProviderReplay::new("x", deep.clone()).is_ok());
    let too_deep = json!([deep]);
    assert!(ProviderReplay::new("x", too_deep.clone()).is_err());
    assert!(
        serde_json::from_value::<ProviderReplay>(json!({"format":"x","payload":too_deep})).is_err()
    );
    assert!(ProviderReplay::new("x", json!(vec![Value::Null; ProviderReplay::MAX_NODES])).is_err());
}

#[test]
fn old_constructors_wire_and_debug_remain_compatible() {
    let parts = vec![AssistantPart::Text("answer".into())];
    let old = ModelMessage::assistant(parts.clone()).unwrap();
    assert!(matches!(&old, ModelMessage::Assistant(_)));
    let wire = serde_json::to_value(&old).unwrap();
    assert!(wire["content"].is_array());
    assert_eq!(serde_json::from_value::<ModelMessage>(wire).unwrap(), old);
    let old_response = ModelResponse::new(parts.clone(), ModelFinishReason::Stop, None).unwrap();
    assert!(
        serde_json::to_value(&old_response)
            .unwrap()
            .get("provider_replay")
            .is_none()
    );
    assert!(old_response.provider_replay().is_none());
    assert!(ModelMessage::assistant(vec![]).is_err());
    assert!(ModelResponse::new(vec![], ModelFinishReason::Stop, None).is_err());
    for parts in [parts, vec![]] {
        let message =
            ModelMessage::assistant_with_provider_replay(parts.clone(), Some(replay())).unwrap();
        let response = ModelResponse::new_with_provider_replay(
            parts,
            ModelFinishReason::Stop,
            None,
            Some(replay()),
        )
        .unwrap();
        assert_eq!(
            serde_json::from_value::<ModelMessage>(serde_json::to_value(&message).unwrap())
                .unwrap(),
            message
        );
        assert_eq!(
            serde_json::from_value::<ModelResponse>(serde_json::to_value(&response).unwrap())
                .unwrap(),
            response
        );
        for debug in [
            format!("{message:?}"),
            format!("{response:?}"),
            format!("{:?}", ModelEvent::ProviderReplay { replay: replay() }),
        ] {
            assert!(!debug.contains("secret-ciphertext"));
            assert!(!debug.contains("test/v1"));
        }
    }
    assert!(
        serde_json::from_value::<ModelResponse>(
            json!({"parts":[],"finish_reason":"stop","usage":null})
        )
        .is_err()
    );
}
