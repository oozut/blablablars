//! The `blablablars` command line application.
//!
//! This binary is the whole product: extensions of every supported kind run
//! inside it (compiled Rust libraries and Python plugins, both in-process) or
//! beside it (a child process speaking JSON on stdio).
//!
//! There is one way to name a plugin — the spec, `KIND:TARGET` — and one way to
//! keep a list of them, the config file. See [`plugins`] for the sources and
//! their order, and `blablablars plugin --help` for managing the list.

mod dictate;
mod plugins;

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use blablablars_core::spec::{PluginKind, PluginSpec};
use blablablars_core::{CoreError, HOOKS, VERSION};
use clap::{Args, Parser, Subcommand};

use crate::plugins::AttachOptions;

#[derive(Debug, Parser)]
#[command(
    name = "blablablars",
    version,
    about = "Analyse text through an extensible pipeline",
    long_about = None,
    after_long_help = "Plugins are named with a spec: rust:PATH, python:PATH|MODULE or \
                       process:COMMAND.\nAdd them for every run with `blablablars plugin add SPEC`."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// How extensions are attached, shared by every subcommand that runs a pipeline.
#[derive(Debug, Default, Args)]
struct AttachArgs {
    /// Attach a plugin for this run: `rust:PATH`, `python:PATH|MODULE` or
    /// `process:COMMAND`. A bare value is inferred from its shape (`.so`,
    /// `.dylib`, `.dll` → rust; `.py` → python; anything else → process).
    /// Repeatable.
    #[arg(long = "plugin", short = 'p', value_name = "SPEC")]
    plugins: Vec<String>,

    /// Attach every plugin found in a directory (`*.so`, `*.dylib`, `*.dll`,
    /// `*.py`). Repeatable.
    #[arg(long = "plugin-path", value_name = "DIR")]
    plugin_paths: Vec<PathBuf>,

    /// Read this plugin list instead of the user's (`blablablars plugin add
    /// --config FILE` writes one).
    #[arg(long = "plugin-config", value_name = "FILE")]
    plugin_config: Option<PathBuf>,

    /// Ignore `BLABLABLARS_PLUGINS`, `BLABLABLARS_PLUGIN_PATH` and the config
    /// file. `--plugin` and `--plugin-path` still apply.
    #[arg(long = "no-plugins")]
    no_plugins: bool,

    /// Directory added to the embedded interpreter's `sys.path`, for plugins and
    /// for the `blablablars` package itself. Repeatable.
    #[cfg(feature = "python")]
    #[arg(long = "python-path", value_name = "DIR")]
    python_paths: Vec<PathBuf>,
}

