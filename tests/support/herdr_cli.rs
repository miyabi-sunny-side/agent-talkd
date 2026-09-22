//! Isolated executable modelling only the Herdr commands consumed by agent-talk.
use serde_json::{Value, json};
use std::{fs, io::Write, process, thread, time::Duration};

#[allow(clippy::too_many_lines)] // One small command dispatcher for the existing CLI fixture.
fn main() {
    let executable = std::env::current_exe().unwrap();
    let root = executable.parent().unwrap();
    let state_path = root.join("state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    let args: Vec<String> = std::env::args().skip(1).collect();
    writeln!(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("calls.jsonl"))
            .unwrap(),
        "{}",
        json!(args)
    )
    .unwrap();
    let mut rows = vec![state["row"].clone()];
    rows.extend(
        state["extra_rows"]
            .as_array()
            .into_iter()
            .flatten()
            .cloned(),
    );
    let command: Vec<&str> = args.iter().map(String::as_str).collect();
    let pane = match command.as_slice() {
        ["agent", "get" | "prompt", pane, ..]
        | ["pane", "read", pane, ..]
        | ["pane", "process-info", "--pane", pane, ..] => Some(*pane),
        _ => None,
    };
    let index = pane.map_or(0, |pane| {
        rows.iter().position(|row| row["pane_id"] == pane).unwrap()
    });
    let row = &mut rows[index];
    let mode = state["mode"].as_str().unwrap_or("");
    if command.starts_with(&["agent", "prompt"]) {
        match mode {
            "timeout" => thread::sleep(Duration::from_secs(30)),
            "oversized" => {
                print!("{}", "x".repeat(4 * 1024 * 1024 + 1));
                return;
            }
            "stderr-flood" => {
                eprint!("{}", "x".repeat(64 * 1024 + 1));
                process::exit(1);
            }
            "invalid-json" => {
                println!("not json");
                return;
            }
            "exit-failure" => {
                println!(
                    "{}",
                    json!({"result":{"type":"agent_prompted","agent":row}})
                );
                process::exit(2);
            }
            "blocked" => {
                eprintln!(
                    "{}",
                    json!({"error":{"code":"agent_blocked","message":"blocked"}})
                );
                process::exit(1);
            }
            _ => {}
        }
    }
    let result = match command.as_slice() {
        ["agent", "list"] => json!({"agents":rows}),
        ["workspace", "list"] => json!({"workspaces":[]}),
        ["tab", "list", "--workspace", workspace] => {
            assert!(rows.iter().any(|r| r["workspace_id"] == *workspace));
            json!({"tabs":[]})
        }
        ["agent", "get", _] => {
            if let Some(pane) = state.get("get_pane_id") {
                row["pane_id"] = pane.clone();
            }
            json!({"agent":row})
        }
        ["pane", "process-info", "--pane", _] => {
            json!({"type":"pane_process_info","process_info":{"pane_id":row["pane_id"],"foreground_processes":[{"pid":state.get("pid").unwrap_or(&json!(123)),"name":"codex"}]}})
        }
        ["pane", "read", _, "--source", "visible", "--format", "text"] => {
            let screen_mode = state["screen_mode"].as_str().unwrap_or("").to_owned();
            match screen_mode.as_str() {
                "restart" => row["agent_session"]["value"] = json!("session-b"),
                "terminal-restart" => row["terminal_id"] = json!("terminal-2"),
                "ended" => row["agent_session"] = Value::Null,
                _ => {}
            }
            if index == 0 {
                state["row"] = row.clone();
            } else {
                state["extra_rows"][index - 1] = row.clone();
            }
            fs::write(state_path, state.to_string()).unwrap();
            if screen_mode == "oversized" {
                print!("{}", "x".repeat(256 * 1024 + 1));
            } else {
                print!("確認してください\n[許可] [拒否]");
            }
            return;
        }
        ["agent", "prompt", _, _] => {
            if mode == "wrong-terminal" {
                row["terminal_id"] = json!("other-terminal");
            }
            json!({"type":"agent_prompted","agent":row})
        }
        _ => panic!("unexpected arguments: {args:?}"),
    };
    println!("{}", json!({"result":result}));
}
