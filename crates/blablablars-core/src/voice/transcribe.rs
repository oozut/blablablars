//! Turning audio into text.
//!
//! Four backends ship today, and between them they cover the runtimes people
//! actually use: `whisper-cli` and `parakeet-cli` from the ggml toolchain (one
//! library, CPU or GPU, no runtime to install), `sherpa-onnx` for the ONNX
//! Runtime world (Whisper ONNX, NVIDIA Parakeet ONNX, transducers), and a
//! command template for everything else — `faster-whisper`, a Python
//! `onnxruntime` script, a remote endpoint, a streaming engine.
//!
//! The contract is deliberately narrow: 16 kHz mono PCM in, text out. Anything
//! smarter — partial results, language detection reporting, word timings —
//! belongs to a backend that needs it, not to the interface every backend has
//! to satisfy.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::CoreError;

use super::audio::{self, Audio};
use super::config::{GpuMode, TranscriberConfig, TranscriberKind};
use super::models::{Layout, Model, OnnxFiles};

/// Anything that can turn audio into a transcript.
pub trait Transcriber: Send + Sync {
    /// What is doing the work, for logs and `--json`.
    fn describe(&self) -> String;

    /// Transcribe `audio`. An empty string means "nothing was said".
    fn transcribe(&self, audio: &Audio) -> Result<String, CoreError>;
}

/// Build the backend the config selects, resolving the model file.
pub fn from_config(
    config: &TranscriberConfig,
    language: &str,
    model: &Model,
) -> Result<Box<dyn Transcriber>, CoreError> {
    let binary = config.program();
    let timeout = Duration::from_secs(config.timeout_seconds);

    Ok(match config.kind {
        TranscriberKind::WhisperCli => Box::new(WhisperCli {
            binary,
            model: model.clone(),
            language: language.to_string(),
            extra_args: config.extra_args.clone(),
            gpu: config.gpu,
            threads: config.threads,
            timeout,
        }),
        TranscriberKind::Parakeet => Box::new(Parakeet {
            binary,
            model: model.clone(),
            extra_args: config.extra_args.clone(),
            gpu: config.gpu,
            threads: config.threads,
            timeout,
        }),
        TranscriberKind::SherpaOnnx => Box::new(SherpaOnnx::new(
            binary,
            model,
            language,
            config.extra_args.clone(),
            config.threads,
            timeout,
        )?),
        TranscriberKind::Command => Box::new(CommandTranscriber {
            argv: config.argv.clone(),
            model: model.clone(),
            language: language.to_string(),
            timeout,
        }),
    })
}

/// The `whisper-cli` program from whisper.cpp.
pub struct WhisperCli {
    pub binary: String,
    pub model: Model,
    pub language: String,
    pub extra_args: Vec<String>,
    pub gpu: GpuMode,
    pub threads: Option<u32>,
    pub timeout: Duration,
}

impl WhisperCli {
    /// The exact command line, exposed so a test can pin the flags that make
    /// the output usable without parsing it back out of stdout.
    pub fn argv(&self, wav: &Path) -> Vec<String> {
        let mut argv = vec![
            "-m".to_string(),
            self.model.path.display().to_string(),
            "-f".to_string(),
            wav.display().to_string(),
            "-l".to_string(),
            self.language.clone(),
            // No timestamps, no progress chatter: stdout must be the transcript.
            "-nt".to_string(),
            "-np".to_string(),
        ];
        if self.gpu == GpuMode::Off {
            argv.push("-ng".to_string());
        }
        if let Some(threads) = self.threads {
            argv.push("-t".to_string());
            argv.push(threads.to_string());
        }
        argv.extend(self.extra_args.iter().cloned());
        argv
    }
}

impl Transcriber for WhisperCli {
    fn describe(&self) -> String {
        format!("whisper-cli ({})", self.model.file_name)
    }

