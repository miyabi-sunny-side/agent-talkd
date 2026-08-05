//! herdr を daemon の視点から扱う薄い adapter 層。
//!
//! `HerdrPane` を宛先解決が使う [`PaneInfo`] へ写し、配送・health check を
//! herdr の API に委譲する。かつては tmux と herdr の 2 backend を束ねる
//! multiplexer だったが、tmux 対応の廃止で herdr 単独になった。

use anyhow::{Result, bail};

use crate::{
    herdr::{AgentStatus, Delivery, Herdr},
    pane_id::is_pane_id,
};

/// 宛先解決が使う pane 情報。
#[derive(Debug, Clone)]
pub struct PaneInfo {
    /// 表示と scope 解決の主名。workspace label
    /// (label が使えないときは `workspace_id` へ fallback)。
    pub session: String,
    /// scope 解決の互換 alias。label を主名にしたときの `workspace_id`。
    /// 旧来の `w2/codex` 形を壊さないために解決だけに使い、表示には使わない。
    pub scope_alias: Option<String>,
    pub window_id: String,
    pub pane_id: String,
    pub cwd: String,
    pub window_index: String,
    pub pane_index: String,
    pub agent: Option<String>,
    /// herdr 自身が持つ agent 状態。
    pub status: AgentStatus,
}

/// 自分が居る pane の id を環境から求める。
pub fn self_pane() -> Option<String> {
    std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|pane| !pane.is_empty())
}

#[derive(Clone)]
pub struct Backend {
    herdr: Herdr,
}

impl Backend {
    pub fn new(herdr: Herdr) -> Self {
        Self { herdr }
    }

    /// herdr を起動せずに固定の pane 一覧を返す代役 (test 専用)。
    #[cfg(test)]
    pub(crate) fn scripted(panes: Vec<crate::herdr::HerdrPane>) -> Self {
        Self {
            herdr: Herdr::scripted(panes),
        }
    }

    #[cfg(test)]
    pub(crate) fn herdr(&self) -> &Herdr {
        &self.herdr
    }

    pub async fn panes(&self) -> Result<Vec<PaneInfo>> {
        Ok(self
            .herdr
            .panes()
            .await?
            .into_iter()
            .map(pane_info)
            .collect())
    }

    /// idle と確認できた pane にだけ配送する。送らなかった場合は `Err` を
    /// 返し、呼び出し側の「配送できなかったので queue する」経路に載せる。
    pub async fn deliver(&self, pane: &str, bell: &str) -> Result<()> {
        if !is_pane_id(pane) {
            bail!("pane id の形式が不明です: {pane}");
        }
        match self.herdr.deliver(pane, bell).await? {
            Delivery::Sent => Ok(()),
            Delivery::Skipped(status) => {
                bail!(
                    "herdr pane {pane} は {} なので配送しません",
                    status.as_str()
                )
            }
        }
    }

    pub async fn capture_pane(&self, pane: &str) -> Result<String> {
        if !is_pane_id(pane) {
            bail!("pane id の形式が不明です: {pane}");
        }
        self.herdr.read(pane).await
    }

    /// 起動時に herdr へ疎通する。応答しなければ daemon は成立しない。
    pub async fn probe(&self) -> Result<()> {
        self.herdr.protocol().await.map(|_| ())
    }

    /// daemon を続けてよいかを判定する。herdr が応答しなくなったら `Err`。
    pub async fn still_serving(&self) -> Result<()> {
        self.herdr.protocol().await.map(|_| ())
    }
}

