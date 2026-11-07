//! PyO3 bindings: the Rust core, importable from Python.
//!
//! The module is published as `blablablars._native`; the Python package wraps it
//! in a friendlier API (`blablablars.Pipeline`, plugin decorators, discovery).
//! Payloads cross this boundary as JSON strings on purpose — that is the exact
//! shape used by the plugin process protocol, so an in-process plugin and a
//! out-of-process plugin cannot drift apart.

use std::sync::Arc;

use blablablars_core::{
    self as core, Extension as CoreExtension, ExtensionError as CoreExtensionError,
    Pipeline as CorePipeline, VERSION,
};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use serde_json::Value;

fn runtime_error(error: core::CoreError) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

/// The result of one built-in analysis.
#[pyclass(module = "blablablars._native", name = "Analysis", frozen)]
#[derive(Clone)]
pub struct Analysis {
    text: String,
    words: usize,
    characters: usize,
    digest: String,
}

#[pymethods]
impl Analysis {
    #[getter]
    fn text(&self) -> &str {
        &self.text
    }

    #[getter]
    fn words(&self) -> usize {
        self.words
    }

    #[getter]
    fn characters(&self) -> usize {
        self.characters
    }

    #[getter]
    fn digest(&self) -> &str {
        &self.digest
    }

    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        dict.set_item("text", &self.text)?;
        dict.set_item("words", self.words)?;
        dict.set_item("characters", self.characters)?;
        dict.set_item("digest", &self.digest)?;
        Ok(dict)
    }

    fn __repr__(&self) -> String {
        format!(
            "Analysis(text={:?}, words={}, characters={}, digest={:?})",
            self.text, self.words, self.characters, self.digest
        )
    }

    fn __eq__(&self, other: &Analysis) -> bool {
        self.text == other.text
            && self.words == other.words
            && self.characters == other.characters
            && self.digest == other.digest
    }
}

/// One pipeline execution: the analysis plus the extensions that ran.
#[pyclass(module = "blablablars._native", name = "Run", frozen)]
#[derive(Clone)]
pub struct Run {
    analysis: Analysis,
    trace: Vec<String>,
}

#[pymethods]
impl Run {
    #[getter]
    fn analysis(&self) -> Analysis {
        self.analysis.clone()
    }

    #[getter]
    fn trace(&self) -> Vec<String> {
        self.trace.clone()
    }

    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        dict.set_item("analysis", self.analysis.to_dict(py)?)?;
        dict.set_item("trace", self.trace.clone())?;
        Ok(dict)
    }

    fn __repr__(&self) -> String {
        format!(
            "Run(analysis={}, trace={:?})",
            self.analysis.__repr__(),
            self.trace
        )
    }
}

/// Adapts a Python callable to the core [`CoreExtension`] trait.
struct CallbackExtension {
    name: String,
    hooks: Vec<String>,
    callback: Py<PyAny>,
}

impl CoreExtension for CallbackExtension {
    fn name(&self) -> &str {
        &self.name
    }

    fn hooks(&self) -> &[String] {
        &self.hooks
    }

    fn call(&self, hook: &str, payload: &Value) -> Result<Value, CoreExtensionError> {
        Python::attach(|py| {
            let payload_json = serde_json::to_string(payload)
                .map_err(|error| CoreExtensionError::failed(&self.name, hook, error.to_string()))?;

            let result = self
                .callback
                .call1(py, (hook, payload_json))
                .map_err(|error| {
                    CoreExtensionError::failed(&self.name, hook, format_py_error(py, &error))
                })?;

            let returned: String = result.extract(py).map_err(|error| {
                CoreExtensionError::failed(
                    &self.name,
                    hook,
                    format!("the callback must return a JSON string: {error}"),
                )
            })?;

            serde_json::from_str(&returned).map_err(|error| {
                CoreExtensionError::failed(
                    &self.name,
                    hook,
                    format!("the callback returned invalid JSON: {error}"),
                )
            })
        })
    }
}

fn format_py_error(py: Python<'_>, error: &PyErr) -> String {
    error
        .value(py)
        .str()
        .map(|text| text.to_string_lossy().into_owned())
        .unwrap_or_else(|_| error.to_string())
}

