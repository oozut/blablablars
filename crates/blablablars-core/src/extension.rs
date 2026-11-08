//! Extension points: the hook vocabulary and the registry that dispatches to it.
//!
//! Everything an extension can do flows through a JSON payload, so a Rust
//! extension (`impl Extension`) and a Python extension (a callback registered by
//! `blablablars._native`) are interchangeable to the pipeline.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{CoreError, ExtensionError};

/// Called with the incoming text before the core analyses it.
pub const HOOK_INPUT: &str = "pipeline.input";
/// Called with the produced analysis before it is returned.
pub const HOOK_OUTPUT: &str = "pipeline.output";
/// Called with `{"text": ...}` right after a model transcribes audio, before
/// the built-in cleanup. This is where a dictionary or a language fix belongs.
pub const HOOK_TRANSCRIPT_RAW: &str = "transcription.raw";
/// Called with `{"text": ...}` after cleanup, before the text is typed. This is
/// where a rewriter, a formatter or an LLM pass belongs.
pub const HOOK_TRANSCRIPT_TEXT: &str = "transcription.text";

/// Every hook the core knows about. Extensions may only subscribe to these.
pub const HOOKS: &[&str] = &[
    HOOK_INPUT,
    HOOK_OUTPUT,
    HOOK_TRANSCRIPT_RAW,
    HOOK_TRANSCRIPT_TEXT,
];

/// True when `hook` is part of the public hook vocabulary.
pub fn is_known_hook(hook: &str) -> bool {
    HOOKS.contains(&hook)
}

/// The pretty form of [`HOOKS`], used in error messages.
pub fn known_hooks() -> String {
    HOOKS.join(", ")
}

/// Implemented by anything that wants to take part in the pipeline.
///
/// `Send + Sync` keeps the registry usable from several threads and from the
/// Python bindings; implementations must not block for long.
pub trait Extension: Send + Sync {
    /// Stable identifier, unique inside one registry.
    fn name(&self) -> &str;

    /// Hooks this extension subscribes to. Validated on registration.
    fn hooks(&self) -> &[String];

    /// Handle one hook invocation and return the (possibly rewritten) payload.
    fn call(&self, hook: &str, payload: &Value) -> Result<Value, ExtensionError>;
}

/// What an extension looks like from the outside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionInfo {
    pub name: String,
    pub hooks: Vec<String>,
}

/// Result of dispatching one hook through the registry.
#[derive(Debug, Clone, PartialEq)]
pub struct Dispatch {
    /// Payload after every subscribing extension ran, in registration order.
    pub payload: Value,
    /// `extension: hook` entries, in the order they ran.
    pub trace: Vec<String>,
}

/// Ordered collection of extensions.
#[derive(Default)]
pub struct Registry {
    entries: Vec<Arc<dyn Extension>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Register an extension. Names must be unique and hooks must be known.
    pub fn register(&mut self, extension: Arc<dyn Extension>) -> Result<(), CoreError> {
        let name = extension.name().to_string();
        if self.entries.iter().any(|entry| entry.name() == name) {
            return Err(CoreError::DuplicateExtension(name));
        }
        for hook in extension.hooks() {
            if !is_known_hook(hook) {
                return Err(CoreError::UnknownHook(hook.clone(), known_hooks()));
            }
        }
        self.entries.push(extension);
        Ok(())
    }

