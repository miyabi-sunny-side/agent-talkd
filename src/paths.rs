//! daemon と MCP adapter が共有する、副作用のない path 導出規則。
//!
//! ここには subprocess 実行も環境探索も置かない。`agent-talk-mcp` はこの module
//! だけを取り込んで daemon と同一の socket path を導出する
//! (docs/decisions/0001-conversation-broker-scope.md の「接続先の positive 定義」)。

use std::path::{Path, PathBuf};

/// tmux socket path の basename を、path に埋め込める安全な名前へ正規化する。
pub fn socket_name(socket: &str) -> String {
    Path::new(socket)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("default")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// runtime root と tmux socket から daemon の RPC socket path を導出する。
pub fn rpc_socket_path(runtime_root: &Path, tmux_socket: &str) -> PathBuf {
    runtime_root
        .join("agent-talkd")
        .join(format!("{}.sock", socket_name(tmux_socket)))
}

/// herdr socket path から、衝突しない一意な名前を導出する。
///
/// herdr の socket は既定が `~/.config/herdr/herdr.sock`、named session が
/// `~/.config/herdr/sessions/<name>/herdr.sock` で **basename が同じ**。
/// basename だけを見ると既定と named session が同じ名前になってしまうため、
/// `sessions/<name>/` の場合だけ session 名を混ぜる。
pub fn herdr_socket_name(herdr_socket: &Path) -> String {
    let session = herdr_socket
        .parent()
        .filter(|parent| {
            parent
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "sessions")
        })
        .and_then(Path::file_name)
        .and_then(|name| name.to_str());
    match session {
        Some(name) => format!("herdr-{}", socket_name(name)),
        None => "herdr".to_owned(),
    }
}

/// runtime root と herdr socket から daemon の RPC socket path を導出する。
pub fn herdr_rpc_socket_path(runtime_root: &Path, herdr_socket: &Path) -> PathBuf {
    runtime_root
        .join("agent-talkd")
        .join(format!("{}.sock", herdr_socket_name(herdr_socket)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_name_normalizes_unsafe_basename_characters() {
        assert_eq!(socket_name("/tmp/tmux-1000/default"), "default");
        assert_eq!(socket_name("/tmp/tmux-1000/my.sock name"), "my_sock_name");
        assert_eq!(socket_name("/tmp/tmux-1000/"), "tmux-1000");
        assert_eq!(socket_name(""), "default");
    }

    #[test]
    fn rpc_socket_path_matches_the_daemon_layout() {
        assert_eq!(
            rpc_socket_path(Path::new("/run/user/1000"), "/tmp/tmux-1000/default"),
            PathBuf::from("/run/user/1000/agent-talkd/default.sock")
        );
    }

    #[test]
    fn herdr_named_sessions_do_not_collide_with_the_default_socket() {
        let home = Path::new("/home/miyabi/.config/herdr");
        assert_eq!(herdr_socket_name(&home.join("herdr.sock")), "herdr");
        assert_eq!(
            herdr_socket_name(&home.join("sessions/review/herdr.sock")),
            "herdr-review"
        );
        // basename は同じでも導出名は別になる。
        assert_ne!(
            herdr_rpc_socket_path(Path::new("/run/user/1000"), &home.join("herdr.sock")),
            herdr_rpc_socket_path(
                Path::new("/run/user/1000"),
                &home.join("sessions/review/herdr.sock")
            )
        );
    }

    #[test]
    fn herdr_rpc_socket_path_matches_the_daemon_layout() {
        assert_eq!(
            herdr_rpc_socket_path(
                Path::new("/run/user/1000"),
                Path::new("/home/miyabi/.config/herdr/herdr.sock")
            ),
            PathBuf::from("/run/user/1000/agent-talkd/herdr.sock")
        );
    }
}
