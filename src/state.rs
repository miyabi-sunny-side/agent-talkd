use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Busy,
    Idle,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Idle => "idle",
        }
    }
}

/// agent の identity。name (tab label 由来の宛先・表示名) と runtime
/// (herdr の検出名) の組。takeover は (name, runtime) のどちらの変化でも起きる
/// ため、生存判定 (reply / access / notification) は必ずこの組で比較する —
/// name だけの比較は、タブ名を保ったまま runtime が交代した pane の新しい
/// 住人を旧 identity と誤認する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity<'a> {
    pub name: &'a str,
    pub runtime: &'a str,
}

/// canonical full label (`<workspace>/<name>`) を組み立てる唯一の場所。
/// workspace を捕捉できていない送信者は bare 名のまま。呼び鈴 (送信時) と
/// `read_message` (読み出し時) が同じ規則で名乗るよう、両方ここを通す。
pub fn full_label(workspace: Option<&str>, name: &str) -> String {
    workspace.map_or_else(
        || name.to_owned(),
        |workspace| format!("{workspace}/{name}"),
    )
}

/// pane 由来でない送信者ラベル。registry の key は herdr 発行の pane ID なので
/// この2つとは衝突せず (`reply_target` が `None` になるのと同じ根拠)、送信側でも
/// 外部送信元ラベル (`--from`) がこの名前を騙るのを予約名として拒否する。
/// `--from` の表示ラベルは `Message::sender_name` 側に載り、`Message::sender` は
/// `human` のままなので、外部 mailbox 起点もここに含まれる。
pub const NON_PANE_SENDERS: [&str; 2] = ["human", "system"];

/// 送信元の種別。呼び鈴と brief を「連絡」と「依頼」に分ける唯一の軸。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenderKind {
    /// 登録済み agent pane からの送信。peer message は user 権限を運ばないので、
    /// 作業指示 (依頼) としては提示しない。
    Peer,
    /// 未登録の human caller と外部 mailbox (`--from`) からの送信。入口に居るのは
    /// user 本人なので「依頼」のまま提示する。
    Human,
}

impl SenderKind {
    /// 保存済み `Message::sender` から種別を復元する。**journal replay 用。**
    /// replay 時点には送信時の registry が無いので、送信時に永続化した送信元
    /// ラベルだけで判定する — 送信時の分岐 (登録済み pane なら `Peer`) と同じ
    /// 結果になるのは、登録済み pane の sender が pane ID で、未登録 caller と
    /// 外部 mailbox の sender が `NON_PANE_SENDERS` だからである。
    /// daemon 自身の通知 (`system`) も pane 由来ではないので `Peer` にはしない —
    /// peer 由来と読み替えてよいのは登録 agent pane からの送信だけ。
    #[must_use]
    pub fn from_sender(sender: &str) -> Self {
        if NON_PANE_SENDERS.contains(&sender) {
            Self::Human
        } else {
            Self::Peer
        }
    }

    /// brief の見出し。
    #[must_use]
    pub fn brief_title(self) -> &'static str {
        match self {
            Self::Peer => "# agent-talk 連絡",
            Self::Human => "# agent-talk 依頼書",
        }
    }
}

/// 送信時点で捕捉した送信者 identity。
///
/// **後からレジストリを引き直さない。** 送信者が退出・改名・pane ID 再利用されると
/// 現在のレジストリからは誤った名前が引けるため、送信時の identity を message へ
/// 永続化する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    /// 送信元 pane。外部 CLI / 内部通知では `human` / `system` などの label。
    pub pane: String,
    /// 送信時点の送信者名。
    pub name: String,
    /// 送信時点の送信者 runtime。pane 由来でない送信者 (`human` / `system` /
    /// 外部 source) は name をそのまま使う。
    pub runtime: String,
    /// 送信時点の送信者 workspace label。pane 由来でない送信者 (`human` /
    /// `system` / 外部 source) は `None`。登録と pane 占有者が食い違う場合は
    /// `Origin` を作らずに送信を拒否する。
    pub workspace: Option<String>,
}

