use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::PathBuf};

pub struct CliFixture {
    pub directory: tempfile::TempDir,
}
impl CliFixture {
    pub fn new(row: &Value) -> Self {
        let fixture = Self {
            directory: tempfile::tempdir().unwrap(),
        };
        std::fs::write(fixture.executable(), include_str!("herdr_cli.py")).unwrap();
        std::fs::set_permissions(fixture.executable(), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::write(
            fixture.directory.path().join("state.json"),
            json!({"row":row}).to_string(),
        )
        .unwrap();
        fixture
    }
    pub fn executable(&self) -> PathBuf {
        self.directory.path().join("herdr")
    }
    pub fn update(&self, update: impl FnOnce(&mut Value)) {
        let path = self.directory.path().join("state.json");
        let mut state: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        update(&mut state);
        std::fs::write(path, state.to_string()).unwrap();
    }
    pub fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(self.directory.path().join("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    pub fn prompts(&self) -> Vec<Value> {
        self.calls()
            .into_iter()
            .filter(|call| call[0] == "agent" && call[1] == "prompt")
            .map(|call| json!({"target":call[2],"text":call[3]}))
            .collect()
    }
}
