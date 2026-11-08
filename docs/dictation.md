# Dictation

Press a shortcut, say something, and the words appear in whatever has focus.

```bash
blablablars dictate --toggle      # the shortcut: press to start, press to stop
blablablars dictate --seconds 10  # record ten seconds, then transcribe
blablablars dictate --input take.wav --print
```

Everything below the shortcut is four replaceable pieces joined in one place:

```
capture ──► transcribe ──► clean ──► deliver
(child       (whisper.cpp,  (pure       (paste, xdotool, wtype,
 process,     Parakeet,      functions)   your command, stdout)
 raw PCM)     ONNX, command)
```

Each piece is behind a trait or a config entry, so a different microphone
backend, model family or output tool never touches the others. The audio
contract between them is one shape everywhere: **mono 16-bit PCM at 16 kHz**.

It runs on Linux, macOS and Windows. Nothing is linked against an audio library:
capture is always a child process writing PCM to stdout, and typing is always a
program or a clipboard, so the same binary works wherever those exist.

## Quick start

```bash
# 1. a model, in the standard location
mkdir -p ~/.local/share/blablablars/models
curl -L -o ~/.local/share/blablablars/models/ggml-small.bin \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin

# 2. a config with every setting written out (optional — defaults apply anyway)
blablablars config init

# 3. what it found
blablablars models

# 4. try it without a microphone, or with one
blablablars dictate --input sample.wav --print
blablablars dictate            # records until Enter
```

Then bind `blablablars dictate --toggle` to a key:

| desktop | how |
| --- | --- |
| GNOME | Settings → Keyboard → Keyboard Shortcuts → Custom Shortcuts → `+` |
| KDE | System Settings → Shortcuts → Custom Shortcuts → `+` |
| sway | `bindsym $mod+d exec blablablars dictate --toggle` |
| Hyprland | `bind = SUPER, D, exec, blablablars dictate --toggle` |
| i3 | `bindsym $mod+d exec --no-startup-id blablablars dictate --toggle` |
| macOS | Shortcuts.app → "Run Shell Script", or `skhd`/Karabiner |
| Windows | `nircmd`, AutoHotkey, a Start-menu shortcut with a hotkey |

The first press starts a detached recorder and returns immediately, so the
shortcut is responsive. The second press stops it, transcribes, types, and exits.
The session is a loopback TCP port noted down in a small file in the runtime
directory, so a crash cannot leave a stale pid file behind, and the same code
works on all three systems — a Unix domain socket would not.

## The config file

`$XDG_CONFIG_HOME/blablablars/config.yml` (`%APPDATA%\blablablars\config.yml` on
Windows, `~/.config/blablablars/config.yml` otherwise), or `$BLABLABLARS_CONFIG`,
or `--config FILE`. Unknown keys are an error: a silent `outptu:` is a bug
report waiting to happen.

```yaml
language: en                    # or `auto` to detect it

capture:
  kind: auto                    # auto | command
  argv: []                      # for `kind: command`

models:
  directory: ~/.local/share/blablablars/models
  default: whisper-large-v3-turbo

transcriber:
  kind: whisper-cli             # whisper-cli | parakeet | sherpa-onnx | command
  binary: null                  # null = whatever `kind` defaults to
  argv: []                      # for `kind: command`
  extra_args: []
  gpu: auto                     # auto | off
  threads: null                 # `-t` / `--num-threads`; unset lets the backend decide
  timeout_seconds: 180

cleanup: true                   # true | false | a table of individual switches

output:
  kind: auto                    # auto | paste | stdout | command | xdotool | wtype
  argv: []                      # for `kind: command`, with a {text} placeholder
  trailing: "\n"                # appended after the text (empty string: none)

feedback:
  command: []                   # a program run as recording/transcribing/done/error

stream:
  enabled: false                # type the words as they settle, not at the end
  step_ms: 4000                 # how often a step transcribes and types
  window_ms: 20000              # how much trailing audio a step shows the model
  command: []                   # a program that streams partial results itself
  command_timeout_seconds: 30
```

`blablablars config show` prints the config as it is in effect, defaults
included; `blablablars config path` prints the file that would be read.

### Cleanup

`cleanup: true` runs the default rules; `cleanup: false` types the transcript as
the model produced it; a table switches them one by one:

