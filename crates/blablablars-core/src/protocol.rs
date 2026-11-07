//! Newline-delimited JSON protocol spoken between the application and a plugin
//! process.
//!
//! One request per line, one response per line, always with a matching `id`.
//! The same wire format is implemented by `blablablars.runtime` on the Python
//! side, so a plugin cannot drift from what the Rust host expects.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Bumped whenever the wire format changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

pub const METHOD_DESCRIBE: &str = "describe";
pub const METHOD_CALL: &str = "call";
pub const METHOD_SHUTDOWN: &str = "shutdown";

/// A request written to the plugin's stdin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self {
            id,
            method: method.into(),
            params,
        }
    }

    pub fn describe(id: u64) -> Self {
        Self::new(id, METHOD_DESCRIBE, json!({}))
    }

    pub fn call(id: u64, hook: &str, payload: Value) -> Self {
        Self::new(id, METHOD_CALL, json!({ "hook": hook, "payload": payload }))
    }

    pub fn shutdown(id: u64) -> Self {
        Self::new(id, METHOD_SHUTDOWN, json!({}))
    }
}

/// A response read from the plugin's stdout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    /// `Null` means "nothing to report"; for `call` it means the payload did not
    /// change. Missing is normalised to `Null` by serde.
    #[serde(default)]
    pub result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ProtocolError>,
}

impl Response {
    pub fn ok(id: u64, result: Value) -> Self {
        Self {
            id,
            result,
            error: None,
        }
    }

    pub fn err(id: u64, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id,
            result: Value::Null,
            error: Some(ProtocolError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }

    /// Collapse the response into a `Result`, turning a protocol error into a message.
    ///
    /// A successful `Null` result is meaningful: it means "payload unchanged".
    pub fn into_result(self) -> Result<Value, ProtocolError> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.result),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProtocolError {}

/// What a plugin answers to `describe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginDescription {
    pub protocol: u32,
    pub name: String,
    pub hooks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl PluginDescription {
    pub fn new(name: impl Into<String>, hooks: Vec<String>) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            name: name.into(),
            hooks,
            version: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_through_json() {
        let request = Request::call(7, "pipeline.input", json!({"text": "hi"}));
        let line = serde_json::to_string(&request).unwrap();
        assert_eq!(
            line,
            r#"{"id":7,"method":"call","params":{"hook":"pipeline.input","payload":{"text":"hi"}}}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
    }

    #[test]
    fn responses_omit_absent_fields() {
        let line = serde_json::to_string(&Response::ok(1, json!({"ok": true}))).unwrap();
        assert_eq!(line, r#"{"id":1,"result":{"ok":true}}"#);
        let parsed: Response = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed.into_result().unwrap(), json!({"ok": true}));
    }

    #[test]
    fn errors_survive_the_round_trip() {
        let line = serde_json::to_string(&Response::err(2, "boom", "it broke")).unwrap();
        let error = serde_json::from_str::<Response>(&line)
            .unwrap()
            .into_result()
            .unwrap_err();
        assert_eq!(error.code, "boom");
        assert_eq!(error.message, "it broke");
    }

    #[test]
    fn a_null_or_missing_result_means_unchanged() {
        for line in [r#"{"id":3,"result":null}"#, r#"{"id":3}"#] {
            let response: Response = serde_json::from_str(line).unwrap();
            assert_eq!(response.into_result().unwrap(), Value::Null);
        }
    }

    #[test]
    fn descriptions_default_to_the_current_protocol_version() {
        let description = PluginDescription::new("p", vec!["pipeline.input".to_string()]);
        assert_eq!(description.protocol, PROTOCOL_VERSION);
    }
}
