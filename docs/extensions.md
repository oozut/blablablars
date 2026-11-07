# Extensions

Everything that can add behaviour to the pipeline is an *extension*, and every
extension speaks the same contract: named hooks, JSON in, JSON out. That is the
only thing the pipeline knows, so the four ways of writing one are
interchangeable.

| transport | runs where | written in | loaded by |
| --- | --- | --- | --- |
| **Rust library** | inside the `blablablars` process (`dlopen`) | Rust | `--rust-plugin`, `--rust-plugin-dir`, `$BLABLABLARS_RUST_PLUGINS`, `Pipeline.register_rust` |
| **Python plugin** | inside the `blablablars` process (embedded CPython) | Python | `--python-plugin`, `$BLABLABLARS_PLUGINS`, `blablablars.plugins` entry points |
| **Python callback** | inside a Python process | Python | `Pipeline.register` (PyO3) |
| **Child process** | beside the `blablablars` process | anything | `--plugin`, `Pipeline.register_process` |

The first two are *in-process*: they are loaded into the running binary, share
its address space and see the same hooks. The fourth exists for languages that
cannot be loaded and for plugins that must be isolated; see
[`plugin-protocol.md`](plugin-protocol.md).

## The contract

* A hook is a name from a fixed vocabulary (`blablablars hooks`): currently
  `pipeline.input` (payload `{"text": "..."}`) and `pipeline.output` (payload
  `{"analysis": {...}}`).
* An extension declares its name and the hooks it subscribes to, and answers
  `call(hook, payload) -> payload`.
* Returning `null`/`None` means "leave the payload alone".
* Extensions run in registration order; each receives the previous one's output.
* An error from one extension fails that hook, and the run, with a message that
  names the extension and the hook.

## Writing a Rust extension

A Rust extension is a `cdylib` that depends on `blablablars-core` and exports one
generated symbol. The full example lives in
[`examples/rust-plugin`](../examples/rust-plugin):

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
blablablars-core = { path = "…/crates/blablablars-core" }
serde_json = "1"
```

```rust
#![allow(unsafe_code)] // the `export_plugin!` glue contains the unsafe parts

use blablablars_core::{export_plugin, Extension, ExtensionError, HOOK_INPUT};
use serde_json::{json, Value};

struct Shout {
    hooks: Vec<String>,
}

impl Extension for Shout {
    fn name(&self) -> &str {
        "shout"
    }

    fn hooks(&self) -> &[String] {
        &self.hooks
    }

    fn call(&self, hook: &str, payload: &Value) -> Result<Value, ExtensionError> {
        match hook {
            HOOK_INPUT => Ok(json!({
                "text": payload["text"].as_str().unwrap_or_default().to_uppercase()
            })),
            other => Err(ExtensionError::failed("shout", other, "not my hook")),
        }
    }
}

export_plugin!(::std::sync::Arc::new(Shout {
    hooks: vec![HOOK_INPUT.to_string()],
}));
```

Build it and hand it to the application:

```bash
cargo build --manifest-path examples/rust-plugin/Cargo.toml
target/debug/blablablars run "hello" \
  --rust-plugin target/plugin-fixture/debug/libblablablars_example_rust_plugin.so
```

Anything that can open the library can load it, including Python:

```python
import blablablars

pipeline = blablablars.Pipeline()
pipeline.register_rust("build/libshout.so")
```

Notes:

* Each library load creates its own instance, so the same extension can be used
  twice with independent state, and unloading one cannot free another's memory.
* `export_plugin!` needs `unsafe_code` to be allowed; the macro marks its own
  module, so `#![allow(unsafe_code)]` at the crate root is enough. Everything
  else is ordinary safe Rust — the only `unsafe` is the C ABI glue.
* The ABI is versioned (`blablablars_core::ABI_VERSION`). On a mismatch the load
  fails with both versions in the message instead of calling into unknown code.
* A panic inside a hook is caught and reported as an extension error, so a buggy
  extension cannot take the host down.

## Writing a Python extension

A Python plugin is a file or module with hooks. It is the same object whether it
runs embedded or as a child process:

```python
from blablablars import HOOK_INPUT, hook

@hook(HOOK_INPUT)
def shout(payload):
    payload["text"] = payload["text"].upper()
    return payload
```

Run it *inside* the binary (needs the package on the interpreter's path):

```bash
target/debug/blablablars run "hello" \
  --python-path python \
  --python-plugin examples/python/hello_plugin.py
```

`--python-plugin` accepts a `.py` file or an importable module name. Install
plugins as packages and expose them through the `blablablars.plugins` entry-point
group, or list files in `$BLABLABLARS_PLUGINS`, to make discovery automatic.

The class style is available too:

```python
from blablablars import Plugin, HOOK_OUTPUT

class Mark(Plugin):
    name = "mark"
    hooks = (HOOK_OUTPUT,)

    def call(self, hook_name, payload):
        payload["analysis"]["marked"] = True
        return payload
```

## Loading from Python

`blablablars.Pipeline` can attach all four transports:

```python
import blablablars

pipeline = blablablars.Pipeline()
pipeline.register(my_plugin)                      # Python callback, in-process
pipeline.register_rust("libshout.so")             # Rust cdylib, in-process
pipeline.register_rust_directory("plugins/rust/") # every .so in a directory
pipeline.register_process("python3 -m blablablars.runtime plugin.py")
```

`register_rust` and `register_rust_directory` use the same loader as the CLI, so
a Rust extension behaves identically from either surface.

## Loading from Rust

```rust
use std::sync::Arc;
use blablablars_core::{DynamicExtension, Pipeline};

let mut pipeline = Pipeline::new();
pipeline.register(Arc::new(DynamicExtension::open("libshout.so")?))?;
```

With the `blablablars-pythonhost` crate, Python plugins run in the same process:

```rust
blablablars_pythonhost::initialize(&["python".into()])?;      // extra sys.path entries
pipeline.register(blablablars_pythonhost::load_file("plugin.py")?)?;
pipeline.register(blablablars_pythonhost::load_module("my_package.plugin")?)?;
for extension in blablablars_pythonhost::load_discovered()? {  // entry points + env
    pipeline.register(extension)?;
}
```

That crate embeds CPython, so it must not be linked together with
`blablablars-python` (the PyO3 module) in one artefact: the module exports
`PyInit__native` while the host owns the interpreter. They are separate crates
for exactly this reason; `default-members` keeps the module out of the ordinary
`cargo test`.

## Failure behaviour

| situation | what happens |
| --- | --- |
| library is not an extension / ABI mismatch | load fails, message names both sides |
| extension subscribes to an unknown hook | load fails, message lists known hooks |
| duplicate extension name | registration fails (`DuplicateExtension`) |
| hook raises / returns a bad payload | run fails with `extension: hook: message` |
| extension panics | caught, reported as an extension error |
| Python plugin raises | reported as an extension error (host stays up) |
| child process dies | reported as a plugin error |

## Threading and state

* `Extension` is `Send + Sync`: the pipeline may dispatch from any thread.
* In-process extensions share the host's memory, so guard shared state with the
  usual primitives. An embedded Python plugin holds the GIL only while its hook
  runs.
* Extensions are `Arc`s owned by the registry; dropping the pipeline or calling
  `unregister` releases them (`DynamicExtension` then calls the library's
  `drop_instance` and unmaps it).

## Security

An in-process extension runs with the application's own privileges: it can read
files, the environment, and everything else the process can. Only load
extensions you would agree to run as the application itself. A child-process
plugin is the choice when that is not acceptable — it can be sandboxed
separately, at the cost of one JSON round-trip per hook.
