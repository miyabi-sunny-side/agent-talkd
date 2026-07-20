use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::state::{AgentState, BrokerState, Message};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    Register {
        pane: String,
        name: String,
        state: AgentState,
    },
    Remove {
        pane: String,
    },
    State {
        pane: String,
        state: AgentState,
    },
    Enqueue {
        pane: String,
        message: Message,
    },
    Complete {
        pane: String,
        id: u64,
    },
}

pub struct Journal {
    path: PathBuf,
    file: File,
    records: usize,
}

impl Journal {
    pub fn open(path: PathBuf) -> Result<(Self, BrokerState)> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let mut state = BrokerState::default();
        let mut records = 0;
        if path.exists() {
            let mut input = String::new();
            File::open(&path)
                .with_context(|| format!("cannot read {}", path.display()))?
                .read_to_string(&mut input)
                .context("cannot read journal")?;
            let complete_len = input.rfind('\n').map_or(0, |index| index + 1);
            for line in input[..complete_len].lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let record: Record =
                    serde_json::from_str(line).context("invalid journal record")?;
                replay(&mut state, record);
                records += 1;
            }
            if complete_len != input.len() {
                OpenOptions::new()
                    .write(true)
                    .open(&path)?
                    .set_len(complete_len as u64)?;
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("cannot append {}", path.display()))?;
        Ok((
            Self {
                path,
                file,
                records,
            },
            state,
        ))
    }

    pub fn append(&mut self, record: &Record) -> Result<()> {
        let original_len = self.file.metadata()?.len();
        let result = (|| -> Result<()> {
            serde_json::to_writer(&mut self.file, record)?;
            self.file.write_all(b"\n")?;
            self.file.sync_data().context("cannot sync journal")?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = self.file.set_len(original_len);
            let _ = self.file.sync_data();
            return Err(error);
        }
        self.records += 1;
        Ok(())
    }

    pub fn checkpoint_if_needed(&mut self, state: &BrokerState) -> Result<()> {
        if self.records < 256 {
            return Ok(());
        }
        let tmp = self.path.with_extension("journal.tmp");
        let mut output = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        let mut count = 0;
        for (pane, agent) in &state.agents {
            write_record(
                &mut output,
                &Record::Register {
                    pane: pane.clone(),
                    name: agent.name.clone(),
                    state: agent.state,
                },
            )?;
            count += 1;
            for message in agent.queue.values() {
                write_record(
                    &mut output,
                    &Record::Enqueue {
                        pane: pane.clone(),
                        message: message.clone(),
                    },
                )?;
                count += 1;
            }
        }
        output.sync_all()?;
        fs::rename(&tmp, &self.path)?;
        sync_parent(&self.path)?;
        self.file = OpenOptions::new().append(true).open(&self.path)?;
        self.records = count;
        Ok(())
    }
}

fn write_record(output: &mut File, record: &Record) -> Result<()> {
    serde_json::to_writer(&mut *output, record)?;
    output.write_all(b"\n")?;
    Ok(())
}

fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn replay(state: &mut BrokerState, record: Record) {
    match record {
        Record::Register {
            pane,
            name,
            state: agent_state,
        } => {
            state.remove(&pane);
            state.restore_agent(pane, name, agent_state);
        }
        Record::Remove { pane } => {
            state.remove(&pane);
        }
        Record::State { pane, state: next } => state.set_state(&pane, next),
        Record::Enqueue { pane, message } => state.restore_message(pane, message),
        Record::Complete { pane, id } => {
            if let Some(agent) = state.agents.get_mut(&pane) {
                agent.queue.remove(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::state::{AgentState, Dispatch, Message};

    #[test]
    fn replays_only_unfinished_messages() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.journal");
        {
            let (mut journal, _) = Journal::open(path.clone()).unwrap();
            journal
                .append(&Record::Register {
                    pane: "%1".into(),
                    name: "claude".into(),
                    state: AgentState::Busy,
                })
                .unwrap();
            journal
                .append(&Record::Enqueue {
                    pane: "%1".into(),
                    message: Message {
                        id: 7,
                        sender: "%2".into(),
                        brief: "brief".into(),
                        bell: "bell".into(),
                        target_name: "claude".into(),
                    },
                })
                .unwrap();
        }
        let (_, state) = Journal::open(path).unwrap();
        assert_eq!(state.agents["%1"].queue.len(), 1);
    }

    #[test]
    fn truncates_a_torn_final_record() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.journal");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"register\",\"pane\":\"%1\",\"name\":\"claude\",\"state\":\"busy\"}\n",
                "{\"type\":\"enqueue\""
            ),
        )
        .unwrap();

        let (_, state) = Journal::open(path.clone()).unwrap();
        assert!(state.agents.contains_key("%1"));
        assert!(fs::read_to_string(path).unwrap().ends_with('\n'));
    }

    #[test]
    fn checkpoint_preserves_active_queue() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.journal");
        let (mut journal, mut state) = Journal::open(path.clone()).unwrap();
        state.register("%1".into(), "claude".into());
        state.set_state("%1", AgentState::Busy);
        journal
            .append(&Record::Register {
                pane: "%1".into(),
                name: "claude".into(),
                state: AgentState::Busy,
            })
            .unwrap();
        for index in 0..2 {
            let Dispatch::Queued(id) = state
                .dispatch(
                    "%1",
                    "%2".into(),
                    index.to_string(),
                    "bell".into(),
                    "claude",
                )
                .unwrap()
            else {
                panic!("message was not queued");
            };
            journal
                .append(&Record::Enqueue {
                    pane: "%1".into(),
                    message: state.agents["%1"].queue[&id].clone(),
                })
                .unwrap();
        }
        for _ in 0..253 {
            journal
                .append(&Record::State {
                    pane: "%1".into(),
                    state: AgentState::Busy,
                })
                .unwrap();
        }

        journal.checkpoint_if_needed(&state).unwrap();
        drop(journal);
        let (_, replayed) = Journal::open(path).unwrap();
        assert_eq!(replayed.agents["%1"].queue.len(), 2);
        assert_eq!(replayed.agents["%1"].state, AgentState::Busy);
    }
}
