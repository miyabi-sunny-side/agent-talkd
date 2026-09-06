//! Thin, bounded Herdr adapter. Session identity is checked immediately before input.
use std::{
    fmt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SCREEN_BYTES: usize = 256 * 1024;
const RPC_TIMEOUT: Duration = Duration::from_secs(5);
pub const MAX_MESSAGE_BYTES: usize = 32 * 1024;

#[derive(Debug)]
pub struct RemoteError {
    pub code: &'static str,
    pub message: String,
}
impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for RemoteError {}
pub fn error(code: &'static str, message: impl Into<String>) -> anyhow::Error {
    RemoteError {
        code,
        message: message.into(),
    }
    .into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    pub source: String,
    pub agent: String,
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub pane_id: String,
    pub name: String,
    pub workspace: String,
    pub cwd: String,
    pub harness: String,
    pub status: String,
    pub session_id: Option<String>,
    pub send_unavailable: Option<String>,
    pub terminal_id: String,
    // Keep the native Herdr field name in the observation API.
    #[allow(clippy::struct_field_names)]
    pub agent_session: Option<AgentSession>,
}

#[derive(Debug, Serialize)]
pub struct Screen {
    pub pane_id: String,
    pub terminal_id: String,
    pub session_id: Option<String>,
    pub text: String,
    /// Unix epoch milliseconds at completion of the pane read.
    pub captured_at: u64,
    pub format: &'static str,
}

#[derive(Clone)]
pub struct Herdr {
    socket: PathBuf,
}
impl Herdr {
    #[must_use]
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// # Errors
    /// Returns an error if Herdr cannot provide a valid agent snapshot.
    pub async fn list(&self) -> Result<Vec<Agent>> {
        tokio::time::timeout(Duration::from_secs(15), self.list_inner())
            .await
            .map_err(|_| error("unavailable", "Herdr agent snapshot timed out"))?
    }

    async fn list_inner(&self) -> Result<Vec<Agent>> {
        let result = self.call("agent.list", json!({}), false).await?;
        let rows = result
            .get("agents")
            .and_then(Value::as_array)
            .context("Herdr agent.list has no agents")?;
        if rows.len() > 256 {
            bail!("Herdr returned too many agents");
        }
        let labels = self.call("workspace.list", json!({}), false).await.ok();
        let mut tabs = std::collections::BTreeMap::new();
        for workspace in rows
            .iter()
            .filter_map(|row| row["workspace_id"].as_str())
            .collect::<std::collections::BTreeSet<_>>()
        {
            if let Ok(result) = self
                .call("tab.list", json!({"workspace_id":workspace}), false)
                .await
            {
                tabs.insert(workspace, result);
            }
        }
        let mut agents = Vec::with_capacity(rows.len());
        for row in rows {
            let mut agent = self.describe(row).await?;
            let tab = tabs
                .get(agent.workspace.as_str())
                .and_then(|result| result["tabs"].as_array())
                .and_then(|tabs| tabs.iter().find(|tab| tab["tab_id"] == row["tab_id"]));
            agent.name = agent_name(row, tab);
            agents.push(agent);
        }
        distinguish_agent_names(&mut agents);
        for agent in &mut agents {
            if let Some(label) = labels
                .as_ref()
                .and_then(|r| r["workspaces"].as_array())
                .and_then(|ws| ws.iter().find(|w| w["workspace_id"] == agent.workspace))
                .and_then(|w| w["label"].as_str())
                .filter(|s| !s.is_empty())
            {
                label.clone_into(&mut agent.workspace);
            }
        }
        Ok(agents)
    }

    /// # Errors
    /// Returns an error for invalid, absent, or unreachable targets.
    pub async fn get(&self, pane: &str) -> Result<Agent> {
        validate_pane(pane)?;
        let result = self
            .call("agent.get", json!({"target":pane}), false)
            .await?;
        let row = result
            .get("agent")
            .context("Herdr agent.get has no agent")?;
        if row["pane_id"].as_str() != Some(pane) {
            return Err(error("session_changed", "Herdr returned a different pane"));
        }
        self.describe(row).await
    }

    async fn describe(&self, row: &Value) -> Result<Agent> {
        let mut agent = parse_agent(row)?;
        if agent.agent_session.is_some()
            && agent.send_unavailable.as_deref() != Some("unregistered")
            && matches!(agent.harness.as_str(), "codex" | "claude")
        {
            let process = self
                .call("pane.process_info", json!({"pane_id":agent.pane_id}), false)
                .await;
            match process.and_then(|v| process_identity(&v, &agent.pane_id, &agent.harness)) {
                Ok(pid) => agent.session_id = Some(session_token(&agent, pid)),
                Err(_) => agent.send_unavailable = Some("unavailable".into()),
            }
        }
        Ok(agent)
    }

    /// Read the visible terminal without accepting output from a changed session.
    /// # Errors
    /// Rejects stale identities, malformed output, oversized screens, and unavailable panes.
    pub async fn screen(
        &self,
        pane: &str,
        expected_terminal: &str,
        expected_session: Option<&str>,
    ) -> Result<Screen> {
        if expected_terminal.is_empty()
            || expected_terminal.len() > 256
            || expected_session.is_some_and(|session| session.is_empty() || session.len() > 4096)
        {
            return Err(error("invalid_input", "Invalid terminal or session ID"));
        }
        let before = self.get(pane).await?;
        validate_screen_session(&before, expected_terminal, expected_session)?;
        let result = self
            .call(
                "pane.read",
                json!({"pane_id":pane,"source":"visible","format":"text","strip_ansi":true}),
                false,
            )
            .await?;
        let captured_at = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
        let read = &result["read"];
        if result["type"] != "pane_read"
            || read["pane_id"] != pane
            || read["source"] != "visible"
            || read["format"] != "text"
            || read["truncated"] != false
        {
            return Err(error("unavailable", "Herdr returned an invalid screen"));
        }
        let text = read["text"]
            .as_str()
            .filter(|text| text.len() <= MAX_SCREEN_BYTES)
            .ok_or_else(|| error("unavailable", "Herdr screen is missing or too large"))?;
        let after = self.get(pane).await?;
        validate_screen_session(&after, expected_terminal, expected_session)?;
        if before.agent_session != after.agent_session
            || before.session_id != after.session_id
            || before.harness != after.harness
        {
            return Err(error(
                "session_changed",
                "The selected session changed while reading the screen",
            ));
        }
        Ok(Screen {
            pane_id: pane.into(),
            terminal_id: expected_terminal.into(),
            session_id: after.session_id,
            text: text.into(),
            captured_at,
            format: "text",
        })
    }

    /// A failed prompt acknowledgement is indeterminate and must never be retried automatically.
    /// # Errors
    /// Rejects changed sessions, unsafe states, invalid text, and failed delivery acknowledgements.
    pub async fn send(&self, pane: &str, expected_session: &str, text: &str) -> Result<()> {
        validate_message(text)?;
        let agent = self.get(pane).await?;
        validate_destination(&agent, expected_session)?;
        // Herdr accepts pane IDs, not terminal UUIDs, and has no compare-and-prompt
        // primitive. A process change between this check and input is not atomic.
        let response = self
            .call(
                "agent.prompt",
                json!({"target":agent.pane_id,"text":text}),
                true,
            )
            .await?;
        if response["type"] != "agent_prompted"
            || response["agent"]["terminal_id"] != agent.terminal_id
        {
            return Err(error(
                "delivery_unknown",
                "Herdr did not acknowledge input to the selected terminal",
            ));
        }
        Ok(())
    }

    async fn call(&self, method: &str, params: Value, mutating: bool) -> Result<Value> {
        let result = tokio::time::timeout(RPC_TIMEOUT, self.exchange(method, params)).await;
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) if err.downcast_ref::<RemoteError>().is_some() => Err(err),
            Ok(Err(err)) => Err(error(
                if mutating {
                    "delivery_unknown"
                } else {
                    "unavailable"
                },
                err.to_string(),
            )),
            Err(_) => Err(error(
                if mutating {
                    "delivery_unknown"
                } else {
                    "unavailable"
                },
                format!("Herdr {method} timed out"),
            )),
        }
    }

    async fn exchange(&self, method: &str, params: Value) -> Result<Value> {
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .context("Cannot connect to Herdr")?;
        let request = format!(
            "{}\n",
            json!({"id":"agent-talk","method":method,"params":params})
        );
        stream.write_all(request.as_bytes()).await?;
        let mut reader = BufReader::new(stream.take(MAX_RESPONSE_BYTES + 1));
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        if line.len() as u64 > MAX_RESPONSE_BYTES || !line.ends_with('\n') {
            bail!("Herdr response is incomplete or too large");
        }
        let response: Value = serde_json::from_str(&line)?;
        if response["id"] != "agent-talk" {
            bail!("Herdr response ID mismatch");
        }
        if let Some(err) = response.get("error") {
            let message = err["message"].as_str().unwrap_or("Herdr rejected request");
            let code = match err["code"].as_str() {
                Some("agent_blocked") => "blocked",
                Some("agent_not_found" | "pane_not_found" | "target_not_found") => "unavailable",
                _ if method == "agent.prompt" => "delivery_unknown",
                _ => "unavailable",
            };
            return Err(error(code, message));
        }
        response
            .get("result")
            .cloned()
            .context("Herdr response has no result")
    }
}

