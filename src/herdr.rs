//! herdr socket API client。
//!
//! herdr は newline-delimited JSON を local socket で話す (protocol 17)。
//! 1 リクエスト 1 接続で、`{"id","method","params"}` を送り 1 行の応答を読む。
//!
//! 配送には **状態ガード** を必ず挟む。
//! herdr の入力系 API 自体には steer ガードが無く、working / blocked な
//! pane にも文字を撃ち込める。承認ダイアログへ Enter を撃ち込む事故を避けるため、
//! `blocked` と `unknown` には一文字も送らない (README の「herdr backend」節)。
//! 初回配達と queue drain は `idle` / `done` / `working` を許す。`unknown` は
//! 安全の証拠にならないので送らない。

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

/// 応答 1 行の上限。壊れた相手からの無限読み出しを防ぐ。
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

/// `pane.process_info` の応答封筒に載る `type`。
const PROCESS_INFO_TYPE: &str = "pane_process_info";

/// herdr が報告する agent の意味的状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl AgentStatus {
    fn parse(value: &str) -> Self {
        match value {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "blocked" => Self::Blocked,
            "done" => Self::Done,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Unknown => "unknown",
        }
    }

    /// 初回配達と queue drain を許すのは `idle` / `done` / `working`。
    ///
    /// steer-safety が守るのは承認ダイアログ等の入力待ち (`blocked`)。
    /// `working` はターン進行中でも呼び鈴を届ける — 長寿命の裏プロセスで
    /// herdr が `idle`/`done` に戻らないと、queue が永久に滞留するため。
    /// `done` はターンが完了して入力欄が空いた状態で、user がその pane を
    /// 表示するまで保たれる**表示上の**バッジにすぎない — これを配送不可と
    /// 同一視すると、非表示 tab 宛の message が user の巡回まで滞留する。
    /// done への配達は未閲覧バッジを消して新ターンを始める。
    /// `Unknown` を許さないのは、herdr の detection manifest に無い画面形状が
    /// idle fallback になり得るため。負の証拠を根拠に入力してはならない。
    pub fn accepts_delivery(self) -> bool {
        matches!(self, Self::Idle | Self::Done | Self::Working)
    }

    /// 受領催促を許すのはターンとターンの間 (`idle` / `done`) だけ。
    ///
    /// 長ターン中の催促連打を避ける。初回配達とは predicate を共有しない。
    pub fn accepts_reminder(self) -> bool {
        matches!(self, Self::Idle | Self::Done)
    }
}

/// 配送の結果。`Skipped` は失敗ではなく「送らないと判断した」ことを表す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Sent,
    Skipped(AgentStatus),
}

/// `pane.process_info` が申告する foreground process 1 つ。
///
/// herdr は pane の foreground process group に居る process だけを挙げる。
/// pane で動いている agent 本体はここに載るが、その agent が spawn した子
/// agent は載らない — 主 agent と子 agent を取り違えないための土台。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundProcess {
    pub pid: i32,
    pub name: String,
}

/// herdr の pane 1 つ。`pane_id` は位置依存 (`w1:p2`)、`terminal_id` は安定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrPane {
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    /// workspace の人間向け名 (`workspace.list` の label)。表示と scope 解決の
    /// 主名になる。未設定・宛先構文と衝突する文字を含む場合は `None`。
    pub workspace_label: Option<String>,
    pub tab_id: String,
    /// tab の人間向け名 (`tab.list` の label)。宛先・表示の主名の源になる。
    /// 宛先構文と衝突する文字を含む label は `None` (workspace label と同じ規則)。
    /// custom 名の無い tab は番号文字列 (例 `"4"`) が入る。
    pub tab_label: Option<String>,
    pub cwd: String,
    pub agent: Option<String>,
    pub status: AgentStatus,
}

#[derive(Clone)]
pub struct Herdr {
    socket: PathBuf,
    /// test 専用の代役。`Some` のとき socket を一切使わず、この一覧を返す。
    /// clone 間で共有されるので、テストが tick の合間に中身を差し替えられる。
    #[cfg(test)]
    pub(crate) scripted: Option<std::sync::Arc<std::sync::Mutex<Vec<HerdrPane>>>>,
    /// scripted モードで `panes()` を失敗させる test 専用 failpoint。
    /// 実装では `tab.list` の取得失敗が pane 列挙ごと失敗する (fail-closed) ため、
    /// その tick を daemon 側テストから再現するために使う。
    #[cfg(test)]
    pub(crate) scripted_fail: std::sync::Arc<std::sync::Mutex<bool>>,
    /// scripted モードで `deliver` された (pane, text) の記録 (test 専用)。
    /// clone 間で共有されるので、broker へ渡した後からでも観測できる。
    #[cfg(test)]
    pub(crate) delivered: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    /// scripted モードで `pane.process_info` が返すもの (test 専用)。
    /// 登録の無い pane は実装と同じく Err になる。
    #[cfg(test)]
    pub(crate) scripted_processes:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, ProcessInfoScript>>>,
}

/// scripted herdr の `pane.process_info` の振る舞い (test 専用)。
///
/// `Answer` は **wire と同じ応答封筒** を持ち、実装と同じ `parse_process_info`
/// を通す。scripted 経路が parse を迂回すると、封筒の破損に対する daemon 側の
/// 振る舞い (解決不能へ倒す) をテストできなくなる。
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) enum ProcessInfoScript {
    Answer(Value),
    /// この method にだけ応答しない herdr。呼び出し側の timeout を測る。
    Never,
}

