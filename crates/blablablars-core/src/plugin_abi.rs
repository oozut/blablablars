//! The stable C ABI for Rust extensions loaded at runtime.
//!
//! A Rust extension is a `cdylib` that exports one symbol,
//! [`ENTRY_POINT`] (`blablablars_plugin_v1`), returning a [`PluginVTable`]. The
//! table carries JSON-in/JSON-out functions — the same payloads as the Python
//! callbacks and the stdio protocol — so the host never shares Rust types across
//! a dynamic-library boundary.
//!
//! One call to the entry point creates one plugin instance, owned by the caller
//! until it calls `drop_instance`. Loading the same library twice therefore
//! yields two independent instances, and unloading one never disturbs the other.
//!
//! # Writing an extension
//!
//! ```ignore
//! use std::sync::Arc;
//! use blablablars_core::{export_plugin, Extension, ExtensionError};
//! use serde_json::Value;
//!
//! struct Shout { hooks: Vec<String> }
//! impl Extension for Shout { /* name, hooks, call */ }
//!
//! export_plugin!(Arc::new(Shout { hooks: vec!["pipeline.input".into()] }));
//! ```
//!
//! The crate must build as a `cdylib` and allow `unsafe_code` (the macro marks
//! its own module, so `#![allow(unsafe_code)]` at the crate root is enough).
//!
//! # Stability
//!
//! [`ABI_VERSION`] is bumped whenever [`PluginVTable`]'s layout or the meaning of
//! a function changes. The host refuses a table with a different version rather
//! than guessing.
#![allow(unsafe_code)]

use std::ffi::{c_char, c_void, CStr, CString};

use serde_json::Value;

use crate::extension::Extension;
use crate::protocol::{PluginDescription, Response};

/// Version of the C ABI described by this module.
pub const ABI_VERSION: u32 = 1;

/// Symbol every dynamic extension must export (NUL-terminated for `dlsym`).
pub const ENTRY_POINT: &[u8] = b"blablablars_plugin_v1\0";

/// What a dynamic extension hands back to the host.
///
/// Every function receives the opaque `instance` pointer stored in this table.
/// Strings are returned as NUL-terminated UTF-8 and must be released with
/// `free_string`; they are allocated by the plugin, so only the plugin's
/// allocator may free them.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginVTable {
    /// Must equal [`ABI_VERSION`].
    pub abi_version: u32,
    /// Opaque handle passed back to every function below.
    pub instance: *mut c_void,
    /// Returns a JSON [`PluginDescription`].
    pub describe: unsafe extern "C" fn(*mut c_void) -> *mut c_char,
    /// Takes `(instance, hook_name, payload_json)` and returns a JSON [`Response`].
    pub call: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> *mut c_char,
    /// Releases a string produced by `describe` or `call`.
    pub free_string: unsafe extern "C" fn(*mut c_char),
    /// Releases the instance. Called exactly once per table, by its owner.
    pub drop_instance: unsafe extern "C" fn(*mut c_void),
}

// The table is a plain set of function pointers plus the extension's own opaque
// handle; the ABI requires the extension to be thread-safe (`Extension` is
// `Send + Sync`), so moving or sharing the table is sound.
unsafe impl Send for PluginVTable {}
unsafe impl Sync for PluginVTable {}

impl std::fmt::Debug for PluginVTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginVTable")
            .field("abi_version", &self.abi_version)
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Helpers shared by the macro below and by hand-written extensions.
// ---------------------------------------------------------------------------

/// The `describe` answer, encoded for the ABI.
pub fn describe_json(plugin: &dyn Extension) -> String {
    let encoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let description = PluginDescription::new(plugin.name(), plugin.hooks().to_vec());
        serde_json::to_string(&description)
    }));
    match encoded {
        Ok(Ok(json)) => json,
        Ok(Err(error)) => error_response("serialisation_error", error.to_string()),
        Err(_) => error_response(
            "extension_error",
            "the extension panicked while describing itself",
        ),
    }
}

/// The `call` answer, encoded for the ABI.
///
/// The envelope is the same [`Response`] the stdio protocol uses, so a dynamic
/// Rust extension and a child-process plugin behave identically on failure.
pub fn call_json(plugin: &dyn Extension, hook: &str, payload_json: &str) -> String {
    // An extension bug must not abort the host: panics unwind across the C ABI
    // boundary as an abort, so they are stopped here and reported as a hook
    // failure instead.
    let executed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let response = match serde_json::from_str::<Value>(payload_json) {
            Err(error) => Response::err(0, "bad_request", error.to_string()),
            Ok(payload) => match plugin.call(hook, &payload) {
                Ok(value) => Response::ok(0, value),
                Err(error) => Response::err(0, "extension_error", error.to_string()),
            },
        };
        serde_json::to_string(&response)
    }));
    match executed {
        Ok(Ok(json)) => json,
        Ok(Err(error)) => error_response("serialisation_error", error.to_string()),
        Err(_) => error_response("extension_error", "the extension panicked"),
    }
}

/// Encode a protocol error for a path that cannot produce a [`Response`].
pub fn error_response(code: &str, message: impl Into<String>) -> String {
    serde_json::to_string(&Response::err(0, code, message)).unwrap_or_else(|_| {
        r#"{"id":0,"error":{"code":"serialisation_error","message":""}}"#.into()
    })
}