fn validate_pane(pane: &str) -> Result<()> {
    let valid = pane.split_once(":p").is_some_and(|(w, p)| {
        w.strip_prefix('w')
            .is_some_and(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_digit()))
            && !p.is_empty()
            && p.bytes().all(|b| b.is_ascii_digit())
    });
    if !valid || pane.len() > 64 {
        return Err(error("invalid_input", "Invalid pane ID"));
    }
    Ok(())
}

/// # Errors
/// Rejects empty, oversized, or terminal-control input.
pub fn validate_message(text: &str) -> Result<()> {
    if text.trim().is_empty()
        || text.len() > MAX_MESSAGE_BYTES
        || text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(error(
            "invalid_input",
            "Message must contain text, at most 32 KiB, without terminal control characters",
        ));
    }
    Ok(())
}

fn parse_agent(row: &Value) -> Result<Agent> {
    let required = |key: &str| {
        row[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .with_context(|| format!("Herdr agent lacks {key}"))
            .map(str::to_owned)
    };
    let pane_id = required("pane_id")?;
    validate_pane(&pane_id)?;
    let terminal_id = required("terminal_id")?;
    let harness = row["agent"].as_str().unwrap_or("").to_owned();
    let status = row["agent_status"]
        .as_str()
        .filter(|s| matches!(*s, "idle" | "working" | "blocked" | "done"))
        .unwrap_or("unknown")
        .to_owned();
    let agent_session: Option<AgentSession> = row
        .get("agent_session")
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()?;
    let session_registered = agent_session.as_ref().is_some_and(|s| {
        s.agent == harness && matches!(s.kind.as_str(), "id" | "path") && !s.value.is_empty()
    });
    let send_unavailable = if !matches!(harness.as_str(), "codex" | "claude") {
        Some("unsupported")
    } else if !session_registered {
        Some("unregistered")
    } else if status == "blocked" {
        Some("blocked")
    } else if status == "unknown" || row["launch_pending"] == true {
        Some("unknown")
    } else {
        None
    }
    .map(str::to_owned);
    Ok(Agent {
        pane_id: pane_id.clone(),
        terminal_id,
        name: agent_name(row, None),
        workspace: required("workspace_id")?,
        cwd: row["foreground_cwd"]
            .as_str()
            .or_else(|| row["cwd"].as_str())
            .unwrap_or("")
            .into(),
        harness,
        status,
        session_id: None,
        send_unavailable,
        agent_session,
    })
}

fn distinguish_agent_names(agents: &mut [Agent]) {
    let mut groups = std::collections::BTreeMap::<_, Vec<usize>>::new();
    for (index, agent) in agents.iter().enumerate() {
        groups
            .entry((agent.workspace.clone(), agent.name.clone()))
            .or_default()
            .push(index);
    }
    for ((_, name), indices) in groups {
        if indices.len() < 2 {
            continue;
        }
        // Keep the pane's own number: neighboring panes may appear or disappear.
        for index in indices {
            if let Some((_, number)) = agents[index].pane_id.split_once(":p") {
                agents[index].name = format!("{name} · 端末 {number}");
            }
        }
    }
}

fn agent_name(row: &Value, tab: Option<&Value>) -> String {
    if let Some(name) = row["name"]
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return name.into();
    }
    if let Some(label) = tab.and_then(|tab| {
        let label = tab["label"].as_str()?.trim();
        (!label.is_empty()
            && tab["number"]
                .as_u64()
                .is_none_or(|number| label != number.to_string()))
        .then_some(label)
    }) {
        return label.into();
    }
    let directory = row["foreground_cwd"]
        .as_str()
        .filter(|cwd| !cwd.is_empty())
        .or_else(|| row["cwd"].as_str())
        .and_then(|cwd| Path::new(cwd).file_name())
        .and_then(|name| name.to_str());
    let base = directory
        .or_else(|| row["agent"].as_str().filter(|name| !name.is_empty()))
        .unwrap_or("セッション");
    let number = tab.and_then(|tab| tab["number"].as_u64()).or_else(|| {
        row["tab_id"]
            .as_str()?
            .split_once(":t")?
            .1
            .parse::<u64>()
            .ok()
    });
    number.map_or_else(|| base.into(), |number| format!("{base} · タブ {number}"))
}