impl Herdr {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            #[cfg(test)]
            scripted: None,
            #[cfg(test)]
            scripted_fail: std::sync::Arc::default(),
            #[cfg(test)]
            delivered: std::sync::Arc::default(),
            #[cfg(test)]
            scripted_processes: std::sync::Arc::default(),
        }
    }

    /// herdr を起動せずに固定の pane 一覧を返す代役 (test 専用)。
    #[cfg(test)]
    pub fn scripted(panes: Vec<HerdrPane>) -> Self {
        Self {
            socket: PathBuf::new(),
            scripted: Some(std::sync::Arc::new(std::sync::Mutex::new(panes))),
            scripted_fail: std::sync::Arc::default(),
            delivered: std::sync::Arc::default(),
            scripted_processes: std::sync::Arc::default(),
        }
    }

    /// scripted モードで、ある pane の foreground process を差し込む (test 専用)。
    /// 実 herdr と同じ応答封筒を組み立てるので、実装と同じ parse を通る。
    #[cfg(test)]
    pub(crate) fn script_processes(&self, pane_id: &str, processes: &[ForegroundProcess]) {
        let processes: Vec<Value> = processes
            .iter()
            .map(|process| json!({"pid": process.pid, "name": process.name}))
            .collect();
        self.script_process_info(
            pane_id,
            json!({
                "type": "pane_process_info",
                "process_info": {
                    "pane_id": pane_id,
                    "shell_pid": 1,
                    "foreground_process_group_id": 1,
                    "foreground_processes": processes,
                },
            }),
        );
    }

    /// scripted モードで、`pane.process_info` の応答封筒をそのまま差し込む
    /// (test 専用)。壊れた封筒に対する daemon の振る舞いを測るために使う。
    #[cfg(test)]
    pub(crate) fn script_process_info(&self, pane_id: &str, result: Value) {
        self.scripted_processes
            .lock()
            .unwrap()
            .insert(pane_id.to_owned(), ProcessInfoScript::Answer(result));
    }

    /// scripted モードで、その pane の `pane.process_info` にだけ **応答しない**
    /// herdr を模す (test 専用)。他の method は普通に答える。
    #[cfg(test)]
    pub(crate) fn silence_process_info(&self, pane_id: &str) {
        self.scripted_processes
            .lock()
            .unwrap()
            .insert(pane_id.to_owned(), ProcessInfoScript::Never);
    }

    /// scripted モードで、その pane の `pane.process_info` を失敗させる
    /// (test 専用)。古い herdr の未知 method や pane 消滅を模す。
    #[cfg(test)]
    pub(crate) fn clear_processes(&self, pane_id: &str) {
        self.scripted_processes.lock().unwrap().remove(pane_id);
    }

    /// protocol 番号を返す。daemon の health check に使う。
    pub async fn protocol(&self) -> Result<u64> {
        #[cfg(test)]
        if self.scripted.is_some() {
            return Ok(17);
        }
        let result = self.call("ping", json!({})).await?;
        result
            .get("protocol")
            .and_then(Value::as_u64)
            .context("herdr ping に protocol がありません")
    }

    pub async fn panes(&self) -> Result<Vec<HerdrPane>> {
        #[cfg(test)]
        if let Some(scripted) = &self.scripted {
            if *self.scripted_fail.lock().unwrap() {
                bail!("scripted snapshot failure (tab.list)");
            }
            return Ok(scripted.lock().unwrap().clone());
        }
        let result = self.call("pane.list", json!({})).await?;
        let panes = result
            .get("panes")
            .and_then(Value::as_array)
            .context("herdr pane.list に panes がありません")?;
        // workspace の label (人間向け名) を引く。取得失敗は pane 列挙を
        // 失敗させず、label なし (workspace_id 表示) へ劣化させる。
        // rename は次回の取得で自然に追従する (bridge は read-only 消費のみ)。
        let labels = self.workspace_labels().await.unwrap_or_default();
        // tab の label (タブ名) を引く。こちらは agent の **identity の源** なので、
        // 取得失敗を label なしへ劣化させると runtime 名への一時交代 (identity
        // 交代・誤配) が起きる。fail-closed: この tick の pane 列挙ごと失敗させ、
        // 呼び出し側 (pull 同期) は snapshot を適用しない。
        let tab_labels = self.tab_labels().await?;
        Ok(panes
            .iter()
            .filter_map(parse_pane)
            .map(|mut pane| {
                pane.workspace_label = labels.get(&pane.workspace_id).cloned();
                pane.tab_label = tab_labels.get(&pane.tab_id).cloned();
                pane
            })
            .collect())
    }

    /// `workspace.list` から `workspace_id` → label の対応を作る。
    /// 宛先構文 (`scope/name`, `w1:p2`) と誤解される文字を含む label は捨てる。
    async fn workspace_labels(&self) -> Result<std::collections::HashMap<String, String>> {
        let result = self.call("workspace.list", json!({})).await?;
        let workspaces = result
            .get("workspaces")
            .and_then(Value::as_array)
            .context("herdr workspace.list に workspaces がありません")?;
        Ok(workspaces
            .iter()
            .filter_map(|workspace| {
                let id = workspace.get("workspace_id")?.as_str()?;
                let label = workspace.get("label")?.as_str()?;
                usable_label(label).then(|| (id.to_owned(), label.to_owned()))
            })
            .collect())
    }

    /// `tab.list` から `tab_id` → label の対応を作る (`workspace_labels` と同型)。
    /// params の `workspace_id` を省略して全 workspace を 1 回で引く。
    /// 宛先構文と衝突する文字を含む label は捨てる。純数字 label (custom 名の
    /// 無い tab) の解釈は名前決定側 (`backend::pane_info`) が行う。
    async fn tab_labels(&self) -> Result<std::collections::HashMap<String, String>> {
        let result = self.call("tab.list", json!({})).await?;
        let tabs = result
            .get("tabs")
            .and_then(Value::as_array)
            .context("herdr tab.list に tabs がありません")?;
        Ok(tabs
            .iter()
            .filter_map(|tab| {
                let id = tab.get("tab_id")?.as_str()?;
                let label = tab.get("label")?.as_str()?;
                usable_label(label).then(|| (id.to_owned(), label.to_owned()))
            })
            .collect())
    }

    /// 初回配達と queue drain: `idle` / `done` / `working` の **agent** に
    /// 呼び鈴を届ける。
    ///
    /// 状態の取得と送信の間には原理的に race があるが、herdr には
    /// 「配達可能なら送る」を atomic に行う API が無い。窓を最小化するため、
    /// 直前に取得した状態で判断する。
    ///
    /// 送信は `pane.send_text` ではなく `agent.prompt` を使う。submit の作法
    /// (Enter の押し方・paste の扱い) は herdr が agent 種別ごとに知っている側で、
    /// `send_text` だと本文が入力欄に残ったまま turn が始まらない。agent が居ない
    /// pane へは herdr が `agent_not_running` で拒否するため、素の shell に呼び鈴が
    /// タイプされる事故も構造的に起きない (Err は呼び出し側の requeue 経路に乗る)。
    /// `wait` は付けない — 単一 event loop を agent の完了待ちで塞がない。
    pub async fn deliver(&self, pane_id: &str, text: &str) -> Result<Delivery> {
        self.deliver_gated(pane_id, text, AgentStatus::accepts_delivery)
            .await
    }

    /// 受領催促: 送信直前にも `accepts_reminder` (`idle` / `done`) を再確認する。
    /// daemon 側の候補抽出とは別に、ここが催促の最終ゲートである。
    pub async fn deliver_reminder(&self, pane_id: &str, text: &str) -> Result<Delivery> {
        self.deliver_gated(pane_id, text, AgentStatus::accepts_reminder)
            .await
    }

    async fn deliver_gated(
        &self,
        pane_id: &str,
        text: &str,
        allowed: fn(AgentStatus) -> bool,
    ) -> Result<Delivery> {
        #[cfg(test)]
        if let Some(scripted) = &self.scripted {
            // 実実装と同じ規則: pane 不在は Err、許可以外は Skipped。
            let status = scripted
                .lock()
                .unwrap()
                .iter()
                .find(|pane| pane.pane_id == pane_id)
                .map(|pane| pane.status)
                .with_context(|| format!("herdr pane {pane_id} の状態を取得できません"))?;
            if !allowed(status) {
                return Ok(Delivery::Skipped(status));
            }
            self.delivered
                .lock()
                .unwrap()
                .push((pane_id.to_owned(), text.to_owned()));
            return Ok(Delivery::Sent);
        }
        let status = self.status_of(pane_id).await?;
        if !allowed(status) {
            return Ok(Delivery::Skipped(status));
        }
        self.call("agent.prompt", json!({"target": pane_id, "text": text}))
            .await?;
        Ok(Delivery::Sent)
    }

    pub async fn read(&self, pane_id: &str) -> Result<String> {
        #[cfg(test)]
        if let Some(scripted) = &self.scripted {
            if scripted
                .lock()
                .unwrap()
                .iter()
                .any(|pane| pane.pane_id == pane_id)
            {
                return Ok(format!("scripted screen of {pane_id}"));
            }
            bail!("herdr pane {pane_id} は存在しません");
        }
        let result = self
            .call(
                "pane.read",
                json!({"pane_id": pane_id, "source": "visible"}),
            )
            .await?;
        // 実 herdr は method 別の封筒を持つ: `pane.read` の中身は `result.read.text`。
        // 欠落を空画面へ黙って劣化させず、protocol error として表面化する。
        result
            .get("read")
            .and_then(|read| read.get("text"))
            .and_then(Value::as_str)
            .map(std::borrow::ToOwned::to_owned)
            .with_context(|| format!("herdr pane.read 応答に read.text がありません ({pane_id})"))
    }

    /// `pane.process_info` で **1 つの pane** の foreground process を引く。
    ///
    /// 呼ぶのは宛先の解決が要る pane だけ — 全 pane を舐めない。
    ///
    /// wire 形式 (2026-08-17 実機採取, herdr 0.7.5 / protocol 17): params の key は
    /// `pane_id` で、応答は `result.process_info.foreground_processes[]`。
    /// **`target` を渡すと herdr は key を無視して focus 中の pane を黙って返す**
    /// ため、申告された `process_info.pane_id` を照合して不一致は失敗にする。
    /// 別 pane の PID を宛先に化けさせないための fail-closed。
    pub async fn foreground_processes(&self, pane_id: &str) -> Result<Vec<ForegroundProcess>> {
        #[cfg(test)]
        if self.scripted.is_some() {
            let script = self
                .scripted_processes
                .lock()
                .unwrap()
                .get(pane_id)
                .cloned()
                .with_context(|| format!("herdr pane {pane_id} の process を取得できません"))?;
            return match script {
                ProcessInfoScript::Answer(result) => parse_process_info(&result, pane_id),
                ProcessInfoScript::Never => std::future::pending().await,
            };
        }
        let result = self
            .call("pane.process_info", json!({"pane_id": pane_id}))
            .await?;
        parse_process_info(&result, pane_id)
    }

    async fn status_of(&self, pane_id: &str) -> Result<AgentStatus> {
        let result = self
            .call("pane.get", json!({"pane_id": pane_id}))
            .await
            .with_context(|| format!("herdr pane {pane_id} の状態を取得できません"))?;
        // `pane.get` の中身は `result.pane`。封筒の欠けた応答は Unknown に
        // 劣化させず protocol error にする (壊れた応答と「本当に unknown」を
        // 混同しない)。`agent_status` の欠落だけは従来どおり Unknown。
        let pane = result
            .get("pane")
            .filter(|value| value.is_object())
            .with_context(|| {
                format!("herdr pane.get 応答に pane object がありません ({pane_id})")
            })?;
        Ok(status_from(pane))
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("herdr socket へ接続できません: {}", self.socket.display()))?;
        let (read_half, mut write_half) = stream.into_split();
        let request = format!(
            "{}\n",
            json!({"id": "agent-talkd", "method": method, "params": params})
        );
        write_half
            .write_all(request.as_bytes())
            .await
            .context("herdr へ要求を書けません")?;
        // 壊れた相手が改行を返さない場合に無限に読まないよう、読み出し側を
        // 先に切ってから行読みする。
        let mut reader = BufReader::new(read_half.take(MAX_RESPONSE_BYTES));
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .context("herdr の応答を読めません")?;
        if line.trim().is_empty() {
            bail!("herdr が {method} に応答しませんでした");
        }
        let response: Value = serde_json::from_str(line.trim())
            .with_context(|| format!("herdr の {method} 応答が JSON ではありません"))?;
        if let Some(error) = response.get("error") {
            let code = error.get("code").and_then(Value::as_str).unwrap_or("error");
            let message = error.get("message").and_then(Value::as_str).unwrap_or("");
            bail!("herdr {method} が失敗しました: {code}: {message}");
        }
        response
            .get("result")
            .cloned()
            .with_context(|| format!("herdr の {method} 応答に result がありません"))
    }
}

