//! Runs Python extensions **inside the application process**.
//!
//! The shipped binary embeds CPython, imports the `blablablars` package once and
//! adapts an imported plugin to the core [`Extension`] trait. From the pipeline's
//! point of view this is indistinguishable from a Rust extension or a child
//! process: same hooks, same JSON payloads.
//!
//! ```no_run
//! # fn main() -> Result<(), blablablars_core::CoreError> {
//! let mut pipeline = blablablars_core::Pipeline::new();
//! blablablars_pythonhost::initialize(&[])?;
//! pipeline.register(blablablars_pythonhost::load_file("my_plugin.py")?)?;
//! # Ok(())
//! # }
//! ```
//!
//! Requirements: the interpreter must be able to `import blablablars`. Point it
//! at the package with `initialize(&[path])` (the CLI's `--python-path` /
//! `BLABLABLARS_PYTHON_PATH`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use blablablars_core::{CoreError, Extension, ExtensionError};
use pyo3::prelude::*;
use serde_json::Value;

/// Start the embedded interpreter, adding `search_paths` to `sys.path`.
///
/// Idempotent: only the first call has an effect, so later paths are ignored.
/// Fails if `import blablablars` does not work — better at startup than on the
/// first hook.
pub fn initialize(search_paths: &[PathBuf]) -> Result<(), CoreError> {
    static STATE: OnceLock<Result<(), String>> = OnceLock::new();
    let state = STATE.get_or_init(|| start(search_paths));
    state.clone().map_err(CoreError::Plugin)
}

fn start(search_paths: &[PathBuf]) -> Result<(), String> {
    Python::initialize();

    Python::attach(|py| -> PyResult<()> {
        let sys = py.import("sys")?;
        let path = sys.getattr("path")?;
        for entry in search_paths.iter().rev() {
            path.call_method1("insert", (0, entry.to_string_lossy().to_string()))?;
        }
        // Fail with a clear message instead of at first hook invocation.
        py.import("blablablars.plugin")?;
        Ok(())
    })
    .map_err(|error| {
        let message = error.to_string();
        let hint = if message.contains("No module named 'blablablars'") {
            " (install the package, or pass --python-path pointing at python/)"
        } else {
            ""
        };
        format!("could not start the embedded Python interpreter: {message}{hint}")
    })
}

/// Load one Python plugin file and adapt it to the core.
pub fn load_file(path: impl AsRef<Path>) -> Result<Arc<dyn Extension>, CoreError> {
    let path = path.as_ref().to_path_buf();
    initialize(&[])?;
    let name_hint = path.display().to_string();
    attach(move |py| {
        let plugins = py.import("blablablars.plugins")?;
        let module =
            plugins.call_method1("load_plugin_file", (path.to_string_lossy().to_string(),))?;
        build_extension(py, &module)
    })
    .map_err(|error| plugin_error("python plugin", &name_hint, error))
}

/// Import an already-installed module and adapt it to the core.
pub fn load_module(module: &str) -> Result<Arc<dyn Extension>, CoreError> {
    initialize(&[])?;
    let module = module.to_string();
    let hint = module.clone();
    attach(move |py| {
        let imported = py.import(&module)?;
        build_extension(py, &imported)
    })
    .map_err(|error| plugin_error("python module", &hint, error))
}

/// Load every plugin `blablablars.plugins.load_plugins()` finds (entry points
/// plus `$BLABLABLARS_PLUGINS`).
pub fn load_discovered() -> Result<Vec<Arc<dyn Extension>>, CoreError> {
    initialize(&[])?;
    attach(|py| {
        let plugins = py.import("blablablars.plugins")?;
        let found = plugins.call_method0("load_plugins")?;
        let mut loaded = Vec::new();
        for item in found.try_iter()? {
            loaded.push(build_extension(py, &item?)?);
        }
        Ok(loaded)
    })
    .map_err(|error| plugin_error("python discovery", "blablablars.plugins", error))
}

