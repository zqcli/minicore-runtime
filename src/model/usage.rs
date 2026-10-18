use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_total_tokens: Option<u64>,
}

impl Usage {
    pub const fn new(input_tokens: u64, output_tokens: u64, reasoning_tokens: u64) -> Self {
        Self::from_optional(
            Some(input_tokens),
            Some(output_tokens),
            Some(reasoning_tokens),
        )
    }

    pub const fn from_optional(
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        reasoning_tokens: Option<u64>,
    ) -> Self {
        Self {
            input_tokens,
            output_tokens,
            reasoning_tokens,
            cache_read_tokens: None,
            cache_write_tokens: None,
            provider_total_tokens: None,
        }
    }

    pub const fn with_cache_read_tokens(mut self, value: Option<u64>) -> Self {
        self.cache_read_tokens = value;
        self
    }

    pub const fn with_cache_write_tokens(mut self, value: Option<u64>) -> Self {
        self.cache_write_tokens = value;
        self
    }

    pub const fn with_provider_total_tokens(mut self, value: Option<u64>) -> Self {
        self.provider_total_tokens = value;
        self
    }

    pub const fn input_tokens(&self) -> Option<u64> {
        self.input_tokens
    }

    pub const fn output_tokens(&self) -> Option<u64> {
        self.output_tokens
    }

    pub const fn reasoning_tokens(&self) -> Option<u64> {
        self.reasoning_tokens
    }

    pub const fn cache_read_tokens(&self) -> Option<u64> {
        self.cache_read_tokens
    }

    pub const fn cache_write_tokens(&self) -> Option<u64> {
        self.cache_write_tokens
    }

    pub const fn provider_total_tokens(&self) -> Option<u64> {
        self.provider_total_tokens
    }

    /// Sums only when all three core token counts are reported and the sum fits.
    pub const fn total_tokens(&self) -> Option<u64> {
        let (Some(input), Some(output), Some(reasoning)) =
            (self.input_tokens, self.output_tokens, self.reasoning_tokens)
        else {
            return None;
        };
        let Some(total) = input.checked_add(output) else {
            return None;
        };
        total.checked_add(reasoning)
    }
}