fn status_from(value: &Value) -> AgentStatus {
    value
        .get("agent_status")
        .and_then(Value::as_str)
        .map_or(AgentStatus::Unknown, AgentStatus::parse)
}

/// `pane.process_info` の応答封筒を読む。**壊れた要素を捨てない。**
///
/// 捨てると、主 agent の要素だけ PID を落とした応答が「同名の子 agent が
/// 1 つだけ居る pane」に化け、子の PID が一意の候補として採用されてしまう。
/// 封筒の `type`・申告 pane・全要素のいずれか 1 つでも壊れていたら応答全体を
/// 失敗にし、呼び出し側 (`cross_session_socket`) を「解決不能」へ倒す。
fn parse_process_info(result: &Value, pane_id: &str) -> Result<Vec<ForegroundProcess>> {
    let kind = result.get("type").and_then(Value::as_str);
    if kind != Some(PROCESS_INFO_TYPE) {
        bail!(
            "herdr pane.process_info 応答の type が {PROCESS_INFO_TYPE} ではありません ({}) ({pane_id})",
            kind.unwrap_or("?")
        );
    }
    let info = result
        .get("process_info")
        .filter(|value| value.is_object())
        .with_context(|| {
            format!("herdr pane.process_info 応答に process_info がありません ({pane_id})")
        })?;
    let reported = info.get("pane_id").and_then(Value::as_str);
    if reported != Some(pane_id) {
        bail!(
            "herdr pane.process_info が別の pane ({}) を返しました ({pane_id} を要求)",
            reported.unwrap_or("?")
        );
    }
    let processes = info
        .get("foreground_processes")
        .and_then(Value::as_array)
        .with_context(|| {
            format!("herdr pane.process_info 応答に foreground_processes がありません ({pane_id})")
        })?;
    processes
        .iter()
        .enumerate()
        .map(|(index, value)| {
            parse_process(value).with_context(|| {
                format!(
                    "herdr pane.process_info 応答の foreground_processes[{index}] を読めません ({pane_id}): {value}"
                )
            })
        })
        .collect()
}