impl AttachArgs {
    /// Turn the flags into the options the loader works with.
    fn options(&self) -> Result<AttachOptions, CoreError> {
        let mut options = AttachOptions {
            specs: self
                .plugins
                .iter()
                .map(|raw| PluginSpec::parse(raw))
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
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the pipeline over TEXT (or standard input when TEXT is omitted).
    Run {
        /// Text to analyse. Reads standard input when missing.
        text: Option<String>,
        /// Print the machine-readable form shared with the Python bindings.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        attach: AttachArgs,
    },
    /// List the hook vocabulary extensions can subscribe to.
    Hooks {
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Attach the plugins this run would load and print what they report about
    /// themselves.
    Plugins {
        #[command(flatten)]
        attach: AttachArgs,
        #[arg(long)]
        json: bool,
    },
    /// Manage the plugin list that every run reads (`add`, `list`, `remove`,
    /// `path`).
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
    /// Record, transcribe and type: press a shortcut, speak, the words appear.
    ///
    /// Without `--toggle`, `--seconds` or `--input`, it records until Enter.
    Dictate(Box<dictate::DictateArgs>),
    /// Write, show or locate the dictation config.
    Config {
        #[command(subcommand)]
        action: dictate::ConfigAction,
    },
    /// List the models found in the configured model directory.
    Models {
        /// Read this config instead of the default one.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// List the transcription backends, which are installed, and what each can
    /// run: whisper.cpp, NVIDIA Parakeet, ONNX Runtime (sherpa-onnx), or your
    /// own command.
    Engines {
        /// Read this config instead of the default one.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Print version information.
    Version {
        #[arg(long)]
        json: bool,
    },
}

/// The long-term interface for adding plugins: write a spec to the config file
/// once, and every run picks it up.
#[derive(Debug, Subcommand)]
enum PluginAction {
    /// Add specs to the plugin list. Existing lines are left alone and a spec
    /// is never stored twice.
    Add {
        /// What to add: `rust:PATH`, `python:PATH|MODULE`, `process:COMMAND`, or
        /// a bare value inferred from its shape.
        #[arg(value_name = "SPEC", required = true)]
        specs: Vec<String>,
        /// Write to this file instead of the user's plugin list.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
    },
    /// List the specs in the plugin list, one per line.
    List {
        /// Read this file instead of the user's plugin list.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove specs from the plugin list.
    Remove {
        /// What to remove; matched against the stored, canonical spec.
        #[arg(value_name = "SPEC", required = true)]
        specs: Vec<String>,
        /// Remove from this file instead of the user's plugin list.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
    },
    /// Print the plugin list that would be read, without reading it.
    Path {
        /// Path that would be used instead of the user's plugin list.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match execute(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn execute(cli: Cli) -> Result<(), CoreError> {
    match cli.command {
        Command::Run { text, json, attach } => {
            let pipeline = plugins::build(&attach.options()?)?;
            let text = match text {
                Some(text) => text,
                None => read_stdin()?,
            };
            let run = pipeline.run(&text)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&run.to_json())?);
            } else {
                println!("text:       {}", run.analysis.text);
                println!("words:      {}", run.analysis.words);
                println!("characters: {}", run.analysis.characters);
                println!("digest:     {}", run.analysis.digest);
                if run.trace.is_empty() {
                    println!("trace:      (no extensions)");
                } else {
                    println!("trace:      {}", run.trace.join(", "));
                }
            }
            Ok(())
        }
        Command::Hooks { json } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&HOOKS)?);
            } else {
                for hook in HOOKS {
                    println!("{hook}");
                }
            }
            Ok(())
        }
        Command::Plugins { attach, json } => {
            let pipeline = plugins::build(&attach.options()?)?;
            let described = pipeline.registry().describe();
            if json {
                println!("{}", serde_json::to_string_pretty(&described)?);
            } else if described.is_empty() {
                println!("(no plugins attached)");
            } else {
                for info in described {
                    println!("{}: {}", info.name, info.hooks.join(", "));
                }
            }
            Ok(())
        }
        Command::Plugin { action } => manage_plugins(action),
        Command::Dictate(args) => dictate::run(*args),
        Command::Config { action } => dictate::manage_config(action),
        Command::Models { config, json } => dictate::list_models(config.as_deref(), json),
        Command::Engines { config, json } => dictate::list_engines(config.as_deref(), json),
        Command::Version { json } => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "name": "blablablars",
                        "version": VERSION,
                        "protocol": blablablars_core::PROTOCOL_VERSION,
                        "hooks": HOOKS,
                        "plugin_kinds": PluginKind::ALL.map(PluginKind::as_str),
                        "transports": {
                            "rust": true,
                            "python": cfg!(feature = "python"),
                            "process": true,
                        },
                        "plugin_config": plugins::config_file(None)?,
                    }))?
                );
            } else {
                println!("blablablars {VERSION}");
            }
            Ok(())
        }
    }
}

fn manage_plugins(action: PluginAction) -> Result<(), CoreError> {
    match action {
        PluginAction::Add { specs, config } => {
            let config = plugins::config_file(config.as_deref())?;
            for raw in &specs {
                let spec = PluginSpec::parse(raw)?;
                let outcome = plugins::add(&config, &spec)?;
                if outcome.added() {
                    println!("added {} to {}", outcome.spec(), config.display());
                } else {
                    println!("already in {}: {}", config.display(), outcome.spec());
                }
            }
            Ok(())
        }
        PluginAction::List { config, json } => {
            let config = plugins::config_file(config.as_deref())?;
            let specs = plugins::read_config(&config)?;
            if json {
                let plugins: Vec<_> = specs
                    .iter()
                    .map(|spec| {
                        serde_json::json!({
                            "spec": spec.to_string(),
                            "kind": spec.kind().as_str(),
                            "target": spec.target(),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "config": config,
                        "plugins": plugins,
                    }))?
                );
            } else if specs.is_empty() {
                println!("(no plugins in {})", config.display());
            } else {
                for spec in &specs {
                    println!("{spec}");
                }
            }
            Ok(())
        }
        PluginAction::Remove { specs, config } => {
            let config = plugins::config_file(config.as_deref())?;
            let mut failures = Vec::new();
            for raw in &specs {
                let spec = PluginSpec::parse(raw)?;
                if plugins::remove(&config, &spec)? {
                    println!("removed {} from {}", spec, config.display());
                } else {
                    failures.push(spec.to_string());
                }
            }
            if failures.is_empty() {
                return Ok(());
            }
            Err(CoreError::Plugin(format!(
                "not in {}: {}",
                config.display(),
                failures.join(", ")
            )))
        }
        PluginAction::Path { config } => {
            let config = plugins::config_file(config.as_deref())?;
            println!("{}", config.display());
            Ok(())
        }
    }
}

fn read_stdin() -> Result<String, CoreError> {
    if std::io::stdin().is_terminal() {
        return Err(CoreError::Plugin(
            "no TEXT given and standard input is a terminal; \
             pass the text as an argument or pipe it in"
                .to_string(),
        ));
    }
    let mut buffer = String::new();
    std::io::stdin().read_to_string(&mut buffer)?;
    Ok(buffer.trim_end_matches(['\n', '\r']).to_string())
}