    fn transcribe(&self, audio: &Audio) -> Result<String, CoreError> {
        let wav = TempWav::write(audio)?;
        let mut command = Command::new(&self.binary);
        command.args(self.argv(wav.path()));

        let output = run_with_timeout(command, self.timeout, &self.binary)?;
        Ok(transcript_from_stdout(&output.stdout))
    }
}

/// NVIDIA Parakeet TDT through the same ggml toolchain as whisper.cpp.
///
/// It is a different model family with the same shape of interface, and on a
/// CPU it is usually several times faster than Whisper at the same quality. The
/// models are multilingual and detect the language themselves, so `language` is
/// not passed: there is no flag for it.
#[derive(Debug)]
pub struct Parakeet {
    pub binary: String,
    pub model: Model,
    pub extra_args: Vec<String>,
    pub gpu: GpuMode,
    pub threads: Option<u32>,
    pub timeout: Duration,
}

impl Parakeet {
    /// The exact command line, so the flag handling can be pinned by a test.
    pub fn argv(&self, wav: &Path) -> Vec<String> {
        let mut argv = vec![
            "-m".to_string(),
            self.model.path.display().to_string(),
            "-f".to_string(),
            wav.display().to_string(),
            // `parakeet-cli` prints timings and token counts on stdout by
            // default; without this flag they would be typed as dictation.
            "-np".to_string(),
        ];
        if self.gpu == GpuMode::Off {
            argv.push("-ng".to_string());
        }
        if let Some(threads) = self.threads {
            argv.push("-t".to_string());
            argv.push(threads.to_string());
        }
        argv.extend(self.extra_args.iter().cloned());
        argv
    }
}

impl Transcriber for Parakeet {
    fn describe(&self) -> String {
        format!("parakeet-cli ({})", self.model.file_name)
    }

    fn transcribe(&self, audio: &Audio) -> Result<String, CoreError> {
        let wav = TempWav::write(audio)?;
        let mut command = Command::new(&self.binary);
        command.args(self.argv(wav.path()));

        let output = run_with_timeout(command, self.timeout, &self.binary)?;
        Ok(transcript_from_stdout(&output.stdout))
    }
}

/// ONNX Runtime models through `sherpa-onnx`.
///
/// An ONNX model is a folder, not a file, and sherpa-onnx wants each part named
/// on the command line: `tokens.txt` plus an encoder and a decoder for Whisper,
/// and an encoder, a decoder and a joiner for a transducer (NVIDIA Parakeet,
/// Zipformer, Conformer). The folder is inspected once, when the backend is
/// built, so a model the toolchain does not recognise fails before recording
/// starts rather than after.
#[derive(Debug)]
pub struct SherpaOnnx {
    pub binary: String,
    pub model: Model,
    pub files: OnnxFiles,
    pub layout: Layout,
    pub language: String,
    pub extra_args: Vec<String>,
    pub threads: Option<u32>,
    pub timeout: Duration,
}

impl SherpaOnnx {
    fn new(
        binary: String,
        model: &Model,
        language: &str,
        extra_args: Vec<String>,
        threads: Option<u32>,
        timeout: Duration,
    ) -> Result<Self, CoreError> {
        let layout = model.layout();
        let Some(files) = model.onnx_files() else {
            return Err(CoreError::Voice(format!(
                "`{}` is not an ONNX model folder: it needs `tokens.txt` and an \
                 `*encoder*.onnx`, which sherpa-onnx reads. Pass `--model DIR`, or \
                 use a ggml `*.bin` model with `transcriber.kind: whisper-cli` or \
                 `parakeet`.",
                model.path.display()
            )));
        };
        Ok(Self {
            binary,
            model: model.clone(),
            files,
            layout,
            language: language.to_string(),
            extra_args,
            threads,
            timeout,
        })
    }

