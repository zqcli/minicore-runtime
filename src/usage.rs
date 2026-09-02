use crate::model::Usage;

/// Accumulates `Usage` across the model requests of one loop.
#[derive(Default)]
pub(crate) struct UsageAccumulator {
    value: Option<Usage>,
}

impl UsageAccumulator {
    pub(crate) fn add(&mut self, value: Usage) -> Result<(), ()> {
        self.value = Some(match self.value {
            Some(current) => sum_usage(current, value)?,
            None => value,
        });
        Ok(())
    }

    pub(crate) fn finish(self) -> Usage {
        self.value.unwrap_or_default()
    }
}

fn sum_usage(left: Usage, right: Usage) -> Result<Usage, ()> {
    Ok(Usage::from_optional(
        sum_field(left.input_tokens(), right.input_tokens())?,
        sum_field(left.output_tokens(), right.output_tokens())?,
        sum_field(left.reasoning_tokens(), right.reasoning_tokens())?,
    )
    .with_cache_read_tokens(sum_field(
        left.cache_read_tokens(),
        right.cache_read_tokens(),
    )?)
    .with_cache_write_tokens(sum_field(
        left.cache_write_tokens(),
        right.cache_write_tokens(),
    )?)
    .with_provider_total_tokens(sum_field(
        left.provider_total_tokens(),
        right.provider_total_tokens(),
    )?))
}

fn sum_field(left: Option<u64>, right: Option<u64>) -> Result<Option<u64>, ()> {
    match (left, right) {
        (Some(left), Some(right)) => left.checked_add(right).map(Some).ok_or(()),
        _ => Ok(None),
    }
}
