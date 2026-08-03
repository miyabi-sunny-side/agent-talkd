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

/// 送信時点で捕捉した送信者 identity。
///
/// **後からレジストリを引き直さない。** 送信者が退出・改名・pane ID 再利用されると
/// 現在のレジストリからは誤った名前が引けるため、送信時の名前を message へ永続化する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    /// 送信元 pane。外部 CLI / 内部通知では `human` / `system` などの label。
    pub pane: String,
    /// 送信時点の送信者名。
    pub name: String,
}

impl Origin {
    pub fn new(pane: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            pane: pane.into(),
            name: name.into(),
        }
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
    pub brief: String,
    pub bell: String,
    pub target_name: String,
}

impl Message {
    /// 表示用の送信者名。捕捉済みならそれを、旧 journal 由来なら生の sender を返す。
    pub fn sender_label(&self) -> &str {
        self.sender_name.as_deref().unwrap_or(&self.sender)
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
    /// 読了 (`read` / `read-message` が本文を返した)。受領催促の文言分岐にだけ使う。
    /// **memory のみ**で journal には残さない。restart 後は未読へ戻るが、
    /// 催促文言が保守側 (「未読なら読んでくれ」) に倒れるだけで害がない。
    pub read: bool,
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
    pub name: String,
    pub state: AgentState,
    pub queue: BTreeSet<u64>,
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
    pub fn register(&mut self, pane: String, name: String) {
        self.agents.insert(
            pane,
            Agent {
                name,
                state: AgentState::Idle,
                queue: BTreeSet::new(),
            },
        );
    }

    pub fn remove(&mut self, pane: &str) {
        self.agents.remove(pane);
    }

    pub fn set_state(&mut self, pane: &str, state: AgentState) {
        if let Some(agent) = self.agents.get_mut(pane) {
            agent.state = state;
        }
    }

    pub fn is_busy(&self, pane: &str) -> bool {
        self.agents
            .get(pane)
            .is_some_and(|agent| agent.state == AgentState::Busy)
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
            brief,
            bell: make_bell(id),
            target_name: expected_name.to_owned(),
        };
        // queue が残っている間は Idle でも直接配達しない。直接配達を許すと、
        // 配達失敗で requeue された古い message を新規 message が追い越す
        // (FIFO の破れ)。queue の先頭は turn-end と health tick の再配達が流す。
        let dispatch = if agent.state == AgentState::Busy || !agent.queue.is_empty() {
            agent.queue.insert(id);
            Dispatch::Queued(id)
        } else {
            agent.state = AgentState::Busy;
            Dispatch::Deliver(id)
        };
        self.messages.insert(
            id,
            StoredMessage {
                message,
                target_pane: pane.to_owned(),
                delivered: false,
                acked: false,
                read: false,
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

    pub fn turn_end(&mut self, pane: &str) -> Option<u64> {
        let agent = self.agents.get_mut(pane)?;
        agent.state = AgentState::Idle;
        let id = agent.queue.pop_first()?;
        agent.state = AgentState::Busy;
        Some(id)
    }

    pub fn message(&self, id: u64) -> Option<&StoredMessage> {
        self.messages.get(&id)
    }

    /// 返信先 pane。**捕捉時と同じ identity で今も登録中の pane にだけ**返す。
    /// 送信者が退出・改名した場合や pane ID が再利用された場合は `None`
    /// (新しい住人へ誤配送しないため)。
    pub fn reply_target(&self, message: &Message) -> Option<String> {
        let captured = message.sender_name.as_deref()?;
        if !message.sender.starts_with('%') {
            return None;
        }
        let agent = self.agents.get(&message.sender)?;
        (agent.name == captured).then(|| message.sender.clone())
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

    /// 読了を記録する (`read` / `read-message` が本文を返したとき)。
    pub fn mark_read(&mut self, id: u64) {
        if let Some(stored) = self.messages.get_mut(&id) {
            stored.read = true;
        }
    }

    /// 受領報告済み (`Acked`) にする。journal への追記が成功した後にだけ呼ぶこと。
    pub fn ack(&mut self, id: u64) {
        if let Some(stored) = self.messages.get_mut(&id) {
            stored.acked = true;
        }
    }

    /// 呼び出し元 pane 宛で **配達完了済み** かつ未受領の ID。本文は含めない。
    pub fn pending_to_me(&self, pane: &str) -> Vec<u64> {
        let current_name = self.agents.get(pane).map(|agent| agent.name.as_str());
        self.messages
            .values()
            .filter(|stored| {
                stored.target_pane == pane
                    && !stored.acked
                    && stored.delivered
                    && current_name == Some(stored.message.target_name.as_str())
            })
            .map(|stored| stored.message.id)
            .collect()
    }

    /// 呼び出し元 pane が送って未受領の ID を宛先 pane ごとに返す。**queue 中も含む。**
    pub fn pending_from_me(&self, pane: &str) -> BTreeMap<String, Vec<u64>> {
        let mut pending: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for stored in self.messages.values() {
            if stored.acked || stored.message.sender != pane {
                continue;
            }
            pending
                .entry(stored.target_pane.clone())
                .or_default()
                .push(stored.message.id);
        }
        pending
    }

    pub fn restore_agent(&mut self, pane: String, name: String, state: AgentState) {
        self.agents.entry(pane).or_insert(Agent {
            name,
            state,
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
                read: false,
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
            agent.state = AgentState::Idle;
            agent.queue.insert(id);
        }
    }

    /// pane 宛の **未受領 (`Pending`) 全件**。読了済みかどうかは問わない
    /// (docs/decisions/0002-message-retention-ack.md「pane 消滅時の掃除」)。
    pub fn messages_for_target(&self, pane: &str) -> Vec<Message> {
        self.messages
            .values()
            .filter(|stored| stored.target_pane == pane && !stored.acked)
            .map(|stored| stored.message.clone())
            .collect()
    }

    pub fn is_queued(&self, pane: &str, id: u64) -> bool {
        self.agents
            .get(pane)
            .is_some_and(|agent| agent.queue.contains(&id))
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
        Origin::new(pane, format!("agent{}", pane.trim_start_matches('%')))
    }

    #[test]
    fn busy_messages_are_fifo_and_delivered_once() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        state.set_state("%1", AgentState::Busy);

        for body in ["one", "two"] {
            assert!(matches!(
                state.dispatch("%1", from("%2"), body.into(), "claude", bell),
                Ok(Dispatch::Queued(_))
            ));
        }

        let first = state.turn_end("%1").unwrap();
        assert_eq!(state.message(first).unwrap().message.brief, "one");
        assert_eq!(state.agents["%1"].state, AgentState::Busy);
        let second = state.turn_end("%1").unwrap();
        assert_eq!(state.message(second).unwrap().message.brief, "two");
        assert!(state.agents["%1"].queue.is_empty());
    }

    #[test]
    fn removing_agent_keeps_unread_messages_for_failure_reporting() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        state.set_state("%1", AgentState::Busy);
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
    fn pending_to_me_hides_queued_ids_that_pending_from_me_still_shows() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        state.set_state("%1", AgentState::Busy);
        let Dispatch::Queued(queued) = state
            .dispatch("%1", from("%2"), "queued".into(), "claude", bell)
            .unwrap()
        else {
            panic!("message should be queued");
        };
        assert!(
            state.pending_to_me("%1").is_empty(),
            "配達完了前の ID は呼び鈴の前に読めてはならない"
        );
        assert_eq!(state.pending_from_me("%2")["%1"], vec![queued]);

        state.complete_delivery("%1", queued);
        assert_eq!(state.pending_to_me("%1"), vec![queued]);
    }

    #[test]
    fn pending_views_track_only_the_callers_own_ids() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        state.set_state("%1", AgentState::Busy);
        let mut mine = Vec::new();
        for (sender, body) in [("%2", "a"), ("%3", "b"), ("%2", "c")] {
            let Dispatch::Queued(id) = state
                .dispatch("%1", from(sender), body.into(), "claude", bell)
                .unwrap()
            else {
                panic!("message should be queued");
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
        state.register("%1".into(), "claude".into());
        state.register("%2".into(), "codex".into());
        let Dispatch::Deliver(id) = state
            .dispatch(
                "%1",
                Origin::new("%2", "codex"),
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
        assert_eq!(state.reply_target(&captured), Some("%2".into()));

        // 送信者が改名 (= pane ID 再利用) しても、捕捉した名前は変わらない。
        state.register("%2".into(), "gemini".into());
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
        state.register("%2".into(), "codex".into());
        assert_eq!(state.reply_target(&captured), Some("%2".into()));
        state.remove("%2");
        assert_eq!(state.reply_target(&captured), None);
    }

    #[test]
    fn a_legacy_message_without_a_captured_name_never_offers_a_reply_target() {
        let mut state = BrokerState::default();
        state.register("%2".into(), "codex".into());
        let legacy = Message {
            id: 1,
            sender: "%2".into(),
            sender_name: None,
            brief: "body".into(),
            bell: "read 1".into(),
            target_name: "claude".into(),
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
                    Origin::new(label, label),
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
            state.set_state("%1", AgentState::Idle);
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
        state.set_state("%1", AgentState::Busy);
        for index in 0..1000 {
            state
                .dispatch("%1", from("%2"), index.to_string(), "claude", bell)
                .unwrap();
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