    /// The exact command line, pinned by tests because the flag names come from
    /// sherpa-onnx and are not guessable.
    pub fn argv(&self, wav: &Path) -> Vec<String> {
        let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
        let mut argv = vec![flag("tokens", &self.files.tokens)];

        match self.layout {
            Layout::TransducerOnnx => {
                argv.push(flag("encoder", &self.files.encoder));
                if let Some(decoder) = &self.files.decoder {
                    argv.push(flag("decoder", decoder));
                }
                if let Some(joiner) = &self.files.joiner {
                    argv.push(flag("joiner", joiner));
                }
            }
            _ => {
                argv.push(flag("whisper-encoder", &self.files.encoder));
                if let Some(decoder) = &self.files.decoder {
                    argv.push(flag("whisper-decoder", decoder));
                }
                argv.push("--whisper-task=transcribe".to_string());
                if !self.language.eq_ignore_ascii_case("auto") {
                    argv.push(format!("--whisper-language={}", self.language));
                }
            }
        }

        if let Some(threads) = self.threads {
            argv.push(format!("--num-threads={threads}"));
        }
        argv.extend(self.extra_args.iter().cloned());
        // The audio file is positional and comes last.
        argv.push(wav.display().to_string());
        argv
    }
}

impl Transcriber for SherpaOnnx {
    fn describe(&self) -> String {
        format!("sherpa-onnx ({})", self.model.file_name)
    }

    fn transcribe(&self, audio: &Audio) -> Result<String, CoreError> {
        let wav = TempWav::write(audio)?;
        let mut command = Command::new(&self.binary);
        command.args(self.argv(wav.path()));

        let output = run_with_timeout(command, self.timeout, &self.binary)?;
        Ok(transcript_from_stdout(&output.stdout))
    }
}

/// Any program: `argv` is a template, stdout is the transcript.
pub struct CommandTranscriber {
    pub argv: Vec<String>,
    pub model: Model,
    pub language: String,
    pub timeout: Duration,
}

impl Transcriber for CommandTranscriber {
    fn describe(&self) -> String {
        format!(
            "command ({})",
            self.argv.first().cloned().unwrap_or_else(|| "?".into())
        )
    }

    fn transcribe(&self, audio: &Audio) -> Result<String, CoreError> {
        let wav = TempWav::write(audio)?;
        let argv = substitute(&self.argv, wav.path(), &self.model, &self.language);
        let program = argv
            .first()
            .cloned()
            .ok_or_else(|| CoreError::Voice("`transcriber.argv` is empty".to_string()))?;
        let mut command = Command::new(&program);
        command.args(&argv[1..]);

        let output = run_with_timeout(command, self.timeout, &program)?;
        Ok(transcript_from_stdout(&output.stdout))
    }
}

/// Replace `{audio}`, `{model}`, `{model_name}` and `{language}` in a template.
pub fn substitute(template: &[String], audio: &Path, model: &Model, language: &str) -> Vec<String> {
    template
        .iter()
        .map(|argument| {
            argument
                .replace("{audio}", &audio.display().to_string())
                .replace("{model_name}", &model.name)
                .replace("{model}", &model.path.display().to_string())
                .replace("{language}", language)
        })
        .collect()
}

