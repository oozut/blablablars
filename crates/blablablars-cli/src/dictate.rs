//! `blablablars dictate`: press a shortcut, speak, and the words appear.
//!
//! One command, three ways in, because a shortcut is a desktop feature and no
//! two desktops agree on how to provide one:
//!
//! * `dictate --toggle` — the shortcut runs this again. The first press starts a
//!   detached recording session, the second stops it, transcribes and types. It
//!   works with GNOME's custom shortcuts, KDE's, `bindsym` in sway, a Windows
//!   shortcut, or a macOS Automator action, because it is only ever "run this
//!   command".
//! * `dictate --seconds N` — record a fixed amount, for a script or a test.
//! * `dictate --input FILE.wav` — skip the microphone entirely.
//!
//! The session is a Unix socket in the runtime directory, so the second press
//! is one small write and never a signal or a pid file to leak.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use blablablars_core::voice::config::{CleanupSetting, OutputKind};
use blablablars_core::voice::{self, Dictation, Recording, VoiceConfig};
use blablablars_core::CoreError;
use clap::{Args, Subcommand};

use crate::plugins::{self, AttachOptions};

/// How long the first press waits for the session to be ready.
const SOCKET_WAIT: Duration = Duration::from_secs(3);

#[derive(Debug, Args)]
pub struct DictateArgs {
    /// Transcribe a WAV file instead of recording (16-bit PCM, 16 kHz mono).
    #[arg(long, value_name = "FILE")]
    pub input: Option<PathBuf>,

    /// Record for this many seconds, then transcribe.
    #[arg(long, value_name = "SECONDS", conflicts_with_all = ["input", "toggle"])]
    pub seconds: Option<f64>,

    /// Start a background recording, or stop the one that is running. Bind this
    /// to your keyboard shortcut.
    #[arg(long)]
    pub toggle: bool,

    /// Internal: the detached recorder that `--toggle` starts.
    #[arg(long, hide = true)]
    pub record_child: bool,

    /// Dictation config (YAML). Defaults to the user's config file, then to the
    /// built-in defaults.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Model name or path, overriding `models.default`.
    #[arg(long, value_name = "NAME")]
    pub model: Option<String>,

    /// Language code, or `auto` to detect it.
    #[arg(long, value_name = "LANG")]
    pub language: Option<String>,

    /// Where the text goes: `auto`, `stdout`, `command`, `xdotool`, `wtype`.
    #[arg(long, value_name = "KIND")]
    pub output: Option<String>,

    /// Print the text instead of typing it. Same as `--output stdout`.
    #[arg(long)]
    pub print: bool,

    /// Skip the cleanup rules and type the transcript as the model produced it.
    #[arg(long)]
    pub no_cleanup: bool,

    /// Report what happened as JSON instead of typing anything.
    #[arg(long)]
    pub json: bool,

    #[command(flatten)]
    pub attach: DictateAttachArgs,
}

/// The plugin flags, shared with the rest of the CLI but kept here so `dictate`
/// can pass them on to the detached recorder unchanged.
#[derive(Debug, Default, Args)]
pub struct DictateAttachArgs {
    #[arg(long = "plugin", short = 'p', value_name = "SPEC")]
    pub plugins: Vec<String>,
    #[arg(long = "plugin-path", value_name = "DIR")]
    pub plugin_paths: Vec<PathBuf>,
    #[arg(long = "plugin-config", value_name = "FILE")]
    pub plugin_config: Option<PathBuf>,
    #[arg(long = "no-plugins")]
    pub no_plugins: bool,
    #[cfg(feature = "python")]
    #[arg(long = "python-path", value_name = "DIR")]
    pub python_paths: Vec<PathBuf>,
}