impl Origin {
    pub fn new(
        pane: impl Into<String>,
        name: impl Into<String>,
        runtime: impl Into<String>,
    ) -> Self {
        Self {
            pane: pane.into(),
            name: name.into(),
            runtime: runtime.into(),
            workspace: None,
        }
    }

    /// 送信受理時点で捕捉した workspace label を載せる。
    #[must_use]
    pub fn with_workspace(mut self, workspace: Option<String>) -> Self {
        self.workspace = workspace;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: u64,
    pub sender: String,
    /// 送信時点で捕捉した送信者名。旧 journal には無いので `None` を許す
    /// (`None` は「捕捉されていない」であって「名前が無い」ではない)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_name: Option<String>,
    /// 送信時点で捕捉した送信者 runtime。runtime 無しの旧 record は
    /// name = runtime 検出名の時代なので、`None` は `sender_name` を runtime と
    /// して読む。旧 daemon は未知フィールドを黙って無視するため読み出し互換。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_runtime: Option<String>,
    /// 送信受理時点で捕捉した送信者の workspace label。旧 journal には無いので
    /// `None` を許す (`None` は「捕捉されていない」であって「workspace が無い」ではない)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_workspace: Option<String>,
    pub brief: String,
    pub bell: String,
    pub target_name: String,
    /// 送信時点で捕捉した宛先 runtime。`None` の互換規則は `sender_runtime` と同じ。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_runtime: Option<String>,
}

impl Message {
    /// 表示用の送信者名。捕捉済みならそれを、旧 journal 由来なら生の sender を返す。
    pub fn sender_label(&self) -> &str {
        self.sender_name.as_deref().unwrap_or(&self.sender)
    }

    /// 表示用の canonical full label (`<workspace>/<name>`)。workspace を捕捉
    /// できていない message (旧 journal 由来・`human` / `system` / 外部 source)
    /// は bare 名へ fallback する。**読み出し時に workspace を推測しない。**
    pub fn sender_full_label(&self) -> String {
        full_label(self.sender_workspace.as_deref(), self.sender_label())
    }

    /// 送信時点で捕捉した送信者 identity。旧 journal 由来 (未捕捉) は `None`。
    pub fn sender_identity(&self) -> Option<Identity<'_>> {
        let name = self.sender_name.as_deref()?;
        Some(Identity {
            name,
            runtime: self.sender_runtime.as_deref().unwrap_or(name),
        })
    }