/// `foreground_processes[]` の 1 要素。object でない・`pid` が正の i32 でない・
/// `name` が文字列でない — どれも要素の破損として扱う (呼び出し側が応答ごと
/// 失敗させる)。
fn parse_process(value: &Value) -> Result<ForegroundProcess> {
    let object = value.as_object().context("object ではありません")?;
    let pid: i32 = object
        .get("pid")
        .and_then(Value::as_i64)
        .and_then(|pid| i32::try_from(pid).ok())
        .filter(|pid| *pid > 0)
        .context("pid が正の i32 ではありません")?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .context("name が文字列ではありません")?;
    Ok(ForegroundProcess {
        pid,
        name: name.to_owned(),
    })
}

fn parse_pane(value: &Value) -> Option<HerdrPane> {
    let field = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(std::borrow::ToOwned::to_owned)
    };
    Some(HerdrPane {
        pane_id: field("pane_id")?,
        terminal_id: field("terminal_id").unwrap_or_default(),
        workspace_id: field("workspace_id").unwrap_or_default(),
        workspace_label: None,
        tab_id: field("tab_id").unwrap_or_default(),
        tab_label: None,
        cwd: field("cwd").unwrap_or_default(),
        agent: field("agent").filter(|agent| !agent.is_empty()),
        status: status_from(value),
    })
}

