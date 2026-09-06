//! Read existing CLI transcripts without a parallel journal or mutable history cache.
use crate::herdr::{Agent, error};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::VecDeque,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

const MAX_HISTORY_BYTES: u64 = 2 * 1024 * 1024;
const MAX_MESSAGES: usize = 500;
const MAX_SCAN_ENTRIES: usize = 100_000;

#[derive(Debug, Serialize)]
pub struct Message {
    pub id: String,
    pub role: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct Conversation {
    pub pane_id: String,
    pub session_id: String,
    pub messages: Vec<Message>,
    pub truncated: bool,
}

/// Synchronous bounded disk I/O; HTTP callers should use `spawn_blocking`.
/// # Errors
/// Returns an error for missing, ambiguous, mismatched, or unreadable native transcripts.
pub fn read(agent: &Agent, home: &Path) -> Result<Conversation> {
    let session = agent
        .agent_session
        .as_ref()
        .ok_or_else(|| error("unregistered", "Native session information is absent"))?;
    let session_id = agent
        .session_id
        .clone()
        .ok_or_else(|| error("unavailable", "Session process identity is unavailable"))?;
    let root = match agent.harness.as_str() {
        "codex" => home.join(".codex/sessions"),
        "claude" => home.join(".claude/projects"),
        _ => {
            return Err(error(
                "unsupported",
                "History supports Codex and Claude Code",
            ));
        }
    };
    let path = resolve_path(&root, &session.kind, &session.value)?;
    let expected_id = if session.kind == "id" {
        session.value.clone()
    } else {
        path.file_stem()
            .and_then(|p| p.to_str())
            .context("Invalid transcript filename")?
            .to_owned()
    };
    let mut file = File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(&path)
        .context("Cannot open native session history")?;
    if !file.metadata()?.is_file() {
        bail!("Native history is not a regular file");
    }
    if agent.harness == "codex" {
        let mut header = String::new();
        BufReader::new((&mut file).take(MAX_HISTORY_BYTES)).read_line(&mut header)?;
        let value: Value =
            serde_json::from_str(&header).context("Native session header is incomplete")?;
        if value["type"] != "session_meta"
            || (session.kind == "id"
                && value["payload"]["id"] != expected_id
                && value["payload"]["session_id"] != expected_id)
        {
            return Err(error(
                "session_changed",
                "Native history belongs to another session",
            ));
        }
    }
    let size = file.metadata()?.len();
    let start = size.saturating_sub(MAX_HISTORY_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(MAX_HISTORY_BYTES).read_to_end(&mut bytes)?;
    let (messages, truncated) = parse_history(&agent.harness, &expected_id, &bytes, start);
    Ok(Conversation {
        pane_id: agent.pane_id.clone(),
        session_id,
        messages,
        truncated,
    })
}

fn resolve_path(root: &Path, kind: &str, value: &str) -> Result<PathBuf> {
    let root = root
        .canonicalize()
        .context("Native session history directory is unavailable")?;
    let path = match kind {
        "path" => PathBuf::from(value)
            .canonicalize()
            .context("Native transcript path is unavailable")?,
        "id" => {
            if value.is_empty()
                || value.len() > 128
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return Err(error("unregistered", "Invalid native session ID"));
            }
            find_session(&root, value)?
        }
        _ => return Err(error("unsupported", "Unsupported native session reference")),
    };
    if !path.starts_with(root) || path.extension().is_none_or(|e| e != "jsonl") {
        bail!("Native transcript is outside its CLI history directory");
    }
    Ok(path)
}

fn find_session(root: &Path, id: &str) -> Result<PathBuf> {
    let mut pending = vec![(root.to_owned(), 0)];
    let mut found = None;
    let mut inspected = 0;
    let filename = format!("{id}.jsonl");
    let suffix = format!("-{id}.jsonl");
    while let Some((directory, depth)) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            inspected += 1;
            if inspected > MAX_SCAN_ENTRIES {
                bail!("Native history directory exceeds scan limit");
            }
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_dir() && depth < 3 {
                pending.push((entry.path(), depth + 1));
            }
            if ty.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name == filename || name.ends_with(&suffix))
            {
                if found.is_some() {
                    bail!("Multiple native transcripts match this session");
                }
                found = Some(entry.path());
            }
        }
    }
    found.ok_or_else(|| error("unavailable", "Native transcript has not appeared yet"))
}

fn parse_history(
    harness: &str,
    expected_id: &str,
    bytes: &[u8],
    start: u64,
) -> (Vec<Message>, bool) {
    let mut messages = VecDeque::new();
    let mut truncated = start > 0;
    let mut offset = 0;
    // A bounded tail may start in the middle of a JSON record.
    if start > 0 {
        offset = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
    }
    while offset < bytes.len() {
        let Some(end) = bytes[offset..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| offset + i)
        else {
            truncated = true;
            break;
        };
        match serde_json::from_slice::<Value>(&bytes[offset..end]) {
            Ok(value) => {
                let matches =
                    harness != "claude" || value["sessionId"].as_str() == Some(expected_id);
                if matches
                    && let Some(message) = parse_message(harness, &value, start + offset as u64)
                {
                    messages.push_back(message);
                    if messages.len() > MAX_MESSAGES {
                        messages.pop_front();
                        truncated = true;
                    }
                }
            }
            Err(_) => truncated = true,
        }
        offset = end + 1;
    }
    (messages.into(), truncated)
}

