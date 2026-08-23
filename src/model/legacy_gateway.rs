// P5/P6 deletion target: remove when the runner consumes Arc<dyn Model> directly.

use std::panic::{AssertUnwindSafe, catch_unwind};

use futures_util::FutureExt;

use super::legacy_provider::LegacyModelCallContext;
use super::legacy_registry::{LegacyProviderRegistry, LegacyResolvedModel};
use super::response::{DeliveryState, ModelError, ModelErrorKind};
use super::types::{LegacyModelSelection, ModelLimits, ModelRequest, ModelResponse};

#[derive(Clone)]
pub(crate) struct LegacyModelGateway {
    registry: LegacyProviderRegistry,
}

impl LegacyModelGateway {
    pub(crate) fn new(registry: LegacyProviderRegistry) -> Self {
        Self { registry }
    }

    pub(crate) fn resolve(
        &self,
        selection: &LegacyModelSelection,
    ) -> Result<LegacyResolvedModel, ModelError> {
        self.registry
            .resolve(selection)
            .ok_or(ModelError::Unavailable)
    }

    pub(crate) async fn generate(
        &self,
        selection: &LegacyModelSelection,
        request: ModelRequest,
        context: LegacyModelCallContext,
    ) -> Result<ModelResponse, ModelError> {
        let Some(resolved) = self.registry.resolve(selection) else {
            context.close();
            return Err(ModelError::Unavailable);
        };
        let descriptor = resolved.descriptor();
        if selection != descriptor.selection()
            || !limits_fit(request.limits(), descriptor.limits())
            || !descriptor.supports_reasoning(request.reasoning())
        {
            context.close();
            return Err(ModelError::InvalidRequest);
        }
        if context.cancellation().is_cancelled() {
            context.close();
            return Err(ModelError::Cancelled);
        }

        let provider_future = match catch_unwind(AssertUnwindSafe(|| {
            resolved.generate(request, context.clone())
        })) {
            Ok(provider_future) => provider_future,
            Err(_) => {
                context.close();
                return Err(ModelError::Panicked);
            }
        };
        let mut provider_future = Box::pin(AssertUnwindSafe(provider_future).catch_unwind());
        let result = tokio::select! {
            biased;
            result = &mut provider_future => match result {
                Ok(result) => result,
                Err(_) => Err(ModelError::Panicked),
            },
            _ = context.cancellation().cancelled() => Err(ModelError::detailed(
                ModelErrorKind::Cancelled,
                DeliveryState::Unknown,
                false,
                None,
            )
            .unwrap_or(ModelError::Internal)),
        };
        context.close();
        result
    }
}

fn limits_fit(requested: &ModelLimits, available: &ModelLimits) -> bool {
    requested
        .context_window_tokens()
        .zip(available.context_window_tokens())
        .is_none_or(|(requested, available)| requested <= available)
        && requested
            .max_output_tokens()
            .zip(available.max_output_tokens())
            .is_none_or(|(requested, available)| requested <= available)
}

const _: () = {
    // P5/P6 deletion target: remove when the runner consumes Arc<dyn Model> directly.
    let _ = LegacyModelGateway::new;
    let _ = LegacyModelGateway::resolve;
    let _ = LegacyModelGateway::generate;
};
