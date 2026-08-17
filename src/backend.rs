//! herdr を daemon の視点から扱う薄い adapter 層。
//!
//! `HerdrPane` を宛先解決が使う [`PaneInfo`] へ写し、配送・health check を
//! herdr の API に委譲する。かつては tmux と herdr の 2 backend を束ねる
//! multiplexer だったが、tmux 対応の廃止で herdr 単独になった。

use anyhow::{Result, bail};

use crate::herdr::{AgentStatus, Delivery, ForegroundProcess, Herdr};

/// 宛先解決が使う pane 情報。
#[derive(Debug, Clone)]
pub struct PaneInfo {
    /// 表示と scope 解決の主名。workspace label
    /// (label が使えないときは `workspace_id` へ fallback)。
    pub session: String,
    /// scope 解決の互換 alias。label を主名にしたときの `workspace_id`。
    /// 旧来の `w2/codex` 形を壊さないために解決だけに使い、表示には使わない。
    pub scope_alias: Option<String>,
    pub pane_id: String,
    pub cwd: String,
    pub window_index: String,
    pub pane_index: String,
    /// 宛先・表示・journal に使う識別名。usable な非数字 tab label があれば
    /// それ、無ければ runtime 検出名 (`agent`)。agent の居ない pane は `None`。
    pub name: Option<String>,
    /// herdr の runtime 検出名 (claude / codex / grok)。skill 記法と
    /// installed skill の解決はこちらを使う。
    pub agent: Option<String>,
    /// herdr 自身が持つ agent 状態。
    pub status: AgentStatus,
}

/// `register` command と同じ文字種で agent 名を検証する。herdr の native 名や
/// tab label をそのまま登録すると、CLI では拒否される `bad/name` 等が
/// pull/startup 経由でだけ登録され、宛先文法 (`scope/name`) が壊れる。
pub fn usable_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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

    /// 配達可能 (idle / done / working) と確認できた pane にだけ配送する。
    /// 送らなかった場合は `Err` を返し、呼び出し側の「配送できなかったので
    /// queue する」経路に載せる。
    ///
    /// pane id は herdr が発行した opaque な文字列で、文法検証はしない —
    /// 未知の id は herdr 自身が拒否する。
    pub async fn deliver(&self, pane: &str, bell: &str) -> Result<()> {
        Self::finish_delivery(self.herdr.deliver(pane, bell).await?, pane)
    }

    /// 受領催促。送信直前の herdr 再確認も reminder predicate を使う。
    pub async fn deliver_reminder(&self, pane: &str, bell: &str) -> Result<()> {
        Self::finish_delivery(self.herdr.deliver_reminder(pane, bell).await?, pane)
    }

    fn finish_delivery(delivery: Delivery, pane: &str) -> Result<()> {
        match delivery {
            Delivery::Sent => Ok(()),
            Delivery::Skipped(status) => {
                bail!(
                    "herdr pane {pane} は {} なので配送しません",
                    status.as_str()
                )
            }
        }
    }

    /// 1 つの pane の foreground process を herdr に問い合わせる。
    /// 宛先解決が要る pane にだけ呼ぶ (pane 一覧の走査には使わない)。
    pub async fn foreground_processes(&self, pane: &str) -> Result<Vec<ForegroundProcess>> {
        self.herdr.foreground_processes(pane).await
    }

    pub async fn capture_pane(&self, pane: &str) -> Result<String> {
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
    // 識別名の決定規則: agent の居る pane に、宛先として使えて **純数字でない**
    // tab label があればそれを name にする。純数字 label は custom 名の無い tab
    // (herdr は番号文字列を label に入れる)、宛先に使えない label は無名と同じ
    // なので、どちらも従来どおり runtime 検出名へ fallback する。
    let name = pane.agent.as_ref().map(|runtime| {
        pane.tab_label
            .as_deref()
            .filter(|label| usable_agent_name(label))
            .filter(|label| !label.chars().all(|c| c.is_ascii_digit()))
            .map_or_else(|| runtime.clone(), str::to_owned)
    });
    PaneInfo {
        // 人間が知っている名前 (workspace label) を主名にする。
        // label が無い workspace は従来どおり workspace_id。
        session: pane
            .workspace_label
            .unwrap_or_else(|| pane.workspace_id.clone()),
        scope_alias,
        pane_id: pane.pane_id,
        cwd: pane.cwd,
        window_index,
        pane_index,
        name,
        agent: pane.agent,
        status: pane.status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn herdr_panes_map_onto_the_existing_addressing_shape() {
        let info = pane_info(crate::herdr::HerdrPane {
            pane_id: "w2:p3".into(),
            terminal_id: "term_x".into(),
            workspace_id: "w2".into(),
            workspace_label: Some("knowledge".into()),
            tab_id: "w2:t1".into(),
            tab_label: None,
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
            tab_label: None,
            cwd: "/tmp".into(),
            agent: None,
            status: AgentStatus::Unknown,
        });
        assert_eq!(unlabeled.session, "w9");
        assert_eq!(unlabeled.scope_alias, None);
        assert_eq!(info.window_index, "1");
        assert_eq!(info.pane_index, "3");
        assert_eq!(info.agent.as_deref(), Some("codex"));
        assert_eq!(info.status, AgentStatus::Blocked);
    }

    /// 識別名の決定規則: usable な非数字 tab label > runtime 検出名。
    /// agent の居ない pane は tab label があっても無名 (登録対象にしない)。
    #[test]
    fn the_pane_name_prefers_a_custom_tab_label_over_the_runtime() {
        let base = crate::herdr::HerdrPane {
            pane_id: "w1:p1".into(),
            terminal_id: "term".into(),
            workspace_id: "w1".into(),
            workspace_label: None,
            tab_id: "w1:t1".into(),
            tab_label: None,
            cwd: "/tmp".into(),
            agent: Some("claude".into()),
            status: AgentStatus::Idle,
        };
        let named = |label: Option<&str>, agent: Option<&str>| {
            pane_info(crate::herdr::HerdrPane {
                tab_label: label.map(str::to_owned),
                agent: agent.map(str::to_owned),
                ..base.clone()
            })
            .name
        };
        assert_eq!(
            named(Some("fable"), Some("claude")).as_deref(),
            Some("fable")
        );
        assert_eq!(
            named(Some("4"), Some("claude")).as_deref(),
            Some("claude"),
            "純数字 label は custom 名なし (番号) なので runtime 名"
        );
        assert_eq!(named(None, Some("claude")).as_deref(), Some("claude"));
        assert_eq!(
            named(Some("日本語"), Some("claude")).as_deref(),
            Some("claude"),
            "宛先に使えない label は無名と同じ扱いで runtime 名へ fallback"
        );
        assert_eq!(
            named(Some("fable"), None),
            None,
            "agent の居ない pane は無名"
        );
    }

    #[tokio::test]
    async fn an_unknown_pane_id_fails_the_delivery_without_any_grammar_check() {
        // id は opaque — 形式では落とさず、herdr が知らない pane として失敗する。
        let backend = Backend::scripted(vec![]);
        let error = backend.deliver("nonsense", "bell").await.unwrap_err();
        assert!(
            error.to_string().contains("状態を取得できません"),
            "{error}"
        );
    }
}
