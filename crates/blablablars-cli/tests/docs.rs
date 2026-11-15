//! The documentation is a promise, so it is checked like one.
//!
//! Two things must not drift: every config key `config show` prints is written
//! down in `docs/dictation.md`, and every key a `--json` emits is named in the
//! shapes list in `docs/cli.md`. A key added to the program without a line in
//! the docs is a broken promise, and this is where it is caught.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use blablablars_core::voice::DictationRun;
use serde_json::Value;

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_blablablars"))
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/blablablars-cli is two levels below the root")
        .to_path_buf()
}

fn document(relative: &str) -> String {
    let path = root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn run(arguments: &[&str]) -> String {
    let output = binary()
        .args(arguments)
        .output()
        .expect("the binary should run");
    assert!(
        output.status.success(),
        "`{}` failed: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn run_json(arguments: &[&str]) -> Value {
    serde_json::from_str(&run(arguments)).expect("stdout should be one JSON document")
}

/// The `a.b.c` path of every leaf key in a YAML document, by indentation alone.
///
/// Enough for the config files here, which are written by the program itself;
/// nothing about the values is interpreted.
fn yaml_keys(yaml: &str) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    for line in yaml.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let indent = line.len() - trimmed.len();
        while stack.last().is_some_and(|(depth, _)| *depth >= indent) {
            stack.pop();
        }
        if value.trim().is_empty() || value.trim_start().starts_with('#') {
            stack.push((indent, key.to_string()));
        } else {
            let mut path: Vec<&str> = stack.iter().map(|(_, name)| name.as_str()).collect();
            path.push(key);
            paths.insert(path.join("."));
        }
    }
    paths
}

/// Every ```` ```yaml ```` block in a Markdown document.
fn yaml_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in markdown.lines() {
        match (&mut current, line.trim()) {
            (None, fence) if fence.starts_with("```yaml") => current = Some(String::new()),
            (None, _) => {}
            (Some(block), "```") => {
                blocks.push(std::mem::take(block));
                current = None;
            }
            (Some(block), _) => {
                block.push_str(line);
                block.push('\n');
            }
        }
    }
    blocks
}

/// Every key name anywhere in a JSON document, at any depth.
fn json_keys(value: &Value) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                keys.insert(key.clone());
                keys.extend(json_keys(nested));
            }
        }
        Value::Array(items) => {
            for item in items {
                keys.extend(json_keys(item));
            }
        }
        _ => {}
    }
    keys
}

/// The keys the shapes list in `docs/cli.md` promises for one command.
fn promised(markdown: &str, command: &str) -> BTreeSet<String> {
    let line = markdown
        .lines()
        .find(|line| line.trim_start().starts_with(command))
        .unwrap_or_else(|| panic!("docs/cli.md has no shapes line for `{command}`"));
    let keys = line
        .trim_start()
        .trim_start_matches(command)
        .replace("(array)", " ");
    keys.split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// A key the program emits that the shapes list does not name is a promise
/// broken quietly, which is the failure this file exists to prevent.
fn assert_every_key_is_promised(command: &str, value: &Value) {
    let promised = promised(&document("docs/cli.md"), command);
    let emitted = json_keys(value);
    let undocumented: Vec<&String> = emitted.difference(&promised).collect();
    assert!(
        undocumented.is_empty(),
        "`{command}` emits keys docs/cli.md does not list: {undocumented:?}"
    );
}

#[test]
fn every_config_key_the_program_prints_is_documented() {
    let printed = yaml_keys(&run(&["config", "show"]));
    assert!(!printed.is_empty(), "`config show` printed no keys");

    let documented: BTreeSet<String> = yaml_blocks(&document("docs/dictation.md"))
        .iter()
        .flat_map(|block| yaml_keys(block))
        .collect();
    let missing: Vec<&String> = printed.difference(&documented).collect();
    assert!(
        missing.is_empty(),
        "docs/dictation.md does not document: {missing:?}"
    );
}

#[test]
fn every_json_key_the_program_emits_is_documented() {
    // `--no-plugins`, so a plugin list on the machine running the tests cannot
    // change what the shape is.
    assert_every_key_is_promised("run --json", &run_json(&["run", "a b", "--json"]));
    assert_every_key_is_promised("version --json", &run_json(&["version", "--json"]));
    assert_every_key_is_promised("hooks --json", &run_json(&["hooks", "--json"]));
    assert_every_key_is_promised(
        "plugins --json",
        &run_json(&["plugins", "--json", "--no-plugins"]),
    );
    assert_every_key_is_promised(
        "plugin list --json",
        &run_json(&["plugin", "list", "--json"]),
    );
    assert_every_key_is_promised("models --json", &run_json(&["models", "--json"]));
    assert_every_key_is_promised("engines --json", &run_json(&["engines", "--json"]));

    // A dictation report is the same serialiser the CLI prints, without needing
    // a microphone, a model or a program to type with.
    let report = DictationRun {
        raw: "raw".to_string(),
        text: "text".to_string(),
        model: "model".to_string(),
        transcriber: "transcriber".to_string(),
        audio_seconds: 1.0,
        elapsed: std::time::Duration::from_millis(5),
        trace: Vec::new(),
    };
    assert_every_key_is_promised("dictate --json", &report.to_json());
}
