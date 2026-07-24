use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SendOptions {
    pub from: Option<String>,
    pub skill: Option<String>,
    #[serde(default)]
    pub no_reply: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub command: String,
    pub args: Vec<String>,
    pub stdin: String,
    pub pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_options: Option<SendOptions>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Response {
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    pub fn error(stderr: impl Into<String>) -> Self {
        Self {
            code: 1,
            stdout: String::new(),
            stderr: format!("agent-talk: {}\n", stderr.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_legacy_request_without_send_options() {
        let request: Request = serde_json::from_str(
            r#"{"command":"send","args":["claude","body"],"stdin":"","pane":"%1"}"#,
        )
        .unwrap();
        assert!(request.send_options.is_none());

        let options: SendOptions = serde_json::from_str(r#"{}"#).unwrap();
        assert!(!options.no_reply);
    }
}