```yaml
cleanup:
  artifacts: true          # [BLANK_AUDIO], [Music], ♪, >>, (applause)
  fillers: true            # um, uh, erm…
  repeated_words: true     # "the the" — a stutter or a duplicated prediction
  punctuation: true        # spacing around punctuation, sentence capitals
  spoken_punctuation: false # "comma", "full stop" typed as punctuation
  terminal: "."            # ensure the text ends like a sentence; "" to leave it
  preserve_case: false     # keep the model's casing
```

Fillers are kept to words that are never also real words: `like`, `so` and
`right` are left alone. A bracketed marker is only removed when it is a shouty
tag (`[BLANK_AUDIO]`) or a known non-speech word — `[1]` and `[a side note]`
survive.

## Backends

`blablablars engines` answers the question that comes right after installing:
which of these can this machine run, and what is still missing?

```
models in ~/.local/share/blablablars/models
* small                                   465.0 MB  ggml *.bin

backends
* whisper-cli  whisper-cli            installed        1 model
      whisper.cpp (ggml models, CPU or GPU)
  parakeet     parakeet-cli           installed        1 model
      NVIDIA Parakeet TDT through the ggml toolchain
  sherpa-onnx  sherpa-onnx-offline    not installed    0 models
      ONNX Runtime through sherpa-onnx (Whisper, Parakeet, transducers)
  command                             set transcriber.argv  1 model
      any program that prints the transcript
```

| `transcriber.kind` | runs | models |
| --- | --- | --- |
| `whisper-cli` | whisper.cpp | `ggml-*.bin` (Whisper, and Parakeet in GGUF) |
| `parakeet` | NVIDIA Parakeet TDT via `parakeet-cli` | `ggml`/GGUF Parakeet |
| `sherpa-onnx` | ONNX Runtime via `sherpa-onnx-offline` | ONNX folders: Whisper, Parakeet, Zipformer, Conformer |
| `command` | anything you name | anything it understands |

`parakeet` is the same ggml toolchain as `whisper-cli` (both ship in the
whisper.cpp checkout), and on a CPU it is the faster of the two. It is
transducer-based, so it has no language flag and no prompt tokens: the model is
what it is. Progress lines are suppressed, because otherwise they would be
transcribed and typed.

`sherpa-onnx` covers the ONNX Runtime world. An ONNX model is a **folder**, not a
file, so `models.directory` may contain folders as well as `*.bin` files; the
layout is detected from the file names and full-precision weights are preferred
over quantised ones:

```bash
# a Whisper ONNX export
blablablars models              # * sherpa-onnx-whisper-small   ONNX folder (Whisper)
# a streaming Parakeet transducer
blablablars models              # * sherpa-onnx-nemo-parakeet   ONNX folder (transducer)
```

## Models

Any `*.bin` whisper.cpp model in `models.directory` is discovered, in name
order, and so is any ONNX model folder:

```bash
blablablars models          # name, size, layout, file; the default is marked with *
blablablars models --json   # {"directory": …, "default": …, "models": [...]}
```

Names are matched leniently: `whisper-large-v3-turbo`, `large-v3-turbo` and
`ggml-large-v3-turbo-q5_0.bin` all reach the same file, an exact match beats a
prefix (`small.en` never silently becomes `small.en-tdrz`), and an ambiguous name
is an error that lists the candidates. `--model NAME` overrides
`models.default` for one run, and a path works too.

A missing model is not a silent failure: the message names the directory and
prints the `curl` line that fills it.

### A backend that is not on the list

`transcriber.kind: command` makes any program a backend, with `{audio}`,
`{model}`, `{model_name}` and `{language}` substituted:

```yaml
transcriber:
  kind: command
  argv: ["faster-whisper", "--model", "{model}", "{audio}"]
```

stdout is the transcript. That is how a remote API, a Python script or a runtime
nobody has written a backend for is wired in — no new Rust code, no new flag. A
name with no file behind it is passed through unresolved, so `{model_name}` can
be a Hugging Face id.

## Output

| kind | what it does |
| --- | --- |
| `auto` | `paste` on macOS and Windows, `wtype` on Wayland, `xdotool` on X11 |
| `paste` | clipboard plus the paste keystroke |
| `stdout` | prints; use this in a terminal or a script |
| `xdotool` | `xdotool type --clearmodifiers --delay 0 -- TEXT` |
| `wtype` | `wtype -- TEXT` |
| `command` | runs `argv`, substituting `{text}`; without a placeholder the text is the last argument |

