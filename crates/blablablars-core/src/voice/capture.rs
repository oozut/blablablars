//! Recording from the microphone.
//!
//! Capture is a child process writing raw 16-bit mono PCM to stdout. That is a
//! deliberate choice over linking an audio library: it needs no ALSA headers to
//! build, it works with whatever sound server the user actually runs, and the
//! list of producers covers the three that any desktop has
//! (`parecord`, `pw-record`, `arecord`) plus any program the user names, which
//! is how macOS and Windows get a backend without this crate growing one.
//!
//! Nothing is buffered in the child: the pipe is drained continuously, so
//! stopping the session cannot lose the last second of speech to a program's
//! unflushed buffer.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::CoreError;

use super::audio::{Audio, WHISPER_RATE};
use super::config::{CaptureConfig, CaptureKind};
use super::inject::find_in_path;

/// How long to give a producer to exit on its own before it is killed.
const STOP_GRACE: Duration = Duration::from_millis(750);

/// A capture session in progress.
pub struct Recording {
    program: String,
    child: Child,
    /// Everything captured so far, filled by the reader thread. Shared so a
    /// streaming session can look at the last few seconds without waiting for
    /// the recording to end.
    audio: Arc<Mutex<Vec<u8>>>,
    /// How much of `audio` the caller has already been given by [`drain`].
    drained: usize,
    audio_reader: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    stderr_reader: Option<std::thread::JoinHandle<Vec<u8>>>,
    started: Instant,
    max_seconds: f64,
}

impl std::fmt::Debug for Recording {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recording")
            .field("program", &self.program)
            .field("elapsed", &self.elapsed())
            .finish()
    }
}

impl Recording {
    /// Start capturing. The microphone is open when this returns.
    pub fn start(config: &CaptureConfig) -> Result<Self, CoreError> {
        let (program, arguments) = capture_command(config)?;

        let mut command = Command::new(&program);
        command
            .args(&arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Its own process group, so stopping the session also stops anything the
        // producer spawned (`parecord | tee`, a wrapper script, a filter).
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }

        let mut child = command.spawn().map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                CoreError::Voice(format!("cannot record: `{program}` was not found on PATH"))
            }
            _ => CoreError::Voice(format!("cannot record: cannot run `{program}`: {error}")),
        })?;

        let mut stdout = child.stdout.take().expect("stdout is piped");
        let mut stderr = child.stderr.take().expect("stderr is piped");
        let budget = byte_budget(config.max_seconds);

        let audio = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&audio);
        let audio_reader = std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        let mut buffer = sink.lock().unwrap_or_else(|error| error.into_inner());
                        if buffer.len() < budget {
                            let room = budget - buffer.len();
                            buffer.extend_from_slice(&chunk[..read.min(room)]);
                        }
                        // Past the cap the pipe keeps being drained, so the
                        // producer never blocks; the caller is about to stop it.
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        });

        let stderr_reader = std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = stderr.read_to_end(&mut buffer);
            buffer
        });

        Ok(Self {
            program,
            child,
            audio,
            drained: 0,
            audio_reader: Some(audio_reader),
            stderr_reader: Some(stderr_reader),
            started: Instant::now(),
            max_seconds: config.max_seconds,
        })
    }

    /// What is recording, for logs.
    pub fn describe(&self) -> String {
        self.program.clone()
    }

    /// How long the session has been open.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// True when the capture cap has been reached.
    pub fn at_limit(&self) -> bool {
        self.started.elapsed().as_secs_f64() >= self.max_seconds
    }

    /// The audio captured since the last call, for a streaming session.
    ///
    /// Nothing is taken twice: the full recording is still returned by
    /// [`stop`], so a streaming session can type as it goes and still hand the
    /// final transcript the whole utterance.
    pub fn drain(&mut self) -> Audio {
        let buffer = self.audio.lock().unwrap_or_else(|error| error.into_inner());
        if self.drained >= buffer.len() {
            return Audio::new(WHISPER_RATE, Vec::new());
        }
        let bytes = buffer[self.drained..].to_vec();
        self.drained = buffer.len();
        drop(buffer);
        Audio::from_s16le(WHISPER_RATE, &bytes)
    }

    /// Stop recording and return what was captured.
    pub fn stop(mut self) -> Result<Audio, CoreError> {
        self.terminate()?;
        let bytes = self.take_audio()?;
        let stderr = self.take_stderr();

        if bytes.is_empty() {
            let detail = String::from_utf8_lossy(&stderr);
            let detail = detail.trim();
            let hint = if detail.is_empty() {
                "the capture program produced no audio".to_string()
            } else {
                detail.lines().take(3).collect::<Vec<_>>().join("\n")
            };
            return Err(CoreError::Voice(format!(
                "nothing was recorded with `{}`: {hint}",
                self.program
            )));
        }

        Ok(Audio::from_s16le(WHISPER_RATE, &bytes))
    }

    fn terminate(&mut self) -> Result<(), CoreError> {
        #[cfg(unix)]
        signal::terminate(&self.child);

        // Wait for a clean exit, then insist.
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return Ok(()),
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => {
                    return Err(CoreError::Voice(format!(
                        "cannot stop `{}`: {error}",
                        self.program
                    )))
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        Ok(())
    }

    fn take_audio(&mut self) -> Result<Vec<u8>, CoreError> {
        let Some(handle) = self.audio_reader.take() else {
            return Ok(Vec::new());
        };
        match handle.join() {
            Ok(Ok(())) => Ok(self
                .audio
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()),
            Ok(Err(error)) => Err(CoreError::Voice(format!(
                "cannot read audio from `{}`: {error}",
                self.program
            ))),
            Err(_) => Err(CoreError::Voice(
                "the audio reader thread panicked".to_string(),
            )),
        }
    }

    fn take_stderr(&mut self) -> Vec<u8> {
        self.stderr_reader
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if self.audio_reader.is_some() {
            let _ = self.terminate();
            let _ = self.take_audio();
            let _ = self.take_stderr();
        }
    }
}