fn validate_screen_session(agent: &Agent, terminal: &str, session: Option<&str>) -> Result<()> {
    if agent.terminal_id != terminal
        || session.is_some_and(|session| agent.session_id.as_deref() != Some(session))
    {
        return Err(error(
            "session_changed",
            "The selected session ended or restarted; refresh the target",
        ));
    }
    Ok(())
}

fn process_identity(result: &Value, pane: &str, harness: &str) -> Result<u64> {
    let info = &result["process_info"];
    if result["type"] != "pane_process_info" || info["pane_id"] != pane {
        bail!("Unexpected process info");
    }
    let processes = info["foreground_processes"]
        .as_array()
        .context("No foreground processes")?;
    let mut candidates = Vec::new();
    for p in processes {
        let pid = p["pid"]
            .as_u64()
            .filter(|p| *p > 0 && i32::try_from(*p).is_ok())
            .context("Invalid process PID")?;
        let name = p["name"].as_str().context("Missing process name")?;
        if name == harness || (harness == "claude" && name == "claude-code") {
            candidates.push(pid);
        }
    }
    if candidates.len() != 1 {
        bail!("Agent foreground process is absent or ambiguous");
    }
    Ok(candidates[0])
}

fn session_token(agent: &Agent, pid: u64) -> String {
    let value = json!([agent.terminal_id, agent.harness, agent.agent_session, pid]);
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

/// # Errors
/// Rejects stale session identities and states that cannot accept input.
pub fn validate_destination(agent: &Agent, expected: &str) -> Result<()> {
    if agent.session_id.is_some()
        && (expected.is_empty() || agent.session_id.as_deref() != Some(expected))
    {
        return Err(error(
            "session_changed",
            "The selected session ended or restarted; refresh the target",
        ));
    }
    if let Some(reason) = &agent.send_unavailable {
        let code = match reason.as_str() {
            "blocked" => "blocked",
            "unknown" => "unknown",
            "unsupported" => "unsupported",
            "unregistered" => "unregistered",
            _ => "unavailable",
        };
        return Err(error(
            code,
            format!("Target cannot receive input: {reason}"),
        ));
    }
    if agent.session_id.is_none() {
        return Err(error(
            "unregistered",
            "Native session information is absent",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row() -> Value {
        json!({"pane_id":"w1:p2","terminal_id":"stable-terminal","workspace_id":"w1","agent":"codex","agent_status":"working","interactive_ready":true,"agent_session":{"source":"herdr:codex","agent":"codex","kind":"id","value":"session-a"}})
    }
    #[test]
    fn refuses_restart_and_blocked_without_treating_working_as_absent() {
        let mut agent = parse_agent(&row()).unwrap();
        let token = session_token(&agent, 123);
        agent.session_id = Some(token.clone());
        validate_destination(&agent, &token).unwrap();
        agent.session_id = Some(session_token(&agent, 124));
        assert_eq!(
            validate_destination(&agent, &token)
                .unwrap_err()
                .downcast_ref::<RemoteError>()
                .unwrap()
                .code,
            "session_changed"
        );
        agent.session_id = Some(token.clone());
        agent.send_unavailable = Some("blocked".into());
        assert_eq!(
            validate_destination(&agent, &token)
                .unwrap_err()
                .downcast_ref::<RemoteError>()
                .unwrap()
                .code,
            "blocked"
        );
    }
    #[test]
    fn unknown_or_missing_session_never_becomes_sendable() {
        let mut value = row();
        value["agent_status"] = json!("new-state");
        assert_eq!(
            parse_agent(&value).unwrap().send_unavailable.as_deref(),
            Some("unknown")
        );
        value["agent_session"] = Value::Null;
        assert_eq!(
            parse_agent(&value).unwrap().send_unavailable.as_deref(),
            Some("unregistered")
        );
    }
    #[test]
    fn rejects_control_sequences_but_preserves_message_text() {
        validate_message("日本語\n$(echo nope)\t`literal`").unwrap();
        assert!(validate_message("\u{1b}[A").is_err());
        assert!(validate_message("\r").is_err());
        assert!(validate_pane("w1:p2;exit").is_err());
    }
    #[test]
    fn malformed_processes_are_not_silently_dropped() {
        let value = json!({"type":"pane_process_info","process_info":{"pane_id":"w1:p2","foreground_processes":[{"name":"codex","pid":23},{"name":"codex"}]}});
        assert!(process_identity(&value, "w1:p2", "codex").is_err());
    }
    #[test]
    fn absent_native_session_has_explicit_reason() {
        let mut value = row();
        value["agent_session"] = Value::Null;
        let agent = parse_agent(&value).unwrap();
        assert_eq!(
            validate_destination(&agent, "old-token")
                .unwrap_err()
                .downcast_ref::<RemoteError>()
                .unwrap()
                .code,
            "unregistered"
        );
    }

    fn fake_herdr(
        status: &str,
        pid: u64,
        expect_prompt: bool,
        text: &'static str,
    ) -> (tempfile::TempDir, Herdr, tokio::task::JoinHandle<()>) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("herdr.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let mut agent = row();
        agent["agent_status"] = json!(status);
        let task = tokio::spawn(async move {
            let mut responses = vec![
                ("agent.get", json!({"type":"agent_info","agent":agent})),
                (
                    "pane.process_info",
                    json!({"type":"pane_process_info","process_info":{"pane_id":"w1:p2","foreground_processes":[{"pid":pid,"name":"codex"}]}}),
                ),
            ];
            if expect_prompt {
                responses.push((
                    "agent.prompt",
                    json!({"type":"agent_prompted","agent":agent}),
                ));
            }
            for (method, result) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut line = String::new();
                BufReader::new(&mut stream)
                    .read_line(&mut line)
                    .await
                    .unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], method);
                if method == "agent.prompt" {
                    assert_eq!(request["params"]["target"], "w1:p2");
                    assert_eq!(request["params"]["text"], text);
                }
                stream
                    .write_all(
                        format!("{}\n", json!({"id":request["id"],"result":result})).as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        (directory, Herdr::new(path), task)
    }

    #[tokio::test]
    async fn sends_verbatim_only_after_live_identity_check() {
        let text = "原文\n$(echo untouched) `literal`";
        let (_directory, herdr, server) = fake_herdr("working", 123, true, text);
        let expected = session_token(&parse_agent(&row()).unwrap(), 123);
        herdr.send("w1:p2", &expected, text).await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn never_calls_prompt_after_restart_or_blocked_state() {
        for (status, pid, reason) in [
            ("working", 124, "session_changed"),
            ("blocked", 123, "blocked"),
        ] {
            let (_directory, herdr, server) = fake_herdr(status, pid, false, "");
            let expected = session_token(&parse_agent(&row()).unwrap(), 123);
            assert_eq!(
                herdr
                    .send("w1:p2", &expected, "text")
                    .await
                    .unwrap_err()
                    .downcast_ref::<RemoteError>()
                    .unwrap()
                    .code,
                reason
            );
            server.await.unwrap();
        }
    }
    #[test]
    fn display_name_prefers_registered_name_then_custom_tab_then_directory_and_tab() {
        let mut value = row();
        value["tab_id"] = json!("w1:t2");
        let custom = json!({"tab_id":"w1:t2","label":"backend delivery","number":2});
        assert_eq!(agent_name(&value, Some(&custom)), "backend delivery");
        value["name"] = json!("named agent");
        assert_eq!(agent_name(&value, Some(&custom)), "named agent");
        value["name"] = Value::Null;
        let numbered = json!({"tab_id":"w1:t2","label":"2","number":2});
        value["cwd"] = json!("/home/user/project");
        assert_eq!(agent_name(&value, Some(&numbered)), "project · タブ 2");
        assert_eq!(agent_name(&value, None), "project · タブ 2");
        value["foreground_cwd"] = json!("/home/user/other");
        assert_eq!(agent_name(&value, Some(&numbered)), "other · タブ 2");
        value["tab_id"] = json!("w1:t3");
        assert_eq!(agent_name(&value, None), "other · タブ 3");
        value["cwd"] = Value::Null;
        value["foreground_cwd"] = Value::Null;
        assert_eq!(agent_name(&value, None), "codex · タブ 3");
    }
    #[test]
    fn split_panes_with_matching_labels_are_distinguished_only_within_their_workspace() {
        for explicit in [None, Some("review")] {
            let mut agents = Vec::new();
            for (workspace, pane, tab) in [
                ("w1", "w1:p9", "w1:t2"),
                ("w1", "w1:p2", "w1:t2"),
                ("w2", "w2:p1", "w2:t2"),
                ("w1", "w1:p7", "w1:t3"),
                ("w1", "w1:p8", "w1:t2"),
            ] {
                let mut value = row();
                value["pane_id"] = json!(pane);
                value["workspace_id"] = json!(workspace);
                value["tab_id"] = json!(tab);
                value["cwd"] = json!("/home/user/project");
                if tab.ends_with("t2") {
                    value["name"] = json!(explicit);
                }
                agents.push(parse_agent(&value).unwrap());
            }
            let mut after_exit = agents.clone();
            after_exit.retain(|agent| agent.pane_id != "w1:p2");
            distinguish_agent_names(&mut after_exit);
            distinguish_agent_names(&mut agents);
            let base = explicit.unwrap_or("project · タブ 2");
            assert_eq!(agents[0].name, format!("{base} · 端末 9"));
            assert_eq!(agents[1].name, format!("{base} · 端末 2"));
            assert_eq!(agents[2].name, base);
            assert_eq!(agents[3].name, "project · タブ 3");
            assert_eq!(after_exit[0].name, agents[0].name);
            assert_eq!(after_exit[3].name, agents[4].name);
        }
    }
}
