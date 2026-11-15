//! The analysis pipeline: the "core part" of the application.
//!
//! A run is deliberately small and total: text goes in, an [`Analysis`] comes
//! out. Extensions sit on either side of it through [`HOOK_INPUT`] and
//! [`HOOK_OUTPUT`], which is what Python plugins hook into.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

use crate::error::{CoreError, ExtensionError};
use crate::extension::{Extension, Registry, HOOK_INPUT, HOOK_OUTPUT};

/// What the core computes for one piece of text.
///
/// `text` is the only field an extension owns; the rest describe it. The core
/// derives them from `text` after the output extensions have run, so they always
/// agree with what is reported, whatever an extension returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Analysis {
    /// The text that was actually analysed (after input extensions ran, and after
    /// the output extensions had their say).
    pub text: String,
    /// Words in `text`, as the core counts them.
    #[serde(default)]
    pub words: usize,
    /// Characters in `text`, as the core counts them.
    #[serde(default)]
    pub characters: usize,
    /// Lowercase hex SHA-256 of `text`: the identity of what is reported.
    #[serde(default)]
    pub digest: String,
    /// Lowercase hex SHA-256 of the text as it entered the analysis stage, before
    /// the output extensions ran: the identity that survives a rewrite of `text`,
    /// for keying caches and records. Equal to `digest` when nothing rewrote it.
    #[serde(default)]
    pub input_digest: String,
}

/// One pipeline execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub analysis: Analysis,
    /// `extension: hook` entries in the order they ran; empty without extensions.
    pub trace: Vec<String>,
}

impl Run {
    /// The JSON shape shared by the CLI (`--json`) and the Python bindings.
    pub fn to_json(&self) -> Value {
        json!({ "analysis": self.analysis, "trace": self.trace })
    }
}

/// Text analysis with an extension registry attached.
#[derive(Default)]
pub struct Pipeline {
    registry: Registry,
}

impl Pipeline {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a pipeline from an already-populated registry.
    pub fn with_registry(registry: Registry) -> Self {
        Self { registry }
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Mutable access, used by the bindings to unregister extensions.
    pub fn registry_mut(&mut self) -> &mut Registry {
        &mut self.registry
    }

    pub fn register(&mut self, extension: std::sync::Arc<dyn Extension>) -> Result<(), CoreError> {
        self.registry.register(extension)
    }

    /// Run `text` through the input hook, the built-in analysis and the output hook.
    pub fn run(&self, text: &str) -> Result<Run, CoreError> {
        let mut trace = Vec::new();

        let mut payload = json!({ "text": text });
        if !self.registry.is_empty() {
            let dispatch = self.registry.dispatch(HOOK_INPUT, payload)?;
            payload = dispatch.payload;
            trace.extend(dispatch.trace);
        }

        let effective = payload.get("text").and_then(Value::as_str).ok_or_else(|| {
            ExtensionError::InvalidPayload {
                hook: HOOK_INPUT.to_string(),
                message: "expected an object with a string `text` field".to_string(),
            }
        })?;
        let analysed = analyse(effective);
        // Taken before the output extensions run, because that is the identity of
        // the text this run started from, and the one thing they cannot rewrite.
        let input_digest = analysed.digest.clone();

        let mut payload = json!({ "analysis": analysed });
        if !self.registry.is_empty() {
            let dispatch = self.registry.dispatch(HOOK_OUTPUT, payload)?;
            payload = dispatch.payload;
            trace.extend(dispatch.trace);
        }

        let returned: Analysis =
            serde_json::from_value(payload.get("analysis").cloned().ok_or_else(|| {
                ExtensionError::InvalidPayload {
                    hook: HOOK_OUTPUT.to_string(),
                    message: "the returned payload must contain an `analysis` object".to_string(),
                }
            })?)?;

        // The output extensions own `text` and nothing else: whatever they report
        // as `text` is analysed again here, so the counts and the digest describe
        // the text the caller is handed back. Without this a hook that redacts
        // "secret" reports sha256("secret") next to "[redacted]", which is a
        // fingerprint of the thing it just removed.
        let analysis = derived(&returned.text, input_digest);

        Ok(Run { analysis, trace })
    }
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("registry", &self.registry)
            .finish()
    }
}

/// The built-in stage. Pure, deterministic and dependency-free by design.
///
/// `digest` and `input_digest` are equal here: they part company only when an
/// output extension rewrites the text after this has run.
pub fn analyse(text: &str) -> Analysis {
    let digest = sha256(text);
    Analysis {
        text: text.to_string(),
        words: text.split_whitespace().count(),
        characters: text.chars().count(),
        digest: digest.clone(),
        input_digest: digest,
    }
}

/// The analysis of `text`, carrying `input_digest` as the identity of the text
/// that entered the stage.
fn derived(text: &str, input_digest: String) -> Analysis {
    Analysis {
        input_digest,
        ..analyse(text)
    }
}