/// A capture program and how to build its arguments.
type Candidate = (&'static str, fn(Option<&str>) -> Vec<String>);

/// The producers to try on this desktop, best first.
///
/// Linux gets the three sound-server tools it always has. macOS and Windows
/// have no shipped command-line recorder, so they get `ffmpeg` (which every
/// install of either tends to have) and `sox`, whose `-d` means "the default
/// device" on all three systems. Any of them can be overridden with
/// `capture.kind: command`.
fn candidates() -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = Vec::new();
    if cfg!(target_os = "linux") || !cfg!(any(target_os = "macos", windows)) {
        candidates.push(("parecord", parecord_args));
        candidates.push(("pw-record", pw_record_args));
        candidates.push(("arecord", arecord_args));
    }
    if cfg!(target_os = "macos") {
        candidates.push(("ffmpeg", ffmpeg_avfoundation_args));
    }
    if cfg!(windows) {
        candidates.push(("ffmpeg", ffmpeg_dshow_args));
    }
    // A universal last resort: `sox -d` opens the default input device of
    // ALSA, CoreAudio or WAVE, whichever the host has.
    candidates.push(("sox", sox_args));
    candidates
}

/// How to install something that can record, in this platform's spelling.
fn install_hint() -> String {
    if cfg!(windows) {
        "Install `ffmpeg` (or SoX) and put it on PATH, or set `capture.argv` in the config \
         to any program that writes raw 16-bit mono 16 kHz PCM to stdout."
            .to_string()
    } else if cfg!(target_os = "macos") {
        "Install `ffmpeg` (`brew install ffmpeg`) or SoX (`brew install sox`), or set \
         `capture.argv` to any program that writes raw 16-bit mono 16 kHz PCM to stdout."
            .to_string()
    } else {
        "Install PulseAudio's `parecord`, PipeWire's `pw-record` or ALSA's `arecord` \
         (or `sox`), or set `capture.argv` in the config to any program that writes raw \
         16-bit mono 16 kHz PCM to stdout."
            .to_string()
    }
}

/// The producer command for this config, with the arguments the backend wants.
pub fn capture_command(config: &CaptureConfig) -> Result<(String, Vec<String>), CoreError> {
    if config.kind == CaptureKind::Command {
        let mut argv = config.argv.clone();
        if argv.is_empty() {
            return Err(CoreError::Voice(
                "`capture.kind: command` needs `capture.argv`".to_string(),
            ));
        }
        let program = argv.remove(0);
        return Ok((program, argv));
    }

    let candidates = candidates();
    for (program, build) in &candidates {
        if find_in_path(program).is_some() {
            return Ok(((*program).to_string(), build(config.device.as_deref())));
        }
    }

    let mut programs: Vec<&str> = candidates.iter().map(|(name, _)| *name).collect();
    programs.dedup();
    Err(CoreError::Voice(format!(
        "cannot record: none of {} is installed. {}",
        programs.join(", "),
        install_hint()
    )))
}

