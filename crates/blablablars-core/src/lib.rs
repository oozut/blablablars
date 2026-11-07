//! # blablablars-core
//!
//! The application's brain, deliberately free of I/O and of any knowledge of
//! Python. Three things live here:
//!
//! * [`pipeline`] — the built-in capability: text in, [`Analysis`] out.
//! * [`extension`] — the hook vocabulary and the [`Registry`] that dispatches to
//!   Rust extensions and to anything else that can speak JSON.
//! * [`protocol`] and [`host`] — the wire format and the child-process adapter
//!   used to plug a Python plugin into the running application.
//!
//! The PyO3 crate (`blablablars-python`) and the CLI (`blablablars-cli`) are
//! thin layers on top of this crate, so behaviour cannot diverge between "the
//! Rust application" and "the Python plugin API".

pub mod error;
pub mod extension;
pub mod host;
pub mod loader;
pub mod pipeline;
pub mod plugin_abi;
pub mod protocol;

pub use error::{CoreError, ExtensionError, Result};
pub use extension::{
    is_known_hook, known_hooks, Extension, ExtensionInfo, Registry, HOOKS, HOOK_INPUT, HOOK_OUTPUT,
};
pub use host::ProcessExtension;
pub use loader::DynamicExtension;
pub use pipeline::{analyse, Analysis, Pipeline, Run};
pub use plugin_abi::ABI_VERSION;
pub use protocol::{
    PluginDescription, ProtocolError, Request, Response, METHOD_CALL, METHOD_DESCRIBE,
    METHOD_SHUTDOWN, PROTOCOL_VERSION,
};

/// The version of the core library, as declared in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
