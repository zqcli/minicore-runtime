// P5/P6 deletion target: remove with legacy provider lookup.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use super::legacy_provider::{LegacyModelCallContext, LegacyModelFuture, LegacyModelProvider};
use super::response::ModelError;
use super::types::{LegacyModelDescriptor, LegacyModelSelection, LegacyProviderId};

struct RegisteredProvider {
    provider: Arc<dyn LegacyModelProvider>,
}

#[derive(Clone)]
pub(crate) struct LegacyResolvedModel {
    provider: Arc<dyn LegacyModelProvider>,
    descriptor: LegacyModelDescriptor,
}

impl LegacyResolvedModel {
    pub(crate) const fn descriptor(&self) -> &LegacyModelDescriptor {
        &self.descriptor
    }

    pub(super) fn generate(
        &self,
        request: super::types::ModelRequest,
        context: LegacyModelCallContext,
    ) -> LegacyModelFuture<'_> {
        self.provider.generate(request, context)
    }
}

impl fmt::Debug for LegacyResolvedModel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LegacyResolvedModel")
            .field("descriptor", &self.descriptor)
            .finish()
    }
}

#[derive(Clone, Default)]
pub(crate) struct LegacyProviderRegistry {
    providers: Arc<BTreeMap<LegacyProviderId, RegisteredProvider>>,
    models: Arc<BTreeMap<LegacyModelSelection, LegacyModelDescriptor>>,
}

#[derive(Default)]
pub(crate) struct LegacyProviderRegistryBuilder {
    providers: BTreeMap<LegacyProviderId, RegisteredProvider>,
    models: BTreeMap<LegacyModelSelection, LegacyModelDescriptor>,
}

impl LegacyProviderRegistry {
    pub(crate) fn builder() -> LegacyProviderRegistryBuilder {
        LegacyProviderRegistryBuilder::default()
    }

    pub(crate) fn resolve(&self, selection: &LegacyModelSelection) -> Option<LegacyResolvedModel> {
        let provider = self.providers.get(selection.provider_id())?;
        let descriptor = self.models.get(selection)?;
        Some(LegacyResolvedModel {
            provider: Arc::clone(&provider.provider),
            descriptor: descriptor.clone(),
        })
    }
}

impl LegacyProviderRegistryBuilder {
    pub(crate) fn register<T>(&mut self, provider: T) -> Result<(), ModelError>
    where
        T: LegacyModelProvider + 'static,
    {
        let provider: Arc<dyn LegacyModelProvider> = Arc::new(provider);
        let provider_id = catch_unwind(AssertUnwindSafe(|| provider.id().clone()))
            .map_err(|_| ModelError::Panicked)?;
        let descriptors = catch_unwind(AssertUnwindSafe(|| provider.models().to_vec()))
            .map_err(|_| ModelError::Panicked)?;
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

        self.providers.insert(
            provider_id,
            RegisteredProvider {
                provider: Arc::clone(&provider),
            },
        );
        for descriptor in descriptors {
            self.models
                .insert(descriptor.selection().clone(), descriptor);
        }
        Ok(())
    }

    pub(crate) fn build(self) -> LegacyProviderRegistry {
        LegacyProviderRegistry {
            providers: Arc::new(self.providers),
            models: Arc::new(self.models),
        }
    }
}

const _: () = {
    // P5/P6 deletion target: remove with legacy provider lookup.
    let _ = std::mem::size_of::<LegacyResolvedModel>();
    let _ = LegacyResolvedModel::descriptor;
    let _ = LegacyResolvedModel::generate;
    let _: fn() -> LegacyProviderRegistryBuilder = LegacyProviderRegistry::builder;
    let _ = LegacyProviderRegistry::resolve;
    let _: fn(
        &mut LegacyProviderRegistryBuilder,
        Arc<dyn LegacyModelProvider>,
    ) -> Result<(), ModelError> =
        LegacyProviderRegistryBuilder::register::<Arc<dyn LegacyModelProvider>>;
    let _: fn(LegacyProviderRegistryBuilder) -> LegacyProviderRegistry =
        LegacyProviderRegistryBuilder::build;
};