`paste` is the only way to type at all on macOS and Windows, which have no
`xdotool` equivalent: it copies with `pbcopy` (macOS) or `clip.exe` (Windows) and
then sends the paste keystroke with AppleScript or PowerShell. On Linux it
composes `wl-copy` + `wtype` on Wayland and `xclip` + `xdotool` on X11 — quicker
than typing character by character, but it overwrites the clipboard.

`auto` prefers Wayland *before* X11, because `DISPLAY` is set under Xwayland even
when `xdotool` cannot reach the focused window. When neither tool is installed
the run fails and says what to install and that `output.kind: stdout` always
works — no silent no-op.

A command example that pastes through the clipboard:

```yaml
output:
  kind: command
  argv: ["sh", "-c", "printf '%s' \"$1\" | wl-copy", "sh", "{text}"]
```

## Extensions

Two hooks sit around the cleanup rules, both with a `{"text": …}` payload:

| hook | sees | use it for |
| --- | --- | --- |
| `transcription.raw` | the model's text, with `audio_seconds`, `model`, `transcriber` | dictionaries, language fixes, jargon |
| `transcription.text` | the cleaned text | rewriters, an LLM pass, formatting |

Returning `None` leaves the text alone; returning a string replaces it. The order
is `audio → transcribe → transcription.raw → cleanup → transcription.text →
deliver`, and `--json` reports the trace so a plugin that changes nothing is
visible as such. Attach plugins with the same specs as everything else:

```bash
blablablars dictate --plugin rust:./libshout.so --print
blablablars plugin add python:my_company.dictionary   # for every run
```

`--json` prints the whole run instead of typing it:

```json
{
  "text": "Hello there.",
  "raw": "um, hello there",
  "model": "small",
  "transcriber": "whisper-cli (ggml-small.bin)",
  "audio_seconds": 1.0,
  "elapsed_ms": 1204,
  "trace": []
}
```

## Streaming

Waiting until you stop talking means a three-minute thought is three minutes of
silence and then a wall of text. `stream.enabled: true` (or `--stream`) types the
words as they settle instead:

```bash
blablablars dictate --toggle --stream            # the shortcut, live
blablablars dictate --seconds 30 --stream        # twenty-nine typed words later
```

Every `stream.step_ms` the audio captured since the last step is transcribed, and
the part of the hypothesis that will not change any more is typed. The last word
is always held back until the next step confirms it, and nothing is ever
retracted — typing as you speak means some words are already in the document by
the time a model revises them.

Two ways to get a hypothesis, picked by `stream.command`:

* **Window** (default, `stream.command: []`) re-runs the configured batch backend
  over the last `stream.window_ms` of audio. That is the only way to stream with
  `whisper-cli` or `parakeet-cli`, and it needs no extra program. The window caps
  the cost of a step, so a long dictation does not get slower as it goes. The
  window never slides past audio nobody has heard yet: if a step types nothing
  (the model is mid-sentence, or the machine is slower than the speaker), the
  next window grows over the untyped part instead of skipping it, and only a
  step that types words lets the window move on.
* **Command** (`stream.command: [...]`) drives one long-lived program that
  streams partial results itself — sherpa-onnx online, Riva, `whisper-server`, or
  your own script:

```yaml
stream:
  enabled: true
  step_ms: 1000
  command: ["sherpa-onnx-online", "--tokens", "{model}/tokens.txt"]
```

The protocol is the same `{"text": …}` shape as everywhere else: write raw 16 kHz
mono 16-bit PCM to stdin, print one JSON object per line on stdout, each one the
whole transcript so far. Diagnostics go to stderr, where they end up in the error
message. When the audio stops, stdin is closed and the last line printed is the
final transcript.

When the session ends, one unhurried pass over the whole recording produces the
final text, so the ending of the last sentence is as good as a batch run. Tune it
with `--stream-step MS` and `--stream-window MS`. To see what each step decided,
run with `BLABLABLARS_DEBUG=1`:

```
typing as you speak with whisper-cli (ggml-small.bin) over the last 4s
recording for 12s...
step 1 at 2.0s: heard "And so my fellow America." -> ""
step 2 at 10.9s: heard "And so my fellow Americans, ask not what…" -> "And so my fellow"
11.0s of audio -> 22 words in 35537 ms with whisper-cli (ggml-small.bin) over the last 4s (2 steps)
```

