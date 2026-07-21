use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::state::{AgentState, BrokerState, ExternalMailboxEvent, Message};

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
    Consumed {
        id: u64,
    },
    Sequence {
        next_id: u64,
    },
    ExternalMailbox {
        event: ExternalMailboxEvent,
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

    pub fn checkpoint_if_needed(&mut self, state: &mut BrokerState) -> Result<()> {
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
        write_record(
            &mut output,
            &Record::Sequence {
                next_id: state.next_id(),
            },
        )?;
        count += 1;
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
        }
        for stored in state.messages.values() {
            if stored.consumed && !state.is_queued(&stored.target_pane, stored.message.id) {
                continue;
            }
            write_record(
                &mut output,
                &Record::Enqueue {
                    pane: stored.target_pane.clone(),
                    message: stored.message.clone(),
                },
            )?;
            count += 1;
            if stored.delivered {
                write_record(
                    &mut output,
                    &Record::Complete {
                        pane: stored.target_pane.clone(),
                        id: stored.message.id,
                    },
                )?;
                count += 1;
            }
            if stored.consumed {
                write_record(
                    &mut output,
                    &Record::Consumed {
                        id: stored.message.id,
                    },
                )?;
                count += 1;
            }
        }
        for events in state.mailboxes.values() {
            for event in events {
                write_record(
                    &mut output,
                    &Record::ExternalMailbox {
                        event: event.clone(),
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
        state.prune_consumed_not_queued();
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
        Record::Remove { pane } => state.remove(&pane),
        Record::State { pane, state: next } => state.set_state(&pane, next),
        Record::Enqueue { pane, mut message } => {
            migrate_legacy_brief(&mut message);
            state.restore_message(pane, message);
        }
        Record::Complete { pane, id } => state.restore_complete(&pane, id),
        Record::Consumed { id } => state.consume(id),
        Record::Sequence { next_id } => state.restore_next_id(next_id),
        Record::ExternalMailbox { event } => state.add_mailbox_event(event),
    }
}

fn migrate_legacy_brief(message: &mut Message) {
    let path = Path::new(&message.brief);
    if !message.brief.contains('\n') && path.extension().is_some_and(|extension| extension == "md")
    {
        message.brief = fs::read_to_string(path).unwrap_or_else(|error| {
            format!(
                "# agent-talk legacy依頼書\n\n旧依頼書 {} を読み込めませんでした: {error}\n",
                path.display()
            )
        });
        message.bell = format!(
            "[agent-talk] 依頼が届きました。agent-talk read {} で本文を確認して対応してください。",
            message.id
        );
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::state::{AgentState, Dispatch};

    fn message(id: u64, brief: &str) -> Message {
        Message {
            id,
            sender: "%2".into(),
            brief: brief.into(),
            bell: format!("read {id}"),
            target_name: "claude".into(),
        }
    }

    #[test]
    fn replays_unread_message_after_delivery() {
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
                    message: message(7, "brief"),
                })
                .unwrap();
            journal
                .append(&Record::Complete {
                    pane: "%1".into(),
                    id: 7,
                })
                .unwrap();
        }
        let (_, state) = Journal::open(path).unwrap();
        assert!(state.agents["%1"].queue.is_empty());
        assert_eq!(state.message(7).unwrap().message.brief, "brief");
        assert!(state.message(7).unwrap().delivered);
    }

    #[test]
    fn migrates_legacy_markdown_payload() {
        let dir = tempdir().unwrap();
        let brief_path = dir.path().join("legacy.md");
        fs::write(&brief_path, "# legacy body\n").unwrap();
        let journal_path = dir.path().join("queue.journal");
        {
            let (mut journal, _) = Journal::open(journal_path.clone()).unwrap();
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
                    message: message(7, &brief_path.to_string_lossy()),
                })
                .unwrap();
        }
        let (_, state) = Journal::open(journal_path).unwrap();
        assert_eq!(state.message(7).unwrap().message.brief, "# legacy body\n");
        assert!(state.message(7).unwrap().message.bell.contains("read 7"));
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
    fn checkpoint_preserves_unread_and_drops_consumed_delivered() {
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
        for body in ["keep", "drop"] {
            let Dispatch::Queued(id) = state
                .dispatch("%1", "%2".into(), body.into(), "claude", |id| {
                    format!("read {id}")
                })
                .unwrap()
            else {
                panic!("message was not queued");
            };
            journal
                .append(&Record::Enqueue {
                    pane: "%1".into(),
                    message: state.message(id).unwrap().message.clone(),
                })
                .unwrap();
            state.complete_delivery("%1", id);
            journal
                .append(&Record::Complete {
                    pane: "%1".into(),
                    id,
                })
                .unwrap();
            if body == "drop" {
                state.consume(id);
                journal.append(&Record::Consumed { id }).unwrap();
            }
        }
        for _ in 0..250 {
            journal
                .append(&Record::State {
                    pane: "%1".into(),
                    state: AgentState::Busy,
                })
                .unwrap();
        }

        journal.checkpoint_if_needed(&mut state).unwrap();
        drop(journal);
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("\"brief\":\"keep\""));
        assert!(!contents.contains("\"brief\":\"drop\""));
        let (_, replayed) = Journal::open(path).unwrap();
        assert_eq!(replayed.messages.len(), 1);
        assert_eq!(replayed.messages[&0].message.brief, "keep");
        assert_eq!(replayed.next_id(), 2);
    }

    #[test]
    fn checkpoint_keeps_consumed_queue_then_drops_failed_original() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.journal");
        let (mut journal, mut state) = Journal::open(path.clone()).unwrap();
        state.register("%1".into(), "claude".into());
        state.set_state("%1", AgentState::Busy);
        state.register("%2".into(), "codex".into());
        journal
            .append(&Record::Register {
                pane: "%1".into(),
                name: "claude".into(),
                state: AgentState::Busy,
            })
            .unwrap();
        journal
            .append(&Record::Register {
                pane: "%2".into(),
                name: "codex".into(),
                state: AgentState::Idle,
            })
            .unwrap();
        let Dispatch::Queued(id) = state
            .dispatch(
                "%1",
                "%2".into(),
                "failed original".into(),
                "claude",
                |id| format!("read {id}"),
            )
            .unwrap()
        else {
            panic!("message was not queued");
        };
        journal
            .append(&Record::Enqueue {
                pane: "%1".into(),
                message: state.message(id).unwrap().message.clone(),
            })
            .unwrap();
        journal.append(&Record::Consumed { id }).unwrap();
        state.consume(id);
        for _ in 0..252 {
            journal
                .append(&Record::State {
                    pane: "%1".into(),
                    state: AgentState::Busy,
                })
                .unwrap();
        }

        journal.checkpoint_if_needed(&mut state).unwrap();
        assert!(state.message(id).is_some(), "queued body must survive");
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("failed original")
        );

        let Dispatch::Deliver(failure_id) = state
            .dispatch(
                "%2",
                "system".into(),
                "failure notification".into(),
                "codex",
                |id| format!("read {id}"),
            )
            .unwrap()
        else {
            panic!("failure notification should be delivered");
        };
        journal
            .append(&Record::Enqueue {
                pane: "%2".into(),
                message: state.message(failure_id).unwrap().message.clone(),
            })
            .unwrap();
        journal
            .append(&Record::Complete {
                pane: "%2".into(),
                id: failure_id,
            })
            .unwrap();
        state.complete_delivery("%2", failure_id);
        journal
            .append(&Record::Remove { pane: "%1".into() })
            .unwrap();
        state.remove("%1");
        for _ in 0..248 {
            journal
                .append(&Record::State {
                    pane: "%1".into(),
                    state: AgentState::Idle,
                })
                .unwrap();
        }

        journal.checkpoint_if_needed(&mut state).unwrap();
        assert!(state.message(id).is_none());
        let contents = fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("failed original"));
        assert!(contents.contains("failure notification"));
        drop(journal);
        let (_, replayed) = Journal::open(path).unwrap();
        assert!(replayed.message(id).is_none());
        assert_eq!(
            replayed.message(failure_id).unwrap().message.brief,
            "failure notification"
        );
    }
}
