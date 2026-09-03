use super::*;

#[test]
fn constructor_catches_descriptor_panic_and_rejects_invalid_descriptor() {
    let model: Arc<dyn Model> = Arc::new(DescriptorPanicModel);
    let kernel = KernelConfig::default_checked().unwrap();
    let error = match ModelDriver::new(model, driver_config(&kernel)) {
        Err(error) => error,
        Ok(_) => panic!("panicking descriptor unexpectedly constructed a driver"),
    };
    assert_eq!(error.kind(), ModelErrorKind::Panicked);

    let mut invalid = descriptor();
    invalid.context_window = 0;
    let model = ScriptModel::new(invalid, Vec::new());
    let model: Arc<dyn Model> = model;
    let error = match ModelDriver::new(model, driver_config(&kernel)) {
        Err(error) => error,
        Ok(_) => panic!("invalid descriptor unexpectedly constructed a driver"),
    };
    assert_eq!(error.kind(), ModelErrorKind::InvalidRequest);

    let invalid_config = ModelDriverConfig::from_kernel_values(
        Duration::ZERO,
        1,
        Duration::ZERO,
        SemanticLimitsSnapshot::from_kernel_values(1, 1, 1, 1, 1, 1),
    );
    let model: Arc<dyn Model> = ScriptModel::new(descriptor(), Vec::new());
    let error = match ModelDriver::new(model, invalid_config) {
        Err(error) => error,
        Ok(_) => panic!("zero model timeout unexpectedly constructed a driver"),
    };
    assert_eq!(error.kind(), ModelErrorKind::InvalidRequest);
}

#[tokio::test(flavor = "current_thread")]
async fn preflight_rejects_reasoning_tools_context_and_mutated_tool_specs_before_start() {
    let mut no_reasoning = descriptor();
    no_reasoning.supported_reasoning = BTreeSet::from([ReasoningPreference::Auto]);
    let no_reasoning = ScriptModel::new(no_reasoning, Vec::new());
    let driver = no_reasoning.driver(&KernelConfig::default_checked().unwrap());
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
    let driver = no_tools.driver(&KernelConfig::default_checked().unwrap());
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
    let driver = model.driver(&KernelConfig::default_checked().unwrap());
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

    let mut oversized_schema = tool_spec("search");
    oversized_schema.input_schema = json!({"padding": "123456789"});
    let request = ModelRequest::new(
        vec![ModelMessage::user("request").unwrap()],
        vec![oversized_schema],
        ModelLimits::default(),
        ReasoningPreference::High,
    )
    .unwrap();
    let limits = SemanticLimits {
        max_tool_schema_bytes: 8,
        ..SemanticLimits::default()
    };
    let driver = model.driver(&limits_kernel(limits));
    assert_eq!(
        driver
            .run(
                request,
                context(CancellationToken::new(), Duration::from_secs(30)),
                &progress,
            )
            .await
            .unwrap_err()
            .kind(),
        ModelErrorKind::InvalidRequest
    );
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
    let driver = model.driver(&kernel(RetryPolicy::new(1, Duration::ZERO).unwrap()));

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
