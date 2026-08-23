#[path = "support/fake_runtime_bindings.rs"]
pub mod fake_runtime_bindings;
pub mod support;

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

use minicore_runtime::error::SessionOpenErrorKind;
use minicore_runtime::{KernelConfig, SessionRuntime, SessionRuntimeOptions};

use fake_runtime_bindings::fixture;
use support::fake_session_log::{FakeSessionLog, Operation};

struct ThreadWake(Thread);

impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

#[test]
fn options_reject_live_runtime_without_time_driver() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let fixture = fixture("host:model");
    let error = SessionRuntimeOptions::new(
        KernelConfig::default_checked().unwrap(),
        fixture.bindings,
        runtime.handle().clone(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), SessionOpenErrorKind::InvalidConfiguration);
}

#[test]
fn non_tokio_executor_can_create_and_shutdown_on_configured_runtime() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_time()
        .build()
        .unwrap();
    let fixture = fixture("host:model");
    let log = FakeSessionLog::new();
    let inspection = log.inspection();
    let options = SessionRuntimeOptions::new(
        KernelConfig::default_checked().unwrap(),
        fixture.bindings,
        runtime.handle().clone(),
    )
    .unwrap();
    let owner = block_on(SessionRuntime::create(
        "ses_00000000000000000000000000000031".parse().unwrap(),
        fixture.spec,
        Box::new(log),
        options,
    ))
    .unwrap();
    block_on(owner.shutdown()).unwrap();
    assert_eq!(
        inspection.operations(),
        vec![Operation::Initialize, Operation::Close]
    );
    assert_eq!(inspection.close_count(), 1);
    assert_eq!(inspection.active_mutable_operations(), 0);
}
