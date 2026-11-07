//! The `blablablars` command line application.
//!
//! This binary is the whole product: extensions of every supported kind run
//! inside it (compiled Rust libraries and Python plugins, both in-process) or
//! beside it (a child process speaking JSON on stdio).

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use blablablars_core::{CoreError, DynamicExtension, Pipeline, ProcessExtension, HOOKS, VERSION};
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "blablablars",
    version,
    about = "Analyse text through an extensible pipeline",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// How extensions are attached, shared by every subcommand that runs a pipeline.
///
/// Extensions are registered in a fixed order — Rust, then Python, then child
/// processes — and command-line flags come before the matching environment
/// variable within each group, so a run is reproducible.
#[derive(Debug, Default, Args)]
struct ExtensionArgs {
    /// Compiled Rust extension (`.so`, `.dylib`, `.dll`) loaded into this
    /// process. Repeatable.
    #[arg(long = "rust-plugin", value_name = "PATH")]
    rust_plugins: Vec<String>,

    /// Directory scanned for Rust extensions (`*.so`, `*.dylib`, `*.dll`).
    #[arg(long = "rust-plugin-dir", value_name = "DIR")]
    rust_plugin_dirs: Vec<PathBuf>,

    /// Python plugin file or importable module, executed inside this process by
    /// the embedded interpreter. Repeatable.
    #[cfg(feature = "python")]
    #[arg(long = "python-plugin", value_name = "PATH|MODULE")]
    python_plugins: Vec<String>,

    /// Directory added to the embedded interpreter's `sys.path`, for plugins and
    /// for the `blablablars` package itself. Repeatable.
    #[cfg(feature = "python")]
    #[arg(long = "python-path", value_name = "DIR")]
    python_paths: Vec<PathBuf>,

    /// Command line of a plugin that runs as its own process, in any language,
    /// e.g. "python3 -m blablablars.runtime my_plugin.py". Repeatable.
    #[arg(long = "plugin", value_name = "COMMAND")]
    process_plugins: Vec<String>,
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
        extensions: ExtensionArgs,
    },
    /// List the hook vocabulary extensions can subscribe to.
    Hooks {
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Attach the given extensions and print what they report about themselves.
    Plugins {
        #[command(flatten)]
        extensions: ExtensionArgs,
        #[arg(long)]
        json: bool,
    },
    /// Print version information.
    Version {
        #[arg(long)]
        json: bool,
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
        Command::Run {
            text,
            json,
            extensions,
        } => {
            let pipeline = build_pipeline(&extensions)?;
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
        Command::Plugins { extensions, json } => {
            let pipeline = build_pipeline(&extensions)?;
            let described = pipeline.registry().describe();
            if json {
                println!("{}", serde_json::to_string_pretty(&described)?);
            } else if described.is_empty() {
                println!("(no extensions attached)");
            } else {
                for info in described {
                    println!("{}: {}", info.name, info.hooks.join(", "));
                }
            }
            Ok(())
        }
        Command::Version { json } => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "name": "blablablars",
                        "version": VERSION,
                        "protocol": blablablars_core::PROTOCOL_VERSION,
                        "hooks": HOOKS,
                        "transports": {
                            "rust": true,
                            "python": cfg!(feature = "python"),
                            "process": true,
                        },
                    }))?
                );
            } else {
                println!("blablablars {VERSION}");
            }
            Ok(())
        }
    }
}

/// Attach every extension named on the command line or in the environment.
///
/// Environment variables (all `PATH`-separator lists):
/// `BLABLABLARS_RUST_PLUGINS`, `BLABLABLARS_RUST_PLUGIN_DIRS`,
/// `BLABLABLARS_PLUGINS` (the same variable the Python package uses),
/// `BLABLABLARS_PYTHON_PATH`.
fn build_pipeline(args: &ExtensionArgs) -> Result<Pipeline, CoreError> {
    let mut pipeline = Pipeline::new();

    let mut rust_paths: Vec<PathBuf> = args.rust_plugins.iter().map(PathBuf::from).collect();
    rust_paths.extend(env_paths("BLABLABLARS_RUST_PLUGINS"));
    for path in &rust_paths {
        pipeline.register(Arc::new(DynamicExtension::open(path)?))?;
    }

    let mut rust_dirs: Vec<PathBuf> = args.rust_plugin_dirs.clone();
    rust_dirs.extend(env_paths("BLABLABLARS_RUST_PLUGIN_DIRS"));
    for extension in DynamicExtension::open_directories(rust_dirs)? {
        pipeline.register(Arc::new(extension))?;
    }

    #[cfg(feature = "python")]
    {
        let mut search_paths: Vec<PathBuf> = args.python_paths.clone();
        search_paths.extend(env_paths("BLABLABLARS_PYTHON_PATH"));

        let mut python_plugins: Vec<String> = args.python_plugins.clone();
        python_plugins.extend(
            env_paths("BLABLABLARS_PLUGINS")
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned()),
        );

        if !search_paths.is_empty() || !python_plugins.is_empty() {
            blablablars_pythonhost::initialize(&search_paths)?;
        }
        for plugin in &python_plugins {
            pipeline.register(blablablars_pythonhost::load_path_or_module(plugin)?)?;
        }
    }
    #[cfg(not(feature = "python"))]
    {
        // `--python-plugin` does not exist in this build, but the environment
        // could still ask for one; say so instead of silently ignoring it.
        if std::env::var_os("BLABLABLARS_PLUGINS").is_some() {
            return Err(CoreError::Plugin(
                "this build was compiled without embedded Python \
                 (rebuild with the default features) — use --plugin instead"
                    .to_string(),
            ));
        }
    }

    for command in &args.process_plugins {
        pipeline.register(Arc::new(ProcessExtension::spawn_line(command)?))?;
    }

    Ok(pipeline)
}

/// A `PATH`-separated list from the environment, empty when unset.
fn env_paths(name: &str) -> Vec<PathBuf> {
    match std::env::var_os(name) {
        Some(value) if !value.is_empty() => std::env::split_paths(&value).collect(),
        _ => Vec::new(),
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
