# blablablars

One core, two surfaces: a Rust application you can ship as a single binary, and a
Python package built from the *same* core so people can call it and extend it
from Python.

> **Status: initial scaffold (0.1.0).** The plumbing — workspace, PyO3 bindings,
> extension registry, plugin protocol, CLI, tests, CI — is real and tested. The
> built-in capability is deliberately small (text in, word/character/digest
> analysis out) so the seams are exercised; swap in the actual domain logic in
> `crates/blablablars-core/src/pipeline.rs` when it is decided.

## Why this shape

* `blablablars-core` has no I/O and no knowledge of Python: it is the product.
* `blablablars-cli` is the deliverable binary, and extensions run **inside it**:
  Rust extensions are `cdylib`s loaded with `dlopen` behind a versioned C ABI,
  Python plugins run in an embedded interpreter. `--no-default-features` drops
  the interpreter again.
* `blablablars-pythonhost` embeds CPython for Python plugins;
  `blablablars-python` is a PyO3 module wrapping the same core, so Python callers
  get the fast in-process path with no reimplementation.
* Extensions cross one boundary only: JSON payloads on named hooks — inside the
  binary (Rust and Python), inside a Python process (PyO3 callbacks), or over
  stdio (a child process, for anything else). The same payload shape is used by
  the CLI output too, so the transports cannot drift.

## Quick start — Rust

```bash
cargo run -p blablablars-cli -- run "hello brave world"
cargo run -p blablablars-cli -- run "hello brave world" --json
cargo run -p blablablars-cli -- hooks
```

Build the shippable binary with `cargo build --release` (or `make build`); the
result is `target/release/blablablars`.

## Quick start — Python

```bash
python3 -m venv .venv
.venv/bin/pip install maturin
VIRTUAL_ENV=$PWD/.venv .venv/bin/maturin develop --release --manifest-path python/pyproject.toml
.venv/bin/python -c 'import blablablars; print(blablablars.Pipeline().run("hi there").analysis)'
```

or simply `make python-develop && make python-test`.

```python
import blablablars

result = blablablars.Pipeline().run("hello brave world")
print(result.analysis.words, result.analysis.digest)
```

## Extensions

Four transports, one contract (hooks + JSON payloads). Everything is documented
in [`docs/extensions.md`](docs/extensions.md).

**Python, inside the binary** — no Rust needed:

```python
from blablablars import HOOK_INPUT, hook

@hook(HOOK_INPUT)
def shout(payload):
    payload["text"] = payload["text"].upper()
    return payload
```

```bash
./target/release/blablablars run "hello there" \
  --python-path python \
  --python-plugin examples/python/hello_plugin.py
```

**Rust, inside the binary** — a `cdylib` + `export_plugin!`; see
[`examples/rust-plugin`](examples/rust-plugin):

```bash
make example
./target/release/blablablars run "hello there" \
  --rust-plugin target/plugin-fixture/debug/libblablablars_example_rust_plugin.so
```

**From Python**, all of it at once:

```python
import blablablars

pipeline = blablablars.Pipeline()
pipeline.register(my_plugin)                       # Python callback
pipeline.register_rust("libshout.so")              # Rust cdylib, in-process
pipeline.register_process("python3 -m blablablars.runtime plugin.py")
```

**Any other language**, as a child process:

```bash
./target/release/blablablars run "hello there" \
  --plugin "python -m blablablars.runtime examples/python/hello_plugin.py"
```

`make demo` runs the child-process example, `make demo-inprocess` runs the Rust
and embedded-Python extensions inside one binary.

## Repository layout

```
crates/
  blablablars-core/      engine, hooks, registry; dynamic loader (C ABI) and
                         the child-process protocol + host adapter
  blablablars-cli/       the `blablablars` binary
  blablablars-pythonhost/ embedded CPython: runs Python plugins in-process
  blablablars-python/    PyO3 module `blablablars._native`
python/
  blablablars/           Python package: Pipeline, Plugin, discovery, runtime
  tests/                 unittest suite
examples/
  python/                a runnable Python plugin
  rust-plugin/           a runnable Rust extension (cdylib, own workspace)
docs/                    architecture, extensions, Python API, wire protocol
```

## Hooks

| hook              | when                         | payload                        |
| ----------------- | ---------------------------- | ------------------------------ |
| `pipeline.input`  | before the core runs          | `{"text": "..."}`              |
| `pipeline.output` | after the core produced a result | `{"analysis": {...}}`       |

Extensions run in registration order; each receives the previous one's payload.
A handler returning `None` leaves the payload untouched.

## Tests

```bash
make test          # cargo test + cargo fmt --check + cargo clippy -D warnings
make python-test   # unittest suite against the built extension
make example       # build the example Rust extension (a real cdylib)
```

The Rust tests build `examples/rust-plugin` on demand and load it in-process;
the CLI tests skip the embedded-Python cases until `maturin develop` has produced
`python/blablablars/_native*.so`.

## License

Apache-2.0 — see [`LICENSE`](LICENSE).
