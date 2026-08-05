//! tmux と herdr を 1 つの daemon から同時に扱うための統合層。
//!
//! 移行期には両方の multiplexer が同時に動く。排他にすると
//! 「tmux 側の agent」と「herdr 側の agent」が会話できなくなるので、
//! **1 daemon が両方を監視し、1 つの registry と 1 つの journal を共有する**。
//!
//! pane id の形式が構造的に交わらない (tmux は `%5`、herdr は `w1:p2`) ので、
//! 単一の名前空間へ混ぜても曖昧にならず、既存 journal もそのまま読める。
//!
//! この二重化は移行のための一時的な足場である。移行が終わったら tmux 側を
//! 削ること (README の TODO 参照)。

use anyhow::{Result, bail};
use tracing::warn;

use crate::{
    herdr::{AgentStatus, Delivery, Herdr},
    tmux::Tmux,
};

/// pane がどちらの multiplexer に属するか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Tmux,
    Herdr,
}

impl BackendKind {
    /// pane id の形式から所属を決める。
    ///
    /// tmux の pane id は必ず `%<digits>`、herdr は `w<digits>:p<digits>`。
    /// 交わらないので前置詞を足さずに判別できる。
    ///
    /// **どちらの形にも厳密に一致しないものは `None`。** 宛先文字列には
    /// `review:security` のような `:` を含む名前も来るため、`:` を含むだけで
    /// herdr の pane id と見なすと agent 名を pane id と誤認する。
    pub fn of(pane_id: &str) -> Option<Self> {
        if let Some(rest) = pane_id.strip_prefix('%') {
            return digits(rest).then_some(Self::Tmux);
        }
        let (workspace, pane) = pane_id.split_once(":p")?;
        let workspace = workspace.strip_prefix('w')?;
        (digits(workspace) && digits(pane)).then_some(Self::Herdr)
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

/// backend 非依存の pane 情報。既存の宛先解決 (`session/agent`) はこの形のまま動く。
#[derive(Debug, Clone)]
pub struct PaneInfo {
    /// 表示と scope 解決の主名。tmux は session 名、herdr は workspace label
    /// (label が使えないときは `workspace_id` へ fallback)。
    pub session: String,
    /// scope 解決の互換 alias。herdr で label を主名にしたときの `workspace_id`。
    /// 旧来の `w2/codex` 形を壊さないために解決だけに使い、表示には使わない。
    pub scope_alias: Option<String>,
    pub window_id: String,
    pub pane_id: String,
    pub cwd: String,
    pub window_index: String,
    pub pane_index: String,
    pub agent: Option<String>,
    pub backend: BackendKind,
    /// multiplexer 自身が持つ agent 状態。tmux には無いので `None`。
    pub status: Option<AgentStatus>,
}

/// 自分が居る pane の id を環境から求める。
///
/// tmux を先に見る。herdr の pane の中で tmux を起動している場合、agent が
/// 実際に居るのは内側の tmux なので `TMUX_PANE` が正しい identity になる。
pub fn self_pane() -> Option<String> {
    std::env::var("TMUX_PANE")
        .ok()
        .filter(|pane| !pane.is_empty())
        .or_else(|| {
            std::env::var("HERDR_PANE_ID")
                .ok()
                .filter(|pane| !pane.is_empty())
        })
}

/// 起動時に採取する health の基準値。
#[derive(Debug, Clone, Default)]
pub struct Baseline {
    pub tmux_pid: Option<u32>,
    pub herdr_protocol: Option<u64>,
}

#[derive(Clone)]
pub struct Multiplexer {
    tmux: Option<Tmux>,
    herdr: Option<Herdr>,
}

impl Multiplexer {
    pub fn new(tmux: Option<Tmux>, herdr: Option<Herdr>) -> Self {
        Self { tmux, herdr }
    }

    pub fn tmux(&self) -> Option<&Tmux> {
        self.tmux.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn herdr(&self) -> Option<&Herdr> {
        self.herdr.as_ref()
    }

    /// 両 backend の pane を連結して返す。
    ///
    /// **片方が落ちていても、もう片方の結果は返す**。移行期には
    /// 「tmux は生きているが herdr を止めた」が普通に起きるため、
    /// 全体を失敗させると会話が全滅する。
    pub async fn panes(&self) -> Result<Vec<PaneInfo>> {
        let (panes, failures, configured) = self.collect_panes().await;
        if configured > 0 && failures == configured {
            bail!("すべての multiplexer から pane 一覧を取得できません");
        }
        Ok(panes)
    }

    /// herdr backend だけの pane snapshot。herdr が構成されていなければ `None`。
    ///
    /// herdr 登録の pull 化 (native identity からの継続登録・evict 判定) 用。
    /// local UDS の poll だけで、tmux の subprocess は起動しない。
    pub async fn herdr_snapshot(&self) -> Option<Result<Vec<PaneInfo>>> {
        let herdr = self.herdr.as_ref()?;
        Some(
            herdr
                .panes()
                .await
                .map(|panes| panes.into_iter().map(herdr_pane_info).collect()),
        )
    }

    /// **全** backend が答えたときだけ pane 一覧を返す。
    ///
    /// reconcile の evict 判定用。片 backend が落ちた部分的な一覧を
    /// 「pane 不在」と読むと、その backend の生存登録を全滅させてしまう。
    /// 不完全な証拠では消さない — evict は次の完全な一覧まで見送る。
    pub async fn panes_from_all_backends(&self) -> Result<Vec<PaneInfo>> {
        let (panes, failures, _) = self.collect_panes().await;
        if failures > 0 {
            bail!("一部の multiplexer から pane 一覧を取得できません");
        }
        Ok(panes)
    }

    async fn collect_panes(&self) -> (Vec<PaneInfo>, u8, u8) {
        let mut panes = Vec::new();
        let mut failures = 0_u8;
        let mut configured = 0_u8;

        if let Some(tmux) = &self.tmux {
            configured += 1;
            match tmux.panes().await {
                Ok(found) => panes.extend(found),
                Err(error) => {
                    failures += 1;
                    warn!(%error, source = "tmux", "pane 一覧を取得できません");
                }
            }
        }
        if let Some(herdr) = &self.herdr {
            configured += 1;
            match herdr.panes().await {
                Ok(found) => panes.extend(found.into_iter().map(herdr_pane_info)),
                Err(error) => {
                    failures += 1;
                    warn!(%error, source = "herdr", "pane 一覧を取得できません");
                }
            }
        }
        (panes, failures, configured)
    }

    /// pane id から backend を選んで配送する。
    ///
    /// herdr 側は idle と確認できたときだけ送る。送らなかった場合は `Err` を
    /// 返し、呼び出し側の「配送できなかったので queue する」経路に載せる。
    pub async fn deliver(&self, pane: &str, bell: &str) -> Result<()> {
        match Self::route(pane)? {
            BackendKind::Tmux => self.require_tmux()?.deliver(pane, bell).await,
            BackendKind::Herdr => match self.require_herdr()?.deliver(pane, bell).await? {
                Delivery::Sent => Ok(()),
                Delivery::Skipped(status) => {
                    bail!(
                        "herdr pane {pane} は {} なので配送しません",
                        status.as_str()
                    )
                }
            },
        }
    }

    /// tmux の pane option 鏡像。herdr では no-op。
    ///
    /// `AGENTS.md` の定めるとおり option は compatibility mirror であって
    /// source of truth ではない。herdr は自身が状態を持つので鏡像が要らない。
    pub async fn set_option(&self, pane: &str, key: &str, value: Option<&str>) -> Result<()> {
        match Self::route(pane)? {
            BackendKind::Tmux => self.require_tmux()?.set_option(pane, key, value).await,
            BackendKind::Herdr => Ok(()),
        }
    }

    pub async fn capture_pane(&self, pane: &str) -> Result<String> {
        match Self::route(pane)? {
            BackendKind::Tmux => self.require_tmux()?.capture_pane(pane).await,
            BackendKind::Herdr => self.require_herdr()?.read(pane).await,
        }
    }

    pub async fn mark_talk_sent(&self, pane: &str) {
        if matches!(BackendKind::of(pane), Some(BackendKind::Tmux))
            && let Some(tmux) = &self.tmux
        {
            tmux.mark_talk_sent(pane).await;
        }
    }

    /// 起動時に各 backend へ疎通し、**応答しないものを外す**。
    ///
    /// 落とすのは失敗ではない。停止した multiplexer の socket file が残って
    /// いるだけ、という状況は普通に起きる。片方が死んでいても、もう片方で
    /// daemon は成立する。両方死んでいるときだけ起動を失敗させる。
    pub async fn probe(&mut self) -> Result<Baseline> {
        let mut baseline = Baseline::default();
        if let Some(tmux) = &self.tmux {
            match tmux.server_pid().await {
                Ok(pid) => baseline.tmux_pid = Some(pid),
                Err(error) => {
                    warn!(%error, source = "tmux", "応答しないので backend から外します");
                    self.tmux = None;
                }
            }
        }
        if let Some(herdr) = &self.herdr {
            match herdr.protocol().await {
                Ok(protocol) => baseline.herdr_protocol = Some(protocol),
                Err(error) => {
                    warn!(%error, source = "herdr", "応答しないので backend から外します");
                    self.herdr = None;
                }
            }
        }
        if self.tmux.is_none() && self.herdr.is_none() {
            bail!("有効な multiplexer がありません");
        }
        Ok(baseline)
    }

    /// daemon を続けてよいかを判定する。
    ///
    /// - tmux server が別プロセスに入れ替わったら `false` (旧 registry が無効)
    /// - 設定された backend が **すべて** 応答しなくなったら `Err`
    /// - 片方でも生きていれば `true`
    pub async fn still_serving(&self, baseline: &Baseline) -> Result<bool> {
        let mut alive = 0_u8;
        let mut configured = 0_u8;

        if let Some(tmux) = &self.tmux {
            configured += 1;
            match tmux.server_pid().await {
                Ok(pid) if Some(pid) == baseline.tmux_pid => alive += 1,
                Ok(_) => return Ok(false),
                Err(error) => warn!(%error, source = "tmux-health", "health check に失敗"),
            }
        }
        if let Some(herdr) = &self.herdr {
            configured += 1;
            match herdr.protocol().await {
                Ok(_) => alive += 1,
                Err(error) => warn!(%error, source = "herdr-health", "health check に失敗"),
            }
        }
        if configured > 0 && alive == 0 {
            bail!("すべての multiplexer が応答しません");
        }
        Ok(true)
    }

    fn route(pane: &str) -> Result<BackendKind> {
        BackendKind::of(pane).ok_or_else(|| anyhow::anyhow!("pane id の形式が不明です: {pane}"))
    }

    fn require_tmux(&self) -> Result<&Tmux> {
        self.tmux
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("tmux backend が有効ではありません"))
    }

    fn require_herdr(&self) -> Result<&Herdr> {
        self.herdr
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("herdr backend が有効ではありません"))
    }
}

/// herdr の pane を backend 非依存の形へ写す。
///
/// `workspace_id` を session、`tab_id` を window として扱うことで、
/// 既存の `session/agent` 宛先解決をそのまま流用できる。
fn herdr_pane_info(pane: crate::herdr::HerdrPane) -> PaneInfo {
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
        // 人間が知っている名前 (workspace label) を tmux の session 名の対応物に
        // する。label が無い workspace は従来どおり workspace_id。
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
        backend: BackendKind::Herdr,
        status: Some(pane.status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_ids_of_the_two_multiplexers_never_collide() {
        assert_eq!(BackendKind::of("%5"), Some(BackendKind::Tmux));
        assert_eq!(BackendKind::of("%40"), Some(BackendKind::Tmux));
        assert_eq!(BackendKind::of("w1:p2"), Some(BackendKind::Herdr));
        assert_eq!(BackendKind::of("w12:p30"), Some(BackendKind::Herdr));
        // どちらでもない形は routing しない (黙って誤配送しない)。
        assert_eq!(BackendKind::of(""), None);
        assert_eq!(BackendKind::of("%"), None);
        assert_eq!(BackendKind::of("%abc"), None);
        assert_eq!(BackendKind::of("plain"), None);
        // 宛先に来る `:` 入りの名前を pane id と誤認しない。
        assert_eq!(BackendKind::of("review:security"), None);
        assert_eq!(BackendKind::of("w1:t1"), None);
        assert_eq!(BackendKind::of("wx:p1"), None);
        assert_eq!(BackendKind::of("w1:pz"), None);
    }

    #[test]
    fn herdr_panes_map_onto_the_existing_addressing_shape() {
        let info = herdr_pane_info(crate::herdr::HerdrPane {
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
        let unlabeled = herdr_pane_info(crate::herdr::HerdrPane {
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
        assert_eq!(info.backend, BackendKind::Herdr);
        assert_eq!(info.status, Some(AgentStatus::Blocked));
    }

    #[tokio::test]
    async fn unknown_pane_shapes_are_refused_instead_of_guessed() {
        let mux = Multiplexer::new(None, None);
        let error = mux.deliver("nonsense", "bell").await.unwrap_err();
        assert!(error.to_string().contains("形式が不明"), "{error}");
    }

    #[tokio::test]
    async fn delivering_to_a_disabled_backend_fails_loudly() {
        let mux = Multiplexer::new(None, None);
        let error = mux.deliver("w1:p1", "bell").await.unwrap_err();
        assert!(error.to_string().contains("herdr backend"), "{error}");
        let error = mux.deliver("%1", "bell").await.unwrap_err();
        assert!(error.to_string().contains("tmux backend"), "{error}");
    }
}
