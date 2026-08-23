mod builder;

pub(crate) use builder::{KERNEL_INVARIANT, PromptBuilder, PromptError};

const _: () = {
    let _ = KERNEL_INVARIANT;
    let _ = std::mem::size_of::<PromptBuilder>();
    let _ = std::mem::size_of::<PromptError>();
    let _ = PromptBuilder::new;
    let _ = PromptBuilder::remaining_context_budget;
    let _ = PromptBuilder::build;
};