/// Is a value a `.py` file rather than a module name?
pub fn looks_like_a_file(value: &str) -> bool {
    value.ends_with(".py") || value.contains(std::path::MAIN_SEPARATOR)
}

/// Load either a file or an installed module, depending on `value`.
pub fn load_path_or_module(value: &str) -> Result<Arc<dyn Extension>, CoreError> {
    if looks_like_a_file(value) {
        load_file(value)
    } else {
        load_module(value)
    }
}

fn attach<T>(f: impl for<'py> FnOnce(Python<'py>) -> PyResult<T>) -> PyResult<T> {
    Python::attach(f)
}

fn plugin_error(kind: &str, subject: &str, error: PyErr) -> CoreError {
    let message = Python::attach(|py| {
        error
            .value(py)
            .str()
            .map(|text| text.to_string_lossy().into_owned())
            .unwrap_or_else(|_| error.to_string())
    });
    CoreError::Plugin(format!("could not load {kind} `{subject}`: {message}"))
}

/// A Python plugin object, called through the GIL.
struct EmbeddedExtension {
    name: String,
    hooks: Vec<String>,
    plugin: Py<PyAny>,
    json: Py<PyAny>,
}

impl Extension for EmbeddedExtension {
    fn name(&self) -> &str {
        &self.name
    }

    fn hooks(&self) -> &[String] {
        &self.hooks
    }

    fn call(&self, hook: &str, payload: &Value) -> Result<Value, ExtensionError> {
        let payload_json = serde_json::to_string(payload)
            .map_err(|error| ExtensionError::failed(&self.name, hook, error.to_string()))?;

        Python::attach(|py| {
            let loads = self
                .json
                .bind(py)
                .getattr("loads")
                .map_err(|error| failed(&self.name, hook, error))?;
            let py_payload = loads
                .call1((payload_json.clone(),))
                .map_err(|error| failed(&self.name, hook, error))?;

            let result = self
                .plugin
                .bind(py)
                .call_method1("call", (hook, py_payload))
                .map_err(|error| failed(&self.name, hook, error))?;

            let encoded = if result.is_none() {
                payload_json
            } else {
                let dumps = self
                    .json
                    .bind(py)
                    .getattr("dumps")
                    .map_err(|error| failed(&self.name, hook, error))?;
                dumps
                    .call1((result,))
                    .and_then(|value| value.extract::<String>())
                    .map_err(|error| failed(&self.name, hook, error))?
            };

            serde_json::from_str(&encoded).map_err(|error| {
                ExtensionError::failed(
                    &self.name,
                    hook,
                    format!("the plugin returned invalid JSON: {error}"),
                )
            })
        })
    }
}

impl std::fmt::Debug for EmbeddedExtension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedExtension")
            .field("name", &self.name)
            .field("hooks", &self.hooks)
            .finish_non_exhaustive()
    }
}

fn failed(name: &str, hook: &str, error: PyErr) -> ExtensionError {
    ExtensionError::failed(
        name,
        hook,
        Python::attach(|py| {
            error
                .value(py)
                .str()
                .map(|text| text.to_string_lossy().into_owned())
                .unwrap_or_else(|_| error.to_string())
        }),
    )
}

fn build_extension(py: Python<'_>, candidate: &Bound<'_, PyAny>) -> PyResult<Arc<dyn Extension>> {
    let plugin_api = py.import("blablablars.plugin")?;
    let plugin = plugin_api.call_method1("coerce_plugin", (candidate,))?;

    let runtime = py.import("blablablars.runtime")?;
    let description = runtime.call_method1("describe", (&plugin,))?;
    let name: String = description.get_item("name")?.extract()?;
    let hooks: Vec<String> = description.get_item("hooks")?.extract()?;

    let json = py.import("json")?;

    Ok(Arc::new(EmbeddedExtension {
        name,
        hooks,
        plugin: plugin.unbind(),
        json: json.unbind().into(),
    }))
}