    /// 送信時点で捕捉した宛先 identity。
    pub fn target_identity(&self) -> Identity<'_> {
        Identity {
            name: &self.target_name,
            runtime: self.target_runtime.as_deref().unwrap_or(&self.target_name),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub message: Message,
    pub target_pane: String,
    pub delivered: bool,
    /// 製品用語では `Acked`（受領報告済み = 削除対象）。
    /// journal の wire tag は旧 daemon 互換のため `Record::Consumed` のまま維持している
    /// (docs/decisions/0002-message-retention-ack.md「journal 形式と旧データの移行」)。
    /// `false` が `Pending`。
    pub acked: bool,
    /// 本文取得済みの受領印。journal の `Record::Seen` で復元する。
    /// pending / 催促の停止条件。本文は残す (`acked` とは別軸)。
    pub seen: bool,
    /// 配達完了時刻。受領催促のタイマー起点 (memory のみ。restart で今から数え直す)。
    pub delivered_at: Option<tokio::time::Instant>,
    /// 最後に受領催促を送った時刻 (memory のみ)。
    pub last_nag_at: Option<tokio::time::Instant>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MailboxDirection {
    Out,
    In,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExternalMailboxEvent {
    pub id: u64,
    pub created_at: i64,
    pub mailbox: String,
    pub source_label: String,
    pub direction: MailboxDirection,
    pub body: String,
    pub skill: Option<String>,
    pub target_name: String,
    pub target_pane: String,
    pub reply_to: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    /// 宛先・表示・journal の from/to に使う識別名 (tab label 由来)。
    pub name: String,
    /// herdr の runtime 検出名 (claude / codex / grok)。skill 記法と
    /// installed skill の解決に使う。tab 名の無い pane では `name` と同じ。
    pub runtime: String,
    pub queue: BTreeSet<u64>,
}

impl Agent {
    /// 新しいメッセージが直配ではなく queue へ入る条件。dispatch と送信側の
    /// 上限判定が同じ predicate を共有しないと、queue 行きの送信だけが
    /// 上限を素通りする。
    pub fn defers_delivery(&self) -> bool {
        !self.queue.is_empty()
    }

    /// 現在の登録 identity。捕捉済み identity との生存比較はこれを使う。
    pub fn identity(&self) -> Identity<'_> {
        Identity {
            name: &self.name,
            runtime: &self.runtime,
        }
    }
}

#[derive(Debug, Default)]
pub struct BrokerState {
    pub agents: HashMap<String, Agent>,
    pub messages: BTreeMap<u64, StoredMessage>,
    pub mailboxes: BTreeMap<String, Vec<ExternalMailboxEvent>>,
    next_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    Deliver(u64),
    Queued(u64),
}

impl BrokerState {
    /// 上書き登録 (test 専用)。production の登録は pull 同期由来の
    /// `restore_agent` だけが行う。
    #[cfg(test)]
    pub fn register(&mut self, pane: String, name: String) {
        self.agents.insert(
            pane,
            Agent {
                runtime: name.clone(),
                name,
                queue: BTreeSet::new(),
            },
        );
    }

    pub fn remove(&mut self, pane: &str) {
        self.agents.remove(pane);
    }

    pub fn queue_len(&self, pane: &str) -> usize {
        self.agents.get(pane).map_or(0, |agent| agent.queue.len())
    }

    pub fn dispatch<F>(
        &mut self,
        pane: &str,
        origin: Origin,
        brief: String,
        expected_name: &str,
        make_bell: F,
    ) -> Result<Dispatch, &'static str>
    where
        F: FnOnce(u64) -> String,
    {
        let agent = self.agents.get_mut(pane).ok_or("target exited")?;
        if agent.name != expected_name {
            return Err("target changed");
        }
        let id = self.next_id;
        self.next_id += 1;
        let message = Message {
            id,
            sender: origin.pane,
            sender_name: Some(origin.name),
            sender_runtime: Some(origin.runtime),
            sender_workspace: origin.workspace,
            brief,
            bell: make_bell(id),
            target_name: expected_name.to_owned(),
            target_runtime: Some(agent.runtime.clone()),
        };
        // queue が残っている間は直接配達しない。直接配達を許すと、
        // 配達失敗で requeue された古い message を新規 message が追い越す
        // (FIFO の破れ)。queue の先頭は health tick の再配達が流す。
        let dispatch = if agent.defers_delivery() {
            agent.queue.insert(id);
            Dispatch::Queued(id)
        } else {
            Dispatch::Deliver(id)
        };
        self.messages.insert(
            id,
            StoredMessage {
                message,
                target_pane: pane.to_owned(),
                delivered: false,
                acked: false,
                seen: false,
                delivered_at: None,
                last_nag_at: None,
            },
        );
        Ok(dispatch)
    }

    pub fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn set_brief(&mut self, id: u64, brief: String) {
        if let Some(stored) = self.messages.get_mut(&id) {
            stored.message.brief = brief;
        }
    }

    pub fn add_mailbox_event(&mut self, event: ExternalMailboxEvent) {
        self.next_id = self.next_id.max(event.id + 1);
        let events = self.mailboxes.entry(event.mailbox.clone()).or_default();
        events.push(event);
        if events.len() > 500 {
            let remove = events.len() - 500;
            events.drain(..remove);
        }
    }

    pub fn mailbox_events(
        &self,
        mailbox: &str,
        after: Option<u64>,
        limit: usize,
    ) -> Vec<ExternalMailboxEvent> {
        self.mailboxes
            .get(mailbox)
            .into_iter()
            .flat_map(|events| events.iter())
            .filter(|event| after.is_none_or(|id| event.id > id))
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn external_event(&self, id: u64) -> Option<&ExternalMailboxEvent> {
        self.mailboxes
            .values()
            .flat_map(|events| events.iter())
            .find(|event| event.id == id)
    }

    pub fn take_queued_head(&mut self, pane: &str) -> Option<u64> {
        let agent = self.agents.get_mut(pane)?;
        agent.queue.pop_first()
    }

    pub fn message(&self, id: u64) -> Option<&StoredMessage> {
        self.messages.get(&id)
    }

    /// 返信先 pane。**捕捉時と同じ identity で今も登録中の pane にだけ**返す。
    /// 送信者が退出・改名した場合や pane ID が再利用された場合は `None`
    /// (新しい住人へ誤配送しないため)。
    pub fn reply_target(&self, message: &Message) -> Option<String> {
        let captured = message.sender_identity()?;
        // 送信者が登録中の pane のときだけ返信先になる。`human` / `system` は
        // registry の key (herdr 発行の pane id) には現れないので自然に None になる。
        let agent = self.agents.get(&message.sender)?;
        (agent.identity() == captured).then(|| message.sender.clone())
    }

    pub fn complete_delivery(&mut self, pane: &str, id: u64) {
        if let Some(agent) = self.agents.get_mut(pane) {
            agent.queue.remove(&id);
        }
        if let Some(stored) = self.messages.get_mut(&id)
            && stored.target_pane == pane
        {
            stored.delivered = true;
            stored.delivered_at = Some(tokio::time::Instant::now());
        }
    }

    /// 受領印 (`Seen`)。journal への追記が成功した後にだけ呼ぶこと。
    pub fn mark_seen(&mut self, id: u64) {
        if let Some(stored) = self.messages.get_mut(&id) {
            stored.seen = true;
        }
    }

    /// 受領報告済み (`Acked`) にする。journal への追記が成功した後にだけ呼ぶこと。
    pub fn ack(&mut self, id: u64) {
        if let Some(stored) = self.messages.get_mut(&id) {
            stored.acked = true;
        }
    }

    /// 呼び出し元 pane 宛で未受領の ID（**queue 中 / 未配達も含む**）。本文は含めない。
    /// 所有者 pull の自己発見経路。push 呼び鈴を待たずに list → read できる。
    pub fn pending_to_me(&self, pane: &str) -> Vec<u64> {
        let current = self.agents.get(pane).map(Agent::identity);
        self.messages
            .values()
            .filter(|stored| {
                stored.target_pane == pane
                    && !stored.acked
                    && !stored.seen
                    && current == Some(stored.message.target_identity())
            })
            .map(|stored| stored.message.id)
            .collect()
    }

    /// 呼び出し元 pane が送って未受領の ID を宛先 pane ごとに返す。**queue 中も含む。**
    pub fn pending_from_me(&self, pane: &str) -> BTreeMap<String, Vec<u64>> {
        let mut pending: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for stored in self.messages.values() {
            if stored.acked || stored.seen || stored.message.sender != pane {
                continue;
            }
            pending
                .entry(stored.target_pane.clone())
                .or_default()
                .push(stored.message.id);
        }
        pending
    }

    pub fn restore_agent(
        &mut self,
        pane: String,
        name: String,
        runtime: String,
        _legacy_state: AgentState,
    ) {
        self.agents.entry(pane).or_insert(Agent {
            name,
            runtime,
            queue: BTreeSet::new(),
        });
    }

    pub fn restore_message(&mut self, pane: String, message: Message) {
        self.next_id = self.next_id.max(message.id + 1);
        if let Some(agent) = self.agents.get_mut(&pane) {
            agent.queue.insert(message.id);
        }
        self.messages.insert(
            message.id,
            StoredMessage {
                message,
                target_pane: pane,
                delivered: false,
                acked: false,
                seen: false,
                delivered_at: None,
                last_nag_at: None,
            },
        );
    }

    pub fn restore_complete(&mut self, pane: &str, id: u64) {
        self.complete_delivery(pane, id);
    }

    pub fn discard_message(&mut self, id: u64) {
        if let Some(stored) = self.messages.remove(&id)
            && let Some(agent) = self.agents.get_mut(&stored.target_pane)
        {
            agent.queue.remove(&id);
        }
    }

    pub fn requeue_after_delivery_failure(&mut self, pane: &str, id: u64) {
        if let Some(stored) = self.messages.get_mut(&id) {
            stored.delivered = false;
            stored.delivered_at = None;
        }
        if let Some(agent) = self.agents.get_mut(pane) {
            agent.queue.insert(id);
        }
    }

    /// pane 宛の **未受領 (`Pending`) 全件**。読了済みかどうかは問わない
    /// (docs/decisions/0002-message-retention-ack.md「pane 消滅時の掃除」)。
    pub fn messages_for_target(&self, pane: &str) -> Vec<Message> {
        self.messages
            .values()
            .filter(|stored| stored.target_pane == pane && !stored.acked && !stored.seen)
            .map(|stored| stored.message.clone())
            .collect()
    }

    pub fn is_queued(&self, pane: &str, id: u64) -> bool {
        self.agents
            .get(pane)
            .is_some_and(|agent| agent.queue.contains(&id))
    }

    /// 配送待ち queue の先頭 ID。空なら `None`。
    pub fn queued_head(&self, pane: &str) -> Option<u64> {
        self.agents
            .get(pane)
            .and_then(|agent| agent.queue.first().copied())
    }

    /// `Acked` は削除対象。ただし queue 中なら本文を durable に保持する
    /// (保持と可視性を同じ真偽値で判定しない)。
    pub fn prune_acked_not_queued(&mut self) {
        let agents = &self.agents;
        self.messages.retain(|id, stored| {
            !stored.acked
                || agents
                    .get(&stored.target_pane)
                    .is_some_and(|agent| agent.queue.contains(id))
        });
    }

    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    pub fn restore_next_id(&mut self, next_id: u64) {
        self.next_id = self.next_id.max(next_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bell(id: u64) -> String {
        format!("read {id}")
    }

    fn from(pane: &str) -> Origin {
        let name = format!("agent{}", pane.trim_start_matches('%'));
        Origin::new(pane, name.clone(), name)
    }

    /// 保存済み送信元ラベルからの種別復元。pane 由来 (herdr 発行の pane ID) だけが
    /// `Peer` で、`human` (未登録 caller と外部 mailbox) と `system` は `Human` 側。
    #[test]
    fn only_pane_derived_senders_replay_as_peers() {
        assert_eq!(SenderKind::from_sender("%2"), SenderKind::Peer);
        assert_eq!(SenderKind::from_sender("w1:p2"), SenderKind::Peer);
        assert_eq!(SenderKind::from_sender("human"), SenderKind::Human);
        assert_eq!(SenderKind::from_sender("system"), SenderKind::Human);
    }

    #[test]
    fn failed_delivery_queue_is_fifo_and_delivered_once() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        let Dispatch::Deliver(first) = state
            .dispatch("%1", from("%2"), "one".into(), "claude", bell)
            .unwrap()
        else {
            panic!("first message should attempt immediate delivery");
        };
        state.requeue_after_delivery_failure("%1", first);
        assert!(matches!(
            state.dispatch("%1", from("%2"), "two".into(), "claude", bell),
            Ok(Dispatch::Queued(_))
        ));

        let first = state.take_queued_head("%1").unwrap();
        assert_eq!(state.message(first).unwrap().message.brief, "one");
        let second = state.take_queued_head("%1").unwrap();
        assert_eq!(state.message(second).unwrap().message.brief, "two");
        assert!(state.agents["%1"].queue.is_empty());
    }

    #[test]
    fn removing_agent_keeps_unread_messages_for_failure_reporting() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        for body in ["one", "two"] {
            state
                .dispatch("%1", from("%2"), body.into(), "claude", bell)
                .unwrap();
        }
        state.remove("%1");
        assert_eq!(state.messages_for_target("%1").len(), 2);
        assert!(!state.agents.contains_key("%1"));
    }

    #[test]
    fn acked_message_is_terminal_and_leaves_the_pending_views() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        let Dispatch::Deliver(id) = state
            .dispatch("%1", from("%2"), "body".into(), "claude", bell)
            .unwrap()
        else {
            panic!("message should be delivered");
        };
        state.complete_delivery("%1", id);
        assert_eq!(state.pending_to_me("%1"), vec![id]);
        assert_eq!(state.pending_from_me("%2")["%1"], vec![id]);
        state.ack(id);
        assert_eq!(state.message(id).unwrap().message.brief, "body");
        assert!(state.message(id).unwrap().acked);
        assert!(state.pending_to_me("%1").is_empty());
        assert!(state.pending_from_me("%2").is_empty());
    }

    #[test]
    fn pending_to_me_includes_queued_ids_that_pending_from_me_also_shows() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        let Dispatch::Deliver(queued) = state
            .dispatch("%1", from("%2"), "queued".into(), "claude", bell)
            .unwrap()
        else {
            panic!("message should attempt delivery");
        };
        state.requeue_after_delivery_failure("%1", queued);
        assert_eq!(
            state.pending_to_me("%1"),
            vec![queued],
            "所有者 pull のため queue 中の自分宛 ID も見える"
        );
        assert_eq!(state.pending_from_me("%2")["%1"], vec![queued]);

        state.complete_delivery("%1", queued);
        assert_eq!(state.pending_to_me("%1"), vec![queued]);
    }

    #[test]
    fn pending_views_track_only_the_callers_own_ids() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        let mut mine = Vec::new();
        for (sender, body) in [("%2", "a"), ("%3", "b"), ("%2", "c")] {
            let id = match state
                .dispatch("%1", from(sender), body.into(), "claude", bell)
                .unwrap()
            {
                Dispatch::Deliver(id) | Dispatch::Queued(id) => id,
            };
            state.complete_delivery("%1", id);
            if sender == "%2" {
                mine.push(id);
            }
        }
        assert_eq!(state.pending_from_me("%2")["%1"], mine);
        assert_eq!(state.pending_from_me("%3")["%1"].len(), 1);
        assert_eq!(state.pending_to_me("%1").len(), 3);
    }