    /// Remove an extension by name; returns whether something was removed.
    pub fn unregister(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.name() != name);
        self.entries.len() != before
    }

    /// Names in dispatch order.
    pub fn names(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.name().to_string()).collect()
    }

    /// Descriptions in dispatch order.
    pub fn describe(&self) -> Vec<ExtensionInfo> {
        self.entries
            .iter()
            .map(|e| ExtensionInfo {
                name: e.name().to_string(),
                hooks: e.hooks().to_vec(),
            })
            .collect()
    }

    /// Run `payload` through every extension subscribed to `hook`.
    ///
    /// Each extension sees the payload returned by the previous one, which is
    /// what makes a chain of small extensions composable.
    pub fn dispatch(&self, hook: &str, payload: Value) -> Result<Dispatch, CoreError> {
        if !is_known_hook(hook) {
            return Err(CoreError::UnknownHook(hook.to_string(), known_hooks()));
        }
        let mut current = payload;
        let mut trace = Vec::new();
        for extension in &self.entries {
            if extension.hooks().iter().any(|h| h == hook) {
                current = extension.call(hook, &current)?;
                trace.push(format!("{}: {hook}", extension.name()));
            }
        }
        Ok(Dispatch {
            payload: current,
            trace,
        })
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("extensions", &self.names())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    struct Recorder {
        name: String,
        hooks: Vec<String>,
        seen: Mutex<Vec<Value>>,
    }

    impl Extension for Recorder {
        fn name(&self) -> &str {
            &self.name
        }

        fn hooks(&self) -> &[String] {
            &self.hooks
        }

        fn call(&self, _hook: &str, payload: &Value) -> Result<Value, ExtensionError> {
            self.seen.lock().unwrap().push(payload.clone());
            let mut next = payload.clone();
            next["seen"] = json!(self.name);
            Ok(next)
        }
    }

    fn recorder(name: &str, hooks: &[&str]) -> Arc<Recorder> {
        Arc::new(Recorder {
            name: name.to_string(),
            hooks: hooks.iter().map(|h| h.to_string()).collect(),
            seen: Mutex::new(Vec::new()),
        })
    }

    #[test]
    fn dispatch_feeds_each_extension_the_previous_payload() {
        let mut registry = Registry::new();
        let first = recorder("first", &[HOOK_INPUT]);
        let second = recorder("second", &[HOOK_INPUT, HOOK_OUTPUT]);
        registry.register(first.clone()).unwrap();
        registry.register(second.clone()).unwrap();

        let dispatch = registry.dispatch(HOOK_INPUT, json!({"text": "a"})).unwrap();

        assert_eq!(dispatch.payload["seen"], json!("second"));
        assert_eq!(
            dispatch.trace,
            vec!["first: pipeline.input", "second: pipeline.input"]
        );
        assert_eq!(first.seen.lock().unwrap().len(), 1);
        assert_eq!(second.seen.lock().unwrap()[0]["seen"], json!("first"));
    }

    #[test]
    fn extensions_only_see_the_hooks_they_subscribe_to() {
        let mut registry = Registry::new();
        let input_only = recorder("input-only", &[HOOK_INPUT]);
        registry.register(input_only.clone()).unwrap();

        let dispatch = registry.dispatch(HOOK_OUTPUT, json!({})).unwrap();

        assert!(dispatch.trace.is_empty());
        assert!(input_only.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let mut registry = Registry::new();
        registry.register(recorder("one", &[HOOK_INPUT])).unwrap();
        let error = registry
            .register(recorder("one", &[HOOK_INPUT]))
            .unwrap_err();
        assert!(matches!(error, CoreError::DuplicateExtension(name) if name == "one"));
    }

    #[test]
    fn unknown_hooks_are_rejected_on_registration_and_dispatch() {
        let mut registry = Registry::new();
        let error = registry
            .register(recorder("bad", &["pipeline.nope"]))
            .unwrap_err();
        assert!(matches!(error, CoreError::UnknownHook(hook, _) if hook == "pipeline.nope"));

        let error = registry.dispatch("pipeline.nope", json!({})).unwrap_err();
        assert!(matches!(error, CoreError::UnknownHook(hook, _) if hook == "pipeline.nope"));
    }

    #[test]
    fn unregister_removes_only_the_named_extension() {
        let mut registry = Registry::new();
        registry.register(recorder("keep", &[HOOK_INPUT])).unwrap();
        registry.register(recorder("drop", &[HOOK_INPUT])).unwrap();

        assert!(registry.unregister("drop"));
        assert!(!registry.unregister("drop"));
        assert_eq!(registry.names(), vec!["keep"]);
    }
}
