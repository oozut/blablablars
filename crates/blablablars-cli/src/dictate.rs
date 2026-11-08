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
//! The session is a loopback socket noted down in the runtime directory, so the
//! second press is one small write and never a signal or a pid to hunt for. It
//! is also the same code on Linux, macOS and Windows, which is the point: a
//! shortcut that works on two of the three is not a shortcut.

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use blablablars_core::voice::config::{CleanupSetting, OutputKind};
use blablablars_core::voice::{self, Dictation, Recording, VoiceConfig};
use blablablars_core::CoreError;
use clap::{Args, Subcommand};

use crate::plugins::{self, AttachOptions};

/// How long the first press waits for the session to be ready.
const SESSION_WAIT: Duration = Duration::from_secs(3);

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
///
/// The handshake is a loopback socket plus a small file that says which port to
/// knock on. A Unix domain socket would do on Linux and macOS, but not on
/// Windows, and a shortcut that works on two of the three platforms is not a
/// shortcut. Port 0 is chosen by the kernel every time, so there is nothing to
/// configure and nothing to collide with.
fn toggle(args: &DictateArgs) -> Result<(), CoreError> {
    let path = session_file()?;

    if let Some(session) = read_session(&path) {
        if request(&session, Request::Stop).is_some() {
            eprintln!("stopping — transcribing");
            return Ok(());
        }
        // The note is there but nobody answered: the previous session died
        // without cleaning up. Its wording says so, so replace it.
        let _ = std::fs::remove_file(&path);
    }

    spawn_recorder(args)?;
    wait_for_session(&path)?;
    eprintln!("recording — press the shortcut again to stop");
    Ok(())
}

/// Start the detached recorder with the flags this run was given.
fn spawn_recorder(args: &DictateArgs) -> Result<(), CoreError> {
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
    Ok(())
}

/// What the second press asks the recorder to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    /// End the session and transcribe.
    Stop,
    /// Just check that somebody is listening.
    Ping,
}

impl Request {
    fn word(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Ping => "ping",
        }
    }
}

/// What the recorder writes down so the next press can find it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Session {
    port: u16,
    /// So another local process cannot end a recording it did not start.
    token: String,
}

/// The detached recorder: listen, capture, and transcribe on stop.
fn record_session(args: &DictateArgs, dictation: Dictation) -> Result<(), CoreError> {
    let path = session_file()?;

    // Two recorders on one machine would fight over the microphone, and the
    // second press would not know which one it was talking to.
    if let Some(other) = read_session(&path) {
        if request(&other, Request::Ping).is_some() {
            return Err(CoreError::Voice(
                "a recording session is already running".to_string(),
            ));
        }
        let _ = std::fs::remove_file(&path);
    }

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|error| CoreError::Voice(format!("cannot open the session socket: {error}")))?;
    let port = listener
        .local_addr()
        .map_err(|error| CoreError::Voice(format!("cannot open the session socket: {error}")))?
        .port();

    let recording = Recording::start(&dictation.config().capture)?;
    let session = Session {
        port,
        token: make_token(),
    };
    write_session(&path, &session)?;
    let _guard = SessionGuard(path);
    dictation.notify("recording");

    listener
        .set_nonblocking(true)
        .map_err(|error| CoreError::Voice(format!("cannot watch the session socket: {error}")))?;

    let started = Instant::now();
    loop {
        if serve_once(&listener, &session)? {
            break;
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

/// Answer every connection that arrived since the last look. `true` when the
/// session should stop.
fn serve_once(listener: &TcpListener, session: &Session) -> Result<bool, CoreError> {
    loop {
        let (mut stream, _) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => {
                return Err(CoreError::Voice(format!(
                    "cannot watch the session socket: {error}"
                )))
            }
        };
        let _ = stream.set_nonblocking(false);
        // A client that connects and says nothing must not wedge the session.
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));

        let mut buffer = [0u8; 128];
        let read = stream.read(&mut buffer).unwrap_or(0);
        let request = String::from_utf8_lossy(&buffer[..read]);
        let (word, token) = request
            .trim()
            .split_once(' ')
            .map(|(word, token)| (word.trim(), token.trim()))
            .unwrap_or((request.trim(), ""));

        if token != session.token {
            let _ = stream.write_all(b"forbidden\n");
            continue;
        }
        if word == Request::Stop.word() {
            let _ = stream.write_all(b"stopping\n");
            return Ok(true);
        }
        let _ = stream.write_all(b"running\n");
    }
}

/// Removes the session note when the session ends, however it ends.
struct SessionGuard(PathBuf);

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Where the session note lives: `$XDG_RUNTIME_DIR/blablablars/` on Linux, the
/// per-user temporary directory on macOS and Windows.
pub fn session_file() -> Result<PathBuf, CoreError> {
    let directory = voice::platform::runtime_dir().ok_or_else(|| {
        CoreError::Voice("no runtime directory: set $XDG_RUNTIME_DIR or $TMPDIR".to_string())
    })?;
    std::fs::create_dir_all(&directory)?;
    Ok(directory.join(format!("dictate-{}.json", voice::platform::user_tag())))
}

