//! A Rust extension, and what an extension looks like from the core's side.
//!
//! Run with `cargo run -p blablablars-core --example uppercase`.

use std::sync::Arc;

use blablablars_core::{Extension, ExtensionError, Pipeline, HOOK_INPUT, HOOK_OUTPUT};
use serde_json::Value;

struct Uppercase {
    hooks: Vec<String>,
}

impl Uppercase {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            hooks: vec![HOOK_INPUT.to_string(), HOOK_OUTPUT.to_string()],
        })
    }
}

impl Extension for Uppercase {
    fn name(&self) -> &str {
        "uppercase"
    }

    fn hooks(&self) -> &[String] {
        &self.hooks
    }

    fn call(&self, hook: &str, payload: &Value) -> Result<Value, ExtensionError> {
        let mut next = payload.clone();
        if hook == HOOK_INPUT {
            let text = next
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| ExtensionError::InvalidPayload {
                    hook: hook.to_string(),
                    message: "missing `text`".to_string(),
                })?
                .to_uppercase();
            next["text"] = Value::String(text);
        } else {
            let analysis = next
                .get_mut("analysis")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| ExtensionError::InvalidPayload {
                    hook: hook.to_string(),
                    message: "missing `analysis`".to_string(),
                })?;
            let text = analysis
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_uppercase();
            analysis.insert("text".to_string(), Value::String(text));
        }
        Ok(next)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut pipeline = Pipeline::new();
    pipeline.register(Uppercase::new())?;

    let run = pipeline.run("hello brave world")?;
    println!("{}", serde_json::to_string_pretty(&run.to_json())?);
    Ok(())
}