impl DictateAttachArgs {
    fn options(&self) -> Result<AttachOptions, CoreError> {
        let mut options = AttachOptions {
            specs: self
                .plugins
                .iter()
                .map(|raw| blablablars_core::PluginSpec::parse(raw))
                .collect::<Result<Vec<_>, _>>()?,
            directories: self.plugin_paths.clone(),
            config: self.plugin_config.clone(),
            #[cfg(feature = "python")]
            python_paths: self.python_paths.clone(),
            #[cfg(not(feature = "python"))]
            python_paths: Vec::new(),
            implicit: false,
        };
        if !self.no_plugins {
            options = options.with_implicit_sources();
        }
        Ok(options)
    }

    /// Rebuild the flags for the detached child process.
    fn to_flags(&self) -> Vec<String> {
        let mut flags = Vec::new();
        for spec in &self.plugins {
            flags.push("--plugin".to_string());
            flags.push(spec.clone());
        }
        for directory in &self.plugin_paths {
            flags.push("--plugin-path".to_string());
            flags.push(directory.display().to_string());
        }
        if let Some(config) = &self.plugin_config {
            flags.push("--plugin-config".to_string());
            flags.push(config.display().to_string());
        }
        if self.no_plugins {
            flags.push("--no-plugins".to_string());
        }
        #[cfg(feature = "python")]
        for path in &self.python_paths {
            flags.push("--python-path".to_string());
            flags.push(path.display().to_string());
        }
        flags
    }
}

/// `blablablars config`: write, show and locate the dictation config.
#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Write a config file with every setting at its default.
    Init {
        /// Write here instead of the default location.
        #[arg(long, value_name = "FILE")]
        path: Option<PathBuf>,
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Print the config file that would be read.
    Path,
    /// Print the config as it is in effect (defaults included).
    Show {
        /// Read this file instead of the default one.
        #[arg(long, value_name = "FILE")]
        path: Option<PathBuf>,
    },
}

/// Run the dictation command.
pub fn run(args: DictateArgs) -> Result<(), CoreError> {
    let (mut config, config_path) = VoiceConfig::load_resolved(args.config.as_deref())?;
    if let Some(language) = &args.language {
        config.language = language.clone();
    }
    if let Some(raw) = &args.output {
        config.output.kind = parse_output_kind(raw)?;
    }
    if args.no_cleanup {
        config.cleanup = CleanupSetting::Enabled(false);
    }
    if args.print || args.json {
        config.output.kind = OutputKind::Stdout;
    }

    let pipeline = plugins::build(&args.attach.options()?)?;
    let dictation = Dictation::with_pipeline(config, args.model.as_deref(), pipeline)?;

    if let Some(path) = &args.input {
        dictation.notify("recording");
        let started = Instant::now();
        let run = dictation.transcribe_file(path)?;
        debug_config(&config_path);
        return report(&args, &dictation, &run, started);
    }

    if args.toggle {
        return toggle(&args);
    }

    if args.record_child {
        return record_session(&args, dictation);
    }

    if let Some(seconds) = args.seconds {
        return record_for(&args, dictation, seconds);
    }

    record_until_enter(&args, dictation)
}

/// Record for a fixed duration.
fn record_for(args: &DictateArgs, dictation: Dictation, seconds: f64) -> Result<(), CoreError> {
    if seconds <= 0.0 {
        return Err(CoreError::Voice(
            "`--seconds` must be greater than zero".to_string(),
        ));
    }
    let started = Instant::now();
    let recording = Recording::start(&dictation.config().capture)?;
    dictation.notify("recording");
    eprintln!("recording for {seconds}s...");
    std::thread::sleep(Duration::from_secs_f64(seconds));
    let audio = recording.stop()?;
    dictation.notify("transcribing");
    let run = dictation.transcribe(&audio)?;
    report(args, &dictation, &run, started)
}

/// Record until the user presses Enter: the fastest way to try the setup.
fn record_until_enter(args: &DictateArgs, dictation: Dictation) -> Result<(), CoreError> {
    use std::io::BufRead as _;

    let started = Instant::now();
    let recording = Recording::start(&dictation.config().capture)?;
    dictation.notify("recording");
    eprintln!("recording — press Enter to stop");
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let audio = recording.stop()?;
    dictation.notify("transcribing");
    let run = dictation.transcribe(&audio)?;
    report(args, &dictation, &run, started)
}