fn write_session(path: &Path, session: &Session) -> Result<(), CoreError> {
    // Written beside and renamed, so a second press never reads half a note.
    let temporary = path.with_extension("json.tmp");
    let note = serde_json::json!({ "port": session.port, "token": session.token });
    std::fs::write(&temporary, serde_json::to_vec(&note)?)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

fn read_session(path: &Path) -> Option<Session> {
    let bytes = std::fs::read(path).ok()?;
    let note: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some(Session {
        port: u16::try_from(note.get("port")?.as_u64()?).ok()?,
        token: note.get("token")?.as_str()?.to_string(),
    })
}

/// One request to a session. `None` when nothing is listening.
fn request(session: &Session, request: Request) -> Option<String> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, session.port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(500)).ok()?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    stream
        .write_all(format!("{} {}\n", request.word(), session.token).as_bytes())
        .ok()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut buffer = [0u8; 64];
    let read = stream.read(&mut buffer).unwrap_or(0);
    Some(String::from_utf8_lossy(&buffer[..read]).to_string())
}

/// A per-session secret, so another local process cannot stop the recording.
fn make_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:x}", std::process::id(), nanos)
}

fn wait_for_session(path: &Path) -> Result<(), CoreError> {
    let deadline = Instant::now() + SESSION_WAIT;
    while Instant::now() < deadline {
        if let Some(session) = read_session(path) {
            if request(&session, Request::Ping).is_some() {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(CoreError::Voice(format!(
        "the recorder did not start within {}s; see {}",
        SESSION_WAIT.as_secs(),
        log_file()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "the recorder's log".to_string())
    )))
}

/// Where the detached recorder writes its diagnostics.
fn log_file() -> Option<PathBuf> {
    let directory = voice::platform::state_dir()?;
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
        "paste" | "clipboard" => OutputKind::Paste,
        other => {
            return Err(CoreError::Voice(format!(
                "unknown output `{other}`: use auto, stdout, command, xdotool, wtype or paste"
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

/// `blablablars engines`
///
/// The question this answers is the one that comes right after installing
/// blablablars: which of these backends can I run here, and what do I still have
/// to download? So it reports the program, whether it is on `PATH`, and how many
/// of the installed models that backend can read.
pub fn list_engines(config_path: Option<&Path>, json: bool) -> Result<(), CoreError> {
    use blablablars_core::voice::config::TranscriberKind;

    let (config, _) = VoiceConfig::load_resolved(config_path)?;
    let models = voice::models::discover(&config.models.directory)?;

    let rows: Vec<_> = TranscriberKind::ALL
        .iter()
        .map(|kind| {
            let configured = *kind == config.transcriber.kind;
            let program = if configured {
                config.transcriber.program()
            } else {
                kind.default_binary().to_string()
            };
            let installed =
                !program.is_empty() && voice::platform::find_program(&program).is_some();
            let runnable = models
                .iter()
                .filter(|model| kind.reads(model.layout()))
                .count();
            (*kind, configured, program, installed, runnable)
        })
        .collect();

    if json {
        let engines: Vec<_> = rows
            .iter()
            .map(|(kind, configured, program, installed, runnable)| {
                serde_json::json!({
                    "kind": kind,
                    "label": kind.label(),
                    "program": program,
                    "installed": installed,
                    "configured": configured,
                    "runnable_models": runnable,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "config": config_path.map(|path| path.display().to_string()),
                "models_directory": config.models.directory,
                "default_model": config.models.default,
                "engines": engines,
            }))?
        );
        return Ok(());
    }

    println!("models in {}", config.models.directory.display());
    if models.is_empty() {
        println!(
            "  (none yet)\n{}",
            voice::models::download_hint(&config.models.default, &config.models.directory)
        );
    } else {
        for model in &models {
            let default = voice::models::resolve(&config.models.directory, &config.models.default)
                .is_ok_and(|resolved| resolved.path == model.path);
            println!(
                "{} {:<38} {:>9}  {}",
                if default { "*" } else { " " },
                model.name,
                model.size_human(),
                model.layout().describe()
            );
        }
    }

    println!();
    println!("backends");
    for (kind, configured, program, installed, runnable) in rows {
        let marker = if configured { "*" } else { " " };
        let state = if installed {
            "installed".to_string()
        } else if kind == TranscriberKind::Command {
            "set transcriber.argv".to_string()
        } else {
            "not installed".to_string()
        };
        let models = if runnable == 1 {
            "1 model".to_string()
        } else {
            format!("{runnable} models")
        };
        println!(
            "{marker} {:<12} {:<22} {state:<15}  {models}",
            kind_name(kind),
            program
        );
        println!("      {}", kind.label());
    }
    Ok(())
}

fn kind_name(kind: blablablars_core::voice::config::TranscriberKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(|name| name.to_string()))
        .unwrap_or_else(|| format!("{kind:?}"))
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
                    "layout": model.layout().describe(),
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
            "{marker} {:<32} {:>9}  {:<24}  {}",
            model.name,
            model.size_human(),
            model.layout().describe(),
            model.file_name
        );
    }
    Ok(())
}
