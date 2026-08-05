//! pane id の文法。daemon の routing と MCP の環境検証が同じ判定を共有する
//! (別実装にすると片側だけ直して症状が残る)。依存を持たない純粋な文法層
//! なので、能力を制限した `agent-talk-mcp` bin にもそのまま含められる。

/// pane がどちらの multiplexer に属するか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Tmux,
    Herdr,
}

impl BackendKind {
    /// pane id の形式から所属を決める。
    ///
    /// tmux の pane id は必ず `%<digits>`、herdr は `w<seg>:p<seg>`。herdr の
    /// segment は数字と大文字英字 — 採番が 9 を超えると英字に進む
    /// (実測: `wX:p4`, `w1:pA`)。交わらないので前置詞を足さずに判別できる。
    ///
    /// **どちらの形にも厳密に一致しないものは `None`。** 宛先文字列には
    /// `review:security` のような `:` を含む名前も来るため、`:` を含むだけで
    /// herdr の pane id と見なすと agent 名を pane id と誤認する。小文字を
    /// 受けないのも同じ理由 — `web:prod` のような名前を pane id と誤読しない。
    pub fn of(pane_id: &str) -> Option<Self> {
        if let Some(rest) = pane_id.strip_prefix('%') {
            return digits(rest).then_some(Self::Tmux);
        }
        let (workspace, pane) = pane_id.split_once(":p")?;
        let workspace = workspace.strip_prefix('w')?;
        (herdr_segment(workspace) && herdr_segment(pane)).then_some(Self::Herdr)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tmux => "tmux",
            Self::Herdr => "herdr",
        }
    }
}

fn digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// herdr の id segment。採番が 9 を超えると大文字英字に進むため、数字と
/// 大文字英字を受ける。小文字は現れないので受けない (`of` の誤認防止と対)。
fn herdr_segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_uppercase())
}
