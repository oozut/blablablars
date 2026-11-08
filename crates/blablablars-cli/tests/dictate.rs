//! End-to-end tests for `blablablars dictate`, `config` and `models`.
//!
//! They use a command transcriber and a command injector, so no model, no
//! microphone and no display are needed: the pieces are the same ones a real
//! setup uses, only the programs behind them are trivial.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use blablablars_core::voice::{audio, Audio};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_blablablars"))
}

/// A scratch directory that every invocation gets pointed at, so the tests never
/// read the developer's own config or models.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        // No parentheses or spaces: the path ends up inside a shell command.
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let serial = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "blb-dictate-{name}-{}-{serial}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("run")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// Run the binary in the sandbox with a clean environment.
    fn run(&self, arguments: &[&str]) -> Output {
        let mut command = binary();
        command
            .args(arguments)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env_remove("BLABLABLARS_CONFIG")
            .env_remove("BLABLABLARS_PLUGINS")
            .env_remove("BLABLABLARS_PLUGIN_PATH")
            .env_remove("BLABLABLARS_PLUGIN_CONFIG")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(std::process::Stdio::null());
        command.output().unwrap()
    }

    /// Write a config whose transcriber is `printf`, and whose output appends to
    /// `typed.txt`. Both are shell one-liners, so the shell quoting has to go
    /// through YAML: use block scalars to keep the quotes intact.
    fn config(&self, transcript: &str) -> PathBuf {
        let typed = self.path("typed.txt");
        let config = format!(
            r#"language: en
capture:
  kind: command
  argv: ["sh", "-c", "head -c 6400 /dev/zero; sleep 20"]
models:
  default: whisper-large-v3-turbo
transcriber:
  kind: command
  argv: ["printf", '{transcript}']
cleanup: true
output:
  kind: command
  argv: ["sh", "-c", "printf '%s' \"$1\" >> \"$0\"", "{typed}", "{{text}}"]
"#,
            typed = typed.display()
        );
        let path = self.path("voice.yml");
        std::fs::write(&path, config).unwrap();
        path
    }

    fn typed(&self) -> String {
        std::fs::read_to_string(self.path("typed.txt")).unwrap_or_default()
    }
}

fn wav(seconds: f64) -> Vec<u8> {
    let samples = (16_000.0 * seconds) as usize;
    let audio = Audio::from_s16le(16_000, &vec![100u8; samples * 2]);
    audio::to_wav_bytes(&audio)
}