/// Move a Rust string to the C side. Returns null only if it contains a NUL.
pub fn to_c_string(text: String) -> *mut c_char {
    match CString::new(text) {
        Ok(value) => value.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Copy a string out of the plugin and release it through the plugin's own
/// allocator.
///
/// # Safety
///
/// `raw` must be null or a pointer returned by this table's `describe`/`call`,
/// and must not have been freed yet.
pub unsafe fn take_string(vtable: &PluginVTable, raw: *mut c_char) -> String {
    if raw.is_null() {
        return String::new();
    }
    let text = unsafe { CStr::from_ptr(raw) }
        .to_string_lossy()
        .into_owned();
    unsafe { (vtable.free_string)(raw) };
    text
}

/// Generate the exported entry point for a Rust extension.
///
/// The argument is an expression yielding `Arc<dyn Extension>` (the trait object
/// annotation is applied for you, so `Arc::new(MyPlugin::new())` is enough):
///
/// ```ignore
/// blablablars_core::export_plugin!(Arc::new(Shout::default()));
/// ```
///
/// Every call to the exported `blablablars_plugin_v1` builds a fresh instance,
/// so a library loaded twice behaves like two plugins.
#[macro_export]
macro_rules! export_plugin {
    ($plugin:expr) => {
        #[allow(unsafe_code)]
        const _: () = {
            use ::std::ffi::{c_char, c_void, CStr, CString};
            use ::std::sync::Arc;

            use $crate::plugin_abi::{
                call_json, describe_json, error_response, to_c_string, PluginVTable, ABI_VERSION,
            };

            fn create_plugin() -> Arc<dyn $crate::Extension> {
                $plugin
            }

            unsafe fn resolve<'a>(pointer: *mut c_void) -> &'a dyn $crate::Extension {
                let plugin = unsafe { &*(pointer as *const Arc<dyn $crate::Extension>) };
                plugin.as_ref()
            }

            unsafe extern "C" fn describe(pointer: *mut c_void) -> *mut c_char {
                to_c_string(describe_json(unsafe { resolve(pointer) }))
            }

            unsafe extern "C" fn call(
                pointer: *mut c_void,
                hook: *const c_char,
                payload: *const c_char,
            ) -> *mut c_char {
                if hook.is_null() || payload.is_null() {
                    return to_c_string(error_response(
                        "bad_request",
                        "hook and payload must not be null",
                    ));
                }
                let plugin = unsafe { resolve(pointer) };
                let hook = match unsafe { CStr::from_ptr(hook) }.to_str() {
                    Ok(hook) => hook,
                    Err(error) => {
                        return to_c_string(error_response("bad_request", error.to_string()))
                    }
                };
                let payload = match unsafe { CStr::from_ptr(payload) }.to_str() {
                    Ok(payload) => payload,
                    Err(error) => {
                        return to_c_string(error_response("bad_request", error.to_string()))
                    }
                };
                to_c_string(call_json(plugin, hook, payload))
            }

            unsafe extern "C" fn free_string(text: *mut c_char) {
                if !text.is_null() {
                    drop(unsafe { CString::from_raw(text) });
                }
            }

            unsafe extern "C" fn drop_instance(pointer: *mut c_void) {
                if !pointer.is_null() {
                    drop(unsafe { Box::from_raw(pointer as *mut Arc<dyn $crate::Extension>) });
                }
            }

            /// The one symbol the host looks for.
            #[no_mangle]
            pub extern "C" fn blablablars_plugin_v1() -> PluginVTable {
                PluginVTable {
                    abi_version: ABI_VERSION,
                    instance: Box::into_raw(Box::new(create_plugin())) as *mut c_void,
                    describe,
                    call,
                    free_string,
                    drop_instance,
                }
            }
        };
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROTOCOL_VERSION;

    struct Echo {
        hooks: Vec<String>,
        explode: bool,
    }

    impl Extension for Echo {
        fn name(&self) -> &str {
            "echo"
        }

        fn hooks(&self) -> &[String] {
            &self.hooks
        }

        fn call(&self, _hook: &str, payload: &Value) -> Result<Value, crate::ExtensionError> {
            if self.explode {
                panic!("boom");
            }
            Ok(payload.clone())
        }
    }

    fn echo(explode: bool) -> Echo {
        Echo {
            hooks: vec![crate::HOOK_INPUT.to_string()],
            explode,
        }
    }

    #[test]
    fn a_description_is_plain_protocol_json() {
        let encoded = describe_json(&echo(false));
        let description: PluginDescription = serde_json::from_str(&encoded).unwrap();
        assert_eq!(description.name, "echo");
        assert_eq!(description.hooks, vec![crate::HOOK_INPUT.to_string()]);
        assert_eq!(description.protocol, PROTOCOL_VERSION);
    }

    #[test]
    fn a_call_returns_a_protocol_response() {
        let encoded = call_json(&echo(false), crate::HOOK_INPUT, r#"{"text":"hi"}"#);
        let response: Response = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            response.into_result().unwrap(),
            serde_json::json!({"text": "hi"})
        );
    }

    #[test]
    fn an_invalid_payload_is_a_bad_request() {
        let encoded = call_json(&echo(false), crate::HOOK_INPUT, "not json");
        let response: Response = serde_json::from_str(&encoded).unwrap();
        let error = response.into_result().unwrap_err();
        assert_eq!(error.code, "bad_request");
    }

    #[test]
    fn a_panicking_extension_does_not_escape_the_boundary() {
        // Without `catch_unwind` this would abort the host process.
        let encoded = call_json(&echo(true), crate::HOOK_INPUT, r#"{"text":"hi"}"#);
        let response: Response = serde_json::from_str(&encoded).unwrap();
        let error = response.into_result().unwrap_err();
        assert_eq!(error.code, "extension_error");
        assert!(error.message.contains("panicked"), "{}", error.message);
    }
}
