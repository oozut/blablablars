//! Errors shared by the core, the CLI and the bindings.

use thiserror::Error;

/// Something went wrong *inside* an extension while handling a hook.
#[derive(Debug, Error)]
pub enum ExtensionError {
    #[error("extension `{extension}` failed on hook `{hook}`: {message}")]
    Failed {
        extension: String,
        hook: String,
        message: String,
    },
    #[error("invalid payload for hook `{hook}`: {message}")]
    InvalidPayload { hook: String, message: String },
}

impl ExtensionError {
    /// Convenience constructor used by extensions that do not implement `name()` cheaply.
    pub fn failed(
        extension: impl Into<String>,
        hook: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::Failed {
            extension: extension.into(),
            hook: hook.into(),
            message: message.into(),
        }
    }
}

/// Errors produced by the core engine itself.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("an extension named `{0}` is already registered")]
    DuplicateExtension(String),
    #[error("unknown hook `{0}` (known hooks: {1})")]
    UnknownHook(String, String),
    #[error("plugin protocol error: {0}")]
    Plugin(String),
    #[error(transparent)]
    Extension(#[from] ExtensionError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Alias used by callers that do not care about the distinction.
pub type Result<T> = std::result::Result<T, CoreError>;
