//! Loading Rust extensions from a shared library at runtime.
//!
//! The host never shares Rust types with the extension: everything crosses the
//! boundary as JSON through the C ABI described in [`crate::plugin_abi`]. That
//! keeps extensions buildable against any compatible build of the crate, and it
//! makes the three in-process transports (Rust `cdylib`, embedded Python,
//! Python callbacks) behave identically.
#![allow(unsafe_code)]

use std::ffi::CString;
use std::path::{Path, PathBuf};

use libloading::Library;
use serde_json::Value;

use crate::error::{CoreError, ExtensionError};
use crate::extension::Extension;
use crate::plugin_abi::{self, PluginVTable, ABI_VERSION};
use crate::protocol::{PluginDescription, Response, PROTOCOL_VERSION};

/// A Rust extension living in a `cdylib` that the host loaded at runtime.
///
/// The library stays mapped for as long as this value exists; dropping it calls
/// the extension's `drop_instance` and then unmaps the library.
pub struct DynamicExtension {
    description: PluginDescription,
    vtable: PluginVTable,
    _library: Library,
}

// The vtable's `instance` pointer is only ever dereferenced by the extension's
// own code, which the ABI requires to be thread-safe (the `Extension` trait is
// `Send + Sync`).
unsafe impl Send for DynamicExtension {}
unsafe impl Sync for DynamicExtension {}

impl DynamicExtension {
    /// `dlopen` `path`, verify it is a compatible extension and run its handshake.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CoreError> {
        let path = path.as_ref();

        let library = unsafe { Library::new(path) }.map_err(|error| {
            CoreError::Plugin(format!(
                "could not load the shared library `{}`: {error}",
                path.display()
            ))
        })?;

        // SAFETY: the symbol signature is the documented ABI entry point. Each
        // call creates a fresh instance, owned by this value until `Drop`.
        let entry: libloading::Symbol<unsafe extern "C" fn() -> PluginVTable> =
            unsafe { library.get(plugin_abi::ENTRY_POINT) }.map_err(|error| {
                CoreError::Plugin(format!(
                "`{}` is not a blablablars extension: no `blablablars_plugin_v1` symbol ({error})",
                path.display()
            ))
            })?;

        let vtable = unsafe { entry() };

        if vtable.abi_version != ABI_VERSION {
            // The table is not the one this build was compiled against, so
            // `drop_instance` cannot be trusted either: the instance is left to
            // the unload rather than released through a layout that may differ.
            return Err(CoreError::Plugin(format!(
                "`{}` targets plugin ABI {}, this build speaks {ABI_VERSION}; rebuild the extension",
                path.display(),
                vtable.abi_version
            )));
        }

        // A table with no instance is one that was never initialised, and every
        // function below would be handed a null pointer.
        if vtable.instance.is_null() {
            return Err(CoreError::Plugin(format!(
                "`{}` returned an entry point with no instance",
                path.display()
            )));
        }

        let description_json =
            unsafe { plugin_abi::take_string(&vtable, (vtable.describe)(vtable.instance)) };
        let description: PluginDescription =
            serde_json::from_str(&description_json).map_err(|error| {
                CoreError::Plugin(format!(
                    "`{}` sent an invalid description: {error} ({description_json:?})",
                    path.display()
                ))
            })?;

        if description.protocol != PROTOCOL_VERSION {
            return Err(CoreError::Plugin(format!(
                "`{}` speaks protocol {}, this build speaks {PROTOCOL_VERSION}",
                path.display(),
                description.protocol
            )));
        }
        for hook in &description.hooks {
            if !crate::extension::is_known_hook(hook) {
                return Err(CoreError::Plugin(format!(
                    "`{}` subscribes to unknown hook `{hook}` (known hooks: {})",
                    path.display(),
                    crate::extension::known_hooks()
                )));
            }
        }

        Ok(Self {
            description,
            vtable,
            _library: library,
        })
    }

    /// Open every `*.so`/`*.dylib`/`*.dll` in `directories`, skipping missing ones.
    pub fn open_directories(
        directories: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Vec<Self>, CoreError> {
        let mut loaded = Vec::new();
        for directory in directories {
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) => {
                    return Err(CoreError::Plugin(format!(
                        "cannot read plugin directory `{}`: {error}",
                        directory.display()
                    )))
                }
            };
            let mut paths: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| is_shared_library(path))
                .collect();
            paths.sort();
            for path in paths {
                loaded.push(Self::open(path)?);
            }
        }
        Ok(loaded)
    }

    pub fn description(&self) -> &PluginDescription {
        &self.description
    }
}

/// True when `path` looks like a loadable extension for this platform.
pub fn is_shared_library(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("so" | "dylib" | "dll")
    )
}

impl Extension for DynamicExtension {
    fn name(&self) -> &str {
        &self.description.name
    }

    fn hooks(&self) -> &[String] {
        &self.description.hooks
    }

    fn call(&self, hook: &str, payload: &Value) -> Result<Value, ExtensionError> {
        let payload_json = serde_json::to_string(payload).map_err(|error| {
            ExtensionError::failed(&self.description.name, hook, error.to_string())
        })?;
        let hook_c = CString::new(hook).map_err(|error| {
            ExtensionError::failed(&self.description.name, hook, error.to_string())
        })?;
        let payload_c = CString::new(payload_json).map_err(|error| {
            ExtensionError::failed(&self.description.name, hook, error.to_string())
        })?;

        let raw = unsafe {
            (self.vtable.call)(self.vtable.instance, hook_c.as_ptr(), payload_c.as_ptr())
        };
        let response_json = unsafe { plugin_abi::take_string(&self.vtable, raw) };

        let response: Response = serde_json::from_str(&response_json).map_err(|error| {
            ExtensionError::failed(
                &self.description.name,
                hook,
                format!("the extension returned an invalid response: {error} ({response_json:?})"),
            )
        })?;
        response.into_result().map_err(|error| {
            ExtensionError::failed(&self.description.name, hook, error.to_string())
        })
    }
}

impl Drop for DynamicExtension {
    fn drop(&mut self) {
        unsafe { (self.vtable.drop_instance)(self.vtable.instance) };
    }
}

impl std::fmt::Debug for DynamicExtension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynamicExtension")
            .field("description", &self.description)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_reported_clearly() {
        let error = DynamicExtension::open("/nonexistent/extension.so").unwrap_err();
        assert!(matches!(&error, CoreError::Plugin(message) if message.contains("could not load")));
    }

    #[test]
    fn shared_library_extensions_are_recognised() {
        assert!(is_shared_library(Path::new("libfoo.so")));
        assert!(is_shared_library(Path::new("foo.dylib")));
        assert!(is_shared_library(Path::new("foo.dll")));
        assert!(!is_shared_library(Path::new("foo.rs")));
    }
}
