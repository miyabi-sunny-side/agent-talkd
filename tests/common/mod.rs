//! 統合テストが共有する `who` 出力の読み取り。
//!
//! `who` の各行は `名前 状態 backend session:win.pane (pane_id)  cwd` で、
//! **末尾に cwd が入る**。そのため行全体への部分文字列一致で agent の有無を
//! 判定してはならない。checkout 先が `/home/runner/...` のように agent 名と
//! 同じ語を含むと、登録の有無に関係なく一致してしまう
//! (GitHub Actions で `daemon_journal_read_recovery_and_pane_exit` が
//! 2026-07-28 から常に落ちていた原因)。

/// agent 行だけを返す。`pending-to-me` などの付随行は除く。
fn agent_rows(who: &str) -> impl Iterator<Item = &str> {
    who.lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty() && !line.starts_with("pending-"))
}

/// 指定した名前で登録されている agent の行を返す。
pub fn agent_row<'a>(who: &'a str, name: &str) -> Option<&'a str> {
    agent_rows(who).find(|line| line.split_whitespace().next() == Some(name))
}

/// 指定した名前の agent が登録されているか。**cwd 列は見ない。**
pub fn has_agent(who: &str, name: &str) -> bool {
    agent_row(who, name).is_some()
}

/// agent 行の backend 列 (常に `herdr`。tmux 併存期の表形式を維持している)。
pub fn agent_backend<'a>(who: &'a str, name: &str) -> Option<&'a str> {
    agent_row(who, name)?.split_whitespace().nth(2)
}