fn parse_message(harness: &str, value: &Value, offset: u64) -> Option<Message> {
    let message = match harness {
        "codex" if value["type"] == "response_item" && value["payload"]["type"] == "message" => {
            &value["payload"]
        }
        "claude"
            if matches!(value["type"].as_str(), Some("user" | "assistant"))
                && value["isSidechain"] != true =>
        {
            &value["message"]
        }
        _ => return None,
    };
    if message["channel"] == "analysis" {
        return None;
    }
    let role = message["role"]
        .as_str()
        .filter(|r| matches!(*r, "user" | "assistant"))?;
    if harness == "codex"
        && role == "user"
        && message["internal_chat_message_metadata_passthrough"]["content_item_kinds"]
            .as_array()
            .is_some_and(|kinds| {
                !kinds.is_empty()
                    && kinds.iter().all(|kind| {
                        matches!(
                            kind.as_str(),
                            Some("agents_md.instructions" | "environments.environment_context")
                        )
                    })
            })
    {
        return None;
    }
    let content = &message["content"];
    let text = if let Some(text) = content.as_str() {
        text.to_owned()
    } else {
        content
            .as_array()?
            .iter()
            .filter(|part| {
                matches!(
                    part["type"].as_str(),
                    Some("text" | "input_text" | "output_text")
                )
            })
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    if text.is_empty() {
        return None;
    }
    Some(Message {
        id: format!("{offset}"),
        role: role.to_owned(),
        text,
        timestamp: value["timestamp"].as_str().map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codex_uses_canonical_messages_without_event_duplicates() {
        let lines = [
            json!({"timestamp":"now","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"exact\ninput"}]}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"exact\ninput"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"report"}]}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","output":"secret tool output"}}),
        ];
        let messages: Vec<_> = lines
            .iter()
            .enumerate()
            .filter_map(|(i, v)| parse_message("codex", v, i as u64))
            .collect();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "exact\ninput");
        assert_eq!(messages[1].role, "assistant");
    }

    #[test]
    fn claude_excludes_thinking_and_tool_results() {
        let value = json!({"type":"assistant","uuid":"a","timestamp":"now","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hidden"},{"type":"text","text":"visible"},{"type":"tool_use","input":"hidden"}]}});
        assert_eq!(parse_message("claude", &value, 0).unwrap().text, "visible");
        let value = json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"hidden"}]}});
        assert!(parse_message("claude", &value, 0).is_none());
    }
    fn agent(harness: &str, id: &str) -> Agent {
        Agent {
            pane_id: "w1:p2".into(),
            name: "test".into(),
            workspace: "w1".into(),
            cwd: "/tmp".into(),
            harness: harness.into(),
            status: "idle".into(),
            session_id: Some("opaque-session".into()),
            send_unavailable: None,
            terminal_id: "terminal".into(),
            agent_session: Some(crate::herdr::AgentSession {
                source: format!("herdr:{harness}"),
                agent: harness.into(),
                kind: "id".into(),
                value: id.into(),
            }),
        }
    }

    #[test]
    fn reads_native_codex_file_and_rejects_filename_session_mismatch() {
        let home = tempfile::tempdir().unwrap();
        let directory = home.path().join(".codex/sessions/2026/09/06");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("rollout-date-session-a.jsonl");
        let header = json!({"type":"session_meta","payload":{"id":"session-a"}});
        let message = json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"report"}]}});
        fs::write(&path, format!("{header}\n{message}\n")).unwrap();
        let result = read(&agent("codex", "session-a"), home.path()).unwrap();
        assert_eq!(result.messages[0].text, "report");
        assert!(!result.truncated);
        fs::write(
            &path,
            format!(
                "{}\n{message}\n",
                json!({"type":"session_meta","payload":{"id":"different"}})
            ),
        )
        .unwrap();
        assert!(read(&agent("codex", "session-a"), home.path()).is_err());
    }

    #[test]
    fn bounded_tail_excludes_partial_records_and_other_claude_sessions() {
        let make = |session: &str, text: &str| {
            json!({"type":"user","sessionId":session,"message":{"role":"user","content":text}})
                .to_string()
        };
        let bytes = format!(
            "partial\n{}\n{}\n{{unfinished",
            make("mine", "visible"),
            make("other", "hidden")
        );
        let (messages, truncated) = parse_history("claude", "mine", bytes.as_bytes(), 100);
        assert!(truncated);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "visible");
        assert_eq!(messages[0].id, "108");
    }

    #[test]
    fn rejects_transcript_path_escape_and_ambiguous_id() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("history");
        fs::create_dir(&root).unwrap();
        let outside = home.path().join("other.jsonl");
        fs::write(&outside, "").unwrap();
        assert!(resolve_path(&root, "path", outside.to_str().unwrap()).is_err());
        fs::write(root.join("session-a.jsonl"), "").unwrap();
        fs::write(root.join("rollout-session-a.jsonl"), "").unwrap();
        assert!(resolve_path(&root, "id", "session-a").is_err());
        assert!(resolve_path(&root, "id", "../other").is_err());
    }

    #[test]
    fn excludes_internal_assistant_analysis() {
        let value = json!({"type":"response_item","payload":{"type":"message","role":"assistant","channel":"analysis","content":[{"type":"output_text","text":"internal"}]}});
        assert!(parse_message("codex", &value, 0).is_none());
    }
    #[test]
    fn codex_context_is_filtered_by_metadata_without_filtering_user_text() {
        let mut value = json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions"}],"internal_chat_message_metadata_passthrough":{"content_item_kinds":["agents_md.instructions","environments.environment_context"]}}});
        assert!(parse_message("codex", &value, 0).is_none());
        value["payload"]["internal_chat_message_metadata_passthrough"]["content_item_kinds"] =
            json!(["user.text"]);
        assert_eq!(
            parse_message("codex", &value, 0).unwrap().text,
            "# AGENTS.md instructions"
        );
        value["payload"]
            .as_object_mut()
            .unwrap()
            .remove("internal_chat_message_metadata_passthrough");
        assert!(parse_message("codex", &value, 0).is_some());
    }
}
