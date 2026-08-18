use std::panic::{AssertUnwindSafe, catch_unwind};

use futures_util::FutureExt;

use super::provider::ModelCallContext;
use super::registry::ProviderRegistry;
use super::types::{
    DeliveryState, ModelError, ModelErrorKind, ModelLimits, ModelRequest, ModelResponse,
    ModelSelection,
};

#[derive(Clone)]
pub struct ModelGateway {
    registry: ProviderRegistry,
}

impl ModelGateway {
    pub fn new(registry: ProviderRegistry) -> Self {
        super::transport::ensure_linked();
        Self { registry }
    }

    pub fn resolve(
        &self,
        selection: &ModelSelection,
    ) -> Result<super::registry::ResolvedModel, ModelError> {
        self.registry
            .resolve(selection)
            .ok_or(ModelError::Unavailable)
    }

    pub async fn generate(
        &self,
        request: ModelRequest,
        ctx: ModelCallContext,
    ) -> Result<ModelResponse, ModelError> {
        let selection = request.selection().clone();
        let Some(resolved) = self.registry.resolve(&selection) else {
            ctx.close();
            return Err(ModelError::Unavailable);
        };
        let descriptor = resolved.descriptor();
        if request.selection() != descriptor.selection()
            || !limits_fit(request.limits(), descriptor.limits())
            || !descriptor.supports_reasoning(request.reasoning())
        {
            ctx.close();
            return Err(ModelError::InvalidRequest);
        }
        if ctx.cancellation().is_cancelled() {
            ctx.close();
            return Err(ModelError::Cancelled);
        }

        // The typed error is redacted; the host process owns the panic hook and its output.
        let provider_future =
            match catch_unwind(AssertUnwindSafe(|| resolved.generate(request, ctx.clone()))) {
                Ok(provider_future) => provider_future,
                Err(_) => {
                    ctx.close();
                    return Err(ModelError::Internal);
                }
            };
        let mut provider_future = Box::pin(AssertUnwindSafe(provider_future).catch_unwind());
        let result = tokio::select! {
            biased;
            result = &mut provider_future => match result {
                Ok(result) => result,
                Err(_) => Err(ModelError::Internal),
            },
            _ = ctx.cancellation().cancelled() => Err(ModelError::detailed(
                ModelErrorKind::Cancelled,
                DeliveryState::Unknown,
                None,
            )
            .unwrap_or(ModelError::Internal)),
        };
        ctx.close();
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