/// PulseAudio/PipeWire-pulse: `--raw` writes to stdout when no file is given.
fn parecord_args(device: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        "--format=s16le".to_string(),
        format!("--rate={WHISPER_RATE}"),
        "--channels=1".to_string(),
        "--raw".to_string(),
    ];
    if let Some(device) = device {
        argv.push(format!("--device={device}"));
    }
    argv
}

/// PipeWire native: `-` means stdout.
fn pw_record_args(device: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        "--rate".to_string(),
        WHISPER_RATE.to_string(),
        "--channels".to_string(),
        "1".to_string(),
        "--format".to_string(),
        "s16".to_string(),
    ];
    if let Some(device) = device {
        argv.push("--target".to_string());
        argv.push(device.to_string());
    }
    argv.push("-".to_string());
    argv
}

/// ALSA.
fn arecord_args(device: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        "-t".to_string(),
        "raw".to_string(),
        "-f".to_string(),
        "S16_LE".to_string(),
        "-r".to_string(),
        WHISPER_RATE.to_string(),
        "-c".to_string(),
        "1".to_string(),
        "-q".to_string(),
    ];
    if let Some(device) = device {
        argv.push("-D".to_string());
        argv.push(device.to_string());
    }
    argv
}

/// ffmpeg on macOS: CoreAudio inputs are addressed as `:INDEX` (or
/// `VIDEO:AUDIO`, which is passed through unchanged).
fn ffmpeg_avfoundation_args(device: Option<&str>) -> Vec<String> {
    let input = match device {
        Some(device) if device.contains(':') => device.to_string(),
        Some(device) => format!(":{device}"),
        None => ":0".to_string(),
    };
    vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-f".to_string(),
        "avfoundation".to_string(),
        "-i".to_string(),
        input,
        "-ac".to_string(),
        "1".to_string(),
        "-ar".to_string(),
        WHISPER_RATE.to_string(),
        "-f".to_string(),
        "s16le".to_string(),
        "-".to_string(),
    ]
}

/// ffmpeg on Windows: DirectShow needs the device's own name, which only the
/// user can provide (`ffmpeg -list_devices true -f dshow -i dummy`).
fn ffmpeg_dshow_args(device: Option<&str>) -> Vec<String> {
    let input = format!("audio={}", device.unwrap_or("default"));
    vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-f".to_string(),
        "dshow".to_string(),
        "-i".to_string(),
        input,
        "-ac".to_string(),
        "1".to_string(),
        "-ar".to_string(),
        WHISPER_RATE.to_string(),
        "-f".to_string(),
        "s16le".to_string(),
        "-".to_string(),
    ]
}

/// SoX: `-d` is the platform's default input device, and `-t raw -` writes the
/// samples to stdout.
fn sox_args(_device: Option<&str>) -> Vec<String> {
    vec![
        "-q".to_string(),
        "-d".to_string(),
        "-r".to_string(),
        WHISPER_RATE.to_string(),
        "-c".to_string(),
        "1".to_string(),
        "-e".to_string(),
        "signed-integer".to_string(),
        "-b".to_string(),
        "16".to_string(),
        "-t".to_string(),
        "raw".to_string(),
        "-".to_string(),
    ]
}

/// How many bytes `max_seconds` of 16 kHz mono 16-bit audio takes.
pub fn byte_budget(max_seconds: f64) -> usize {
    (max_seconds.max(0.0) * f64::from(WHISPER_RATE) * 2.0) as usize
}

/// Asking a child to stop politely, so it flushes what it already has.
#[cfg(unix)]
mod signal {
    #![allow(unsafe_code)]

    use std::process::Child;