/// 宛先・表示に安全に使える label か。`/` は scope 区切り、`:` は pane id 形式、
/// 空白は表の列区切りと衝突するため拒否する (拒否時は `workspace_id` へ fallback)。
fn usable_label(label: &str) -> bool {
    !label.is_empty()
        && !label
            .chars()
            .any(|c| c == '/' || c == ':' || c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };

    use super::*;

    /// 実 herdr の代わりに立てる偽サーバー。
    ///
    /// 実 socket 上で newline JSON をやり取りするので、wire 形式そのものを
    /// 検証できる。受け取った要求は全て記録し、テストから参照する。
    struct FakeHerdr {
        socket: PathBuf,
        requests: Arc<Mutex<Vec<Value>>>,
        _dir: TempDir,
    }

    impl FakeHerdr {
        fn start<F>(responder: F) -> Self
        where
            F: Fn(&str, &Value) -> Value + Send + Sync + 'static,
        {
            let dir = TempDir::new().unwrap();
            let socket = dir.path().join("herdr.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&requests);
            let responder = Arc::new(responder);
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let recorded = Arc::clone(&recorded);
                    let responder = Arc::clone(&responder);
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(stream);
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.is_err() || line.trim().is_empty() {
                            return;
                        }
                        let request: Value = serde_json::from_str(line.trim()).unwrap();
                        let method = request
                            .get("method")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        let params = request.get("params").cloned().unwrap_or(Value::Null);
                        recorded.lock().unwrap().push(request);
                        let response = responder(&method, &params);
                        let payload = format!("{response}\n");
                        let _ = reader.get_mut().write_all(payload.as_bytes()).await;
                    });
                }
            });
            Self {
                socket,
                requests,
                _dir: dir,
            }
        }

        fn client(&self) -> Herdr {
            Herdr::new(self.socket.clone())
        }

        fn methods(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter_map(|request| {
                    request
                        .get("method")
                        .and_then(Value::as_str)
                        .map(std::borrow::ToOwned::to_owned)
                })
                .collect()
        }
    }

    fn ok(result: &Value) -> Value {
        json!({"id": "agent-talkd", "result": result})
    }

    /// tab の無い `tab.list` 応答。tab 名を使わないテストの共通応答。
    fn empty_tab_list() -> Value {
        ok(&json!({"type": "tab_list", "tabs": []}))
    }

    /// 実 herdr の `pane.get` 封筒 (2026-08-03 実機採取): 中身は `result.pane`。
    fn pane_get(pane: &Value) -> Value {
        ok(&json!({"type": "pane", "pane": pane}))
    }

    fn pane(pane_id: &str, agent: &str, status: &str) -> Value {
        json!({
            "pane_id": pane_id,
            "terminal_id": format!("term_{pane_id}"),
            "workspace_id": pane_id.split(':').next().unwrap_or_default(),
            "tab_id": format!("{}:t1", pane_id.split(':').next().unwrap_or_default()),
            "cwd": "/home/miyabi/projects",
            "agent": agent,
            "agent_status": status,
        })
    }

    /// 達成条件 1: herdr の pane と agent 状態を列挙できる。
    #[tokio::test]
    async fn panes_expose_agent_identity_and_status() {
        let fake = FakeHerdr::start(|method, _| match method {
            "pane.list" => ok(&json!({
                "type": "pane_list",
                "panes": [
                    pane("w1:p1", "codex", "working"),
                    pane("w1:p2", "claude", "idle"),
                    json!({"pane_id": "w1:p6", "agent_status": "unknown"}),
                ],
            })),
            "tab.list" => empty_tab_list(),
            _ => ok(&json!({})),
        });

        let panes = fake.client().panes().await.unwrap();

        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0].pane_id, "w1:p1");
        assert_eq!(panes[0].agent.as_deref(), Some("codex"));
        assert_eq!(panes[0].status, AgentStatus::Working);
        assert_eq!(panes[0].terminal_id, "term_w1:p1");
        assert_eq!(panes[1].agent.as_deref(), Some("claude"));
        assert_eq!(panes[1].status, AgentStatus::Idle);
        // agent の居ない素の pane も列挙され、状態は Unknown になる。
        assert_eq!(panes[2].agent, None);
        assert_eq!(panes[2].status, AgentStatus::Unknown);
    }

    /// 達成条件 2 の中核: blocked / unknown な pane には一文字も送らない。
    ///
    /// 「送信を試みて失敗する」ではなく「**そもそも入力系 API を発行しない**」
    /// ことを、偽サーバーが受け取った method 列で証明する。
    #[tokio::test]
    async fn deliver_never_sends_text_to_a_pane_that_is_not_deliverable() {
        for status in ["blocked", "unknown"] {
            let owned = status.to_owned();
            let fake = FakeHerdr::start(move |method, _| match method {
                "pane.get" => pane_get(&pane("w1:p1", "codex", &owned)),
                _ => ok(&json!({})),
            });

            let delivery = fake.client().deliver("w1:p1", "[agent-talk] #1").await;

            assert_eq!(
                delivery.unwrap(),
                Delivery::Skipped(AgentStatus::parse(status)),
                "{status} は配送を拒否しなければならない"
            );
            assert_eq!(
                fake.methods(),
                vec!["pane.get"],
                "{status} な pane へは入力系 API を一切発行してはならない"
            );
        }
    }

    /// 達成条件 2 の対: idle / done / working なら agent へ prompt として届く。
    /// 拒否だけして届かないのでは無意味だし、入力欄に置くだけでは turn が始まらない。
    /// done を含めるのは、非表示 tab の完了バッジが user の巡回まで配達を
    /// 塞がないため。working を含めるのは、長寿命の裏プロセスで herdr が
    /// idle/done に戻らない相手へ呼び鈴が滞留しないため。
    #[tokio::test]
    async fn deliver_prompts_the_agent_of_an_idle_done_or_working_pane() {
        for status in ["idle", "done", "working"] {
            let owned = status.to_owned();
            let fake = FakeHerdr::start(move |method, _| match method {
                "pane.get" => pane_get(&pane("w1:p2", "claude", &owned)),
                _ => ok(&json!({})),
            });

            let delivery = fake
                .client()
                .deliver("w1:p2", "[agent-talk] #1")
                .await
                .unwrap();

            assert_eq!(delivery, Delivery::Sent, "{status} は配送可能");
            // send_text / send_keys ではなく agent.prompt ちょうど1発。
            assert_eq!(fake.methods(), vec!["pane.get", "agent.prompt"], "{status}");
            let sent = fake.requests.lock().unwrap().last().cloned().unwrap();
            assert_eq!(sent["params"]["target"], "w1:p2");
            assert_eq!(sent["params"]["text"], "[agent-talk] #1");
            // event loop を塞ぐ wait を仕込まない。
            assert!(sent["params"].get("wait").is_none(), "{sent}");
        }
    }

    /// 催促の最終ゲート: working でも入力系 API を発行しない。
    /// daemon の候補抽出をすり抜けても、送信直前の再確認が契約 4 を守る。
    #[tokio::test]
    async fn deliver_reminder_never_sends_text_to_a_working_blocked_or_unknown_pane() {
        for status in ["working", "blocked", "unknown"] {
            let owned = status.to_owned();
            let fake = FakeHerdr::start(move |method, _| match method {
                "pane.get" => pane_get(&pane("w1:p1", "codex", &owned)),
                _ => ok(&json!({})),
            });

            let delivery = fake
                .client()
                .deliver_reminder("w1:p1", "[agent-talk] nag")
                .await;

            assert_eq!(
                delivery.unwrap(),
                Delivery::Skipped(AgentStatus::parse(status)),
                "{status} は催促を拒否しなければならない"
            );
            assert_eq!(
                fake.methods(),
                vec!["pane.get"],
                "{status} な pane へ催促の入力系 API を発行してはならない"
            );
        }
    }

    #[tokio::test]
    async fn deliver_reminder_prompts_an_idle_or_done_pane() {
        for status in ["idle", "done"] {
            let owned = status.to_owned();
            let fake = FakeHerdr::start(move |method, _| match method {
                "pane.get" => pane_get(&pane("w1:p2", "claude", &owned)),
                _ => ok(&json!({})),
            });

            let delivery = fake
                .client()
                .deliver_reminder("w1:p2", "[agent-talk] nag")
                .await
                .unwrap();

            assert_eq!(delivery, Delivery::Sent, "{status} は催促可能");
            assert_eq!(fake.methods(), vec!["pane.get", "agent.prompt"], "{status}");
        }
    }

    #[test]
    fn working_accepts_delivery_but_not_reminders() {
        assert!(AgentStatus::Working.accepts_delivery());
        assert!(!AgentStatus::Working.accepts_reminder());
        assert!(AgentStatus::Idle.accepts_delivery() && AgentStatus::Idle.accepts_reminder());
        assert!(AgentStatus::Done.accepts_delivery() && AgentStatus::Done.accepts_reminder());
        assert!(!AgentStatus::Blocked.accepts_delivery());
        assert!(!AgentStatus::Blocked.accepts_reminder());
        assert!(!AgentStatus::Unknown.accepts_delivery());
        assert!(!AgentStatus::Unknown.accepts_reminder());
    }

    /// agent が pane から消えた race では herdr が `agent_not_running` を返す。
    /// これを成功や terminal 消費に変換せず Err にする (呼び出し側が requeue する)。
    #[tokio::test]
    async fn a_vanished_agent_fails_the_delivery_instead_of_typing_into_a_shell() {
        let fake = FakeHerdr::start(|method, _| match method {
            "pane.get" => pane_get(&pane("w1:p2", "claude", "idle")),
            "agent.prompt" => json!({
                "id": "agent-talkd",
                "error": {
                    "code": "agent_not_running",
                    "message": "agent is no longer running in the target pane",
                },
            }),
            _ => ok(&json!({})),
        });

        let error = fake
            .client()
            .deliver("w1:p2", "[agent-talk] #1")
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("agent_not_running"), "{error}");
    }

    /// `pane.read` は `result.read.text` を読む。封筒の欠けた応答を
    /// 空画面へ劣化させない (壊れた応答と空画面を混同しない)。
    #[tokio::test]
    async fn read_returns_nested_text_and_rejects_a_missing_envelope() {
        let fake = FakeHerdr::start(|method, _| match method {
            "pane.read" => ok(&json!({
                "type": "read",
                "read": {
                    "pane_id": "w1:p2",
                    "source": "visible",
                    "format": "text",
                    "text": "screen body",
                    "revision": 3,
                    "truncated": false,
                },
            })),
            _ => ok(&json!({})),
        });
        assert_eq!(fake.client().read("w1:p2").await.unwrap(), "screen body");

        // 旧フラット形 (実装が誤読していた形) は protocol error になる。
        let flat = FakeHerdr::start(|method, _| match method {
            "pane.read" => ok(&json!({"type": "read", "text": "flat"})),
            _ => ok(&json!({})),
        });
        let error = flat.client().read("w1:p2").await.unwrap_err().to_string();
        assert!(error.contains("read.text"), "{error}");
    }

    /// `pane.get` の封筒が欠けた応答は Unknown ではなく protocol error。
    /// どちらの場合も入力系 API は一文字も出ない。
    #[tokio::test]
    async fn a_flat_pane_get_response_is_a_protocol_error_not_unknown() {
        let fake = FakeHerdr::start(|method, _| match method {
            "pane.get" => ok(&pane("w1:p1", "codex", "idle")),
            _ => ok(&json!({})),
        });
        let error = fake
            .client()
            .deliver("w1:p1", "bell")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("pane object"), "{error}");
        assert_eq!(
            fake.methods(),
            vec!["pane.get"],
            "壊れた応答でも入力系 API を発行してはならない"
        );
    }

    /// workspace.list の label が pane に結び付き、宛先構文と衝突する label は
    /// `workspace_id` へ fallback する。workspace.list の失敗は pane 列挙を壊さない。
    #[tokio::test]
    async fn workspace_labels_join_panes_and_unusable_labels_fall_back() {
        let fake = FakeHerdr::start(|method, _| match method {
            "pane.list" => ok(&json!({
                "type": "pane_list",
                "panes": [
                    pane("w1:p1", "codex", "idle"),
                    pane("w2:p1", "claude", "idle"),
                    pane("w3:p1", "cursor", "idle"),
                    pane("w4:p1", "gemini", "idle"),
                    pane("w5:p1", "devin", "idle"),
                ],
            })),
            // 実機封筒 (2026-08-03 採取): result.workspaces[] に label が入る。
            "workspace.list" => ok(&json!({
                "type": "workspace_list",
                "workspaces": [
                    {"workspace_id": "w1", "number": 1, "label": "knowledge"},
                    {"workspace_id": "w2", "number": 2, "label": "has space"},
                    {"workspace_id": "w3", "number": 3, "label": "a/b"},
                    {"workspace_id": "w4", "number": 4, "label": ""},
                    {"workspace_id": "w5", "number": 5, "label": "label:colon"},
                ],
            })),
            "tab.list" => empty_tab_list(),
            _ => ok(&json!({})),
        });
        let panes = fake.client().panes().await.unwrap();
        assert_eq!(panes[0].workspace_label.as_deref(), Some("knowledge"));
        // 空白 / `/` / 空文字 / `:` の label は宛先・pane id 構文と衝突するので
        // 採用しない (workspace_id 表示へ fallback)。
        assert_eq!(panes[1].workspace_label, None);
        assert_eq!(panes[2].workspace_label, None);
        assert_eq!(panes[3].workspace_label, None);
        assert_eq!(panes[4].workspace_label, None);

        // workspace.list が失敗しても pane 列挙は劣化継続 (label なし)。
        let degraded = FakeHerdr::start(|method, _| match method {
            "pane.list" => ok(&json!({
                "type": "pane_list",
                "panes": [pane("w1:p1", "codex", "idle")],
            })),
            "workspace.list" => json!({
                "id": "agent-talkd",
                "error": {"code": "internal", "message": "boom"},
            }),
            "tab.list" => empty_tab_list(),
            _ => ok(&json!({})),
        });
        let panes = degraded.client().panes().await.unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].workspace_label, None);
    }

    /// `tab.list` の label が `tab_id` で pane に結び付く。宛先構文と衝突する
    /// label は捨てる (workspace label と同じ規則)。純数字 label はこの層では
    /// 保持し、名前決定 (`backend::pane_info`) が runtime 名へ落とす。
    /// wire 形式: params の `workspace_id` を省略して全 workspace を 1 回で引く。
    #[tokio::test]
    async fn tab_labels_join_panes_by_tab_id() {
        let tabbed_pane = |pane_id: &str, tab_id: &str| {
            json!({
                "pane_id": pane_id,
                "terminal_id": format!("term_{pane_id}"),
                "workspace_id": "w1",
                "tab_id": tab_id,
                "cwd": "/home/miyabi/projects",
                "agent": "claude",
                "agent_status": "idle",
            })
        };
        let fake = FakeHerdr::start(move |method, _| match method {
            "pane.list" => ok(&json!({
                "type": "pane_list",
                "panes": [
                    tabbed_pane("w1:p1", "w1:t1"),
                    tabbed_pane("w1:p2", "w1:t2"),
                    tabbed_pane("w1:p3", "w1:t3"),
                    tabbed_pane("w1:p4", "w1:t4"),
                ],
            })),
            // 実機封筒 (2026-08-14 採取): result.tabs[] に TabInfo が入り、
            // label は required。custom 名の無い tab は番号文字列が label になる。
            "tab.list" => ok(&json!({
                "type": "tab_list",
                "tabs": [
                    {"tab_id": "w1:t1", "workspace_id": "w1", "number": 1, "label": "fable", "focused": false, "pane_count": 1, "agent_status": "idle"},
                    {"tab_id": "w1:t2", "workspace_id": "w1", "number": 2, "label": "4", "focused": false, "pane_count": 1, "agent_status": "idle"},
                    {"tab_id": "w1:t3", "workspace_id": "w1", "number": 3, "label": "has space", "focused": false, "pane_count": 1, "agent_status": "idle"},
                ],
            })),
            _ => ok(&json!({})),
        });
        let panes = fake.client().panes().await.unwrap();
        assert_eq!(panes[0].tab_label.as_deref(), Some("fable"));
        assert_eq!(panes[1].tab_label.as_deref(), Some("4"));
        assert_eq!(panes[2].tab_label, None, "空白入り label は採用しない");
        assert_eq!(
            panes[3].tab_label, None,
            "tab.list に無い tab は label なし"
        );

        // wire 形式: workspace_id を送らず全 workspace を引く。
        let request = fake
            .requests
            .lock()
            .unwrap()
            .iter()
            .find(|request| request["method"] == "tab.list")
            .cloned()
            .expect("tab.list が発行される");
        assert!(request["params"].get("workspace_id").is_none(), "{request}");
    }

    /// `tab.list` の取得失敗は pane 列挙ごと失敗する (fail-closed)。
    /// workspace.list の劣化継続と違い、tab 名は identity の源なので
    /// label なしへ黙って劣化させると runtime 名への一時交代 (誤配) が起きる。
    #[tokio::test]
    async fn a_failed_tab_list_fails_the_pane_listing_instead_of_degrading() {
        let fake = FakeHerdr::start(|method, _| match method {
            "pane.list" => ok(&json!({
                "type": "pane_list",
                "panes": [pane("w1:p1", "claude", "idle")],
            })),
            "tab.list" => json!({
                "id": "agent-talkd",
                "error": {"code": "internal", "message": "boom"},
            }),
            _ => ok(&json!({})),
        });
        let error = fake.client().panes().await.unwrap_err().to_string();
        assert!(error.contains("tab.list"), "{error}");
    }

    /// `pane.process_info` の wire 形式 (2026-08-17 実機採取, herdr 0.7.5 /
    /// protocol 17)。params の key は `pane_id`、中身は
    /// `result.process_info.foreground_processes[]`。pane で動く agent 本体だけが
    /// 載り、その agent が spawn した子 agent は載らない。
    #[tokio::test]
    async fn foreground_processes_read_the_real_process_info_envelope() {
        let fake = FakeHerdr::start(|method, params| match method {
            "pane.process_info" => ok(&json!({
                "type": "pane_process_info",
                "process_info": {
                    "pane_id": params["pane_id"],
                    "shell_pid": 1_873_555,
                    "foreground_process_group_id": 1_873_555,
                    "foreground_processes": [
                        {"pid": 1_873_555, "name": "claude", "argv": ["claude"], "cmdline": "claude", "cwd": "/tmp"},
                        {"pid": 1_875_034, "name": "agent-talk-mcp", "argv": ["agent-talk-mcp"], "cmdline": "agent-talk-mcp", "cwd": "/tmp"},
                    ],
                },
            })),
            _ => ok(&json!({})),
        });

        let processes = fake.client().foreground_processes("w2G:p3").await.unwrap();

        assert_eq!(
            processes,
            vec![
                ForegroundProcess {
                    pid: 1_873_555,
                    name: "claude".into()
                },
                ForegroundProcess {
                    pid: 1_875_034,
                    name: "agent-talk-mcp".into()
                },
            ]
        );
        // wire 形式: params の key は `pane_id` ちょうど1つ。
        let request = fake.requests.lock().unwrap().last().cloned().unwrap();
        assert_eq!(request["method"], "pane.process_info");
        assert_eq!(request["params"]["pane_id"], "w2G:p3");
        assert!(request["params"].get("target").is_none(), "{request}");
    }

    /// 実 herdr は `pane_id` 以外の key で呼ぶと、**エラーにせず focus 中の
    /// pane を黙って返す**。別 pane の PID を宛先に化けさせないため、申告された
    /// `process_info.pane_id` が要求と違う応答は失敗にする。
    #[tokio::test]
    async fn a_process_info_answer_about_another_pane_is_refused() {
        let fake = FakeHerdr::start(|method, _| match method {
            "pane.process_info" => ok(&json!({
                "type": "pane_process_info",
                "process_info": {
                    "pane_id": "w2G:p3",
                    "shell_pid": 1,
                    "foreground_process_group_id": 1,
                    "foreground_processes": [{"pid": 1_873_555, "name": "claude"}],
                },
            })),
            _ => ok(&json!({})),
        });
        let error = fake
            .client()
            .foreground_processes("w1:p2")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("別の pane"), "{error}");

        // 封筒の欠けた応答も「process が居ない」へ劣化させず protocol error。
        let flat = FakeHerdr::start(|method, _| match method {
            "pane.process_info" => ok(&json!({"type": "pane_process_info"})),
            _ => ok(&json!({})),
        });
        let error = flat
            .client()
            .foreground_processes("w1:p2")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("process_info"), "{error}");
    }

    /// 封筒の `type` も照合する。`process_info` を持っていても、名乗りが
    /// `pane_process_info` でない応答は別 method の答えなので採用しない。
    #[tokio::test]
    async fn a_process_info_answer_with_a_foreign_type_is_refused() {
        let fake = FakeHerdr::start(|method, params| match method {
            "pane.process_info" => ok(&json!({
                "type": "pane",
                "process_info": {
                    "pane_id": params["pane_id"],
                    "foreground_processes": [{"pid": 1_873_555, "name": "claude"}],
                },
            })),
            _ => ok(&json!({})),
        });
        let error = fake
            .client()
            .foreground_processes("w1:p2")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("pane_process_info"), "{error}");
    }

    /// 壊れた要素を捨てると、**主 agent の要素だけ PID を落とした応答** が
    /// 「同名の子 agent が 1 つだけ居る pane」に化け、子の PID が一意の候補と
    /// して採用されてしまう。要素が 1 つでも壊れていたら応答全体を失敗させる。
    #[tokio::test]
    async fn one_malformed_process_element_fails_the_whole_answer() {
        for broken in [
            json!({"name": "claude"}),           // pid 欠損
            json!({"pid": 0, "name": "claude"}), // 正の PID ではない
            json!({"pid": -1, "name": "claude"}),
            json!({"pid": 4_294_967_296_i64, "name": "claude"}), // i32 に収まらない
            json!({"pid": 1_873_555.5, "name": "claude"}),       // 整数ではない
            json!({"pid": "1873555", "name": "claude"}),         // 数値ではない
            json!({"pid": 1_873_555}),                           // name 欠損
            json!({"pid": 1_873_555, "name": 17}),               // 文字列ではない
            json!("claude"),                                     // object ではない
        ] {
            let label = broken.to_string();
            let fake = FakeHerdr::start(move |method, params| match method {
                "pane.process_info" => ok(&json!({
                    "type": "pane_process_info",
                    "process_info": {
                        "pane_id": params["pane_id"],
                        "shell_pid": 1_873_555,
                        "foreground_process_group_id": 1_873_555,
                        "foreground_processes": [
                            broken.clone(),
                            // 主 agent が spawn した同名の子 agent。主 agent の
                            // 要素が捨てられると、これが唯一の候補になる。
                            {"pid": 1_875_291, "name": "claude"},
                        ],
                    },
                })),
                _ => ok(&json!({})),
            });
            let error = fake
                .client()
                .foreground_processes("w1:p2")
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("foreground_processes[0]"),
                "{label}: {error}"
            );
        }
    }

    /// herdr の error 応答を握り潰さない。
    #[tokio::test]
    async fn socket_errors_surface_instead_of_being_swallowed() {
        let fake = FakeHerdr::start(
            |_, _| json!({"id": "agent-talkd", "error": {"code": "not_found", "message": "no such pane"}}),
        );

        let error = fake.client().panes().await.unwrap_err().to_string();

        assert!(error.contains("not_found"), "{error}");
        assert!(error.contains("no such pane"), "{error}");
    }

    #[tokio::test]
    async fn protocol_is_reported_for_health_checks() {
        let fake = FakeHerdr::start(|_, _| ok(&json!({"type": "pong", "protocol": 17})));

        assert_eq!(fake.client().protocol().await.unwrap(), 17);
    }
}
