# Python API

Everything here is the Rust core: `blablablars._native` is a PyO3 module, and the
rest of the package is a thin, friendly layer over it.

## Calling the core

```python
import blablablars

# One-shot
analysis = blablablars.analyse("hello brave world")
analysis.text       # "hello brave world"
analysis.words      # 3
analysis.characters # 17
analysis.digest     # sha256 hex

# With a pipeline (extension point even when empty)
pipeline = blablablars.Pipeline()
run = pipeline.run("hello brave world")
run.analysis.words  # 3
run.trace           # [] — no extensions ran

# JSON in, JSON out (the same shape the CLI prints with --json)
pipeline.run_json({"text": "hello brave world"})
# {"analysis": {"text": ..., "words": 3, ...}, "trace": []}
```

`Pipeline.run_json` is the compatibility surface: it is the same payload the
stdio protocol uses, so anything you can express to a child-process plugin you
can express in-process.

## Writing an extension

### Decorated functions (shortest form)

```python
from blablablars import HOOK_INPUT, hook

@hook(HOOK_INPUT)
def shout(payload):
    payload["text"] = payload["text"].upper()
    return payload
```

Decorating a *module* this way makes the whole module a plugin:

```python
import blablablars
import my_plugins_module

pipeline = blablablars.Pipeline()
pipeline.register(my_plugins_module)   # name = module __name__
pipeline.run("hi")                     # analysed as "HI"
```

`blablablars.plugin_from_namespace(obj, name=...)` builds the plugin explicitly
when you want a different name.

### Plugin class

```python
import blablablars

class Shout(blablablars.Plugin):
    name = "shout"
    hooks = (blablablars.HOOK_INPUT,)

    def call(self, hook_name, payload):
        payload["text"] = payload["text"].upper()
        return payload
```

or let decorated methods drive the base `call`:

```python
class Audit(blablablars.Plugin):
    name = "audit"

    @blablablars.hook(blablablars.HOOK_INPUT, blablablars.HOOK_OUTPUT)
    def log(self, payload):
        print("hook seen:", payload, file=sys.stderr)  # never stdout in a plugin
        return None                                    # leave payload untouched
```

Handlers run in definition order and are chained: each non-`None` return value
becomes the next handler's payload.

### Rules

* `hooks` may only contain the known hook names; unknown names are rejected at
  `register()` time.
* `name` must be unique within one pipeline.
* An exception inside a handler propagates to the caller as `RuntimeError` with
  the original message.
* Returning a payload that no longer contains a string `text` (for
  `pipeline.input`) or an `analysis` object (for `pipeline.output`) is an error,
  because the core cannot continue.

## Running a plugin inside the Rust application

Write a file:

```python
# examples/python/hello_plugin.py
from blablablars import HOOK_INPUT, HOOK_OUTPUT, hook

@hook(HOOK_INPUT)
def shout(payload):
    payload["text"] = payload["text"].upper()
    return payload

@hook(HOOK_OUTPUT)
def annotate(payload):
    payload["analysis"]["text"] = "[shouted] " + payload["analysis"]["text"]
    return payload
```

Point the binary at the runtime (repeat `--plugin` to chain plugins):

```bash
blablablars run "hello there" --json \
  --plugin "python -m blablablars.runtime examples/python/hello_plugin.py"
```

The command line is split on whitespace, so no quoting support — use a wrapper
script if you need arguments containing spaces.

`blablablars plugins --plugin "..."` performs only the handshake and prints what
the plugin reports, which is the fastest way to debug a plugin that does not
start.

## Discovery

`blablablars.load_plugins()` returns every installed plugin, from two sources:

1. the `blablablars.plugins` entry-point group (declare it in a package's
   `pyproject.toml`; the entry point must load a plugin object or a module of
   `@hook` functions);
2. files listed in `$BLABLABLARS_PLUGINS`, separated by `os.pathsep`.

```python
import blablablars

pipeline = blablablars.Pipeline(blablablars.load_plugins())
```

Note: discovery is *invoked* explicitly — `Pipeline()` never loads plugins behind
your back, so a library user's behaviour cannot change because something got
installed.

## stdout belongs to the protocol

When a plugin is served over stdio, stdout is the wire. `print()` to debug will
corrupt the stream (the host reports "non-protocol output on stdout"); write to
`sys.stderr` instead.
