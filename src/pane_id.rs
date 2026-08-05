//! pane id の文法。daemon の routing と MCP の環境検証が同じ判定を共有する
//! (別実装にすると片側だけ直して症状が残る)。依存を持たない純粋な文法層
//! なので、能力を制限した `agent-talk-mcp` bin にもそのまま含められる。

/// herdr の pane id (`w<seg>:p<seg>`) かどうか。
///
/// segment は数字と大文字英字。採番には数字だけでなく大文字英字も現れる
/// (実測: `wX:p4`, `w1:pA`)。
///
/// **形式に厳密に一致しないものは拒否する。** 宛先文字列には
/// `review:security` のような `:` を含む名前も来るため、`:` を含むだけで
/// pane id と見なすと agent 名を pane id と誤認する。小文字を受けないのも
/// 同じ理由 — `web:prod` のような名前を pane id と誤読しない。
pub fn is_pane_id(value: &str) -> bool {
    let Some((workspace, pane)) = value.split_once(":p") else {
        return false;
    };
    let Some(workspace) = workspace.strip_prefix('w') else {
        return false;
    };
    segment(workspace) && segment(pane)
}

/// herdr の id segment。数字と大文字英字を受ける。小文字は現れないので
/// 受けない (`is_pane_id` の誤認防止と対)。
fn segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_uppercase())
}
