use crate::model::Usage;

/// Accumulates `Usage` across the model requests of one loop.
#[derive(Default)]
pub(crate) struct UsageAccumulator {
    value: Usage,
}

impl UsageAccumulator {
    pub(crate) fn add(&mut self, value: Usage) -> Result<(), ()> {
        self.value = sum_usage(self.value, value)?;
        Ok(())
    }

    pub(crate) fn finish(self) -> Usage {
        self.value
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
        (left, right) => Ok(left.or(right)),
    }
}

#[cfg(test)]
#[test]
fn missing_fields_do_not_erase_reported_usage() {
    let mut usage = UsageAccumulator::default();
    usage.add(Usage::default()).unwrap();
    usage
        .add(Usage::from_optional(Some(2), None, None).with_cache_read_tokens(Some(3)))
        .unwrap();
    usage
        .add(Usage::from_optional(None, Some(4), None))
        .unwrap();
    usage
        .add(Usage::from_optional(Some(5), None, None))
        .unwrap();
    let total = usage.finish();
    assert_eq!(total.input_tokens(), Some(7));
    assert_eq!(total.output_tokens(), Some(4));
    assert_eq!(total.cache_read_tokens(), Some(3));
    assert_eq!(total.reasoning_tokens(), None);
    assert_eq!(total.cache_write_tokens(), None);
}
