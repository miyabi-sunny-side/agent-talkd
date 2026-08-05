use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsStr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::paths::{herdr_rpc_socket_path, herdr_socket_name};

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
    pub herdr_socket: PathBuf,
    /// daemon が listen する RPC socket。クライアントは `$HERDR_SOCKET_PATH`
    /// から同じ path を導出してこの daemon に到達する。
    pub rpc_socket: PathBuf,
    pub http_socket: PathBuf,
    /// 設定されていれば HTTP を TCP でも待ち受ける (agent-terrace 相当)。
    pub http_tcp: Option<String>,
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
        Self::discover_optional()?
            .context("herdr に接続できません (sandbox 内なら承認付きで再実行)")
    }

    pub fn discover_optional() -> Result<Option<Self>> {
        let Some(herdr_socket) = discover_herdr_socket() else {
            return Ok(None);
        };
        let runtime = env::var_os("XDG_RUNTIME_DIR")
            .map_or_else(|| home().join(".cache/agent-talkd/run"), PathBuf::from);
        let state = env::var_os("XDG_STATE_HOME")
            .map_or_else(|| home().join(".local/state"), PathBuf::from);
        let name = herdr_socket_name(&herdr_socket);

        let option = |key: &str| env::var(key).ok().filter(|value| !value.is_empty());
        let queue_limit = option("AGENT_TALK_QUEUE_LIMIT")
            .and_then(|value| value.parse().ok())
            .filter(|limit| *limit > 0)
            .unwrap_or(1000);
        let log_level = option("AGENT_TALK_LOG_LEVEL").unwrap_or_else(|| "info".into());
        let skill_syntax = parse_skill_syntax(option("AGENT_TALK_SKILL_SYNTAX").as_deref())?;
        let allowed_skills = option("AGENT_TALK_ALLOWED_SKILLS")
            .map(|value| parse_token_set("AGENT_TALK_ALLOWED_SKILLS", &value))
            .transpose()?;
        let allowed_sources = option("AGENT_TALK_ALLOWED_SOURCES")
            .map(|value| parse_token_set("AGENT_TALK_ALLOWED_SOURCES", &value))
            .transpose()?
            .unwrap_or_else(|| BTreeSet::from(["mobile".into()]));

        let rpc_socket = env::var_os("AGENT_TALK_RPC_SOCKET").map_or_else(
            || herdr_rpc_socket_path(&runtime, &herdr_socket),
            PathBuf::from,
        );
        let http_socket = http_socket_for(&rpc_socket);
        Ok(Some(Self {
            herdr_socket,
            rpc_socket,
            http_socket,
            http_tcp: env::var("AGENT_TALK_HTTP_ADDR")
                .ok()
                .filter(|value| !value.is_empty()),
            journal: state.join("agent-talkd").join(format!("{name}.journal")),
            log: state.join("agent-talkd").join("agent-talkd.log"),
            queue_limit,
            log_level,
            skill_syntax,
            allowed_skills,
            allowed_sources,
        }))
    }
}

/// herdr backend を有効にするかを決める。
///
/// **既定パスの存在だけでは有効にしない。** socket file が転がっているだけで
/// 勝手に掴むと、隔離した検証環境から稼働中の共有 herdr へ接続してしまう。
/// 有効化の条件は次のいずれかに限る。
///
/// - `AGENT_TALK_HERDR_SOCKET` が明示されている (空文字なら明示的に無効)
/// - 自分が herdr の pane の中に居る (`HERDR_SOCKET_PATH` / `HERDR_ENV`)
fn discover_herdr_socket() -> Option<PathBuf> {
    if let Some(value) = env::var_os("AGENT_TALK_HERDR_SOCKET") {
        if value.is_empty() {
            return None;
        }
        return Some(PathBuf::from(value));
    }
    if let Some(value) = env::var_os("HERDR_SOCKET_PATH").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        return path.exists().then_some(path);
    }
    env::var_os("HERDR_ENV")?;
    let default = home().join(".config/herdr/herdr.sock");
    default.exists().then_some(default)
}

fn http_socket_for(rpc_socket: &Path) -> PathBuf {
    let mut filename = rpc_socket
        .file_stem()
        .unwrap_or_else(|| OsStr::new("agent-talkd"))
        .to_os_string();
    filename.push(".http.sock");
    rpc_socket.with_file_name(filename)
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
            bail!("AGENT_TALK_SKILL_SYNTAX の形式が不正です: '{entry}'");
        };
        if agent.is_empty()
            || agent.len() > MAX_TOKEN_LEN
            || !agent
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("AGENT_TALK_SKILL_SYNTAX のagent名が不正です: '{agent}'");
        }
        let syntax = match syntax {
            "slash" => SkillSyntax::Slash,
            "dollar" => SkillSyntax::Dollar,
            _ => bail!("AGENT_TALK_SKILL_SYNTAX の記法が不正です: '{syntax}'"),
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

fn home() -> PathBuf {
    env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
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

    #[test]
    fn http_socket_uses_the_effective_rpc_parent_and_stem() {
        assert_eq!(
            http_socket_for(Path::new("/tmp/custom runtime/broker.sock")),
            PathBuf::from("/tmp/custom runtime/broker.http.sock")
        );
        assert_eq!(
            http_socket_for(Path::new("/run/user/1000/agent-talkd/herdr.sock")),
            PathBuf::from("/run/user/1000/agent-talkd/herdr.http.sock")
        );
    }
}
