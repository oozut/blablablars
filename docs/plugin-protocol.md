# Plugin protocol (v1)

The contract between the application and a plugin process. Versioned by
`PROTOCOL_VERSION` in `crates/blablablars-core/src/protocol.rs` and mirrored in
the native module as `blablablars.PROTOCOL_VERSION`.

* Transport: the child's stdin/stdout, **one JSON document per line**.
* One response per request, always carrying the matching `id`.
* Nothing else may be written to stdout. Logging goes to stderr, which the host
  passes through to its own stderr.
* UTF-8, no framing beyond the newline.

The host implementation is `crates/blablablars-core/src/host.rs`; the reference
implementation is `python/blablablars/runtime.py`. Any language can implement it.

The same JSON documents are also the in-process contracts: `describe` answers a
`PluginDescription` and `call` answers a `Response`, which is what a Rust
`cdylib` extension returns through `plugin_abi` (with `id` fixed at `0`) and what
the embedded Python host calls a plugin with. One shape, three transports, no
translation layer to keep in step.

## Handshake

On startup the host sends:

```json
{"id":1,"method":"describe","params":{}}
```

The plugin replies:

```json
{"id":1,"result":{"protocol":1,"name":"hello_plugin","hooks":["pipeline.input","pipeline.output"]}}
```

The host refuses to register the plugin, with a clear error, when:

* `protocol` differs from its own version;
* `name` is empty or already registered;
* any entry in `hooks` is not in the known hook vocabulary.

`version` is optional and informational.

## Calling a hook

```json
{"id":2,"method":"call","params":{"hook":"pipeline.input","payload":{"text":"hi"}}}
```

```json
{"id":2,"result":{"text":"HI"}}
```

The plugin must answer for the hooks it advertised; for any other hook the host
never calls it (a subscribing plugin is selected by the registry, not by the
plugin itself). `{"id":2,"result":null}` means "payload unchanged": the host keeps the payload
it sent. Observers can therefore return `null` without echoing anything.

## Shutdown

```json
{"id":3,"method":"shutdown","params":{}}
```

```json
{"id":3,"result":{}}
```

After replying the plugin may exit. The host writes `shutdown`, then kills the
child if it is still running, so a plugin cannot leak processes.

## Errors

```json
{"id":2,"error":{"code":"extension_error","message":"ValueError: boom"}}
```

| code                 | meaning                                                    |
| -------------------- | ---------------------------------------------------------- |
| `bad_request`        | the line was not valid JSON                                |
| `unknown_method`     | method is not `describe`, `call` or `shutdown`             |
| `hook_not_subscribed`| `call` for a hook the plugin did not advertise             |
| `extension_error`    | the plugin's handler raised; `message` is shown to the user |

The host wraps any of these into an `ExtensionError` naming the extension, the
hook and the plugin's message, so failures are attributable in the CLI output.

## Invariants the host relies on

1. Requests are strictly sequential: one outstanding request at a time. The host
   serialises access with a mutex, so a plugin does not need to be concurrent.
2. A plugin that exits, crashes or writes garbage mid-request is an error, not a
   hang: the host's `read_line` returns 0 and reports
   "plugin closed its stdout before answering request N".
3. Startup latency is paid once per pipeline, not per hook: the process is
   long-lived for the duration of a run.

## Implementing it in another language

Read a line, parse it, and dispatch on `method`:

* `describe` → return `protocol`, `name`, `hooks`;
* `call` → look up `params.hook`, run your handler on `params.payload`, return
  the resulting payload;
* `shutdown` → answer, flush, exit.

Flush stdout after every response. Then pass `--plugin "<your command>"` to
`blablablars`.
