//! Read existing CLI transcripts without a parallel journal or mutable history cache.
use crate::herdr::{Agent, error};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const MAX_HISTORY_BYTES: u64 = 2 * 1024 * 1024;
const MAX_MESSAGES: usize = 500;
const MAX_HEADER_BYTES: u64 = 64 * 1024;
// Reserve all header and cursor proof reads inside the 2 MiB request budget.
const PAGE_BYTES: u64 = MAX_HISTORY_BYTES - MAX_HEADER_BYTES - 4096;
const ANCHOR_BYTES: u64 = 256;

#[derive(Debug)]
pub enum Page {
    Latest,
    Before(String),
    After(String),
}

/// Opaque URL-safe cursor. Proofs cover the immutable prefix and local boundary;
/// inode, observed size and mtime additionally detect replacement and truncation.
/// Arbitrary rewrites outside these bounded proofs cannot be detected without
/// scanning the transcript; native transcripts are append-only in normal use.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    dev: u64,
    ino: u64,
    size: u64,
    modified: (i64, i64),
    offset: u64,
    fragment: bool,
    prefix_len: u64,
    prefix: String,
    anchor: String,
}

impl Cursor {
    fn decode(text: &str) -> Result<Self> {
        let invalid = || error("invalid_cursor", "Invalid history cursor");
        if text.is_empty() || text.len() > 2048 || !text.len().is_multiple_of(2) {
            return Err(invalid());
        }
        let bytes = text
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let hex = std::str::from_utf8(pair).map_err(|_| invalid())?;
                u8::from_str_radix(hex, 16).map_err(|_| invalid())
            })
            .collect::<Result<Vec<_>>>()?;
        let cursor: Self = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if cursor.version != 1
            || cursor.offset > cursor.size
            || cursor.prefix_len != cursor.size.min(ANCHOR_BYTES)
            || [&cursor.prefix, &cursor.anchor]
                .iter()
                .any(|hash| hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(invalid());
        }
        Ok(cursor)
    }

    fn encode(&self) -> String {
        use std::fmt::Write;
        serde_json::to_vec(self)
            .expect("cursor serializes")
            .iter()
            .fold(String::new(), |mut out, byte| {
                write!(out, "{byte:02x}").expect("string write");
                out
            })
    }

    fn validate(&self, file: &mut File, metadata: &fs::Metadata) -> Result<()> {
        if self.dev != metadata.dev()
            || self.ino != metadata.ino()
            || self.size > metadata.len()
            || (self.size == metadata.len()
                && self.modified != (metadata.mtime(), metadata.mtime_nsec()))
            || self.prefix != digest_at(file, 0, self.prefix_len)?
            || self.anchor
                != digest_at(
                    file,
                    self.offset.saturating_sub(ANCHOR_BYTES),
                    self.offset.min(ANCHOR_BYTES),
                )?
        {
            return Err(error(
                "cursor_changed",
                "Native history changed; reload the conversation",
            ));
        }
        Ok(())
    }
}

fn digest_at(file: &mut File, offset: u64, length: u64) -> Result<String> {
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; usize::try_from(length)?];
    file.read_exact(&mut bytes)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn make_cursor(file: &mut File, metadata: &fs::Metadata, point: Point) -> Result<String> {
    let prefix_len = metadata.len().min(ANCHOR_BYTES);
    Ok(Cursor {
        version: 1,
        dev: metadata.dev(),
        ino: metadata.ino(),
        size: metadata.len(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        offset: point.offset,
        fragment: point.fragment,
        prefix_len,
        prefix: digest_at(file, 0, prefix_len)?,
        anchor: digest_at(
            file,
            point.offset.saturating_sub(ANCHOR_BYTES),
            point.offset.min(ANCHOR_BYTES),
        )?,
    }
    .encode())
}

/// Validate syntax before doing Herdr or disk I/O.
pub fn validate_cursor(text: &str) -> Result<()> {
    Cursor::decode(text).map(|_| ())
}

#[derive(Clone, Copy, Debug)]
struct Point {
    offset: u64,
    fragment: bool,
}

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
    pub pending_tail: bool,
    pub older_cursor: Option<String>,
    pub next_cursor: String,
    pub has_more: bool,
}

/// Synchronous bounded disk I/O; HTTP callers should use `spawn_blocking`.
/// # Errors
/// Returns an error for missing, ambiguous, mismatched, or unreadable native transcripts.
#[cfg(test)]
pub fn read(agent: &Agent, home: &Path) -> Result<Conversation> {
    read_page(agent, home, &Page::Latest)
}