    #[test]
    fn sender_identity_is_captured_at_send_time_and_never_re_resolved() {
        let mut state = BrokerState::default();
        state.register("w1:p1".into(), "claude".into());
        state.register("w1:p2".into(), "codex".into());
        let Dispatch::Deliver(id) = state
            .dispatch(
                "w1:p1",
                Origin::new("w1:p2", "codex", "codex"),
                "body".into(),
                "claude",
                bell,
            )
            .unwrap()
        else {
            panic!("message should be delivered");
        };
        let captured = state.message(id).unwrap().message.clone();
        assert_eq!(captured.sender_label(), "codex");
        assert_eq!(state.reply_target(&captured), Some("w1:p2".into()));

        // 送信者が改名 (= pane ID 再利用) しても、捕捉した名前は変わらない。
        state.register("w1:p2".into(), "gemini".into());
        assert_eq!(
            state.message(id).unwrap().message.sender_label(),
            "codex",
            "現在のレジストリを引き直してはならない"
        );
        assert_eq!(
            state.reply_target(&captured),
            None,
            "新しい住人へ返信させてはならない"
        );

        // 送信者が退出した場合も返信先を出さない。
        state.register("w1:p2".into(), "codex".into());
        assert_eq!(state.reply_target(&captured), Some("w1:p2".into()));
        state.remove("w1:p2");
        assert_eq!(state.reply_target(&captured), None);
    }