/// Transcribe, then type (or print) the result.
fn report(
    args: &DictateArgs,
    dictation: &Dictation,
    run: &blablablars_core::DictationRun,
    started: Instant,
) -> Result<(), CoreError> {
    if args.json {
        println!("{}", serde_json::to_string_pretty(&run.to_json())?);
        return Ok(());
    }

    if run.text.is_empty() {
        dictation.notify("done");
        eprintln!(
            "nothing was transcribed from {:.1}s of audio ({}); nothing was typed",
            run.audio_seconds, run.transcriber
        );
        return Ok(());
    }

    let via = dictation.deliver(&run.text)?;
    dictation.notify("done");
    eprintln!(
        "{:.1}s of audio -> {} characters in {:.0} ms with {} via {via}",
        run.audio_seconds,
        run.text.chars().count(),
        started.elapsed().as_millis(),
        run.transcriber
    );
    Ok(())
}

/// The first press starts a recorder; the second stops it.
#[cfg(unix)]
fn toggle(args: &DictateArgs) -> Result<(), CoreError> {
    let socket = socket_path()?;

    if request_stop(&socket)? {
        eprintln!("stopping — transcribing");
        return Ok(());
    }

    let exe = std::env::current_exe().map_err(|error| {
        CoreError::Voice(format!(
            "cannot find our own binary to start a recorder: {error}"
        ))
    })?;

    let mut command = std::process::Command::new(exe);
    command.arg("dictate").arg("--record-child");
    if let Some(config) = &args.config {
        command.arg("--config").arg(config);
    }
    if let Some(model) = &args.model {
        command.arg("--model").arg(model);
    }
    if let Some(language) = &args.language {
        command.arg("--language").arg(language);
    }
    if let Some(output) = &args.output {
        command.arg("--output").arg(output);
    }
    if args.no_cleanup {
        command.arg("--no-cleanup");
    }
    command.args(args.attach.to_flags());
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());

    match log_file() {
        Some(log) => {
            if let Ok(handle) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)
            {
                command.stderr(std::process::Stdio::from(handle));
            }
        }
        None => {
            command.stderr(std::process::Stdio::null());
        }
    }

    command
        .spawn()
        .map_err(|error| CoreError::Voice(format!("cannot start a recorder: {error}")))?;

    wait_for_socket(&socket)?;
    eprintln!("recording — press the shortcut again to stop");
    Ok(())
}

#[cfg(not(unix))]
fn toggle(_args: &DictateArgs) -> Result<(), CoreError> {
    Err(CoreError::Voice(
        "`--toggle` needs Unix domain sockets, which this build does not have. \
         Use `--seconds N`, or bind two shortcuts: `dictate` and a stop signal."
            .to_string(),
    ))
}

/// The detached recorder: bind the socket, capture, and transcribe on stop.
#[cfg(unix)]
fn record_session(args: &DictateArgs, dictation: Dictation) -> Result<(), CoreError> {
    let socket = socket_path()?;
    let listener = bind_listener(&socket)?;
    let _guard = SocketGuard(socket.clone());

    let recording = Recording::start(&dictation.config().capture)?;
    dictation.notify("recording");
    listener
        .set_nonblocking(true)
        .map_err(|error| CoreError::Voice(format!("cannot watch the session socket: {error}")))?;

    let started = Instant::now();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if is_stop_request(&mut stream) {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                return Err(CoreError::Voice(format!(
                    "cannot watch the session socket: {error}"
                )))
            }
        }
        if recording.at_limit() {
            eprintln!(
                "recording reached the {:.0}s limit in the config",
                dictation.config().capture.max_seconds
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let audio = recording.stop()?;
    dictation.notify("transcribing");
    match dictation.transcribe(&audio) {
        Ok(run) => report(args, &dictation, &run, started),
        Err(error) => {
            dictation.notify("error");
            Err(error)
        }
    }
}

#[cfg(not(unix))]
fn record_session(_args: &DictateArgs, _dictation: Dictation) -> Result<(), CoreError> {
    Err(CoreError::Voice(
        "the background recorder needs Unix domain sockets".to_string(),
    ))
}

/// Removes the socket file when the session ends, however it ends.
#[cfg(unix)]
struct SocketGuard(PathBuf);

#[cfg(unix)]
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `$XDG_RUNTIME_DIR/blablablars/dictate.sock`, or the temp directory.
#[cfg(unix)]
pub fn socket_path() -> Result<PathBuf, CoreError> {
    let base = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::temp_dir(),
    };
    let directory = base.join("blablablars");
    std::fs::create_dir_all(&directory)?;
    Ok(directory.join(format!("dictate-{}.sock", user_id())))
}