/// herdr の pane を宛先解決の形へ写す。
///
/// `workspace_id` を session、`tab_id` を window として扱う。
fn pane_info(pane: crate::herdr::HerdrPane) -> PaneInfo {
    let window_index = pane
        .tab_id
        .rsplit_once(":t")
        .map_or_else(|| pane.tab_id.clone(), |(_, index)| index.to_owned());
    let pane_index = pane
        .pane_id
        .rsplit_once(":p")
        .map_or_else(|| pane.pane_id.clone(), |(_, index)| index.to_owned());
    let scope_alias = pane
        .workspace_label
        .is_some()
        .then(|| pane.workspace_id.clone());
    PaneInfo {
        // 人間が知っている名前 (workspace label) を主名にする。
        // label が無い workspace は従来どおり workspace_id。
        session: pane
            .workspace_label
            .unwrap_or_else(|| pane.workspace_id.clone()),
        scope_alias,
        window_id: pane.tab_id,
        pane_id: pane.pane_id,
        cwd: pane.cwd,
        window_index,
        pane_index,
        agent: pane.agent,
        status: pane.status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_id_grammar_accepts_only_the_herdr_shape() {
        assert!(is_pane_id("w1:p2"));
        assert!(is_pane_id("w12:p30"));
        // 採番には大文字英字も現れる (実測: wX:p4, w1:pA)。
        assert!(is_pane_id("wX:p4"));
        assert!(is_pane_id("w1:pA"));
        assert!(is_pane_id("wX:pA"));
        // どちらでもない形は routing しない (黙って誤配送しない)。
        assert!(!is_pane_id(""));
        assert!(!is_pane_id("plain"));
        // 撤去済みの tmux 形式 (`%5`) はもう pane id ではない。
        assert!(!is_pane_id("%5"));
        // 宛先に来る `:` 入りの名前を pane id と誤認しない。
        assert!(!is_pane_id("review:security"));
        assert!(!is_pane_id("w1:t1"));
        // 小文字まで受けると `web:prod` のような宛先名を
        // workspace "eb" / pane "rod" と誤読するため、拒否を維持する。
        assert!(!is_pane_id("web:prod"));
        assert!(!is_pane_id("wx:p1"));
        assert!(!is_pane_id("w1:pz"));
        // 空 segment と非 ASCII は拒否。
        assert!(!is_pane_id("w:p1"));
        assert!(!is_pane_id("w1:p"));
        assert!(!is_pane_id("w1:pÀ"));
    }

    #[test]
    fn herdr_panes_map_onto_the_existing_addressing_shape() {
        let info = pane_info(crate::herdr::HerdrPane {
            pane_id: "w2:p3".into(),
            terminal_id: "term_x".into(),
            workspace_id: "w2".into(),
            workspace_label: Some("knowledge".into()),
            tab_id: "w2:t1".into(),
            cwd: "/home/miyabi/projects/agent-talkd".into(),
            agent: Some("codex".into()),
            status: AgentStatus::Blocked,
        });
        // 人間向けの label が主名、workspace_id は互換 alias。
        assert_eq!(info.session, "knowledge");
        assert_eq!(info.scope_alias.as_deref(), Some("w2"));
        let unlabeled = pane_info(crate::herdr::HerdrPane {
            pane_id: "w9:p1".into(),
            terminal_id: "term_y".into(),
            workspace_id: "w9".into(),
            workspace_label: None,
            tab_id: "w9:t1".into(),
            cwd: "/tmp".into(),
            agent: None,
            status: AgentStatus::Unknown,
        });
        assert_eq!(unlabeled.session, "w9");
        assert_eq!(unlabeled.scope_alias, None);
        assert_eq!(info.window_id, "w2:t1");
        assert_eq!(info.window_index, "1");
        assert_eq!(info.pane_index, "3");
        assert_eq!(info.agent.as_deref(), Some("codex"));
        assert_eq!(info.status, AgentStatus::Blocked);
    }

    #[tokio::test]
    async fn unknown_pane_shapes_are_refused_instead_of_guessed() {
        let backend = Backend::scripted(vec![]);
        let error = backend.deliver("nonsense", "bell").await.unwrap_err();
        assert!(error.to_string().contains("形式が不明"), "{error}");
        let error = backend.deliver("%1", "bell").await.unwrap_err();
        assert!(error.to_string().contains("形式が不明"), "{error}");
    }
}
