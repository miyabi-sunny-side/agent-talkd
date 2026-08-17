//! Claude Code の cross-session socket (cc-socks) を、herdr が申告する pane の
//! agent PID へ照合する。
//!
//! Claude Code は session ごとに `$XDG_RUNTIME_DIR/cc-socks/<PID>.sock` を作り、
//! 組み込みの cross-session channel はこの path を `uds:<path>` の宛先として
//! 受け取る。
//!
//! 採用の条件はただ一つ、**herdr が pane に検出している runtime と同名の
//! foreground process の PID と、socket file 名の PID が完全に一致すること**。
//! herdr の `pane.process_info` は pane の foreground process group しか挙げない
//! ので、主 agent が spawn した子 agent (同じ runtime の別 process) はここに
//! 現れない — 主 agent の窓口だけが選ばれ、子 agent の socket を宛先に化けさせる
//! 余地が無い。名前が一致する process が 0 件または 2 件以上なら解決不能に倒す。
//!
//! **導出結果は永続化しない。** PID は再利用され socket は session と共に消える
//! ので、毎回導出する以外に正しい答えは無い。

use std::{
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
};

use crate::herdr::ForegroundProcess;

/// pane の agent に結び付いた Claude Code の cross-session socket。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSocket {
    pub pid: i32,
    pub uds: PathBuf,
}

/// herdr が pane に検出している runtime 名と一致する foreground process が
/// **ちょうど 1 つ** のときだけ、その PID を返す。
///
/// 0 件 (agent が消えた・runtime 名が process 名と違う) と 2 件以上 (同名の
/// process が並んだ) はどちらも解決不能。どれが窓口かを推測しない。
pub fn agent_pid(processes: &[ForegroundProcess], runtime: &str) -> Option<i32> {
    let mut matched = processes
        .iter()
        .filter(|process| process.name == runtime)
        .map(|process| process.pid);
    let pid = matched.next()?;
    matched.next().is_none().then_some(pid)
}

/// `root/<pid>.sock` が実在してかつ unix socket のときだけ path を返す。
///
/// symlink は辿らない (`symlink_metadata`) — cc-socks に置かれた符牒が別の
/// socket を指していても、その宛先へ誘導しない。
pub fn socket_for_pid(root: &Path, pid: i32) -> Option<PathBuf> {
    let path = root.join(format!("{pid}.sock"));
    std::fs::symlink_metadata(&path)
        .ok()?
        .file_type()
        .is_socket()
        .then_some(path)
}

/// 上 2 つの合成。pane の agent PID が一意に決まり、その PID の socket が
/// 実在するときだけ `Some`。
pub fn agent_socket(
    root: &Path,
    processes: &[ForegroundProcess],
    runtime: &str,
) -> Option<PaneSocket> {
    let pid = agent_pid(processes, runtime)?;
    Some(PaneSocket {
        pid,
        uds: socket_for_pid(root, pid)?,
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use super::*;

    fn process(pid: i32, name: &str) -> ForegroundProcess {
        ForegroundProcess {
            pid,
            name: name.to_owned(),
        }
    }

    /// 主 agent は runtime 名と同名の foreground process ちょうど 1 つ。
    /// MCP server 等の同居 process は名前が違うので影響しない。
    #[test]
    fn the_agent_pid_is_the_single_foreground_process_named_after_the_runtime() {
        let pane = [
            process(1_873_555, "claude"),
            process(1_875_033, "uv"),
            process(1_875_034, "agent-talk-mcp"),
        ];
        assert_eq!(agent_pid(&pane, "claude"), Some(1_873_555));
        // 別 runtime を要求しても、その名前の process が居なければ解決不能。
        assert_eq!(agent_pid(&pane, "codex"), None);
        assert_eq!(agent_pid(&[], "claude"), None);
        // 同名が 2 つ並ぶ pane はどちらが窓口か決められない。
        assert_eq!(
            agent_pid(&[process(10, "claude"), process(11, "claude")], "claude"),
            None
        );
    }

    #[test]
    fn only_a_bound_socket_named_after_that_pid_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cc-socks");
        std::fs::create_dir(&root).unwrap();
        let bound = root.join("4242.sock");
        let _listener = UnixListener::bind(&bound).unwrap();
        // socket ではない同名 file、別 PID、symlink はいずれも採用しない。
        std::fs::write(root.join("4243.sock"), b"not a socket").unwrap();
        std::os::unix::fs::symlink(&bound, root.join("4244.sock")).unwrap();

        assert_eq!(socket_for_pid(&root, 4242), Some(bound.clone()));
        assert_eq!(socket_for_pid(&root, 4243), None);
        assert_eq!(socket_for_pid(&root, 4244), None, "symlink は辿らない");
        assert_eq!(socket_for_pid(&root, 9999), None);
        // root が無い場合も解決不能 (XDG_RUNTIME_DIR 無し・cc-socks 未作成)。
        assert_eq!(socket_for_pid(&dir.path().join("absent"), 4242), None);

        // 合成: herdr の申告 PID と socket 名が一致したときだけ採用する。
        let processes = [process(4242, "claude"), process(4243, "uv")];
        assert_eq!(
            agent_socket(&root, &processes, "claude"),
            Some(PaneSocket {
                pid: 4242,
                uds: bound
            })
        );
        // 申告が別 PID なら、cc-socks に socket が並んでいても採用しない。
        assert_eq!(
            agent_socket(&root, &[process(4243, "claude")], "claude"),
            None
        );
    }
}
