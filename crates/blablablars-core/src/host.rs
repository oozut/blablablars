//! Host side of the plugin protocol: run a plugin as a child process and adapt
//! it to the [`Extension`] trait.
//!
//! This is what lets a Python plugin take part in the Rust application without
//! embedding a Python interpreter.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

use serde_json::Value;

use crate::error::CoreError;
use crate::extension::{Extension, HOOKS};
use crate::protocol::{PluginDescription, Request, Response, PROTOCOL_VERSION};

/// An extension backed by a long-lived child process speaking the JSON protocol.
///
/// Requests are serialised through a mutex, so the extension can be shared
/// across threads even though the child handles one request at a time.
pub struct ProcessExtension {
    description: PluginDescription,
    connection: Mutex<Connection>,
}

struct Connection {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl ProcessExtension {
    /// Spawn `program` with `args`, then perform the `describe` handshake.
    pub fn spawn(program: &str, args: &[String]) -> Result<Self, CoreError> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                CoreError::Plugin(format!("could not start plugin `{program}`: {error}"))
            })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            CoreError::Plugin(format!("plugin `{program}` has no writable stdin"))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            CoreError::Plugin(format!("plugin `{program}` has no readable stdout"))
        })?;

        let mut connection = Connection {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        };

        let value = connection.request(Request::describe(connection.next_id))?;
        let description: PluginDescription = serde_json::from_value(value).map_err(|error| {
            CoreError::Plugin(format!(
                "invalid `describe` response from `{program}`: {error}"
            ))
        })?;

        if description.protocol != PROTOCOL_VERSION {
            return Err(CoreError::Plugin(format!(
                "plugin `{}` speaks protocol {}, this build speaks {}",
                description.name, description.protocol, PROTOCOL_VERSION
            )));
        }
        for hook in &description.hooks {
            if !HOOKS.contains(&hook.as_str()) {
                return Err(CoreError::Plugin(format!(
                    "plugin `{}` subscribes to unknown hook `{hook}` (known hooks: {})",
                    description.name,
                    HOOKS.join(", ")
                )));
            }
        }

        Ok(Self {
            description,
            connection: Mutex::new(connection),
        })
    }

    /// Spawn from a whitespace-separated command line, e.g.
    /// `"python3 -m blablablars.runtime plugin.py"`.
    ///
    /// Quoting is intentionally not supported: put anything exotic in a wrapper
    /// script and reference that instead.
    pub fn spawn_line(command_line: &str) -> Result<Self, CoreError> {
        let mut parts = command_line.split_whitespace();
        let program = parts
            .next()
            .ok_or_else(|| CoreError::Plugin("plugin command line is empty".to_string()))?;
        let args: Vec<String> = parts.map(str::to_string).collect();
        Self::spawn(program, &args)
    }

    pub fn description(&self) -> &PluginDescription {
        &self.description
    }
}

impl Extension for ProcessExtension {
    fn name(&self) -> &str {
        &self.description.name
    }

    fn hooks(&self) -> &[String] {
        &self.description.hooks
    }

    fn call(&self, hook: &str, payload: &Value) -> Result<Value, crate::error::ExtensionError> {
        let mut connection = self.connection.lock().map_err(|_| {
            crate::error::ExtensionError::failed(
                &self.description.name,
                hook,
                "plugin connection lock was poisoned by an earlier panic",
            )
        })?;
        let request = Request::call(connection.next_id, hook, payload.clone());
        let value = connection.request(request).map_err(|error| {
            crate::error::ExtensionError::failed(&self.description.name, hook, error.to_string())
        })?;
        // `null` means "I did not change anything", which keeps simple observers
        // from having to echo a payload they never inspected.
        Ok(if value.is_null() {
            payload.clone()
        } else {
            value
        })
    }
}

impl std::fmt::Debug for ProcessExtension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessExtension")
            .field("description", &self.description)
            .finish()
    }
}

