//! Provider-agnostic AI Copilot supporting local LLMs (Ollama) and BYOK cloud APIs.

pub mod client;
pub mod redact;
pub mod traits;

pub use client::{ApiKeyLookup, DefaultApiKeyLookup, HttpAiCopilotProvider};
pub use redact::{
    FilteredDiff, MaskedText, REDACTED, filter_sensitive_diff, is_sensitive_path, mask_secrets,
    sanitize_diff,
};
pub use traits::{
    AiCopilotError, AiCopilotProvider, AiCopilotResult, AiEndpointConfig, AiProviderKind,
    CommitMessageSuggestion, ConflictResolutionAdvice, RangeSummaryRequest,
};
