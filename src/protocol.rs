use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub command: String,
    pub args: Vec<String>,
    pub stdin: String,
    pub pane: Option<String>,
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
