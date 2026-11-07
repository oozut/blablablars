//! End-to-end tests for the `blablablars` binary.
//!
//! They deliberately avoid third-party dev-dependencies: the binary path comes
//! from `CARGO_BIN_EXE_*` and plugins are plain `python3` scripts.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

fn blablablars() -> Command {
    Command::new(env!("CARGO_BIN_EXE_blablablars"))
}

fn run(args: &[&str]) -> Output {
    blablablars()
        .args(args)
        .output()
        .expect("the binary should run")
}

fn run_stdin(args: &[&str], stdin: &str) -> Output {
    let mut child = blablablars()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary should run");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(stdin.as_bytes())
        .expect("stdin should accept writes");
    child.wait_with_output().expect("the binary should finish")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output)).expect("stdout should be JSON")
}

/// Write a protocol-compliant plugin to a temporary directory.
fn temporary_plugin(name: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("blablablars-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir should be creatable");
    let path = dir.join(format!("{name}.py"));
    std::fs::write(&path, body).expect("plugin should be writable");
    path
}

const UPPERCASE_PLUGIN: &str = r#"
import json, sys

for line in sys.stdin:
    request = json.loads(line)
    method = request["method"]
    if method == "describe":
        result = {"protocol": 1, "name": "uppercase", "hooks": ["pipeline.input"]}
        sys.stdout.write(json.dumps({"id": request["id"], "result": result}) + "\n")
    elif method == "call":
        payload = request["params"]["payload"]
        payload["text"] = payload["text"].upper()
        sys.stdout.write(json.dumps({"id": request["id"], "result": payload}) + "\n")
    elif method == "shutdown":
        sys.stdout.write(json.dumps({"id": request["id"], "result": {}}) + "\n")
        sys.stdout.flush()
        break
    sys.stdout.flush()
"#;

#[test]
fn version_is_reported_plainly_and_as_json() {
    let plain = run(&["version"]);
    assert!(plain.status.success());
    assert!(stdout(&plain).starts_with("blablablars "));

    let payload = json(&run(&["version", "--json"]));
    assert_eq!(payload["name"], "blablablars");
    assert_eq!(payload["protocol"], 1);
}

#[test]
fn hooks_are_listed() {
    assert_eq!(
        stdout(&run(&["hooks"])),
        "pipeline.input\npipeline.output\n"
    );

    let listed = json(&run(&["hooks", "--json"]));
    assert_eq!(
        listed,
        serde_json::json!(["pipeline.input", "pipeline.output"])
    );
}

#[test]
fn run_prints_a_human_report() {
    let output = run(&["run", "hello brave world"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("text:       hello brave world"));
    assert!(text.contains("words:      3"));
    assert!(text.contains("characters: 17"));
    assert!(text.contains("trace:      (no extensions)"));
}

#[test]
fn run_json_matches_the_shared_shape() {
    let payload = json(&run(&["run", "hello world", "--json"]));
    assert_eq!(payload["analysis"]["text"], "hello world");
    assert_eq!(payload["analysis"]["words"], 2);
    assert_eq!(payload["analysis"]["characters"], 11);
    assert_eq!(payload["trace"], serde_json::json!([]));
}

#[test]
fn run_reads_standard_input_when_no_text_is_given() {
    let output = run_stdin(&["run", "--json"], "piped text\n");
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(json(&output)["analysis"]["text"], "piped text");
}

#[test]
fn a_plugin_can_change_the_result() {
    let plugin = temporary_plugin("uppercase", UPPERCASE_PLUGIN);
    let command = format!("python3 {}", plugin.display());

    let output = run(&["run", "shout", "--json", "--plugin", &command]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let payload = json(&output);
    assert_eq!(payload["analysis"]["text"], "SHOUT");
    assert_eq!(
        payload["trace"],
        serde_json::json!(["uppercase: pipeline.input"])
    );
}

#[test]
fn plugins_are_described_after_the_handshake() {
    let plugin = temporary_plugin("uppercase-described", UPPERCASE_PLUGIN);
    let command = format!("python3 {}", plugin.display());

    let output = run(&["plugins", "--plugin", &command]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "uppercase: pipeline.input\n");
}

#[test]
fn a_broken_plugin_fails_the_command_with_a_message() {
    let plugin = temporary_plugin(
        "broken",
        "import sys\nsys.stdout.write('not json\\n')\nsys.stdout.flush()\nimport time; time.sleep(5)\n",
    );
    let command = format!("python3 {}", plugin.display());

    let output = run(&["run", "hi", "--plugin", &command]);

    assert!(!output.status.success());
    assert!(stderr(&output).contains("plugin"));
}

#[test]
fn a_missing_program_fails_with_a_helpful_message() {
    let output = run(&["run", "hi", "--plugin", "definitely-not-a-real-program-xyz"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("could not start plugin"));
}
