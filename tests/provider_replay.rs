use minicore_runtime::model::{
    AssistantPart, ModelEvent, ModelFinishReason, ModelMessage, ModelResponse, ProviderReplay,
};
use minicore_runtime::value::MAX_JSON_NESTING;
use serde_json::{Value, json};

fn replay() -> ProviderReplay {
    ProviderReplay::new("test/v1", json!({"opaque":"secret-ciphertext"})).unwrap()
}

#[test]
fn envelope_bounds_depth_nodes_and_serde_validation() {
    let payload = json!({"large": "a".repeat(256 * 1024 + 1), "wide": vec![Value::Null; 16_385]});
    let replay = ProviderReplay::new("x", payload.clone()).unwrap();
    assert_eq!(replay.payload(), &payload);
    let wire = serde_json::to_vec(&replay).unwrap();
    assert_eq!(wire.len(), replay.byte_len());
    assert!(wire.len() > 256 * 1024);
    assert_eq!(
        serde_json::from_slice::<ProviderReplay>(&wire).unwrap(),
        replay
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
    for _ in 0..MAX_JSON_NESTING {
        deep = json!([deep]);
    }
    assert!(ProviderReplay::new("x", deep.clone()).is_ok());
    let too_deep = json!([deep]);
    assert!(ProviderReplay::new("x", too_deep.clone()).is_err());
    assert!(
        serde_json::from_value::<ProviderReplay>(json!({"format":"x","payload":too_deep})).is_err()
    );
}

#[test]
fn old_constructors_wire_and_debug_remain_compatible() {
    let parts = vec![AssistantPart::Text("answer".into())];
    let old = ModelMessage::assistant(parts.clone()).unwrap();
    assert!(matches!(&old, ModelMessage::Assistant(_)));
    assert!(format!("{old:?}").starts_with("ModelMessage::Assistant {"));
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
        assert!(format!("{message:?}").starts_with("ModelMessage::AssistantWithReplay {"));
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
