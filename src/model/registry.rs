use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use super::provider::{ModelCallContext, ModelFuture, ModelProvider};
use super::types::{ModelDescriptor, ModelError, ModelSelection, ProviderId};

struct RegisteredProvider {
    provider: Arc<dyn ModelProvider>,
}

#[derive(Clone)]
pub struct ResolvedModel {
    provider: Arc<dyn ModelProvider>,
    descriptor: ModelDescriptor,
}

impl ResolvedModel {
    pub const fn descriptor(&self) -> &ModelDescriptor {
        &self.descriptor
    }

    pub(super) fn generate(
        &self,
        request: super::types::ModelRequest,
        ctx: ModelCallContext,
    ) -> ModelFuture<'_> {
        self.provider.generate(request, ctx)
    }
}

impl fmt::Debug for ResolvedModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedModel")
            .field("descriptor", &self.descriptor)
            .finish()
    }
}

#[derive(Clone, Default)]
pub struct ProviderRegistry {
    providers: Arc<BTreeMap<ProviderId, RegisteredProvider>>,
    models: Arc<BTreeMap<ModelSelection, ModelDescriptor>>,
}

#[derive(Default)]
pub struct ProviderRegistryBuilder {
    providers: BTreeMap<ProviderId, RegisteredProvider>,
    models: BTreeMap<ModelSelection, ModelDescriptor>,
}

impl ProviderRegistry {
    pub fn builder() -> ProviderRegistryBuilder {
        ProviderRegistryBuilder::default()
    }

    pub fn resolve(&self, selection: &ModelSelection) -> Option<ResolvedModel> {
        let provider = self.providers.get(selection.provider_id())?;
        let descriptor = self.models.get(selection)?;
        Some(ResolvedModel {
            provider: Arc::clone(&provider.provider),
            descriptor: descriptor.clone(),
        })
    }
}

impl ProviderRegistryBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<T>(&mut self, provider: T) -> Result<(), ModelError>
    where
        T: ModelProvider + 'static,
    {
        let provider: Arc<dyn ModelProvider> = Arc::new(provider);
        let provider_id = catch_unwind(AssertUnwindSafe(|| provider.id().clone()))
            .map_err(|_| ModelError::Internal)?;
        let descriptors = catch_unwind(AssertUnwindSafe(|| provider.models().to_vec()))
            .map_err(|_| ModelError::Internal)?;
        if descriptors.is_empty() || self.providers.contains_key(&provider_id) {
            return Err(ModelError::InvalidRequest);
        }

        let mut selections = BTreeSet::new();
        for descriptor in &descriptors {
            if descriptor.api_model_name().is_empty()
                || descriptor.selection().provider_id() != &provider_id
                || !selections.insert(descriptor.selection().clone())
                || self.models.contains_key(descriptor.selection())
            {
                return Err(ModelError::InvalidRequest);
            }
        }

        let registered = RegisteredProvider {
            provider: Arc::clone(&provider),
        };
        self.providers.insert(provider_id, registered);
        for descriptor in descriptors {
            self.models
                .insert(descriptor.selection().clone(), descriptor);
        }
        Ok(())
    }

    pub fn build(self) -> ProviderRegistry {
        ProviderRegistry {
            providers: Arc::new(self.providers),
            models: Arc::new(self.models),
        }
    }
}
