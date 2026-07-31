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
}