impl Connection {
    fn request(&mut self, request: Request) -> Result<Value, CoreError> {
        let id = request.id;
        self.next_id = id + 1;

        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.flush()?;

        let mut reply = String::new();
        if self.stdout.read_line(&mut reply)? == 0 {
            return Err(CoreError::Plugin(format!(
                "plugin closed its stdout before answering request {id} \
                 (did it write something other than protocol JSON to stdout?)"
            )));
        }

        let response: Response = serde_json::from_str(reply.trim()).map_err(|error| {
            CoreError::Plugin(format!(
                "plugin sent non-protocol output on stdout for request {id}: {error}: {}",
                reply.trim()
            ))
        })?;
        if response.id != id {
            return Err(CoreError::Plugin(format!(
                "plugin answered request {id} with id {}",
                response.id
            )));
        }
        response
            .into_result()
            .map_err(|error| CoreError::Plugin(error.to_string()))
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = writeln!(
            self.stdin,
            "{}",
            serde_json::json!({
                "id": self.next_id,
                "method": "shutdown",
                "params": {}
            })
        );
        let _ = self.stdin.flush();
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Pipeline;
    use std::sync::Arc;

    /// A protocol-compliant plugin that needs nothing but python3.
    const INLINE_PLUGIN: &str = r#"
import json, sys

def reply(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    method = request["method"]
    if method == "describe":
        reply({"id": request["id"], "result": {
            "protocol": 1, "name": "inline", "hooks": ["pipeline.input", "pipeline.output"]}})
    elif method == "call":
        payload = request["params"]["payload"]
        if "text" in payload:
            payload["text"] = payload["text"].upper()
        reply({"id": request["id"], "result": payload})
    elif method == "shutdown":
        reply({"id": request["id"], "result": {}})
        break
    else:
        reply({"id": request["id"], "error": {"code": "unknown_method", "message": method}})
"#;

    fn inline_extension() -> ProcessExtension {
        ProcessExtension::spawn("python3", &["-c".to_string(), INLINE_PLUGIN.to_string()])
            .expect("inline python plugin should start")
    }

    #[test]
    fn handshake_reports_name_and_hooks() {
        let extension = inline_extension();
        assert_eq!(extension.name(), "inline");
        assert_eq!(
            extension.hooks(),
            vec!["pipeline.input".to_string(), "pipeline.output".to_string()]
        );
    }

    #[test]
    fn a_process_extension_takes_part_in_the_pipeline() {
        let mut pipeline = Pipeline::new();
        pipeline.register(Arc::new(inline_extension())).unwrap();

        let run = pipeline.run("shout").unwrap();

        assert_eq!(run.analysis.text, "SHOUT");
        assert_eq!(
            run.trace,
            vec!["inline: pipeline.input", "inline: pipeline.output"]
        );
    }

    #[test]
    fn a_null_result_means_payload_unchanged() {
        let script = r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    method = request["method"]
    if method == "describe":
        result = {"protocol": 1, "name": "observer", "hooks": ["pipeline.input"]}
        sys.stdout.write(json.dumps({"id": request["id"], "result": result}) + "\n")
    elif method == "call":
        sys.stdout.write(json.dumps({"id": request["id"], "result": None}) + "\n")
    elif method == "shutdown":
        sys.stdout.write(json.dumps({"id": request["id"], "result": {}}) + "\n")
        sys.stdout.flush()
        break
    sys.stdout.flush()
"#;
        let extension = ProcessExtension::spawn("python3", &["-c".to_string(), script.to_string()])
            .expect("observer plugin should start");

        let mut pipeline = Pipeline::new();
        pipeline.register(Arc::new(extension)).unwrap();
        let run = pipeline.run("unchanged text").unwrap();

        assert_eq!(run.analysis.text, "unchanged text");
        assert_eq!(run.trace, vec!["observer: pipeline.input"]);
    }

    #[test]
    fn a_plugin_that_closes_unexpectedly_is_an_error_not_a_panic() {
        let error = ProcessExtension::spawn(
            "python3",
            &["-c".to_string(), "import sys; sys.exit(3)".to_string()],
        )
        .unwrap_err();
        assert!(matches!(error, CoreError::Plugin(_)));
    }

    #[test]
    fn protocol_version_mismatch_is_refused() {
        let script = r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if request["method"] == "describe":
        sys.stdout.write(json.dumps({"id": request["id"], "result": {
            "protocol": 999, "name": "future", "hooks": []}}) + "\n")
        sys.stdout.flush()
"#;
        let error = ProcessExtension::spawn("python3", &["-c".to_string(), script.to_string()])
            .unwrap_err();
        assert!(
            matches!(error, CoreError::Plugin(message) if message.contains("speaks protocol 999"))
        );
    }

    #[test]
    fn an_empty_command_line_is_refused() {
        let error = ProcessExtension::spawn_line("   ").unwrap_err();
        assert!(matches!(error, CoreError::Plugin(message) if message.contains("empty")));
    }
}
