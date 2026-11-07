# Architecture

## Layers

```
            ┌──────────────────────────┐
            │ blablablars (binary)     │  CLI: run / hooks / plugins / version
            └────────────┬─────────────┘
                         │
   in-process plugins ───┤  process plugins ──── JSON over stdio ──┐
                         │                                         │
            ┌────────────▼─────────────┐              ┌────────────▼─────────────┐
            │ blablablars-core         │              │ blablablars.runtime      │
            │ pipeline + registry      │              │ (Python, one plugin)     │
            └────────────▲─────────────┘              └──────────────────────────┘
                         │
            ┌────────────┴─────────────┐
            │ blablablars-python (PyO3)│  `blablablars._native`
            └────────────▲─────────────┘
                         │
            ┌────────────┴─────────────┐
            │ blablablars (Python pkg) │  Pipeline / Plugin / discovery / runtime
            └──────────────────────────┘
```

Three rules keep this honest:

1. **The core never imports Python.** It takes and returns data. PyO3 lives in
   its own crate, so the Rust-only build never links libpython.
2. **Extensions only meet JSON.** `Extension::call(hook, &Value) -> Value` is
   implemented by Rust extensions, by Python callbacks (behind PyO3), and by
   child processes (behind the protocol). Adding a fourth kind of extension does
   not change the pipeline.
3. **One wire shape.** `{"text": ...}` going in and
   `{"analysis": {...}, "trace": [...]}` coming out is what the CLI prints with
   `--json`, what `Pipeline.run_json` returns, and what the protocol carries.

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

## Why child processes for plugins

Embedding CPython in the shipped binary would drag in interpreter startup,
version pinning and `sys.path` questions for every user, and would make a
crashing plugin able to take down the host. A long-lived child process per plugin
gives crash isolation, language freedom (anything that can read stdin and write
stdout), and no build-time Python requirement. The cost is one request round-trip
per hook, which is irrelevant for extension work.

The trade-off is documented in `docs/plugin-protocol.md`: the child may only
write protocol JSON to stdout; diagnostics belong on stderr.

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