    /// SIGTERM: capture programs flush and close on it, and a SIGKILL would
    /// drop whatever they still hold in their own buffer. The negation sends
    /// it to the whole process group the recorder was started in.
    pub fn terminate(child: &Child) {
        let group = -(child.id() as libc::pid_t);
        // SAFETY: `group` is the process group of a child this process started,
        // and SIGTERM cannot be caught in a way that makes `kill` itself unsafe.
        // A failure (the child already exited) is ignored: `stop` then falls
        // back to `Child::kill`.
        unsafe {
            libc::kill(group, libc::SIGTERM);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_raw(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "blb-capture-{tag}-{}-{:?}.raw",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn command_capture(argv: &[&str]) -> CaptureConfig {
        CaptureConfig {
            kind: CaptureKind::Command,
            device: None,
            argv: argv.iter().map(|s| s.to_string()).collect(),
            max_seconds: 60.0,
        }
    }

    #[test]
    fn an_auto_capture_prefers_the_pulse_program_and_sets_the_format() {
        // The sandbox has `arecord` only; whichever is found must carry the
        // flags that make the output exactly what whisper wants.
        let (program, argv) = capture_command(&CaptureConfig::default()).unwrap();
        match program.as_str() {
            "parecord" => {
                assert!(argv.contains(&"--raw".to_string()));
                assert!(argv.contains(&format!("--rate={WHISPER_RATE}")));
            }
            "pw-record" => {
                assert!(argv.contains(&WHISPER_RATE.to_string()));
                assert_eq!(argv.last().unwrap(), "-");
            }
            "arecord" => {
                assert!(argv.contains(&"S16_LE".to_string()));
                assert!(argv.contains(&"raw".to_string()));
            }
            other => panic!("unexpected capture program {other}"),
        }
    }

    #[test]
    fn a_device_is_passed_the_way_each_backend_expects_it() {
        assert!(parecord_args(Some("mic")).contains(&"--device=mic".to_string()));
        let pw = pw_record_args(Some("mic"));
        assert_eq!(&pw[pw.len() - 3..], ["--target", "mic", "-"]);
        let alsa = arecord_args(Some("hw:1"));
        assert_eq!(&alsa[alsa.len() - 2..], ["-D", "hw:1"]);
    }

    #[test]
    fn a_command_capture_is_used_verbatim() {
        let (program, argv) = capture_command(&command_capture(&["cat", "/tmp/some.raw"])).unwrap();
        assert_eq!(program, "cat");
        assert_eq!(argv, ["/tmp/some.raw"]);
    }

    #[test]
    fn a_recorded_session_returns_its_samples() {
        let raw = scratch_raw("ok", &[1, 0, 2, 0, 3, 0]);
        let config = command_capture(&["cat", raw.to_str().unwrap()]);

        let recording = Recording::start(&config).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let audio = recording.stop().unwrap();

        assert_eq!(audio.samples(), [1, 2, 3]);
        assert_eq!(audio.rate(), WHISPER_RATE);
        let _ = std::fs::remove_file(raw);
    }

    #[test]
    fn a_streaming_session_can_look_at_the_audio_before_it_ends() {
        // The producer trickles samples out, so a drain in the middle sees the
        // first half and the second drain sees only what came after it.
        let config = command_capture(&[
            "sh",
            "-c",
            "printf '\\001\\000\\002\\000'; sleep 0.3; printf '\\003\\000'; sleep 0.3",
        ]);
        let mut recording = Recording::start(&config).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let first = recording.drain();
        assert_eq!(first.samples(), [1, 2], "the first drain takes what exists");

        std::thread::sleep(Duration::from_millis(300));
        let second = recording.drain();
        assert_eq!(
            second.samples(),
            [3],
            "the second drain takes only the rest"
        );

        assert!(recording.drain().is_empty(), "nothing is taken twice");
        let whole = recording.stop().unwrap();
        assert_eq!(whole.samples(), [1, 2, 3], "the whole recording survives");
    }

    #[test]
    fn a_session_that_produces_nothing_reports_the_programs_error() {
        let config = command_capture(&["sh", "-c", "echo 'no microphone here' >&2; exit 1"]);
        let recording = Recording::start(&config).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let error = recording.stop().unwrap_err().to_string();

        assert!(error.contains("nothing was recorded"), "{error}");
        assert!(error.contains("no microphone here"), "{error}");
    }

    #[test]
    fn a_producer_that_keeps_running_is_stopped() {
        let config = command_capture(&["sh", "-c", "head -c 3200 /dev/zero; sleep 30"]);
        let recording = Recording::start(&config).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let started = Instant::now();
        let audio = recording.stop().unwrap();

        assert_eq!(audio.len(), 1600);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "stopping must not wait for the producer"
        );
    }

    #[test]
    fn a_missing_program_is_a_capture_error() {
        let config = command_capture(&["blablablars-no-such-recorder"]);
        let error = Recording::start(&config).unwrap_err().to_string();
        assert!(error.contains("not found on PATH"), "{error}");
    }

    #[test]
    fn every_platform_gets_a_way_to_record() {
        let candidates = candidates();
        assert!(candidates.len() >= 2, "there must always be a fallback");
        // SoX is the last resort on all three systems: `-d` is the platform's
        // default input device.
        assert_eq!(candidates.last().unwrap().0, "sox");
        if cfg!(target_os = "macos") {
            assert!(candidates.iter().any(|(name, _)| *name == "ffmpeg"));
        }
        if cfg!(windows) {
            assert!(candidates.iter().any(|(name, _)| *name == "ffmpeg"));
        }
        if cfg!(target_os = "linux") {
            assert!(candidates
                .iter()
                .any(|(name, _)| matches!(*name, "parecord" | "pw-record" | "arecord")));
        }
    }

    #[test]
    fn the_ffmpeg_recorders_ask_for_the_same_pcm_everywhere() {
        // macOS: CoreAudio inputs are `:INDEX`; the audio-only default is `:0`.
        let mac = ffmpeg_avfoundation_args(None);
        assert!(mac.windows(2).any(|pair| pair == ["-i", ":0"]));
        let named = ffmpeg_avfoundation_args(Some("2"));
        assert!(named.windows(2).any(|pair| pair == ["-i", ":2"]));
        let full = ffmpeg_avfoundation_args(Some("0:2"));
        assert!(full.windows(2).any(|pair| pair == ["-i", "0:2"]));

        // Windows: DirectShow only accepts the device's own name.
        let windows = ffmpeg_dshow_args(Some("Microphone (USB)"));
        assert!(windows
            .windows(2)
            .any(|pair| pair == ["-i", "audio=Microphone (USB)"]));

        for argv in [mac, windows] {
            assert!(argv.windows(2).any(|pair| pair == ["-ar", "16000"]));
            assert!(argv.windows(2).any(|pair| pair == ["-ac", "1"]));
            assert!(argv.windows(2).any(|pair| pair == ["-f", "s16le"]));
            assert_eq!(argv.last().unwrap(), "-", "PCM goes to stdout");
        }
    }

    #[test]
    fn sox_writes_raw_samples_to_stdout() {
        let argv = sox_args(None);
        assert!(argv.windows(2).any(|pair| pair == ["-t", "raw"]));
        assert!(argv.windows(2).any(|pair| pair == ["-b", "16"]));
        assert_eq!(argv.last().unwrap(), "-");
        assert!(argv.contains(&"signed-integer".to_string()));
    }

    #[test]
    fn a_platform_without_any_recorder_says_how_to_install_one() {
        // The hint is built for the platform the test runs on, so it always
        // names something that exists there.
        let hint = install_hint();
        assert!(hint.contains("PCM"));
        if cfg!(windows) {
            assert!(hint.contains("ffmpeg"), "{hint}");
        } else if cfg!(target_os = "macos") {
            assert!(hint.contains("brew"), "{hint}");
        } else {
            assert!(hint.contains("parecord"), "{hint}");
        }
    }

    #[test]
    fn the_byte_budget_matches_the_duration() {
        assert_eq!(byte_budget(1.0), 32_000);
        assert_eq!(byte_budget(0.5), 16_000);
        assert_eq!(byte_budget(-1.0), 0);
    }

    #[test]
    fn forgetting_to_stop_does_not_leak_the_producer() {
        let config = command_capture(&["sh", "-c", "head -c 640 /dev/zero; sleep 30"]);
        let pid = {
            let recording = Recording::start(&config).unwrap();
            recording.child.id()
        };
        std::thread::sleep(Duration::from_millis(200));
        // `drop` terminated it: the pid is no longer a live child of ours.
        let alive = std::path::Path::new(&format!("/proc/{pid}")).exists();
        assert!(!alive, "the capture child must not outlive the session");
    }
}
