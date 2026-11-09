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
- `LoopHandle::shutdown()` (or `AgentLoop::shutdown()`) requests
  `CancelReason::Shutdown` through the same cancellation state machine.

## Semantics

- After cancellation returns, `update` and `answer` return `NotActive`,
  even before the runner publishes its report. Calls overlapping cancellation
  may already have been accepted; acceptance does not guarantee application.

- Cancellation, non-completed ending claims and final sealing compete in one
  `AtomicU8` state machine. The first claim wins; a known failure or deadline is
  claimed before awaited history saving, preventing later cancellation from
  replacing its primary reason. Claiming an ending does not publish completion;
  `is_finished` stays false and `wait`/`join` still wait for saving to settle.
- A no-tool completed candidate stays cancellable while saving is pending.
  A callback error or panic is retained separately in `history_failure`; it does
  not overwrite an earlier cancellation, failure or completed candidate.
- A cancel that lands after the report is published returns `false` (the
  loop already ended); a completed loop that is later cancelled stays
  `Completed`.
- Inputs the loop already took from `LoopInputs` are in the report for every
  ending, cancellation included; inputs it never took stay with the host. The
  report reflects only what actually ran.
- `wait`/`join` return the same `Arc<LoopReport>` regardless of who ended
  the loop; if the runner vanished before publishing, waiters see
  `LoopWaitError::CompletionClosed`.
- While waiting for user input, the loop deadline is a hard upper bound;
  when it wins, the loop ends as `Cancelled(Deadline)` through the same
  linearization as every other ending path.

- Cancellation does not drop an in-flight `LoopHistory` acknowledgement; the
  runner waits for owned saving before publishing its report. Deadlines stop
  subsequent execution but do not cut off that host-owned work.

## Best Effort

`cancel()` never blocks and never guarantees that any particular tool call
is interrupted before its side effect completes. The host must design tool
side effects to be safely interruptible or accepted even when a cancel is
in flight — the runtime makes no durability promise about interrupted tool
work.
Dropping the future returned by `join()` drops its owning `AgentLoop` and
requests `OwnerDropped` cancellation. Use `LoopHandle::wait()` for a cancellable
wait that does not own the loop. With no loop deadline an interaction waits
until answered or cancelled; it has no artificial 24-hour horizon. A deadline
ending is sealed by the runner, not written as a cancellation state marker;
an explicit cancellation that wins before the deadline ending is claimed
determines the reason.
