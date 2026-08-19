use super::*;
use crate::model_v2::Usage;

struct UsageField {
    sum: Option<u64>,
    valid: bool,
    seen: bool,
}

impl Default for UsageField {
    fn default() -> Self {
        Self {
            sum: None,
            valid: true,
            seen: false,
        }
    }
}

impl UsageField {
    fn add(&mut self, value: Option<u64>) {
        self.seen = true;
        if !self.valid {
            return;
        }
        let Some(value) = value else {
            self.valid = false;
            self.sum = None;
            return;
        };
        let sum = self.sum.unwrap_or(0).checked_add(value);
        self.sum = sum;
        if sum.is_none() {
            self.valid = false;
        }
    }

    fn finish(&self) -> Option<u64> {
        if self.seen && self.valid {
            self.sum
        } else {
            None
        }
    }
}

#[derive(Default)]
struct UsageAccumulator {
    input_tokens: UsageField,
    output_tokens: UsageField,
    reasoning_tokens: UsageField,
    cache_read_tokens: UsageField,
    cache_write_tokens: UsageField,
    provider_total_tokens: UsageField,
}

impl UsageAccumulator {
    fn add(&mut self, usage: Option<&Usage>) {
        let Some(usage) = usage else {
            self.input_tokens.add(None);
            self.output_tokens.add(None);
            self.reasoning_tokens.add(None);
            self.cache_read_tokens.add(None);
            self.cache_write_tokens.add(None);
            self.provider_total_tokens.add(None);
            return;
        };
        self.input_tokens.add(usage.input_tokens());
        self.output_tokens.add(usage.output_tokens());
        self.reasoning_tokens.add(usage.reasoning_tokens());
        self.cache_read_tokens.add(usage.cache_read_tokens());
        self.cache_write_tokens.add(usage.cache_write_tokens());
        self.provider_total_tokens
            .add(usage.provider_total_tokens());
    }

    fn finish(self) -> Usage {
        Usage::from_optional(
            self.input_tokens.finish(),
            self.output_tokens.finish(),
            self.reasoning_tokens.finish(),
        )
        .with_cache_read_tokens(self.cache_read_tokens.finish())
        .with_cache_write_tokens(self.cache_write_tokens.finish())
        .with_provider_total_tokens(self.provider_total_tokens.finish())
    }
}

impl ConversationLog {
    pub(crate) async fn usage(&self) -> Usage {
        let state = read_lock(&self.inner.state);
        let mut aggregate = UsageAccumulator::default();
        for entry in &state.entries {
            if let ConversationEntry::Assistant { usage, .. } = entry.as_ref() {
                aggregate.add(usage.as_ref());
            }
        }
        aggregate.finish()
    }
}
