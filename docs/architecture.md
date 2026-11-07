# Architecture

## Layers

```
     ┌──────────────────────────────────────────────────────────────┐
     │ blablablars  (one binary)                                    │
     │                                                              │
     │   CLI ──▶ Pipeline ──▶ Registry ──▶ hooks                     │
     │              │                                               │
     │              ├── loader + plugin_abi ──▶ Rust cdylib          │
     │              ├── pythonhost (embedded CPython) ──▶ *.py       │
     │              └── host ──▶ child process ⇄ JSON on stdio       │
     └──────────────────────────────────────────────────────────────┘

     blablablars-python (PyO3 module `blablablars._native`)
       └── the same Pipeline/Registry, reached from Python; it can attach the
           same Rust cdylibs (loader) and child processes (host)
```

Three rules keep this honest:

1. **The core never imports Python.** It takes and returns data. Embedding lives
   in `blablablars-pythonhost` and the bindings in `blablablars-python`; neither
   is a dependency of the core, and a `--no-default-features` build has no
   interpreter at all.
2. **Extensions only meet JSON.** `Extension::call(hook, &Value) -> Value` is
   implemented by Rust libraries loaded with `dlopen`, by embedded Python
   plugins, by Python callbacks (behind PyO3), and by child processes (behind the
   protocol). Adding a fourth kind of extension does not change the pipeline.
3. **One wire shape.** `{"text": ...}` going in and
   `{"analysis": {...}, "trace": [...]}` coming out is what the CLI prints with
   `--json`, what `Pipeline.run_json` returns, and what the protocol carries.

## The four transports

| transport | crate | how it is reached |
| --- | --- | --- |
| Rust `cdylib` | `core::loader` + `core::plugin_abi` | `--rust-plugin`, `Pipeline.register_rust` |
| embedded Python | `blablablars-pythonhost` | `--python-plugin`, discovery entry points |
| Python callback | `blablablars-python` (PyO3) | `Pipeline.register` |
| child process | `core::host` | `--plugin`, `Pipeline.register_process` |

All four are documented in [`extensions.md`](extensions.md).

## A `run` in detail

1. The caller supplies text.
2. `pipeline.input` is dispatched through the registry. Each subscribed
   extension receives the current payload and returns the next one.
3. The core validates that `text` is still a string and computes the analysis
   (words, characters, SHA-256 digest).
4. `pipeline.output` is dispatched with `{"analysis": {...}}`.
5. The returned payload must still contain an `analysis` object; it is
   deserialised back into `Analysis`.
6. The caller gets `Run { analysis, trace }`, where `trace` lists
   `extension: hook` entries in execution order.

Failures are typed, never panics: `ExtensionError` for something an extension did
wrong, `CoreError` for registration and dispatch problems, `ProtocolError` for a
plugin that misbehaves on the wire.

## Why both in-process and child processes

In-process extensions are what most plugin authors want: no serialisation
round-trip, no extra process, direct access to the shared state the extension was
configured with. Rust libraries are loaded with `dlopen` behind a versioned C ABI
(`plugin_abi`), and Python plugins run in an embedded interpreter (`pythonhost`)
that imports the same `blablablars` package the Python API exposes.

The child-process transport remains for the cases in-process loading cannot
serve: a plugin written in another language, a plugin that must not be able to
crash or inspect the host, or a build that ships without an interpreter
(`--no-default-features`). The cost is one JSON round-trip per hook.

Either way the extension sees the same hooks and payloads; only the transport
changes. The child-process contract is in `docs/plugin-protocol.md`: the child
may only write protocol JSON to stdout, diagnostics belong on stderr.

## Where to add the real domain logic

`crates/blablablars-core/src/pipeline.rs`:

* replace `analyse()` with the actual built-in computation;
* keep `run()`'s ordering (input hook → built-in → output hook) so extensions
  stay useful;
* if you need new extension points, add a constant to `HOOKS` in
  `crates/blablablars-core/src/extension.rs` — the Python package derives its
  `HOOKS` from the native module automatically, so both sides update together.

Any new field on `Analysis` needs `blablablars-python/src/lib.rs` updated (the
`Analysis` pyclass mirrors it). That duplication is the price of a typed Python
API; `tests/test_api.py` and the Rust unit tests pin the shape.