`heard` is the cleaned hypothesis, `->` is what was allowed to be typed. A step
that heard something and typed nothing is the normal case for the first step of
a sentence.

`--json` always takes the batch path: a report describes one finished
transcription.

## Latency

Dictation is a latency feature, so the budget matters:

* **Model size.** `tiny.en` and `base.en` are for interactive use on a weak CPU;
  `small` is the usual sweet spot; a `large-v3-turbo` quantised to `q5_0` is the
  best quality per second if the machine can carry it.
* **Threads.** `transcriber.threads` becomes `whisper-cli -t N`. On an older CPU
  the difference is large: 4 threads → 65 s, 8 threads → 35 s on the same 11 s
  clip. Set it to the number of physical cores; leaving it unset lets
  whisper.cpp decide.
* **GPU.** `gpu: auto` passes nothing (whisper.cpp uses the GPU its build has)
  and `gpu: off` adds `-ng`. There is no separate GPU binary: a `whisper-cli`
  built with CUDA, Vulkan or Metal is picked up as it is. `blablablars version`
  and `--json` never claim a backend the binary does not have.
* **Streaming.** A step costs one transcription of `stream.window_ms` of audio,
  so a smaller model or a shorter window is what makes it feel immediate on a
  CPU. `--stream-step 1000 --stream-window 4000` with `base.en` is the fast
  corner; `--stream-step 4000 --stream-window 20000` with `small` the accurate
  one. Parakeet is the better choice when a step has to fit in about a second.

## Troubleshooting

| symptom | cause |
| --- | --- |
| `no models in …` | the directory is empty; the message prints a `curl` line |
| `nothing was recorded` | the capture program produced no bytes: no microphone, muted input, or a wrong device |
| `no soundcards found` from `arecord` | ALSA sees nothing; PipeWire/PulseAudio users need `parecord`/`pw-record`, which `capture.kind: auto` already prefers |
| `cannot type into the focused window` | neither `xdotool` nor `wtype` is installed; install one, use `output.kind: paste`, or use `stdout` |
| the wrong words | try a bigger model, set `language`, or add a dictionary through `transcription.raw` |
| the first press says "already running" | a session is genuinely running; press the shortcut again, or remove the stale session file in the runtime directory |
| streaming types nothing | nothing new was said; the last word of a step is held until the next step confirms it |
| `the streaming engine printed nothing within 30s` | the program in `stream.command` is too slow for `stream.command_timeout_seconds`, or it printed to stderr instead of stdout |
| macOS: no microphone | the first capture asks for microphone permission; grant it to the terminal (or to `blablablars`) in System Settings → Privacy |

`--record-child` is what `--toggle` runs; it is hidden and not part of the
interface. The detached recorder's diagnostics go to
`$XDG_STATE_HOME/blablablars/dictate.log` on Linux,
`~/Library/Logs/blablablars/` on macOS and `%LOCALAPPDATA%\blablablars\` on
Windows.

## Platforms

| | config | models | capture (`auto`) | typing (`auto`) |
| --- | --- | --- | --- | --- |
| Linux | `$XDG_CONFIG_HOME/blablablars` | `$XDG_DATA_HOME/blablablars/models` | `parecord`, `pw-record`, `arecord`, then `sox` | `wtype`, `xdotool` |
| macOS | `~/Library/Application Support/blablablars` | same, under `~/Library/Application Support` | `ffmpeg -f avfoundation`, then `sox` | `paste` (`pbcopy` + AppleScript) |
| Windows | `%APPDATA%\blablablars` | `%LOCALAPPDATA%\blablablars\models` | `ffmpeg -f dshow`, then `sox` | `paste` (`clip.exe` + PowerShell) |

The XDG variables win over the platform defaults on every system, so a sandbox or
a test can point everything somewhere harmless. `capture.kind: command` with your
own `argv` covers a device that `auto` picks badly, on any platform.

All three platforms are built and tested in CI, on x86-64 and arm64 both: the
matrix in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) is the
supported set, and it runs the whole Rust suite on each of those machines rather
than only cross-compiling. What has *not* been exercised there is a real
microphone and a real focused window — the tests record from a WAV or a shell
command — so treat the capture and typing rows above as the configuration each
desktop needs, not as a claim that a specific desktop's tool was run.
