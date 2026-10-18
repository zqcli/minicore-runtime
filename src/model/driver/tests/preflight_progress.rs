use super::*;

#[test]
fn constructor_catches_descriptor_panic_and_rejects_invalid_descriptor() {
    let model: Arc<dyn Model> = Arc::new(DescriptorPanicModel);
    let kernel = ModelDriverConfig::default();
    let error = match ModelDriver::new(model, kernel.clone()) {
        Err(error) => error,
        Ok(_) => panic!("panicking descriptor unexpectedly constructed a driver"),
    };
    assert_eq!(error.kind(), ModelErrorKind::Panicked);

    let mut invalid = descriptor();
    invalid.context_window = 0;
    let model = ScriptModel::new(invalid, Vec::new());
    let model: Arc<dyn Model> = model;
    let error = match ModelDriver::new(model, kernel.clone()) {
        Err(error) => error,
        Ok(_) => panic!("invalid descriptor unexpectedly constructed a driver"),
    };
    assert_eq!(error.kind(), ModelErrorKind::InvalidRequest);
}

#[tokio::test(flavor = "current_thread")]
async fn preflight_rejects_reasoning_tools_context_and_mutated_tool_specs_before_start() {
    let mut no_reasoning = descriptor();
    no_reasoning.supported_reasoning = BTreeSet::from([ReasoningPreference::Auto]);
    let no_reasoning = ScriptModel::new(no_reasoning, Vec::new());
    let driver = no_reasoning.driver(&ModelDriverConfig::default());
    let (progress, _receiver) = progress_channel();
    assert_eq!(
        driver
            .run(
                request(),
                context(CancellationToken::new(), Duration::from_secs(30)),
                &progress,
            )
            .await
            .unwrap_err()
            .kind(),
        ModelErrorKind::InvalidRequest
    );
    assert_eq!(no_reasoning.starts(), 0);

    let mut no_tools = descriptor();
    no_tools.supports_tools = false;
    let no_tools = ScriptModel::new(no_tools, Vec::new());
    let driver = no_tools.driver(&ModelDriverConfig::default());
    assert_eq!(
        driver
            .run(
                tool_request(),
                context(CancellationToken::new(), Duration::from_secs(30)),
                &progress,
            )
            .await
            .unwrap_err()
            .kind(),
        ModelErrorKind::InvalidRequest
    );
    assert_eq!(no_tools.starts(), 0);

    let model = ScriptModel::new(descriptor(), Vec::new());
    let driver = model.driver(&ModelDriverConfig::default());
    let too_large = request_with(ReasoningPreference::High, Vec::new(), Some(129));
    assert_eq!(
        driver
            .run(
                too_large,
                context(CancellationToken::new(), Duration::from_secs(30)),
                &progress,
            )
            .await
            .unwrap_err()
            .kind(),
        ModelErrorKind::InvalidRequest
    );

    for schema in [
        json!([]),
        json!({"padding": "x".repeat(crate::value::MAX_JSON_BYTES)}),
    ] {
        let mut wire = serde_json::to_value(tool_spec("search")).unwrap();
        wire["input_schema"] = schema;
        assert!(serde_json::from_value::<ToolSpec>(wire).is_err());
    }
    assert_eq!(model.starts(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn progress_is_best_effort_for_accepted_full_and_closed_channels() {
    let events = || {
        vec![
            Ok(ModelEvent::text_delta("text").unwrap()),
            Ok(ModelEvent::reasoning_delta("reason").unwrap()),
            Ok(finish(ModelFinishReason::Stop)),
        ]
    };

    let model = ScriptModel::new(
        descriptor(),
        vec![
            Behavior::Events(events()),
            Behavior::Events(events()),
            Behavior::Events(events()),
        ],
    );
    let driver = model.driver(&with_retry(RetryPolicySnapshot::new(1, Duration::ZERO)));

    let (accepted, mut accepted_rx) = mpsc::channel(2);
    assert!(
        driver
            .run(
                request(),
                context(CancellationToken::new(), Duration::from_secs(30)),
                &accepted,
            )
            .await
            .is_ok()
    );
    assert!(matches!(
        accepted_rx.try_recv(),
        Ok(ModelDriverProgress::TextDelta(_))
    ));
    assert!(matches!(
        accepted_rx.try_recv(),
        Ok(ModelDriverProgress::ReasoningDelta(_))
    ));

    let (full, _full_rx) = mpsc::channel(1);
    full.try_send(ModelDriverProgress::TextDelta(
        BoundedText::new("occupied").unwrap(),
    ))
    .unwrap();
    assert!(
        driver
            .run(
                request(),
                context(CancellationToken::new(), Duration::from_secs(30)),
                &full,
            )
            .await
            .is_ok()
    );

    let (closed, closed_rx) = mpsc::channel(1);
    drop(closed_rx);
    assert!(
        driver
            .run(
                request(),
                context(CancellationToken::new(), Duration::from_secs(30)),
                &closed,
            )
            .await
            .is_ok()
    );
    assert_eq!(model.starts(), 3);
}
