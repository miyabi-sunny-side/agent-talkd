use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::tmux::socket_name;

const MAX_TOKEN_LEN: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSyntax {
    Slash,
    Dollar,
}

impl SkillSyntax {
    pub fn prefix(self) -> char {
        match self {
            Self::Slash => '/',
            Self::Dollar => '$',
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub tmux_socket: String,
    pub rpc_socket: PathBuf,
    pub journal: PathBuf,
    pub log: PathBuf,
    pub queue_limit: usize,
    pub log_level: String,
    pub skill_syntax: BTreeMap<String, SkillSyntax>,
    pub allowed_skills: Option<BTreeSet<String>>,
    pub allowed_sources: BTreeSet<String>,
}

impl Config {
    pub fn discover() -> Result<Self> {
        let tmux_socket = discover_tmux_socket()?;
        let name = socket_name(&tmux_socket);
        let runtime = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".cache/agent-talkd/run"));
        let state = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local/state"));
        let queue_limit = tmux_option(&tmux_socket, "@agent_talkd_queue_limit")
            .and_then(|value| value.parse().ok())
            .filter(|limit| *limit > 0)
            .unwrap_or(1000);
        let log_level = tmux_option(&tmux_socket, "@agent_talkd_log_level")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "info".into());
        let skill_syntax =
            parse_skill_syntax(tmux_option(&tmux_socket, "@agent_talkd_skill_syntax").as_deref())?;
        let allowed_skills = tmux_option(&tmux_socket, "@agent_talkd_allowed_skills")
            .filter(|value| !value.is_empty())
            .map(|value| parse_token_set("@agent_talkd_allowed_skills", &value))
            .transpose()?;
        let allowed_sources = tmux_option(&tmux_socket, "@agent_talkd_allowed_sources")
            .filter(|value| !value.is_empty())
            .map(|value| parse_token_set("@agent_talkd_allowed_sources", &value))
            .transpose()?
            .unwrap_or_else(|| BTreeSet::from(["mobile".into()]));
        Ok(Self {
            tmux_socket,
            rpc_socket: env::var_os("AGENT_TALK_RPC_SOCKET")
                .map(PathBuf::from)
                .unwrap_or_else(|| runtime.join("agent-talkd").join(format!("{name}.sock"))),
            journal: state.join("agent-talkd").join(format!("{name}.journal")),
            log: state.join("agent-talkd").join("agent-talkd.log"),
            queue_limit,
            log_level,
            skill_syntax,
            allowed_skills,
            allowed_sources,
        })
    }
}

fn parse_skill_syntax(value: Option<&str>) -> Result<BTreeMap<String, SkillSyntax>> {
    let mut mapping = BTreeMap::from([
        ("claude".into(), SkillSyntax::Slash),
        ("codex".into(), SkillSyntax::Dollar),
    ]);
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(mapping);
    };
    for entry in value.split(',') {
        let Some((agent, syntax)) = entry.split_once('=') else {
            bail!("@agent_talkd_skill_syntax の形式が不正です: '{entry}'");
        };
        if agent.is_empty()
            || agent.len() > MAX_TOKEN_LEN
            || !agent
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("@agent_talkd_skill_syntax のagent名が不正です: '{agent}'");
        }
        let syntax = match syntax {
            "slash" => SkillSyntax::Slash,
            "dollar" => SkillSyntax::Dollar,
            _ => bail!("@agent_talkd_skill_syntax の記法が不正です: '{syntax}'"),
        };
        mapping.insert(agent.into(), syntax);
    }
    Ok(mapping)
}

fn parse_token_set(option: &str, value: &str) -> Result<BTreeSet<String>> {
    value
        .split(',')
        .map(|token| {
            if !is_safe_token(token) {
                bail!("{option} のtokenが不正です: '{token}'");
            }
            Ok(token.to_owned())
        })
        .collect()
}

pub fn is_safe_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOKEN_LEN
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, ':' | '_' | '-'))
}

fn tmux_option(socket: &str, name: &str) -> Option<String> {
    let output = std::process::Command::new("tmux")
        .args(["-S", socket, "show-option", "-gqv", name])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn discover_tmux_socket() -> Result<String> {
    if let Some(value) = env::var_os("AGENT_TALK_TMUX_SOCKET") {
        return Ok(value.to_string_lossy().into_owned());
    }
    if let Ok(tmux) = env::var("TMUX")
        && let Some(socket) = tmux.split(',').next()
        && !socket.is_empty()
    {
        return Ok(socket.to_owned());
    }
    let output = std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{socket_path}"])
        .output()
        .context("cannot locate tmux server")?;
    if !output.status.success() {
        bail!("tmux サーバーに接続できません (sandbox 内なら承認付きで再実行)");
    }
    let socket = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if socket.is_empty() || Path::new(&socket).file_name().is_none() {
        bail!("tmux socket path is empty");
    }
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_syntax_defaults_and_overrides_are_typed() {
        let mapping = parse_skill_syntax(Some("cursor=slash,codex=slash")).unwrap();
        assert_eq!(mapping["claude"], SkillSyntax::Slash);
        assert_eq!(mapping["codex"], SkillSyntax::Slash);
        assert_eq!(mapping["cursor"], SkillSyntax::Slash);
        assert!(parse_skill_syntax(Some("cursor=@")).is_err());
    }

    #[test]
    fn safe_tokens_match_the_terminal_boundary_contract() {
        for valid in ["deliver", "review:security", "my_skill-2"] {
            assert!(is_safe_token(valid), "{valid}");
        }
        for invalid in ["", "Deliver", "bad/name", "bad value", "$()", "日本語"] {
            assert!(!is_safe_token(invalid), "{invalid}");
        }
        assert!(!is_safe_token(&"a".repeat(MAX_TOKEN_LEN + 1)));
    }
}