/// A stable per-user id, so two users on one machine do not share a socket.
#[cfg(unix)]
fn user_id() -> u32 {
    #[allow(unsafe_code)]
    // SAFETY: `geteuid` takes no arguments, cannot fail, and is always safe to
    // call; it is only `unsafe` because it is an FFI call.
    unsafe {
        libc::geteuid()
    }
}

/// Reads one request from a client and answers: `stop` ends the session, and
/// anything else (`ping`, from a second press that found a stale socket file)
/// just asks whether it is alive.
#[cfg(unix)]
fn is_stop_request(stream: &mut std::os::unix::net::UnixStream) -> bool {
    use std::io::Read as _;

    // The listener is non-blocking; the accepted stream is not, but a client
    // that never closes must not wedge the session.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));

    let mut request = String::new();
    let _ = stream.take(64).read_to_string(&mut request);

    if request.trim() == "stop" {
        return true;
    }
    let _ = stream.write_all(b"running\n");
    false
}

#[cfg(unix)]
fn bind_listener(socket: &Path) -> Result<std::os::unix::net::UnixListener, CoreError> {
    use std::os::unix::net::UnixListener;

    if let Ok(listener) = UnixListener::bind(socket) {
        return Ok(listener);
    }
    // A previous session may have left the file behind after a crash. Only
    // remove it when nothing answers on it — and ask with `ping`, so a live
    // session is not stopped by the question.
    if session_alive(socket) {
        return Err(CoreError::Voice(
            "a recording session is already running".to_string(),
        ));
    }
    let _ = std::fs::remove_file(socket);
    UnixListener::bind(socket).map_err(|error| {
        CoreError::Voice(format!(
            "cannot create the session socket `{}`: {error}",
            socket.display()
        ))
    })
}

/// Send one request and wait for the answer. `None` when nothing is listening.
#[cfg(unix)]
fn roundtrip(socket: &Path, request: &str) -> Result<Option<String>, CoreError> {
    use std::io::Read as _;
    use std::os::unix::net::UnixStream;

    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(_) => return Ok(None),
    };
    stream.write_all(request.as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    // The answer is short; a session that is busy transcribing may take a
    // moment to close the connection.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut reply = String::new();
    let _ = stream.take(64).read_to_string(&mut reply);
    Ok(Some(reply))
}

/// Ask a running session to stop. `Ok(false)` when none is running.
#[cfg(unix)]
fn request_stop(socket: &Path) -> Result<bool, CoreError> {
    Ok(roundtrip(socket, "stop")?.is_some())
}

/// Whether a session is listening, without asking it to stop.
#[cfg(unix)]
fn session_alive(socket: &Path) -> bool {
    matches!(roundtrip(socket, "ping"), Ok(Some(reply)) if reply.trim() == "running")
}

#[cfg(unix)]
fn wait_for_socket(socket: &Path) -> Result<(), CoreError> {
    let deadline = Instant::now() + SOCKET_WAIT;
    while Instant::now() < deadline {
        if socket.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(CoreError::Voice(format!(
        "the recorder did not start within {}s; see {}",
        SOCKET_WAIT.as_secs(),
        log_file()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "the recorder's log".to_string())
    )))
}

/// Where the detached recorder writes its diagnostics.
fn log_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    let directory = base.join("blablablars");
    std::fs::create_dir_all(&directory).ok()?;
    Some(directory.join("dictate.log"))
}

