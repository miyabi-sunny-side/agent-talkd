use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsStr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::paths::{herdr_rpc_socket_path, herdr_socket_name, rpc_socket_path, socket_name};

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
    /// tmux backend。移行期には不在でもよい。
    pub tmux_socket: Option<String>,
    /// herdr backend。移行期には不在でもよい。
    pub herdr_socket: Option<PathBuf>,
    /// daemon が listen する RPC socket。**両 backend 分をすべて開く**ので、
    /// どちらの pane から来たクライアントも同じ daemon に到達する。
    pub rpc_sockets: Vec<PathBuf>,
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
    /// クライアントが繋ぐべき主 socket。自分が居る側の backend を優先する。
    pub fn rpc_socket(&self) -> &Path {
        &self.rpc_sockets[0]
    }

    /// この構成が面倒を見る multiplexer の名前。daemon の status に載せ、
    /// より狭い daemon を置換すべきかの判定に使う。
    pub fn backend_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if self.tmux_socket.is_some() {
            names.push("tmux".to_owned());
        }
        if self.herdr_socket.is_some() {
            names.push("herdr".to_owned());
        }
        names
    }

    pub fn discover() -> Result<Self> {
        Self::discover_optional()?
            .context("tmux にも herdr にも接続できません (sandbox 内なら承認付きで再実行)")
    }

    pub fn discover_optional() -> Result<Option<Self>> {
        let tmux_socket = discover_tmux_socket()?;
        let herdr_socket = discover_herdr_socket();
        if tmux_socket.is_none() && herdr_socket.is_none() {
            return Ok(None);
        }
        let runtime = env::var_os("XDG_RUNTIME_DIR")
            .map_or_else(|| home().join(".cache/agent-talkd/run"), PathBuf::from);
        let state = env::var_os("XDG_STATE_HOME")
            .map_or_else(|| home().join(".local/state"), PathBuf::from);

        // journal と設定は tmux 側を正とする。両方あるとき herdr を journal の
        // 基準にすると、tmux 単独で動いていた既存 journal を読めなくなる。
        let name = tmux_socket.as_deref().map_or_else(
            || {
                herdr_socket
                    .as_deref()
                    .map_or_else(|| "default".to_owned(), herdr_socket_name)
            },
            socket_name,
        );

        let option = |key: &str| {
            tmux_socket
                .as_deref()
                .and_then(|socket| tmux_option(socket, key))
        };
        let queue_limit = option("@agent_talkd_queue_limit")
            .and_then(|value| value.parse().ok())
            .filter(|limit| *limit > 0)
            .unwrap_or(1000);
        let log_level = option("@agent_talkd_log_level")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "info".into());
        let skill_syntax = parse_skill_syntax(option("@agent_talkd_skill_syntax").as_deref())?;
        let allowed_skills = option("@agent_talkd_allowed_skills")
            .filter(|value| !value.is_empty())
            .map(|value| parse_token_set("@agent_talkd_allowed_skills", &value))
            .transpose()?;
        let allowed_sources = option("@agent_talkd_allowed_sources")
            .filter(|value| !value.is_empty())
            .map(|value| parse_token_set("@agent_talkd_allowed_sources", &value))
            .transpose()?
            .unwrap_or_else(|| BTreeSet::from(["mobile".into()]));

        let rpc_sockets =
            rpc_socket_list(&runtime, tmux_socket.as_deref(), herdr_socket.as_deref());
        let http_socket = http_socket_for(&rpc_sockets[0]);
        Ok(Some(Self {
            tmux_socket,
            herdr_socket,
            rpc_sockets,
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

/// daemon が開く RPC socket を列挙する。
///
/// 両 backend が居るときは **両方の path で listen** する。tmux の pane に居る
/// クライアントは `$TMUX` から、herdr の pane に居るクライアントは
/// `$HERDR_SOCKET_PATH` から、それぞれ別の path を導出するが、
/// どちらも同じ daemon プロセスへ届く。これで backend をまたいだ会話が成立する。
fn rpc_socket_list(
    runtime: &Path,
    tmux_socket: Option<&str>,
    herdr_socket: Option<&Path>,
) -> Vec<PathBuf> {
    if let Some(explicit) = env::var_os("AGENT_TALK_RPC_SOCKET") {
        return vec![PathBuf::from(explicit)];
    }
    let mut sockets = Vec::new();
    if let Some(tmux_socket) = tmux_socket {
        sockets.push(rpc_socket_path(runtime, tmux_socket));
    }
    if let Some(herdr_socket) = herdr_socket {
        let path = herdr_rpc_socket_path(runtime, herdr_socket);
        if !sockets.contains(&path) {
            sockets.push(path);
        }
    }
    sockets
}

/// herdr backend を有効にするかを決める。
///
/// **既定パスの存在だけでは有効にしない。** socket file が転がっているだけで
/// 勝手に掴むと、隔離した検証環境から稼働中の共有 herdr へ接続してしまう。
/// 有効化の条件は次のいずれかに限る。
///
/// - `AGENT_TALK_HERDR_SOCKET` が明示されている (空文字なら明示的に無効)
/// - 自分が herdr の pane の中に居る (`HERDR_SOCKET_PATH` / `HERDR_ENV`)
///
/// tmux の pane から herdr 側の agent とも会話したい場合は、前者を設定する。
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
    env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
}

fn discover_tmux_socket() -> Result<Option<String>> {
    if let Some(value) = env::var_os("AGENT_TALK_TMUX_SOCKET") {
        let value = value.to_string_lossy().into_owned();
        return Ok((!value.is_empty()).then_some(value));
    }
    if let Ok(tmux) = env::var("TMUX")
        && let Some(socket) = tmux.split(',').next()
        && !socket.is_empty()
    {
        return Ok(Some(socket.to_owned()));
    }
    // herdr の pane に居て tmux の中には居ない場合、既定の tmux サーバーを
    // 探しに行ってはならない。無関係な (別の作業で動いている) tmux サーバーを
    // 掴んでしまい、その registry と journal を巻き込む。
    if env::var_os("HERDR_PANE_ID").is_some_and(|value| !value.is_empty()) {
        return Ok(None);
    }
    let output = std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{socket_path}"])
        .output();
    let Ok(output) = output else {
        return Ok(None);
    };
    if !output.status.success() {
        return Ok(None);
    }
    let socket = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if socket.is_empty() || Path::new(&socket).file_name().is_none() {
        bail!("tmux socket path is empty");
    }
    Ok(Some(socket))
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
            http_socket_for(Path::new("/run/user/1000/agent-talkd/tmux-main.sock")),
            PathBuf::from("/run/user/1000/agent-talkd/tmux-main.http.sock")
        );
    }
}