/// A transcript from a program's stdout: first non-empty line, trimmed.
///
/// Taking the first line keeps a backend that prints a banner or a warning on
/// stdout from typing it into the user's document.
fn transcript_from_stdout(stdout: &[u8]) -> String {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

/// A WAV file that removes itself, so the audio never outlives the call.
struct TempWav {
    path: PathBuf,
}

impl TempWav {
    fn write(audio: &Audio) -> Result<Self, CoreError> {
        let path = std::env::temp_dir().join(format!(
            "blablablars-{}-{}.wav",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        audio::write_wav(&path, audio)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempWav {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Run a program with a deadline, capturing both streams.
///
/// `Command::output` has no timeout, and a model that hangs on a bad GPU driver
/// would otherwise freeze the shortcut with a recording session stuck open.
fn run_with_timeout(
    mut command: Command,
    timeout: Duration,
    program: &str,
) -> Result<std::process::Output, CoreError> {
    use std::io::Read;

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => CoreError::Voice(format!(
                "`{program}` was not found on PATH. Install it, or point the config at the \
                 right program."
            )),
            _ => CoreError::Voice(format!("cannot run `{program}`: {error}")),
        })?;

    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let stdout_thread = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer);
        buffer
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                return Err(CoreError::Voice(format!(
                    "cannot wait for `{program}`: {error}"
                )));
            }
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CoreError::Voice(format!(
                "`{program}` did not finish within {}s",
                timeout.as_secs_f32()
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();

    if !status.success() {
        let tail = String::from_utf8_lossy(&stderr);
        let tail = tail.trim();
        let detail = if tail.is_empty() {
            format!("exit status {status}")
        } else {
            tail.lines()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        };
        return Err(CoreError::Voice(format!("`{program}` failed: {detail}")));
    }

    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::audio::WHISPER_RATE;

    fn model() -> Model {
        Model {
            name: "small".to_string(),
            file_name: "ggml-small.bin".to_string(),
            path: PathBuf::from("/models/ggml-small.bin"),
            bytes: 10,
        }
    }

    fn audio() -> Audio {
        Audio::new(WHISPER_RATE, vec![0; 1600])
    }

    fn command(argv: &[&str], timeout: Duration) -> CommandTranscriber {
        CommandTranscriber {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            model: model(),
            language: "en".to_string(),
            timeout,
        }
    }

    #[test]
    fn the_whisper_flags_keep_stdout_to_the_transcript() {
        let transcriber = WhisperCli {
            binary: "whisper-cli".into(),
            model: model(),
            language: "en".into(),
            extra_args: vec!["--beam-size".into(), "5".into()],
            gpu: GpuMode::Auto,
            threads: None,
            timeout: Duration::from_secs(5),
        };
        let argv = transcriber.argv(Path::new("/tmp/a.wav"));

        assert_eq!(argv[0..2], ["-m", "/models/ggml-small.bin"]);
        assert_eq!(argv[2..4], ["-f", "/tmp/a.wav"]);
        assert_eq!(argv[4..6], ["-l", "en"]);
        assert!(argv.contains(&"-nt".to_string()));
        assert!(argv.contains(&"-np".to_string()));
        assert!(!argv.contains(&"-ng".to_string()), "GPU is on by default");
        assert_eq!(argv.last().unwrap(), "5");
        assert!(
            !argv.contains(&"-t".to_string()),
            "the thread count is left to the backend unless configured"
        );
    }

    #[test]
    fn a_configured_thread_count_is_passed_through() {
        let transcriber = WhisperCli {
            binary: "whisper-cli".into(),
            model: model(),
            language: "en".into(),
            extra_args: Vec::new(),
            gpu: GpuMode::Auto,
            threads: Some(8),
            timeout: Duration::from_secs(5),
        };
        let argv = transcriber.argv(Path::new("/tmp/a.wav"));
        let index = argv.iter().position(|flag| flag == "-t").unwrap();
        assert_eq!(argv[index + 1], "8");
    }

    #[test]
    fn gpu_off_is_the_only_flag_that_disables_it() {
        let transcriber = WhisperCli {
            binary: "whisper-cli".into(),
            model: model(),
            language: "auto".into(),
            extra_args: Vec::new(),
            gpu: GpuMode::Off,
            threads: None,
            timeout: Duration::from_secs(5),
        };
        assert!(transcriber
            .argv(Path::new("/tmp/a.wav"))
            .contains(&"-ng".to_string()));
        assert_eq!(transcriber.describe(), "whisper-cli (ggml-small.bin)");
    }

    #[test]
    fn templates_fill_in_every_placeholder() {
        let template = vec![
            "engine".to_string(),
            "--audio={audio}".to_string(),
            "--weights".to_string(),
            "{model}".to_string(),
            "{model_name}".to_string(),
            "{language}".to_string(),
        ];
        let argv = substitute(&template, Path::new("/tmp/x.wav"), &model(), "fr");

        assert_eq!(
            argv,
            [
                "engine",
                "--audio=/tmp/x.wav",
                "--weights",
                "/models/ggml-small.bin",
                "small",
                "fr"
            ]
        );
    }

    #[test]
    fn a_command_transcriber_reads_stdout() {
        let transcriber = command(
            &["sh", "-c", "echo '  hello from {model_name}  '"],
            Duration::from_secs(10),
        );
        assert_eq!(
            transcriber.transcribe(&audio()).unwrap(),
            "hello from small"
        );
    }

    #[test]
    fn a_banner_line_is_joined_but_blank_lines_are_dropped() {
        assert_eq!(transcript_from_stdout(b"\n  one \n\n two  \n"), "one two");
        assert_eq!(transcript_from_stdout(b""), "");
    }

    #[test]
    fn background_chatter_on_stdout_is_joined_too() {
        // Not ideal, but it never invents text, and a backend that wants more
        // control can print exactly one line.
        assert_eq!(transcript_from_stdout(b"a\nb\n"), "a b");
    }

    #[test]
    fn a_failing_program_reports_its_stderr() {
        let transcriber = command(
            &["sh", "-c", "echo 'model file is corrupt' >&2; exit 3"],
            Duration::from_secs(10),
        );
        let error = transcriber.transcribe(&audio()).unwrap_err().to_string();
        assert!(error.contains("model file is corrupt"), "{error}");
    }

    #[test]
    fn a_missing_program_says_what_to_install() {
        let transcriber = command(&["blablablars-definitely-missing"], Duration::from_secs(10));
        let error = transcriber.transcribe(&audio()).unwrap_err().to_string();
        assert!(error.contains("not found on PATH"), "{error}");
    }

    #[test]
    fn a_hung_program_is_killed_at_the_deadline() {
        let transcriber = command(&["sh", "-c", "sleep 30"], Duration::from_millis(200));
        let started = Instant::now();
        let error = transcriber.transcribe(&audio()).unwrap_err().to_string();
        assert!(error.contains("did not finish"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_temporary_wav_is_removed_afterwards() {
        let transcriber = CommandTranscriber {
            argv: vec![
                "sh".to_string(),
                "-c".to_string(),
                "cat \"$0\" > /dev/null; printf '%s' \"$0\"".to_string(),
                "{audio}".to_string(),
            ],
            model: model(),
            language: "en".to_string(),
            timeout: Duration::from_secs(10),
        };
        // `$0` is the substituted audio path, printed back for the assertion.
        let printed = transcriber.transcribe(&audio()).unwrap();

        assert!(printed.ends_with(".wav"), "{printed}");
        assert!(!Path::new(&printed).exists(), "the WAV must be cleaned up");
    }

    #[test]
    fn an_empty_argv_is_rejected() {
        let transcriber = command(&[], Duration::from_secs(1));
        assert!(transcriber.transcribe(&audio()).is_err());
    }
    fn scratch_model(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("blb-transcribe-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        for (name, body) in files {
            std::fs::write(directory.join(name), body).unwrap();
        }
        directory
    }

    fn model_at(path: PathBuf) -> Model {
        Model {
            name: "x".into(),
            file_name: path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default(),
            path,
            bytes: 1,
        }
    }

    #[test]
    fn parakeet_is_not_asked_to_print_its_progress() {
        // `parakeet-cli` writes timings and token counts on stdout by default.
        // Anything it prints beyond the transcript is typed as dictation, so
        // `-np` is not cosmetic.
        let transcriber = Parakeet {
            binary: "parakeet-cli".into(),
            model: model_at(PathBuf::from("/models/ggml-parakeet-tdt-0.6b-v3-f16.bin")),
            extra_args: vec!["--print-segments".into()],
            gpu: GpuMode::Off,
            threads: Some(6),
            timeout: Duration::from_secs(10),
        };
        let argv = transcriber.argv(Path::new("/tmp/a.wav"));
        assert_eq!(argv[0], "-m");
        assert_eq!(argv[1], "/models/ggml-parakeet-tdt-0.6b-v3-f16.bin");
        assert!(argv.contains(&"-np".to_string()));
        assert!(argv.contains(&"-ng".to_string()));
        assert!(argv.windows(2).any(|pair| pair == ["-t", "6"]));
        // Extra arguments come last, so they can override a default.
        assert_eq!(argv.last().unwrap(), "--print-segments");
    }

    #[test]
    fn an_onnx_transducer_gets_encoder_decoder_joiner_and_tokens() {
        let directory = scratch_model(
            "transducer",
            &[
                ("tokens.txt", "a\n"),
                ("encoder.int8.onnx", ""),
                ("decoder.int8.onnx", ""),
                ("joiner.int8.onnx", ""),
            ],
        );
        let model = model_at(directory);
        let transcriber = SherpaOnnx::new(
            "sherpa-onnx-offline".into(),
            &model,
            "auto",
            vec![],
            Some(4),
            Duration::from_secs(10),
        )
        .unwrap();

        let argv = transcriber.argv(Path::new("/tmp/a.wav"));
        assert!(argv.iter().any(|arg| arg.starts_with("--tokens=")));
        assert!(argv.iter().any(|arg| arg.starts_with("--encoder=")));
        assert!(argv.iter().any(|arg| arg.starts_with("--decoder=")));
        assert!(argv.iter().any(|arg| arg.starts_with("--joiner=")));
        assert!(
            !argv.iter().any(|arg| arg.starts_with("--whisper-encoder=")),
            "a transducer is not a Whisper model"
        );
        assert!(argv.contains(&"--num-threads=4".to_string()));
        assert_eq!(argv.last().unwrap(), "/tmp/a.wav");
    }

    #[test]
    fn an_onnx_whisper_gets_its_language_and_no_joiner_flag() {
        let directory = scratch_model(
            "whisper-onnx",
            &[
                ("tiny.en-tokens.txt", "a\n"),
                ("tiny.en-encoder.int8.onnx", ""),
                ("tiny.en-decoder.int8.onnx", ""),
            ],
        );
        let model = model_at(directory);

        let german = SherpaOnnx::new(
            "sherpa-onnx-offline".into(),
            &model,
            "de",
            vec![],
            None,
            Duration::from_secs(10),
        )
        .unwrap()
        .argv(Path::new("/tmp/a.wav"));
        assert!(german
            .iter()
            .any(|arg| arg.starts_with("--whisper-encoder=")));
        assert!(german.iter().any(|arg| arg == "--whisper-language=de"));
        assert!(!german.iter().any(|arg| arg.starts_with("--tokens=tiny")));
        assert!(!german.iter().any(|arg| arg.starts_with("--encoder=")));

        // `auto` means "let the model decide", which is the absence of the flag.
        let auto = SherpaOnnx::new(
            "sherpa-onnx-offline".into(),
            &model,
            "auto",
            vec![],
            None,
            Duration::from_secs(10),
        )
        .unwrap()
        .argv(Path::new("/tmp/a.wav"));
        assert!(!auto
            .iter()
            .any(|arg| arg.starts_with("--whisper-language=")));
    }

    #[test]
    fn a_folder_that_is_not_an_onnx_model_is_rejected_before_recording() {
        let directory = scratch_model("not-a-model", &[("readme.md", "hello")]);
        let error = SherpaOnnx::new(
            "sherpa-onnx-offline".into(),
            &model_at(directory),
            "en",
            vec![],
            None,
            Duration::from_secs(10),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("not an ONNX model folder"), "{error}");
        assert!(error.contains("whisper-cli"), "{error}");
    }

    #[test]
    fn a_command_backend_can_ignore_the_model_entirely() {
        // A streaming engine or a remote API has no local model file; the name
        // is still passed through, as written.
        let transcriber = command(
            &["sh", "-c", "echo \"model is $1\"", "sh", "{model}"],
            Duration::from_secs(5),
        );
        let _ = transcriber;
        let rendered = substitute(
            &["--model".to_string(), "{model_name}".to_string()],
            Path::new("/tmp/a.wav"),
            &Model::unresolved("Voxtral-Mini-3B"),
            "en",
        );
        assert_eq!(rendered, ["--model", "Voxtral-Mini-3B"]);
    }
}