    /// 生存判定は (name, runtime) の組で行う。タブ名を保ったまま runtime だけ
    /// 交代した pane は、送信側では `reply_target` を失い、宛先側では
    /// `pending_to_me` から旧宛 message が消える。
    #[test]
    fn identity_comparisons_require_both_the_name_and_the_runtime() {
        let mut state = BrokerState::default();
        state.restore_agent(
            "w1:p1".into(),
            "opus".into(),
            "claude".into(),
            AgentState::Idle,
        );
        state.restore_agent(
            "w1:p2".into(),
            "fable".into(),
            "claude".into(),
            AgentState::Idle,
        );
        let Dispatch::Deliver(id) = state
            .dispatch(
                "w1:p1",
                Origin::new("w1:p2", "fable", "claude"),
                "body".into(),
                "opus",
                bell,
            )
            .unwrap()
        else {
            panic!("message should be delivered");
        };
        state.complete_delivery("w1:p1", id);
        let captured = state.message(id).unwrap().message.clone();
        assert_eq!(state.reply_target(&captured), Some("w1:p2".into()));
        assert_eq!(state.pending_to_me("w1:p1"), vec![id]);

        // 送信側: タブ名 fable のまま runtime だけ codex へ交代。
        state.remove("w1:p2");
        state.restore_agent(
            "w1:p2".into(),
            "fable".into(),
            "codex".into(),
            AgentState::Idle,
        );
        assert_eq!(
            state.reply_target(&captured),
            None,
            "runtime だけ交代した pane の新しい住人へ返信させない"
        );

        // 宛先側: タブ名 opus のまま runtime 交代した新しい住人には見せない。
        state.remove("w1:p1");
        state.restore_agent(
            "w1:p1".into(),
            "opus".into(),
            "codex".into(),
            AgentState::Idle,
        );
        assert!(
            state.pending_to_me("w1:p1").is_empty(),
            "旧宛の message を新しい住人の pending に見せない"
        );
    }

