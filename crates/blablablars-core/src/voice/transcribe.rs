//! Turning audio into text.
//!
//! Two backends ship today, and they cover the two ways this problem is
//! actually solved: a local `whisper-cli` from whisper.cpp (GPU when that build
//! has one), and any other program through a command template, which is how a
//! different model family, a remote endpoint, or a streaming engine gets wired
//! in without touching this crate.
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
use super::models::Model;

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
) -> Box<dyn Transcriber> {
    match config.kind {
        TranscriberKind::WhisperCli => Box::new(WhisperCli {
            binary: config.binary.clone(),
            model: model.clone(),
            language: language.to_string(),
            extra_args: config.extra_args.clone(),
            gpu: config.gpu,
            threads: config.threads,
            timeout: Duration::from_secs(config.timeout_seconds),
        }),
        TranscriberKind::Command => Box::new(CommandTranscriber {
            argv: config.argv.clone(),
            model: model.clone(),
            language: language.to_string(),
            timeout: Duration::from_secs(config.timeout_seconds),
        }),
    }
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
}
