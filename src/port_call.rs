use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use futures_util::FutureExt;
use tokio::time::Instant as TokioInstant;
use tokio_util::sync::CancellationToken;

use crate::time::{DeadlineOverflow, DeadlineSource, effective_deadline};

pub(crate) enum PortCallOutcome<T, E> {
    Returned(Result<T, E>),
    Cancelled,
    DeadlineExceeded(DeadlineSource),
    InvalidDeadline(DeadlineOverflow),
    Panicked,
}

pub(crate) async fn run_port_call<F, Fut, T, E>(
    parent_cancellation: &CancellationToken,
    turn_deadline: Instant,
    port_timeout: Duration,
    make_future: F,
) -> PortCallOutcome<T, E>
where
    F: FnOnce(CancellationToken, Instant) -> Fut,
    Fut: Future<Output = Result<T, E>> + Send,
{
    if parent_cancellation.is_cancelled() {
        return PortCallOutcome::Cancelled;
    }
    let deadline = match effective_deadline(turn_deadline, port_timeout) {
        Ok(deadline) => deadline,
        Err(error) => return PortCallOutcome::InvalidDeadline(error),
    };
    if TokioInstant::now() >= deadline.tokio() {
        return PortCallOutcome::DeadlineExceeded(deadline.source());
    }

    let child_cancellation = parent_cancellation.child_token();
    let future = match catch_unwind(AssertUnwindSafe(|| {
        make_future(child_cancellation.clone(), deadline.standard())
    })) {
        Ok(future) => AssertUnwindSafe(future).catch_unwind(),
        Err(_) => {
            child_cancellation.cancel();
            return PortCallOutcome::Panicked;
        }
    };
    tokio::pin!(future);
    let result = tokio::select! {
        biased;
        _ = parent_cancellation.cancelled() => {
            child_cancellation.cancel();
            return PortCallOutcome::Cancelled;
        }
        _ = tokio::time::sleep_until(deadline.tokio()) => {
            child_cancellation.cancel();
            return PortCallOutcome::DeadlineExceeded(deadline.source());
        }
        result = &mut future => result,
    };
    child_cancellation.cancel();
    match result {
        Ok(result) => PortCallOutcome::Returned(result),
        Err(_) => PortCallOutcome::Panicked,
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    use super::*;

    struct ProbeFuture {
        cancellation: CancellationToken,
        dropped: Arc<AtomicBool>,
        cancelled_before_drop: Arc<AtomicBool>,
        polled: Arc<AtomicBool>,
        notify: Arc<Notify>,
        result: Option<Result<(), &'static str>>,
        panic_on_poll: bool,
    }

    impl Future for ProbeFuture {
        type Output = Result<(), &'static str>;

        fn poll(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
            self.polled.store(true, Ordering::SeqCst);
            self.notify.notify_waiters();
            if self.panic_on_poll {
                panic!("scripted port future poll panic");
            }
            self.result.take().map_or(Poll::Pending, Poll::Ready)
        }
    }

    impl Drop for ProbeFuture {
        fn drop(&mut self) {
            self.cancelled_before_drop
                .store(self.cancellation.is_cancelled(), Ordering::SeqCst);
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    struct Probe {
        dropped: Arc<AtomicBool>,
        cancelled_before_drop: Arc<AtomicBool>,
        polled: Arc<AtomicBool>,
        notify: Arc<Notify>,
    }

    impl Probe {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                dropped: Arc::new(AtomicBool::new(false)),
                cancelled_before_drop: Arc::new(AtomicBool::new(false)),
                polled: Arc::new(AtomicBool::new(false)),
                notify: Arc::new(Notify::new()),
            })
        }

        async fn wait_polled(&self) {
            loop {
                let notified = self.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.polled.load(Ordering::SeqCst) {
                    return;
                }
                notified.await;
            }
        }

        fn pending_future(&self, cancellation: CancellationToken) -> ProbeFuture {
            self.future(cancellation, None, false)
        }

        fn ready_future(
            &self,
            cancellation: CancellationToken,
            result: Result<(), &'static str>,
        ) -> ProbeFuture {
            self.future(cancellation, Some(result), false)
        }

        fn panic_future(&self, cancellation: CancellationToken) -> ProbeFuture {
            self.future(cancellation, None, true)
        }

        fn future(
            &self,
            cancellation: CancellationToken,
            result: Option<Result<(), &'static str>>,
            panic_on_poll: bool,
        ) -> ProbeFuture {
            ProbeFuture {
                cancellation,
                dropped: Arc::clone(&self.dropped),
                cancelled_before_drop: Arc::clone(&self.cancelled_before_drop),
                polled: Arc::clone(&self.polled),
                notify: Arc::clone(&self.notify),
                result,
                panic_on_poll,
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn construction_panic_is_reported_and_cancels_child() {
        let child = Arc::new(Mutex::new(None));
        let observed_child = Arc::clone(&child);
        let result = run_port_call(
            &CancellationToken::new(),
            Instant::now() + Duration::from_secs(30),
            Duration::from_secs(30),
            move |cancellation, _| -> std::future::Ready<Result<(), ()>> {
                *observed_child.lock().unwrap() = Some(cancellation);
                panic!("scripted port future construction panic");
            },
        )
        .await;
        assert!(matches!(result, PortCallOutcome::Panicked));
        assert!(child.lock().unwrap().as_ref().unwrap().is_cancelled());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn poll_panic_cancels_child_before_future_drop() {
        let probe = Probe::new();
        let task_probe = Arc::clone(&probe);
        let result = run_port_call(
            &CancellationToken::new(),
            Instant::now() + Duration::from_secs(30),
            Duration::from_secs(30),
            |child, _| task_probe.panic_future(child),
        )
        .await;
        assert!(matches!(result, PortCallOutcome::Panicked));
        assert!(probe.dropped.load(Ordering::SeqCst));
        assert!(probe.cancelled_before_drop.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn returned_ok_and_err_results_cancel_child_before_future_drop() {
        for result in [Ok(()), Err("adapter")] {
            let probe = Probe::new();
            let task_probe = Arc::clone(&probe);
            let outcome = run_port_call(
                &CancellationToken::new(),
                Instant::now() + Duration::from_secs(30),
                Duration::from_secs(30),
                move |child, _| task_probe.ready_future(child, result),
            )
            .await;
            assert!(matches!(outcome, PortCallOutcome::Returned(_)));
            assert!(probe.dropped.load(Ordering::SeqCst));
            assert!(probe.cancelled_before_drop.load(Ordering::SeqCst));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pre_cancel_and_expired_deadline_do_not_call_closure() {
        let calls = Arc::new(AtomicBool::new(false));
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let cancelled_calls = Arc::clone(&calls);
        assert!(matches!(
            run_port_call(
                &cancellation,
                Instant::now() + Duration::from_secs(30),
                Duration::from_secs(30),
                move |_, _| {
                    cancelled_calls.store(true, Ordering::SeqCst);
                    async { Ok::<(), ()>(()) }
                },
            )
            .await,
            PortCallOutcome::Cancelled
        ));
        assert!(!calls.load(Ordering::SeqCst));

        let expired_calls = Arc::clone(&calls);
        assert!(matches!(
            run_port_call(
                &CancellationToken::new(),
                Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
                Duration::from_secs(30),
                move |_, _| {
                    expired_calls.store(true, Ordering::SeqCst);
                    async { Ok::<(), ()>(()) }
                },
            )
            .await,
            PortCallOutcome::DeadlineExceeded(DeadlineSource::Turn)
        ));
        assert!(!calls.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancellation_wins_when_cancel_timeout_and_result_are_ready() {
        let cancellation = CancellationToken::new();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let task_cancellation = cancellation.clone();
        let task_started = Arc::clone(&started);
        let task_release = Arc::clone(&release);
        let task = tokio::spawn(async move {
            run_port_call(
                &task_cancellation,
                Instant::now() + Duration::from_secs(30),
                Duration::from_secs(5),
                move |_, _| async move {
                    task_started.notify_one();
                    task_release.notified().await;
                    Ok::<(), ()>(())
                },
            )
            .await
        });
        started.notified().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        cancellation.cancel();
        release.notify_one();
        assert!(matches!(task.await.unwrap(), PortCallOutcome::Cancelled));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn deadline_wins_when_deadline_and_result_are_ready() {
        let result = tokio::spawn(async {
            let cancellation = CancellationToken::new();
            run_port_call(
                &cancellation,
                Instant::now() + Duration::from_secs(5),
                Duration::from_secs(30),
                |_, deadline| async move {
                    tokio::time::sleep_until(TokioInstant::from_std(deadline)).await;
                    Ok::<(), ()>(())
                },
            )
            .await
        });
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(matches!(
            result.await.unwrap(),
            PortCallOutcome::DeadlineExceeded(DeadlineSource::Turn)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_effective_deadline_is_not_a_panic_or_closure_call() {
        let called = Arc::new(AtomicBool::new(false));
        let closure_called = Arc::clone(&called);
        let result = run_port_call(
            &CancellationToken::new(),
            Instant::now() + Duration::from_secs(30),
            Duration::MAX,
            move |_, _| {
                closure_called.store(true, Ordering::SeqCst);
                async { Ok::<(), ()>(()) }
            },
        )
        .await;
        assert!(matches!(
            result,
            PortCallOutcome::InvalidDeadline(DeadlineOverflow)
        ));
        assert!(!called.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_cancels_child_before_future_drop() {
        let probe = Probe::new();
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task_probe = Arc::clone(&probe);
        let task = tokio::spawn(async move {
            run_port_call(
                &task_cancellation,
                Instant::now() + Duration::from_secs(30),
                Duration::from_secs(30),
                |child, _| task_probe.pending_future(child),
            )
            .await
        });
        probe.wait_polled().await;
        cancellation.cancel();
        assert!(matches!(task.await.unwrap(), PortCallOutcome::Cancelled));
        assert!(probe.dropped.load(Ordering::SeqCst));
        assert!(probe.cancelled_before_drop.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn timeout_cancels_child_before_future_drop() {
        let probe = Probe::new();
        let task_probe = Arc::clone(&probe);
        let task = tokio::spawn(async move {
            run_port_call(
                &CancellationToken::new(),
                Instant::now() + Duration::from_secs(30),
                Duration::from_secs(5),
                |child, _| task_probe.pending_future(child),
            )
            .await
        });
        probe.wait_polled().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(matches!(
            task.await.unwrap(),
            PortCallOutcome::DeadlineExceeded(DeadlineSource::Port)
        ));
        assert!(probe.dropped.load(Ordering::SeqCst));
        assert!(probe.cancelled_before_drop.load(Ordering::SeqCst));
    }
}