    #[test]
    fn a_legacy_message_without_a_captured_name_never_offers_a_reply_target() {
        let mut state = BrokerState::default();
        state.register("%2".into(), "codex".into());
        let legacy = Message {
            id: 1,
            sender: "%2".into(),
            sender_name: None,
            sender_runtime: None,
            sender_workspace: None,
            brief: "body".into(),
            bell: "read 1".into(),
            target_name: "claude".into(),
            target_runtime: None,
        };
        assert_eq!(legacy.sender_label(), "%2");
        assert_eq!(state.reply_target(&legacy), None);
    }

    #[test]
    fn non_pane_senders_never_offer_a_reply_target() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        for label in ["human", "system"] {
            let Dispatch::Deliver(id) = state
                .dispatch(
                    "%1",
                    Origin::new(label, label, label),
                    "body".into(),
                    "claude",
                    bell,
                )
                .unwrap()
            else {
                panic!("message should be delivered");
            };
            let message = state.message(id).unwrap().message.clone();
            assert_eq!(message.sender_label(), label);
            assert_eq!(state.reply_target(&message), None);
            state.ack(id);
        }
    }

    #[test]
    fn pending_to_me_requires_the_registered_name_to_still_match() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        let Dispatch::Deliver(id) = state
            .dispatch("%1", from("%2"), "body".into(), "claude", bell)
            .unwrap()
        else {
            panic!("message should be delivered");
        };
        state.complete_delivery("%1", id);
        state.register("%1".into(), "codex".into());
        assert!(state.pending_to_me("%1").is_empty());
    }

    #[test]
    fn many_sends_have_unique_ids_and_no_duplicates() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        for index in 0..1000 {
            let dispatch = state
                .dispatch("%1", from("%2"), index.to_string(), "claude", bell)
                .unwrap();
            if let Dispatch::Deliver(id) = dispatch {
                state.requeue_after_delivery_failure("%1", id);
            }
        }
        let ids: Vec<_> = state.agents["%1"].queue.iter().copied().collect();
        assert_eq!(ids.len(), 1000);
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn mailbox_retention_keeps_latest_500_and_replays_ordered_events() {
        let mut state = BrokerState::default();
        for id in 0..501 {
            state.add_mailbox_event(ExternalMailboxEvent {
                id,
                created_at: id.cast_signed(),
                mailbox: "mobile".into(),
                source_label: "mobile".into(),
                direction: MailboxDirection::Out,
                body: format!("body-{id}"),
                skill: None,
                target_name: "claude".into(),
                target_pane: "%1".into(),
                reply_to: None,
            });
        }
        let events = state.mailbox_events("mobile", None, 500);
        assert_eq!(events.len(), 500);
        assert_eq!(events.first().unwrap().id, 1);
        assert_eq!(events.last().unwrap().id, 500);
        assert_eq!(state.mailbox_events("mobile", Some(499), 10)[0].id, 500);
    }
}