/// The text pipeline with an extension registry attached.
#[pyclass(module = "blablablars._native", name = "Pipeline")]
#[derive(Default)]
pub struct Pipeline {
    inner: CorePipeline,
}

#[pymethods]
impl Pipeline {
    #[new]
    fn new() -> Self {
        Self::default()
    }

    /// Register a callback for one or more hooks.
    ///
    /// The callback receives `(hook_name, payload_json)` and must return the new
    /// payload as a JSON string.
    fn register(&mut self, name: String, hooks: Vec<String>, callback: Py<PyAny>) -> PyResult<()> {
        if name.is_empty() {
            return Err(PyValueError::new_err("extension name must not be empty"));
        }
        if hooks.is_empty() {
            return Err(PyValueError::new_err(
                "an extension must subscribe to at least one hook",
            ));
        }
        Python::attach(|py| {
            if !callback.bind(py).is_callable() {
                return Err(PyValueError::new_err("callback must be callable"));
            }
            Ok(())
        })?;

        self.inner
            .register(Arc::new(CallbackExtension {
                name,
                hooks,
                callback,
            }))
            .map_err(runtime_error)
    }

    /// Remove an extension by name. Returns whether one was removed.
    fn unregister(&mut self, name: &str) -> bool {
        self.inner.registry_mut().unregister(name)
    }

    /// Extension names in dispatch order.
    fn extensions(&self) -> Vec<String> {
        self.inner.registry().names()
    }

    /// `(name, hooks)` pairs in dispatch order.
    fn describe<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let pairs = PyList::empty(py);
        for info in self.inner.registry().describe() {
            pairs.append((info.name, info.hooks))?;
        }
        Ok(pairs)
    }

    /// Hook names an extension may subscribe to.
    #[staticmethod]
    fn hooks() -> Vec<String> {
        core::HOOKS.iter().map(|hook| hook.to_string()).collect()
    }

    /// Run the pipeline over `text`.
    fn run_text(&self, text: &str) -> PyResult<Run> {
        let run = self.inner.run(text).map_err(runtime_error)?;
        Ok(Run {
            analysis: Analysis {
                text: run.analysis.text,
                words: run.analysis.words,
                characters: run.analysis.characters,
                digest: run.analysis.digest,
            },
            trace: run.trace,
        })
    }

    /// Run the pipeline, taking and returning JSON (the shared wire shape).
    fn run_json(&self, payload_json: &str) -> PyResult<String> {
        let payload: Value = serde_json::from_str(payload_json)
            .map_err(|error| PyValueError::new_err(format!("invalid JSON payload: {error}")))?;
        let text = payload.get("text").and_then(Value::as_str).ok_or_else(|| {
            PyValueError::new_err("payload must be an object with a `text` string")
        })?;
        let run = self.inner.run(text).map_err(runtime_error)?;
        serde_json::to_string(&run.to_json()).map_err(|error| runtime_error(error.into()))
    }

    fn __repr__(&self) -> String {
        format!("Pipeline(extensions={:?})", self.inner.registry().names())
    }
}

/// Analyse text without any extension.
#[pyfunction]
fn analyse(text: &str) -> Analysis {
    let analysis = core::analyse(text);
    Analysis {
        text: analysis.text,
        words: analysis.words,
        characters: analysis.characters,
        digest: analysis.digest,
    }
}

/// The hook vocabulary, as a tuple.
#[pyfunction]
fn hooks() -> Vec<String> {
    Pipeline::hooks()
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<Analysis>()?;
    module.add_class::<Run>()?;
    module.add_class::<Pipeline>()?;
    module.add_function(wrap_pyfunction!(analyse, module)?)?;
    module.add_function(wrap_pyfunction!(hooks, module)?)?;
    module.add("__version__", VERSION)?;
    module.add("PROTOCOL_VERSION", core::PROTOCOL_VERSION)?;
    module.add("HOOK_INPUT", core::HOOK_INPUT)?;
    module.add("HOOK_OUTPUT", core::HOOK_OUTPUT)?;
    Ok(())
}
