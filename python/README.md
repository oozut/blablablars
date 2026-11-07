# blablablars (Python)

The Python face of the Rust core: `blablablars._native` is built with PyO3 and
wrapped by `blablablars.Pipeline`, `blablablars.Plugin` and the plugin runtime.

```python
import blablablars

result = blablablars.Pipeline().run("hello world")
print(result.analysis.words)  # 2
```

See `docs/python-api.md` and `docs/plugin-protocol.md` in the repository root.
