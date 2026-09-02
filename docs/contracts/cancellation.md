# Cancellation Contract

Cancellation and deadline behavior of one loop.

## Sources

- `LoopHandle::cancel()` — explicit host cancel; resolves to
  `CancelReason::User` when it wins the ending race.
- Owner drop — dropping the `AgentLoop` cancels best-effort;
  `CancelReason::OwnerDropped`.
- `LoopOptions::deadline` — an absolute loop deadline;
  `CancelReason::Deadline` when it fires first. With no deadline, per-port
  timeouts govern without an artificial loop cap.
- Shutdown-style cancellation is represented through the same internal cancel
  token (`CancelReason::Shutdown` when requested through the control path).

## Semantics

- Cancel/finish uses a single-CAS `AtomicU8` state machine: `mark_cancel`
  and `finish_once` each perform one compare-and-swap, so cancel and report
  sealing cannot race or double-publish.
- A cancel that lands after the report is published returns `false` (the
  loop already ended); a completed loop that is later cancelled stays
  `Completed`.
- Cancelling drops unapplied pending steers: they never appear in the
  report. The report reflects only what actually ran.
- `wait`/`join` return the same `Arc<LoopReport>` regardless of who ended
  the loop; if the runner vanished before publishing, waiters see
  `LoopWaitError::CompletionClosed`.
- While waiting for user input, the loop deadline is a hard upper bound;
  when it wins, the loop ends as `Cancelled(Deadline)` through the same
  linearization as every other ending path.

## Best Effort

`cancel()` never blocks and never guarantees that any particular tool call
is interrupted before its side effect completes. The host must design tool
side effects to be safely interruptible or accepted even when a cancel is
in flight — the runtime makes no durability promise about interrupted tool
work.