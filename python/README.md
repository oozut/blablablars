# blablablars (Python)

The Python face of the Rust core: `blablablars._native` is built with PyO3 and
wrapped by `blablablars.Pipeline`, `blablablars.Plugin` and the plugin runtime.

```python
import blablablars

result = blablablars.Pipeline().run("hello world")
print(result.analysis.words)  # 2
```

Extensions attach by the same `KIND:TARGET` spec the binary uses, so a line from
a plugin list works here unchanged:

```python
pipeline = blablablars.Pipeline()
pipeline.load("rust:libshout.so", "python:my_plugin.py")
```

See `docs/python-api.md` and `docs/plugin-protocol.md` in the repository root.