fn write_wav(sandbox: &Sandbox, seconds: f64) -> PathBuf {
    let path = sandbox.path("speech.wav");
    std::fs::write(&path, wav(seconds)).unwrap();
    path
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_wav_file_is_cleaned_up_and_typed_into_the_focused_window() {
    let sandbox = Sandbox::new("wav");
    let config = sandbox.config("um, hello   there");
    let wav = write_wav(&sandbox, 1.0);

    let output = sandbox.run(&[
        "dictate",
        "--config",
        config.to_str().unwrap(),
        "--input",
        wav.to_str().unwrap(),
    ]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(sandbox.typed(), "Hello there.");
    // Nothing goes to stdout when the text is typed.
    assert_eq!(stdout(&output), "");
    // The summary names what happened.
    assert!(
        stderr(&output).contains("1.0s of audio"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn the_json_report_carries_the_transcript_and_the_trace() {
    let sandbox = Sandbox::new("json");
    let config = sandbox.config("hello there");
    let wav = write_wav(&sandbox, 1.0);

    let output = sandbox.run(&[
        "dictate",
        "--config",
        config.to_str().unwrap(),
        "--input",
        wav.to_str().unwrap(),
        "--json",
    ]);

    assert!(output.status.success(), "{}", stderr(&output));
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["text"], "Hello there.");
    assert_eq!(report["raw"], "hello there");
    assert_eq!(report["audio_seconds"], 1.0);
    assert!(report["elapsed_ms"].as_u64().is_some());
    assert!(report["trace"].is_array());
}

#[test]
fn a_file_without_speech_transcribes_to_nothing_and_types_nothing() {
    let sandbox = Sandbox::new("empty");
    let config = sandbox.config("");
    let wav = write_wav(&sandbox, 1.0);

    let output = sandbox.run(&[
        "dictate",
        "--config",
        config.to_str().unwrap(),
        "--input",
        wav.to_str().unwrap(),
    ]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(sandbox.typed(), "");
    assert!(stderr(&output).contains("nothing was transcribed"));
}

#[test]
fn cleanup_can_be_turned_off_from_the_command_line() {
    let sandbox = Sandbox::new("nocleanup");
    let config = sandbox.config("um, hello   there");
    let wav = write_wav(&sandbox, 1.0);

    let output = sandbox.run(&[
        "dictate",
        "--config",
        config.to_str().unwrap(),
        "--input",
        wav.to_str().unwrap(),
        "--no-cleanup",
    ]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(sandbox.typed(), "um, hello there");
}

#[test]
fn an_audio_file_that_is_too_short_is_refused_with_advice() {
    let sandbox = Sandbox::new("short");
    let config = sandbox.config("hello");
    let wav = write_wav(&sandbox, 0.02);

    let output = sandbox.run(&[
        "dictate",
        "--config",
        config.to_str().unwrap(),
        "--input",
        wav.to_str().unwrap(),
    ]);

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("hold the shortcut a little longer"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_unknown_output_is_named() {
    let sandbox = Sandbox::new("output");
    let config = sandbox.config("hello");
    let wav = write_wav(&sandbox, 1.0);

    let output = sandbox.run(&[
        "dictate",
        "--config",
        config.to_str().unwrap(),
        "--input",
        wav.to_str().unwrap(),
        "--output",
        "telepathy",
    ]);

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("unknown output `telepathy`"),
        "{}",
        stderr(&output)
    );
}

#[cfg(unix)]
#[test]
fn the_shortcut_starts_a_recording_and_the_next_press_types_it() {
    let sandbox = Sandbox::new("toggle");
    let config = sandbox.config("good morning");
    let typed = sandbox.path("typed.txt");
    let config = config.to_str().unwrap().to_string();

    let start = sandbox.run(&["dictate", "--config", &config, "--toggle"]);
    assert!(start.status.success(), "{}", stderr(&start));
    assert!(stderr(&start).contains("recording"), "{}", stderr(&start));

    // Enough audio for the minimum length check, and for the producer to have
    // written its bytes.
    std::thread::sleep(Duration::from_millis(400));

    let stop = sandbox.run(&["dictate", "--config", &config, "--toggle"]);
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert!(stderr(&stop).contains("stopping"), "{}", stderr(&stop));

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !typed.exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(sandbox.typed(), "Good morning.");

    // The session cleans up its socket, so a third press starts a new one.
    let again = sandbox.run(&["dictate", "--config", &config, "--toggle"]);
    assert!(again.status.success(), "{}", stderr(&again));
    assert!(stderr(&again).contains("recording"), "{}", stderr(&again));
    std::thread::sleep(Duration::from_millis(200));
    let _ = sandbox.run(&["dictate", "--config", &config, "--toggle"]);
}

#[test]
fn config_init_writes_a_file_that_reads_back() {
    let sandbox = Sandbox::new("init");
    let path = sandbox.path("written.yml");

    let output = sandbox.run(&["config", "init", "--path", path.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), path.display().to_string());

    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("models:"));
    assert!(written.contains("transcriber:"));

    // Refuses to overwrite without --force.
    let again = sandbox.run(&["config", "init", "--path", path.to_str().unwrap()]);
    assert!(!again.status.success());
    assert!(stderr(&again).contains("already exists"));

    let forced = sandbox.run(&[
        "config",
        "init",
        "--path",
        path.to_str().unwrap(),
        "--force",
    ]);
    assert!(forced.status.success(), "{}", stderr(&forced));

    let shown = sandbox.run(&["config", "show", "--path", path.to_str().unwrap()]);
    assert!(shown.status.success(), "{}", stderr(&shown));
    assert!(stdout(&shown).contains("models:"));
    assert!(stdout(&shown).contains("directory:"));
}

#[test]
fn config_path_is_the_documented_location() {
    let sandbox = Sandbox::new("path");
    let output = sandbox.run(&["config", "path"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        sandbox
            .root
            .join("config/blablablars/config.yml")
            .display()
            .to_string()
    );
}

#[test]
fn models_lists_what_is_in_the_directory_and_marks_the_default() {
    let sandbox = Sandbox::new("models");
    let directory = sandbox.path("data/blablablars/models");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("ggml-small.bin"), vec![0u8; 1024]).unwrap();
    std::fs::write(
        directory.join("ggml-large-v3-turbo-q5_0.bin"),
        vec![0u8; 2048],
    )
    .unwrap();

    let output = sandbox.run(&["models"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("ggml-small.bin"), "{text}");
    assert!(
        text.contains("* large-v3-turbo"),
        "the default model is marked: {text}"
    );

    let json = sandbox.run(&["models", "--json"]);
    let report: serde_json::Value = serde_json::from_str(&stdout(&json)).unwrap();
    assert_eq!(report["models"].as_array().unwrap().len(), 2);
    assert_eq!(report["default"], "whisper-large-v3-turbo");
    let marked = report["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["default"] == true)
        .unwrap();
    assert_eq!(
        marked["file"],
        directory
            .join("ggml-large-v3-turbo-q5_0.bin")
            .display()
            .to_string()
    );
}

#[test]
fn an_empty_model_directory_says_where_to_get_one() {
    let sandbox = Sandbox::new("nomodels");
    let output = sandbox.run(&["models"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("no models found"), "{text}");
    assert!(text.contains("huggingface.co"), "{text}");
    assert!(text.contains("ggml-large-v3-turbo"), "{text}");
}

#[test]
fn a_whisper_backend_without_a_model_says_so_and_offers_a_way_out() {
    let sandbox = Sandbox::new("nomodel");
    std::fs::create_dir_all(sandbox.path("data/blablablars/models")).unwrap();
    let wav = write_wav(&sandbox, 1.0);
    let config = sandbox.path("whisper.yml");
    std::fs::write(
        &config,
        "transcriber:
  kind: whisper-cli
  binary: whisper-cli
",
    )
    .unwrap();

    let output = sandbox.run(&[
        "dictate",
        "--config",
        config.to_str().unwrap(),
        "--input",
        wav.to_str().unwrap(),
    ]);

    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("no models"), "{message}");
    assert!(message.contains("huggingface.co"), "{message}");
}
