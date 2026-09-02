use std::time::{Duration, Instant};

use tokio::time::Instant as TokioInstant;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeadlineSource {
    Turn,
    Port,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EffectiveDeadline {
    standard: Instant,
    tokio: TokioInstant,
    source: DeadlineSource,
}

impl EffectiveDeadline {
    pub(crate) const fn standard(self) -> Instant {
        self.standard
    }

    pub(crate) const fn tokio(self) -> TokioInstant {
        self.tokio
    }

    pub(crate) const fn source(self) -> DeadlineSource {
        self.source
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DeadlineOverflow;

pub(crate) fn effective_deadline(
    turn_deadline: Instant,
    port_timeout: Duration,
) -> Result<EffectiveDeadline, DeadlineOverflow> {
    effective_deadline_from(TokioInstant::now(), turn_deadline, port_timeout)
}

fn effective_deadline_from(
    now: TokioInstant,
    turn_deadline: Instant,
    port_timeout: Duration,
) -> Result<EffectiveDeadline, DeadlineOverflow> {
    let port_deadline = now.checked_add(port_timeout).ok_or(DeadlineOverflow)?;
    let turn_deadline = TokioInstant::from_std(turn_deadline);
    let (tokio, source) = if turn_deadline <= port_deadline {
        (turn_deadline, DeadlineSource::Turn)
    } else {
        (port_deadline, DeadlineSource::Port)
    };
    Ok(EffectiveDeadline {
        standard: tokio.into_std(),
        tokio,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_deadline_selects_an_earlier_turn() {
        let standard_now = Instant::now();
        let now = TokioInstant::from_std(standard_now);
        let turn = standard_now + Duration::from_secs(2);
        let selected = effective_deadline_from(now, turn, Duration::from_secs(5)).unwrap();
        assert_eq!(selected.standard(), turn);
        assert_eq!(selected.tokio(), TokioInstant::from_std(turn));
        assert_eq!(selected.source(), DeadlineSource::Turn);
    }

    #[test]
    fn effective_deadline_selects_an_earlier_port_timeout() {
        let standard_now = Instant::now();
        let now = TokioInstant::from_std(standard_now);
        let selected = effective_deadline_from(
            now,
            standard_now + Duration::from_secs(5),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(selected.standard(), standard_now + Duration::from_secs(2));
        assert_eq!(selected.source(), DeadlineSource::Port);
    }

    #[test]
    fn equal_deadlines_choose_turn_conservatively() {
        let standard_now = Instant::now();
        let now = TokioInstant::from_std(standard_now);
        let turn = standard_now + Duration::from_secs(3);
        let selected = effective_deadline_from(now, turn, Duration::from_secs(3)).unwrap();
        assert_eq!(selected.standard(), turn);
        assert_eq!(selected.source(), DeadlineSource::Turn);
    }

    #[test]
    fn deadline_overflow_is_checked() {
        let standard_now = Instant::now();
        let now = TokioInstant::from_std(standard_now);
        assert_eq!(
            effective_deadline_from(now, standard_now, Duration::MAX),
            Err(DeadlineOverflow)
        );
    }
}
