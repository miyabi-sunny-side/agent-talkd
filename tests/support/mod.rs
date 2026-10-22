use serde_json::{Value, json};
use std::path::PathBuf;

pub struct CliFixture {
    pub directory: tempfile::TempDir,
}
impl CliFixture {
    pub fn new(row: &Value) -> Self {
        let fixture = Self {
            directory: tempfile::tempdir().expect("create fixture directory"),
        };
        let binary = std::env::current_exe()
            .expect("locate test executable")
            .parent()
            .and_then(std::path::Path::parent)
            .expect("test executable lives under target/<profile>/deps")
            .join("examples/herdr-fixture");
        std::fs::copy(binary, fixture.executable())
            .expect("cargo test builds the isolated herdr-fixture example");
        std::fs::write(
            fixture.directory.path().join("state.json"),
            json!({"row":row}).to_string(),
        )
        .expect("write fixture state");
        fixture
    }
    pub fn executable(&self) -> PathBuf {
        self.directory.path().join("herdr")
    }
    pub fn update(&self, update: impl FnOnce(&mut Value)) {
        let path = self.directory.path().join("state.json");
        let mut state: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read fixture state"))
                .expect("fixture state is JSON written by this harness");
        update(&mut state);
        std::fs::write(path, state.to_string()).expect("write fixture state");
    }
    pub fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(self.directory.path().join("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("fixture logs one JSON call per line"))
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