fn parse_output_kind(raw: &str) -> Result<OutputKind, CoreError> {
    Ok(match raw.trim().to_ascii_lowercase().as_str() {
        "auto" => OutputKind::Auto,
        "stdout" | "print" => OutputKind::Stdout,
        "command" => OutputKind::Command,
        "xdotool" => OutputKind::Xdotool,
        "wtype" => OutputKind::Wtype,
        other => {
            return Err(CoreError::Voice(format!(
                "unknown output `{other}`: use auto, stdout, command, xdotool or wtype"
            )))
        }
    })
}

fn debug_config(path: &Option<PathBuf>) {
    if std::env::var_os("BLABLABLARS_DEBUG").is_some() {
        match path {
            Some(path) => eprintln!("config: {}", path.display()),
            None => eprintln!("config: built-in defaults (no config file)"),
        }
    }
}

/// `blablablars config ...`
pub fn manage_config(action: ConfigAction) -> Result<(), CoreError> {
    match action {
        ConfigAction::Init { path, force } => {
            let path = match path.or_else(voice::config::default_config_path) {
                Some(path) => path,
                None => {
                    return Err(CoreError::Voice(
                        "no home directory: pass `--path FILE`".to_string(),
                    ))
                }
            };
            if path.exists() && !force {
                return Err(CoreError::Voice(format!(
                    "`{}` already exists; pass `--force` to overwrite it",
                    path.display()
                )));
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, default_config_document())?;
            println!("{}", path.display());
            Ok(())
        }
        ConfigAction::Path => {
            let path = voice::config::default_config_path().ok_or_else(|| {
                CoreError::Voice("no home directory: set $BLABLABLARS_CONFIG".to_string())
            })?;
            println!("{}", path.display());
            Ok(())
        }
        ConfigAction::Show { path } => {
            let (config, _) = VoiceConfig::load_resolved(path.as_deref())?;
            print!("{}", config.to_yaml()?);
            Ok(())
        }
    }
}

/// The document `config init` writes: every default, plus how to use it.
pub fn default_config_document() -> String {
    let body = VoiceConfig::default()
        .to_yaml()
        .unwrap_or_else(|_| String::new());
    format!(
        "\
# blablablars dictation config
#
# A keyboard shortcut runs `blablablars dictate --toggle`; the first press
# starts recording and the second press transcribes and types into the focused
# window. `blablablars models` lists the models found in `models.directory`, and
# `blablablars dictate --input file.wav --print` tries a transcript without a
# microphone.
#
# Every setting below is the default: delete what you do not want to change.

{body}"
    )
}

/// `blablablars models`
pub fn list_models(config_path: Option<&Path>, json: bool) -> Result<(), CoreError> {
    let (config, _) = VoiceConfig::load_resolved(config_path)?;
    let directory = &config.models.directory;
    let models = voice::models::discover(directory)?;
    let resolved = voice::models::resolve(directory, &config.models.default).ok();

    if json {
        let entries: Vec<_> = models
            .iter()
            .map(|model| {
                serde_json::json!({
                    "name": model.name,
                    "file": model.path,
                    "bytes": model.bytes,
                    "size": model.size_human(),
                    "default": resolved.as_ref().is_some_and(|found| found.path == model.path),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "directory": directory,
                "default": config.models.default,
                "models": entries,
            }))?
        );
        return Ok(());
    }

    println!("directory: {}", directory.display());
    println!("default:   {}", config.models.default);
    if models.is_empty() {
        println!(
            "no models found.\n{}",
            voice::models::download_hint(&config.models.default, directory)
        );
        return Ok(());
    }
    for model in &models {
        let marker = if resolved
            .as_ref()
            .is_some_and(|found| found.path == model.path)
        {
            "*"
        } else {
            " "
        };
        println!(
            "{marker} {:<32} {:>9}  {}",
            model.name,
            model.size_human(),
            model.file_name
        );
    }
    Ok(())
}
