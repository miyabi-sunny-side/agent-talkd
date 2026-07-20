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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: u64,
    pub sender: String,
    pub brief: String,
    pub bell: String,
    pub target_name: String,
}

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub message: Message,
    pub target_pane: String,
    pub delivered: bool,
    pub consumed: bool,
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
        sender: String,
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
            sender,
            brief,
            bell: make_bell(id),
            target_name: expected_name.to_owned(),
        };
        let dispatch = if agent.state == AgentState::Busy {
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
                consumed: false,
            },
        );
        Ok(dispatch)
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

    pub fn complete_delivery(&mut self, pane: &str, id: u64) {
        if let Some(agent) = self.agents.get_mut(pane) {
            agent.queue.remove(&id);
        }
        if let Some(stored) = self.messages.get_mut(&id)
            && stored.target_pane == pane
        {
            stored.delivered = true;
        }
    }

    pub fn consume(&mut self, id: u64) {
        if let Some(stored) = self.messages.get_mut(&id) {
            stored.consumed = true;
        }
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
                consumed: false,
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
        }
        if let Some(agent) = self.agents.get_mut(pane) {
            agent.state = AgentState::Idle;
            agent.queue.insert(id);
        }
    }

    pub fn messages_for_target(&self, pane: &str) -> Vec<Message> {
        self.messages
            .values()
            .filter(|stored| stored.target_pane == pane && !stored.consumed)
            .map(|stored| stored.message.clone())
            .collect()
    }

    pub fn is_queued(&self, pane: &str, id: u64) -> bool {
        self.agents
            .get(pane)
            .is_some_and(|agent| agent.queue.contains(&id))
    }

    pub fn prune_consumed_not_queued(&mut self) {
        let agents = &self.agents;
        self.messages.retain(|id, stored| {
            !stored.consumed
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

    #[test]
    fn busy_messages_are_fifo_and_delivered_once() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        state.set_state("%1", AgentState::Busy);

        for body in ["one", "two"] {
            assert!(matches!(
                state.dispatch("%1", "%2".into(), body.into(), "claude", bell),
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
                .dispatch("%1", "%2".into(), body.into(), "claude", bell)
                .unwrap();
        }
        state.remove("%1");
        assert_eq!(state.messages_for_target("%1").len(), 2);
        assert!(!state.agents.contains_key("%1"));
    }

    #[test]
    fn consumed_message_remains_readable_until_checkpoint() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        let Dispatch::Deliver(id) = state
            .dispatch("%1", "%2".into(), "body".into(), "claude", bell)
            .unwrap()
        else {
            panic!("message should be delivered");
        };
        state.complete_delivery("%1", id);
        state.consume(id);
        assert_eq!(state.message(id).unwrap().message.brief, "body");
        assert!(state.message(id).unwrap().consumed);
    }

    #[test]
    fn many_sends_have_unique_ids_and_no_duplicates() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        state.set_state("%1", AgentState::Busy);
        for index in 0..1000 {
            state
                .dispatch("%1", "%2".into(), index.to_string(), "claude", bell)
                .unwrap();
        }
        let ids: Vec<_> = state.agents["%1"].queue.iter().copied().collect();
        assert_eq!(ids.len(), 1000);
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
