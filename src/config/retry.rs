use std::time::Duration;

use thiserror::Error;

const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RetryPolicyError {
    #[error("retry attempt count is outside its bound")]
    InvalidAttempts,
    #[error("retry delay exceeds its bound")]
    DelayTooLong,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    max_attempts: u8,
    base_delay: Duration,
}

impl RetryPolicy {
    pub fn new(max_attempts: u8, base_delay: Duration) -> Result<Self, RetryPolicyError> {
        if !(1..=4).contains(&max_attempts) {
            return Err(RetryPolicyError::InvalidAttempts);
        }
        if base_delay > MAX_RETRY_DELAY {
            return Err(RetryPolicyError::DelayTooLong);
        }
        Ok(Self {
            max_attempts,
            base_delay,
        })
    }

    pub const fn max_attempts(&self) -> u8 {
        self.max_attempts
    }

    pub const fn base_delay(&self) -> Duration {
        self.base_delay
    }

    pub fn delay_for_retry(
        &self,
        retry_index: u8,
        retry_after: Option<Duration>,
    ) -> Option<Duration> {
        let mut exponential = self.base_delay;
        for _ in 0..retry_index {
            exponential = exponential
                .checked_mul(2)
                .unwrap_or(MAX_RETRY_DELAY)
                .min(MAX_RETRY_DELAY);
            if exponential == MAX_RETRY_DELAY {
                break;
            }
        }
        match retry_after {
            Some(value) if value > MAX_RETRY_DELAY => None,
            Some(value) => Some(value.max(exponential)),
            None => Some(exponential),
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(250),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{RetryPolicy, RetryPolicyError};

    #[test]
    fn retry_policy_is_checked_and_exponential_with_a_cap() {
        assert_eq!(
            RetryPolicy::new(0, Duration::ZERO),
            Err(RetryPolicyError::InvalidAttempts)
        );
        assert_eq!(
            RetryPolicy::new(5, Duration::ZERO),
            Err(RetryPolicyError::InvalidAttempts)
        );
        assert_eq!(
            RetryPolicy::new(1, Duration::from_secs(31)),
            Err(RetryPolicyError::DelayTooLong)
        );
        let policy = RetryPolicy::new(4, Duration::from_secs(10)).unwrap();
        assert_eq!(
            policy.delay_for_retry(0, None),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            policy.delay_for_retry(1, None),
            Some(Duration::from_secs(20))
        );
        assert_eq!(
            policy.delay_for_retry(2, None),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            policy.delay_for_retry(3, None),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            policy.delay_for_retry(0, Some(Duration::from_secs(29))),
            Some(Duration::from_secs(29))
        );
        assert_eq!(
            policy.delay_for_retry(0, Some(Duration::from_secs(31))),
            None
        );
    }
}