/// Read one bounded native page in chronological order.
/// # Errors
/// Rejects invalid/stale cursors and unavailable or mismatched transcripts.
#[allow(clippy::too_many_lines)] // Transcript identity checks and bounded page read share one file handle.
pub fn read_page(agent: &Agent, home: &Path, page: &Page) -> Result<Conversation> {
    let cursor = match page {
        Page::Latest => None,
        Page::Before(text) | Page::After(text) => Some(Cursor::decode(text)?),
    };
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
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("Native history is not a regular file");
    }
    if let Some(cursor) = &cursor {
        cursor.validate(&mut file, &metadata)?;
    }
    file.seek(SeekFrom::Start(0))?;
    if agent.harness == "codex" {
        let mut header = String::new();
        BufReader::new((&mut file).take(MAX_HEADER_BYTES)).read_line(&mut header)?;
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
    let size = metadata.len();
    let forward = matches!(page, Page::After(_));
    let point = cursor.as_ref().map_or(
        Point {
            offset: size,
            fragment: false,
        },
        |cursor| Point {
            offset: cursor.offset,
            fragment: cursor.fragment,
        },
    );
    let (start, end) = if forward {
        (
            point.offset,
            size.min(point.offset.saturating_add(PAGE_BYTES)),
        )
    } else {
        // Include one preceding byte so a record exactly at the budget is
        // recognized from its delimiter instead of discarded as a fragment.
        (point.offset.saturating_sub(PAGE_BYTES + 1), point.offset)
    };
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; usize::try_from(end - start)?];
    file.read_exact(&mut bytes)?;
    let parsed = parse_page(
        &agent.harness,
        &expected_id,
        &bytes,
        start,
        size,
        point,
        forward,
    );
    let older_cursor = parsed
        .older
        .map(|point| make_cursor(&mut file, &metadata, point))
        .transpose()?;
    let next_cursor = make_cursor(&mut file, &metadata, parsed.next)?;
    // Reject a concurrent rewrite/truncation; an append can be picked up next time.
    let final_metadata = file.metadata()?;
    if final_metadata.len() < size
        || (final_metadata.len() == size
            && (final_metadata.mtime(), final_metadata.mtime_nsec())
                != (metadata.mtime(), metadata.mtime_nsec()))
    {
        return Err(error(
            "cursor_changed",
            "Native history changed while reading",
        ));
    }
    Ok(Conversation {
        pane_id: agent.pane_id.clone(),
        session_id,
        messages: parsed.messages,
        truncated: parsed.truncated,
        pending_tail: parsed.pending_tail,
        older_cursor,
        next_cursor,
        has_more: parsed.has_more,
    })
}

struct ParsedPage {
    messages: Vec<Message>,
    older: Option<Point>,
    next: Point,
    truncated: bool,
    pending_tail: bool,
    has_more: bool,
}

