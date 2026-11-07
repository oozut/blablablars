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
* `blablablars-cli` is the deliverable binary. It stays lean because Python
  plugins are *child processes* talking JSON over stdio, not an embedded
  interpreter.
* `blablablars-python` is a PyO3 extension wrapping the same core, so Python
  callers get the fast in-process path with no reimplementation.
* Extensions cross one boundary only: JSON payloads on named hooks. The same
  payload shape is used in-process (PyO3), out-of-process (stdio protocol) and
  in the CLI output, so the three cannot drift.

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

## Writing an extension in Python

```python
from blablablars import HOOK_INPUT, hook

@hook(HOOK_INPUT)
def shout(payload):
    payload["text"] = payload["text"].upper()
    return payload
```

Run it inside the Rust application without touching Rust:

```bash
./target/release/blablablars run "hello there" \
  --plugin "python -m blablablars.runtime examples/python/hello_plugin.py"
```

or inside Python, in-process:

```python
import blablablars

pipeline = blablablars.Pipeline([my_plugin])
result = pipeline.run("hello there")
```

Details: [`docs/python-api.md`](docs/python-api.md).

## Repository layout

```
crates/
  blablablars-core/   engine, hooks, registry, plugin protocol + host adapter
  blablablars-cli/    the `blablablars` binary
  blablablars-python/ PyO3 module `blablablars._native`
python/
  blablablars/        Python package: Pipeline, Plugin, discovery, runtime
  tests/              unittest suite (26 tests)
examples/
  python/             a runnable Python plugin
docs/                 architecture, Python API, wire protocol
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
```

## License

Apache-2.0 — see [`LICENSE`](LICENSE).
