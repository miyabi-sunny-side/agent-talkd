use std::collections::{BTreeMap, HashMap};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    pub state: AgentState,
    pub queue: BTreeMap<u64, Message>,
}

#[derive(Debug, Default)]
pub struct BrokerState {
    pub agents: HashMap<String, Agent>,
    next_id: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Dispatch {
    Deliver(Message),
    Queued(u64),
}

impl BrokerState {
    pub fn register(&mut self, pane: String, name: String) -> Vec<Message> {
        let displaced = self
            .agents
            .remove(&pane)
            .map(|agent| agent.queue.into_values().collect())
            .unwrap_or_default();
        self.agents.insert(
            pane,
            Agent {
                name,
                state: AgentState::Idle,
                queue: BTreeMap::new(),
            },
        );
        displaced
    }

    pub fn remove(&mut self, pane: &str) -> Vec<Message> {
        self.agents
            .remove(pane)
            .map(|agent| agent.queue.into_values().collect())
            .unwrap_or_default()
    }

    pub fn set_state(&mut self, pane: &str, state: AgentState) {
        if let Some(agent) = self.agents.get_mut(pane) {
            agent.state = state;
        }
    }

    pub fn dispatch(
        &mut self,
        pane: &str,
        sender: String,
        brief: String,
        bell: String,
        expected_name: &str,
    ) -> Result<Dispatch, &'static str> {
        let id = self.next_id;
        self.next_id += 1;
        let agent = self.agents.get_mut(pane).ok_or("target exited")?;
        if agent.name != expected_name {
            return Err("target changed");
        }
        let message = Message {
            id,
            sender,
            brief,
            bell,
            target_name: expected_name.to_owned(),
        };
        if agent.state == AgentState::Busy {
            agent.queue.insert(id, message);
            Ok(Dispatch::Queued(id))
        } else {
            agent.state = AgentState::Busy;
            Ok(Dispatch::Deliver(message))
        }
    }

    pub fn turn_end(&mut self, pane: &str) -> Option<Message> {
        let agent = self.agents.get_mut(pane)?;
        agent.state = AgentState::Idle;
        let id = *agent.queue.keys().next()?;
        let message = agent.queue.remove(&id)?;
        agent.state = AgentState::Busy;
        Some(message)
    }

    pub fn restore_agent(&mut self, pane: String, name: String, state: AgentState) {
        self.agents.entry(pane).or_insert(Agent {
            name,
            state,
            queue: BTreeMap::new(),
        });
    }

    pub fn restore_message(&mut self, pane: String, message: Message) {
        self.next_id = self.next_id.max(message.id + 1);
        if let Some(agent) = self.agents.get_mut(&pane) {
            agent.queue.insert(message.id, message);
        }
    }

    pub fn remove_message(&mut self, pane: &str, id: u64) {
        if let Some(agent) = self.agents.get_mut(pane) {
            agent.queue.remove(&id);
        }
    }

    pub fn requeue_after_delivery_failure(&mut self, pane: &str, message: Message) {
        if let Some(agent) = self.agents.get_mut(pane) {
            agent.state = AgentState::Idle;
            agent.queue.insert(message.id, message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_messages_are_fifo_and_delivered_once() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        state.set_state("%1", AgentState::Busy);

        for body in ["one", "two"] {
            assert!(matches!(
                state.dispatch("%1", "%2".into(), body.into(), body.into(), "claude"),
                Ok(Dispatch::Queued(_))
            ));
        }

        assert_eq!(state.turn_end("%1").unwrap().brief, "one");
        assert_eq!(state.agents["%1"].state, AgentState::Busy);
        assert_eq!(state.turn_end("%1").unwrap().brief, "two");
        assert!(state.agents["%1"].queue.is_empty());
    }

    #[test]
    fn removing_agent_drains_every_message() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        state.set_state("%1", AgentState::Busy);
        for body in ["one", "two"] {
            state
                .dispatch("%1", "%2".into(), body.into(), body.into(), "claude")
                .unwrap();
        }
        assert_eq!(state.remove("%1").len(), 2);
        assert!(!state.agents.contains_key("%1"));
    }

    #[test]
    fn many_sends_have_unique_ids_and_no_duplicates() {
        let mut state = BrokerState::default();
        state.register("%1".into(), "claude".into());
        state.set_state("%1", AgentState::Busy);
        for index in 0..1000 {
            state
                .dispatch(
                    "%1",
                    "%2".into(),
                    index.to_string(),
                    "bell".into(),
                    "claude",
                )
                .unwrap();
        }
        let ids: Vec<_> = state.agents["%1"].queue.keys().copied().collect();
        assert_eq!(ids.len(), 1000);
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