/// Read windows overlap at the first complete record boundary when going back.
/// A window containing only a giant record advances with a fragment marker,
/// so neither direction needs unbounded buffering or gets stuck on that record.
#[allow(clippy::too_many_lines)]
fn parse_page(
    harness: &str,
    expected_id: &str,
    bytes: &[u8],
    start: u64,
    size: u64,
    point: Point,
    forward: bool,
) -> ParsedPage {
    let end = start + bytes.len() as u64;
    let mut truncated = point.fragment;
    let leading_fragment = if forward { point.fragment } else { start > 0 };
    let first_newline = bytes.iter().position(|b| *b == b'\n');
    let mut offset = if leading_fragment {
        first_newline.map_or(bytes.len(), |i| i + 1)
    } else {
        0
    };
    // Without a delimiter this window is wholly inside an oversized record.
    let no_boundary = leading_fragment && first_newline.is_none();
    let mut older = if start == 0 {
        None
    } else if leading_fragment && offset == bytes.len() {
        truncated = true;
        Some(Point {
            offset: start,
            fragment: true,
        })
    } else {
        Some(Point {
            offset: start + offset as u64,
            fragment: false,
        })
    };
    let mut messages = VecDeque::new();
    let mut next = Point {
        offset: start + offset as u64,
        fragment: no_boundary,
    };
    while offset < bytes.len() {
        let Some(line_end) = bytes[offset..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| offset + i)
        else {
            break;
        };
        match serde_json::from_slice::<Value>(&bytes[offset..line_end]) {
            Ok(value) => {
                let matches =
                    harness != "claude" || value["sessionId"].as_str() == Some(expected_id);
                if matches
                    && let Some(message) = parse_message(harness, &value, start + offset as u64)
                {
                    messages.push_back(message);
                    if messages.len() > MAX_MESSAGES {
                        messages.pop_front();
                        older = Some(Point {
                            offset: messages
                                .front()
                                .expect("nonempty")
                                .id
                                .parse()
                                .expect("byte ID"),
                            fragment: false,
                        });
                    }
                }
            }
            Err(_) => truncated = true,
        }
        offset = line_end + 1;
        next = Point {
            offset: start + offset as u64,
            fragment: false,
        };
        if forward && messages.len() == MAX_MESSAGES {
            break;
        }
    }
    let incomplete = offset < bytes.len() && !bytes[offset..].contains(&b'\n');
    let pending_tail = end == size && (incomplete || (no_boundary && !bytes.is_empty()));
    if forward && incomplete && offset == 0 && end - start == PAGE_BYTES {
        // The single record exceeds the page budget. Skip it in bounded chunks.
        next = Point {
            offset: end,
            fragment: true,
        };
        truncated = true;
    }
    if !forward && point.offset < size {
        next = point;
    }
    // Backwards pages can include only filtered records and still make progress.
    // Forward has_more excludes an ordinary unfinished tail; polling waits for it.
    let has_more = if forward {
        end < size || (offset < bytes.len() && bytes[offset..].contains(&b'\n'))
    } else {
        older.is_some()
    };
    ParsedPage {
        messages: messages.into(),
        older,
        next,
        truncated,
        pending_tail,
        has_more,
    }
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

    fn fixture(lines: &[String]) -> (tempfile::TempDir, PathBuf, Agent) {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".claude/projects/example");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("mine.jsonl");
        fs::write(&path, lines.join("")).unwrap();
        (home, path, agent("claude", "mine"))
    }

    fn line(text: &str) -> String {
        format!(
            "{}\n",
            json!({"type":"user","sessionId":"mine","message":{"role":"user","content":text}})
        )
    }

    #[test]
    fn pages_more_than_500_messages_without_gaps_or_duplicates() {
        let lines: Vec<_> = (0..1203).map(|i| line(&i.to_string())).collect();
        let (home, _, agent) = fixture(&lines);
        let latest = read_page(&agent, home.path(), &Page::Latest).unwrap();
        assert_eq!(latest.messages.len(), 500);
        assert_eq!(latest.messages[0].text, "703");
        assert!(latest.has_more);
        assert!(!latest.truncated);
        let older = read_page(
            &agent,
            home.path(),
            &Page::Before(latest.older_cursor.unwrap()),
        )
        .unwrap();
        assert_eq!(older.messages[0].text, "203");
        let oldest = read_page(
            &agent,
            home.path(),
            &Page::Before(older.older_cursor.unwrap()),
        )
        .unwrap();
        assert_eq!(oldest.messages.len(), 203);
        assert_eq!(oldest.messages[0].text, "0");
        assert!(!oldest.has_more);
        let next = read_page(&agent, home.path(), &Page::After(oldest.next_cursor)).unwrap();
        assert_eq!(next.messages[0].text, "203");
        assert_eq!(next.messages.last().unwrap().text, "702");
        assert!(next.has_more);
        let next = read_page(&agent, home.path(), &Page::After(next.next_cursor)).unwrap();
        assert_eq!(next.messages[0].text, "703");
        assert!(!next.has_more);
    }

    #[test]
    fn appended_partial_tail_is_delivered_once_after_completion() {
        use std::io::Write;
        let incomplete = line("完成した本文");
        let (home, path, agent) =
            fixture(&[line("first"), incomplete[..incomplete.len() - 2].into()]);
        let initial = read_page(&agent, home.path(), &Page::Latest).unwrap();
        assert_eq!(initial.messages.len(), 1);
        assert!(initial.pending_tail);
        let pending = read_page(
            &agent,
            home.path(),
            &Page::After(initial.next_cursor.clone()),
        )
        .unwrap();
        assert!(pending.messages.is_empty());
        assert!(pending.pending_tail);
        let mut file = File::options().append(true).open(path).unwrap();
        write!(
            file,
            "{}{}",
            &incomplete[incomplete.len() - 2..],
            line("new")
        )
        .unwrap();
        let next = read_page(&agent, home.path(), &Page::After(initial.next_cursor)).unwrap();
        assert_eq!(
            next.messages
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>(),
            ["完成した本文", "new"]
        );
        assert!(!next.pending_tail);
        assert!(
            read_page(&agent, home.path(), &Page::After(next.next_cursor))
                .unwrap()
                .messages
                .is_empty()
        );
    }

    #[test]
    fn byte_windows_recover_crossing_records_in_both_directions() {
        let lines: Vec<_> = (0..90)
            .map(|i| line(&format!("{i}:{}", "x".repeat(60_000))))
            .collect();
        let (home, _, agent) = fixture(&lines);
        let mut page = read_page(&agent, home.path(), &Page::Latest).unwrap();
        let mut texts: Vec<_> = page.messages.iter().map(|m| m.text.clone()).collect();
        while let Some(cursor) = page.older_cursor {
            page = read_page(&agent, home.path(), &Page::Before(cursor)).unwrap();
            let mut older: Vec<_> = page.messages.iter().map(|m| m.text.clone()).collect();
            older.append(&mut texts);
            texts = older;
        }
        assert_eq!(texts.len(), 90);
        for (i, text) in texts.iter().enumerate() {
            assert!(text.starts_with(&format!("{i}:")));
        }
        let mut count = page.messages.len();
        while count < 90 {
            page = read_page(&agent, home.path(), &Page::After(page.next_cursor)).unwrap();
            assert!(!page.messages.is_empty());
            for message in &page.messages {
                assert_eq!(message.text, texts[count]);
                count += 1;
            }
        }
    }

    #[test]
    fn oversized_record_allows_bounded_progress_to_earlier_and_later_messages() {
        let (home, _, agent) = fixture(&[
            line("first"),
            line(&"x".repeat(5 * 1024 * 1024)),
            line("last"),
        ]);
        let mut page = read_page(&agent, home.path(), &Page::Latest).unwrap();
        assert_eq!(page.messages[0].text, "last");
        let mut saw_truncated = page.truncated;
        for _ in 0..5 {
            let Some(cursor) = page.older_cursor else {
                break;
            };
            page = read_page(&agent, home.path(), &Page::Before(cursor)).unwrap();
            saw_truncated |= page.truncated;
        }
        assert_eq!(page.messages[0].text, "first");
        assert!(saw_truncated);
        for _ in 0..5 {
            page = read_page(&agent, home.path(), &Page::After(page.next_cursor)).unwrap();
            if page.messages.iter().any(|m| m.text == "last") {
                return;
            }
            assert!(page.has_more);
        }
        panic!("oversized record prevented progress");
    }

    #[test]
    fn exact_byte_budget_record_is_recoverable_at_a_page_boundary() {
        let overhead = line("").len();
        let text = "x".repeat(usize::try_from(PAGE_BYTES).unwrap() - overhead);
        let (home, _, agent) = fixture(&[line("first"), line(&text), line("last")]);
        let latest = read_page(&agent, home.path(), &Page::Latest).unwrap();
        assert_eq!(latest.messages[0].text, "last");
        let older = read_page(
            &agent,
            home.path(),
            &Page::Before(latest.older_cursor.unwrap()),
        )
        .unwrap();
        assert_eq!(older.messages.len(), 1);
        assert_eq!(older.messages[0].text, text);
    }

    #[test]
    fn giant_unfinished_tail_advances_and_resumes_after_its_delimiter() {
        use std::io::Write;
        let (home, path, agent) = fixture(&[line("first"), "x".repeat(5 * 1024 * 1024)]);
        let initial = read_page(&agent, home.path(), &Page::Latest).unwrap();
        assert!(initial.pending_tail);
        assert!(initial.truncated);
        let mut file = File::options().append(true).open(path).unwrap();
        write!(file, "\n{}", line("last")).unwrap();
        let next = read_page(&agent, home.path(), &Page::After(initial.next_cursor)).unwrap();
        assert_eq!(next.messages[0].text, "last");
        assert!(!next.pending_tail);
        assert!(!next.has_more);
    }

    #[test]
    fn cursor_rejects_malformed_replaced_truncated_and_rewritten_files() {
        let (home, path, agent) = fixture(&[line("first"), line("second")]);
        assert!(
            read_page(&agent, home.path(), &Page::After("garbage".into()))
                .unwrap_err()
                .to_string()
                .contains("cursor")
        );
        let initial = read_page(&agent, home.path(), &Page::Latest).unwrap();
        fs::write(&path, line("short")).unwrap();
        assert!(read_page(&agent, home.path(), &Page::After(initial.next_cursor)).is_err());
        let initial = read_page(&agent, home.path(), &Page::Latest).unwrap();
        fs::write(&path, line("other")).unwrap();
        assert!(read_page(&agent, home.path(), &Page::After(initial.next_cursor)).is_err());
        let initial = read_page(&agent, home.path(), &Page::Latest).unwrap();
        let replacement = path.with_extension("tmp");
        fs::write(&replacement, line("other")).unwrap();
        fs::rename(replacement, path).unwrap();
        assert!(read_page(&agent, home.path(), &Page::After(initial.next_cursor)).is_err());
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
        let end = 100 + bytes.len() as u64;
        let page = parse_page(
            "claude",
            "mine",
            bytes.as_bytes(),
            100,
            end,
            Point {
                offset: end,
                fragment: false,
            },
            false,
        );
        assert!(page.pending_tail);
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].text, "visible");
        assert_eq!(page.messages[0].id, "108");
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