fn sha256(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::Extension;
    use std::sync::Arc;

    struct Rewrite {
        name: &'static str,
        hooks: Vec<String>,
        from: &'static str,
        to: String,
    }

    impl Rewrite {
        fn new(name: &'static str, hook: &str, from: &'static str, to: &str) -> Arc<Self> {
            Arc::new(Self {
                name,
                hooks: vec![hook.to_string()],
                from,
                to: to.to_string(),
            })
        }
    }

    impl Extension for Rewrite {
        fn name(&self) -> &str {
            self.name
        }

        fn hooks(&self) -> &[String] {
            &self.hooks
        }

        fn call(&self, _hook: &str, payload: &Value) -> Result<Value, ExtensionError> {
            let mut next = payload.clone();
            let key = if next.get("text").is_some() {
                "text"
            } else {
                "analysis"
            };
            let serialised = next[key].to_string().replace(self.from, &self.to);
            next[key] = serde_json::from_str(&serialised)
                .map_err(|error| ExtensionError::failed(self.name, "rewrite", error.to_string()))?;
            Ok(next)
        }
    }

    #[test]
    fn analysis_counts_unicode_characters_not_bytes() {
        let analysis = analyse("héllo wörld");
        assert_eq!(analysis.characters, 11);
        assert_eq!(analysis.words, 2);
        assert_eq!(analysis.digest.len(), 64);
        assert_ne!(analysis.digest, analyse("hello world").digest);
    }

    #[test]
    fn a_plain_run_has_no_trace() {
        let run = Pipeline::new().run("one two").unwrap();
        assert!(run.trace.is_empty());
        assert_eq!(run.analysis.words, 2);
    }

    #[test]
    fn input_extensions_change_what_is_analysed() {
        let mut pipeline = Pipeline::new();
        pipeline
            .register(Rewrite::new("shout", HOOK_INPUT, "hi", "HELLO THERE"))
            .unwrap();

        let run = pipeline.run("hi").unwrap();

        assert_eq!(run.analysis.text, "HELLO THERE");
        assert_eq!(run.analysis.words, 2);
        assert_eq!(run.trace, vec!["shout: pipeline.input"]);
    }

    #[test]
    fn output_extensions_change_the_returned_analysis() {
        let mut pipeline = Pipeline::new();
        pipeline
            .register(Rewrite::new("redact", HOOK_OUTPUT, "secret", "xxxxxx"))
            .unwrap();

        let run = pipeline.run("a secret").unwrap();

        assert_eq!(run.analysis.text, "a xxxxxx");
        assert_eq!(run.trace, vec!["redact: pipeline.output"]);
    }

    /// An output extension that hands back an analysis object and nothing else.
    struct OnlyText(&'static str);

    impl Extension for OnlyText {
        fn name(&self) -> &str {
            "terse"
        }
        fn hooks(&self) -> &[String] {
            static HOOKS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            HOOKS.get_or_init(|| vec![HOOK_OUTPUT.to_string()])
        }
        fn call(&self, _hook: &str, _payload: &Value) -> Result<Value, ExtensionError> {
            Ok(json!({ "analysis": { "text": self.0 } }))
        }
    }

    #[test]
    fn the_report_never_disagrees_with_the_text_it_reports() {
        let mut pipeline = Pipeline::new();
        pipeline
            .register(Rewrite::new("redact", HOOK_OUTPUT, "secret", "[redacted]"))
            .unwrap();

        let run = pipeline.run("a secret").unwrap();

        // The counts and the digest describe what came back, not what went in.
        assert_eq!(run.analysis.text, "a [redacted]");
        assert_eq!(run.analysis.words, 2);
        assert_eq!(run.analysis.characters, 12);
        assert_eq!(run.analysis.digest, analyse("a [redacted]").digest);
        // ... and the identity of the input is still there to key on, so a hook
        // that redacts cannot pass off a fingerprint of the secret as the
        // reported text's identity.
        assert_eq!(run.analysis.input_digest, analyse("a secret").digest);
        assert_ne!(run.analysis.input_digest, run.analysis.digest);
    }

    #[test]
    fn a_run_nothing_rewrote_reports_one_identity() {
        let run = Pipeline::new().run("one two").unwrap();

        assert_eq!(run.analysis.digest, analyse("one two").digest);
        assert_eq!(run.analysis.input_digest, run.analysis.digest);
    }

    #[test]
    fn an_output_extension_may_return_a_bare_text_and_the_core_fills_in_the_rest() {
        let mut pipeline = Pipeline::new();
        pipeline
            .register(Arc::new(OnlyText("three whole words")))
            .unwrap();

        let run = pipeline.run("hi").unwrap();

        assert_eq!(run.analysis.text, "three whole words");
        assert_eq!(run.analysis.words, 3);
        assert_eq!(run.analysis.characters, 17);
        assert_eq!(run.analysis.digest, analyse("three whole words").digest);
        assert_eq!(run.analysis.input_digest, analyse("hi").digest);
    }

    #[test]
    fn an_input_extension_that_drops_the_text_is_an_error() {
        struct Breaker;
        impl Extension for Breaker {
            fn name(&self) -> &str {
                "breaker"
            }
            fn hooks(&self) -> &[String] {
                static HOOKS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
                HOOKS.get_or_init(|| vec![HOOK_INPUT.to_string()])
            }
            fn call(&self, _hook: &str, _payload: &Value) -> Result<Value, ExtensionError> {
                Ok(json!({ "nope": true }))
            }
        }

        let mut pipeline = Pipeline::new();
        pipeline.register(Arc::new(Breaker)).unwrap();
        let error = pipeline.run("hi").unwrap_err();
        assert!(matches!(
            error,
            CoreError::Extension(ExtensionError::InvalidPayload { .. })
        ));
    }

    #[test]
    fn run_to_json_matches_the_python_and_cli_shape() {
        let run = Pipeline::new().run("hi there").unwrap();
        let value = run.to_json();
        assert_eq!(value["analysis"]["words"], json!(2));
        assert_eq!(value["trace"], json!([]));
    }
}
