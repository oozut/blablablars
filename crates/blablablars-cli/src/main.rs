//! The `blablablars` command line application.

use std::io::{IsTerminal, Read};
use std::process::ExitCode;
use std::sync::Arc;

use blablablars_core::{CoreError, Pipeline, ProcessExtension, HOOKS, VERSION};
use clap::{Parser, Subcommand};

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

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the pipeline over TEXT (or standard input when TEXT is omitted).
    Run {
        /// Text to analyse. Reads standard input when missing.
        text: Option<String>,
        /// Print the machine-readable form shared with the Python bindings.
        #[arg(long)]
        json: bool,
        /// Plugin command line, e.g. "python3 -m blablablars.runtime my_plugin.py".
        /// Repeatable; extensions run in the order given.
        #[arg(long = "plugin", value_name = "COMMAND")]
        plugins: Vec<String>,
    },
    /// List the hook vocabulary extensions can subscribe to.
    Hooks {
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Start the given plugins and print what they report about themselves.
    Plugins {
        #[arg(long = "plugin", value_name = "COMMAND", required = true)]
        plugins: Vec<String>,
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
            plugins,
        } => {
            let pipeline = build_pipeline(&plugins)?;
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
        Command::Plugins { plugins, json } => {
            let pipeline = build_pipeline(&plugins)?;
            let described = pipeline.registry().describe();
            if json {
                println!("{}", serde_json::to_string_pretty(&described)?);
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
                    }))?
                );
            } else {
                println!("blablablars {VERSION}");
            }
            Ok(())
        }
    }
}

fn build_pipeline(plugins: &[String]) -> Result<Pipeline, CoreError> {
    let mut pipeline = Pipeline::new();
    for command in plugins {
        let extension = ProcessExtension::spawn_line(command)?;
        pipeline.register(Arc::new(extension))?;
    }
    Ok(pipeline)
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
