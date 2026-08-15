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
        /// herdr の runtime 検出名。tab label が name になったとき、skill 記法の
        /// 解決に使う runtime を保つ。旧形式 journal (field 無し) は
        /// runtime = name として読む。旧 daemon は未知フィールドを黙って
        /// 無視するため読み出し互換も保たれる。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime: Option<String>,
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
        /// この enqueue と **同一 append** で terminal `Acked` にする message ID。
        ///
        /// 失敗通知を「ちょうど1通」にするために必要。通知の永続化と original の
        /// 退役を2回の append に分けると、その間のクラッシュで original が `Pending` の
        /// まま通知だけ残り、次の reconcile がもう1通作ってしまう
        /// (docs/decisions/0002-message-retention-ack.md「失敗通知は重複生成しない」)。
        ///
        /// 旧 journal には無いので `None` を既定にする。旧 daemon は未知フィールドを
        /// 黙って無視するため読み出し互換も保たれる。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retires: Option<u64>,
        /// `retires` に加えて、同じ append で terminal `Acked` にする ID 群。
        ///
        /// 未受領通知を送信元ごとに1通へ集約するとき、回収した original 全件の
        /// 退役を通知の永続化と同一 append に保つために必要 (atomicity の理由は
        /// `retires` と同じ)。`retires` を list へ型変更すると旧 daemon が新 journal を
        /// 読めなくなるため、未知フィールドとして無視される別 field にしている。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        also_retires: Vec<u64>,
    },
    Complete {
        pane: String,
        id: u64,
    },
    /// 製品用語では `Acked`（受領報告済み = 削除対象）。
    /// 旧 daemon が読めなくなるため wire tag は `consumed` のまま維持し、意味だけを
    /// 読み替えている (docs/decisions/0002-message-retention-ack.md)。
    /// 旧 journal の `Consumed`（読了 = 次の圧縮で削除）は新意味と一致するので移行処理は不要。
    Consumed {
        id: u64,
    },
    /// 本文取得済みの受領印。本文は残し、pending / 催促だけ止める。
    /// 旧 `consumed` とは別 tag。旧 daemon は新 journal を読めない。
    Seen {
        id: u64,
    },
    /// checkpoint の **末尾** に書かれる境界マーカー。`Journal::open` はこれを見て
    /// 「前回 checkpoint 以降の追記数」を 0 に戻す。
    Sequence {
        next_id: u64,
    },
    ExternalMailbox {
        event: ExternalMailboxEvent,
    },
}

const CHECKPOINT_APPENDS: usize = 256;

pub struct Journal {
    path: PathBuf,
    file: File,
    /// 前回 checkpoint 以降に追記したレコード数。総レコード数ではない
    /// (docs/decisions/0002-message-retention-ack.md「checkpoint 発火条件」)。
    appended: usize,
    /// 次の N 回の `append` を失敗させる test 専用 failpoint。
    /// durability 契約 (append + fsync が成功する前に可視性を進めない) を
    /// 実際に証明するために必要。production build には存在しない。
    #[cfg(test)]
    fail_appends: usize,
    /// N 回成功させたあと、以降の `append` をすべて失敗させる test 専用 failpoint。
    /// 「durable になった直後の境界」を狙うために使う。
    #[cfg(test)]
    fail_after: Option<usize>,
}

impl Journal {
    pub fn open(path: PathBuf) -> Result<(Self, BrokerState)> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let mut state = BrokerState::default();
        let mut appended = 0;
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
                // `Sequence` は checkpoint 境界。旧形式では先頭または不在なので、
                // 初回だけ保守的に多く数えて1回圧縮し、以後は正確になる。
                let boundary = matches!(record, Record::Sequence { .. });
                replay(&mut state, record);
                if boundary {
                    appended = 0;
                } else {
                    appended += 1;
                }
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
                appended,
                #[cfg(test)]
                fail_appends: 0,
                #[cfg(test)]
                fail_after: None,
            },
            state,
        ))
    }

    /// 次の `count` 回の `append` を失敗させる (test 専用)。
    #[cfg(test)]
    pub fn fail_next_appends(&mut self, count: usize) {
        self.fail_appends = count;
    }

    /// `allowed` 回成功させたあと、以降の `append` をすべて失敗させる (test 専用)。
    #[cfg(test)]
    pub fn fail_appends_after(&mut self, allowed: usize) {
        self.fail_after = Some(allowed);
    }

    #[cfg(test)]
    pub fn clear_failpoints(&mut self) {
        self.fail_appends = 0;
        self.fail_after = None;
    }

    pub fn append(&mut self, record: &Record) -> Result<()> {
        // ファイルに触れる前に失敗させる。追記数も進めない。
        #[cfg(test)]
        {
            if self.fail_appends > 0 {
                self.fail_appends -= 1;
                anyhow::bail!("injected journal append failure");
            }
            match self.fail_after.as_mut() {
                Some(0) => anyhow::bail!("injected journal append failure"),
                Some(remaining) => *remaining -= 1,
                None => {}
            }
        }
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
        self.appended += 1;
        Ok(())
    }

    pub fn checkpoint_if_needed(&mut self, state: &mut BrokerState) -> Result<()> {
        if self.appended < CHECKPOINT_APPENDS {
            return Ok(());
        }
        let tmp = self.path.with_extension("journal.tmp");
        let mut output = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        for (pane, agent) in &state.agents {
            write_record(
                &mut output,
                &Record::Register {
                    pane: pane.clone(),
                    name: agent.name.clone(),
                    runtime: Some(agent.runtime.clone()),
                    // 旧 daemon が checkpoint を読めるよう field は残す。
                    // live state の正は herdr であり、この値は復元に使わない。
                    state: AgentState::Idle,
                },
            )?;
        }
        for stored in state.messages.values() {
            // `Acked` は削除対象。ただし queue 中なら本文を durable に保持する。
            if stored.acked && !state.is_queued(&stored.target_pane, stored.message.id) {
                continue;
            }
            write_record(
                &mut output,
                &Record::Enqueue {
                    pane: stored.target_pane.clone(),
                    message: stored.message.clone(),
                    // retire 関係は replay 済み。snapshot では独立した Consumed で表す。
                    retires: None,
                    also_retires: Vec::new(),
                },
            )?;
            if stored.delivered {
                write_record(
                    &mut output,
                    &Record::Complete {
                        pane: stored.target_pane.clone(),
                        id: stored.message.id,
                    },
                )?;
            }
            if stored.acked {
                write_record(
                    &mut output,
                    &Record::Consumed {
                        id: stored.message.id,
                    },
                )?;
            }
            if stored.seen {
                write_record(
                    &mut output,
                    &Record::Seen {
                        id: stored.message.id,
                    },
                )?;
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
            }
        }
        // `Sequence` は末尾に書く。次回 open 時の追記数カウンタをここで 0 に戻すため。
        write_record(
            &mut output,
            &Record::Sequence {
                next_id: state.next_id(),
            },
        )?;
        output.sync_all()?;
        fs::rename(&tmp, &self.path)?;
        sync_parent(&self.path)?;
        self.file = OpenOptions::new().append(true).open(&self.path)?;
        // 成功時のみリセットする。失敗時は追記数を失わない。
        self.appended = 0;
        state.prune_acked_not_queued();
        Ok(())
    }

    #[cfg(test)]
    pub fn appended_since_checkpoint(&self) -> usize {
        self.appended
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
            runtime,
            state: agent_state,
        } => {
            state.remove(&pane);
            // 旧形式 (runtime 無し) は name = runtime 検出名の時代なので同一視する。
            let runtime = runtime.unwrap_or_else(|| name.clone());
            state.restore_agent(pane, name, runtime, agent_state);
        }
        Record::Remove { pane } => state.remove(&pane),
        // 旧 journal の hook 由来 state は読み込み互換のため parse するが、
        // live state の正は herdr なので replay しない。
        Record::State { .. } => {}
        Record::Enqueue {
            pane,
            mut message,
            retires,
            also_retires,
        } => {
            migrate_legacy_brief(&mut message);
            state.restore_message(pane, message);
            // 1レコードなので、通知の復元と originals の退役は必ず同時に起きる。
            for retired in retires.into_iter().chain(also_retires) {
                state.ack(retired);
            }
        }
        Record::Complete { pane, id } => state.restore_complete(&pane, id),
        // 旧 journal の `Consumed`（読了）も新意味の `Acked`（受領済み = 即削除対象）
        // として replay される。
        Record::Consumed { id } => state.ack(id),
        Record::Seen { id } => state.mark_seen(id),
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
            "[agent-talk] 依頼が届きました。read_message {} で本文を確認してください。",
            message.id
        );
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::state::{AgentState, Dispatch, Origin};

    fn message(id: u64, brief: &str) -> Message {
        Message {
            id,
            sender: "%2".into(),
            sender_name: Some("codex".into()),
            sender_runtime: None,
            sender_workspace: None,
            brief: brief.into(),
            bell: format!("read {id}"),
            target_name: "claude".into(),
            target_runtime: None,
        }
    }

    fn enqueue(state: &mut BrokerState, pane: &str, body: &str) -> u64 {
        let dispatch = state
            .dispatch(
                pane,
                Origin::new("%2", "codex", "codex"),
                body.into(),
                "claude",
                |id| format!("read {id}"),
            )
            .unwrap();
        let id = match dispatch {
            Dispatch::Deliver(id) | Dispatch::Queued(id) => id,
        };
        if matches!(dispatch, Dispatch::Deliver(_)) {
            state.requeue_after_delivery_failure(pane, id);
        }
        id
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
                    runtime: None,
                    state: AgentState::Busy,
                })
                .unwrap();
            journal
                .append(&Record::Enqueue {
                    pane: "%1".into(),
                    message: message(7, "brief"),
                    retires: None,
                    also_retires: Vec::new(),
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

    /// 旧形式 journal (runtime フィールド無しの Register) は runtime = name と
    /// して読める。tab 名導入前の journal は name が runtime 検出名そのもの。
    #[test]
    fn a_legacy_register_without_runtime_replays_runtime_as_name() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.journal");
        fs::write(
            &path,
            "{\"type\":\"register\",\"pane\":\"%1\",\"name\":\"claude\",\"state\":\"idle\"}\n",
        )
        .unwrap();
        let (_, state) = Journal::open(path).unwrap();
        assert_eq!(state.agents["%1"].name, "claude");
        assert_eq!(state.agents["%1"].runtime, "claude");
    }

    /// 新形式の Register は runtime を保ち、checkpoint 後も失わない。
    #[test]
    fn a_register_with_runtime_survives_replay_and_checkpoint() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.journal");
        {
            let (mut journal, _) = Journal::open(path.clone()).unwrap();
            journal
                .append(&Record::Register {
                    pane: "%1".into(),
                    name: "fable".into(),
                    runtime: Some("claude".into()),
                    state: AgentState::Idle,
                })
                .unwrap();
        }
        let (mut journal, mut state) = Journal::open(path.clone()).unwrap();
        assert_eq!(state.agents["%1"].name, "fable");
        assert_eq!(state.agents["%1"].runtime, "claude");
        for _ in 0..256 {
            journal
                .append(&Record::State {
                    pane: "%1".into(),
                    state: AgentState::Busy,
                })
                .unwrap();
        }
        journal.checkpoint_if_needed(&mut state).unwrap();
        drop(journal);
        let (_, replayed) = Journal::open(path).unwrap();
        assert_eq!(replayed.agents["%1"].name, "fable");
        assert_eq!(
            replayed.agents["%1"].runtime, "claude",
            "checkpoint の再書き出しでも runtime を失わない"
        );
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
                    runtime: None,
                    state: AgentState::Busy,
                })
                .unwrap();
            journal
                .append(&Record::Enqueue {
                    pane: "%1".into(),
                    message: message(7, &brief_path.to_string_lossy()),
                    retires: None,
                    also_retires: Vec::new(),
                })
                .unwrap();
        }
        let (_, state) = Journal::open(journal_path).unwrap();
        assert_eq!(state.message(7).unwrap().message.brief, "# legacy body\n");
        let bell = &state.message(7).unwrap().message.bell;
        assert!(
            bell.contains("read_message 7"),
            "旧 journal からの復元も MCP 文言の呼び鈴になる: {bell}"
        );
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
    fn checkpoint_preserves_pending_and_drops_acked_delivered() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.journal");
        let (mut journal, mut state) = Journal::open(path.clone()).unwrap();
        state.register("%1".into(), "claude".into());
        journal
            .append(&Record::Register {
                pane: "%1".into(),
                name: "claude".into(),
                runtime: None,
                state: AgentState::Busy,
            })
            .unwrap();
        for body in ["keep", "drop"] {
            let id = enqueue(&mut state, "%1", body);
            journal
                .append(&Record::Enqueue {
                    pane: "%1".into(),
                    message: state.message(id).unwrap().message.clone(),
                    retires: None,
                    also_retires: Vec::new(),
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
                state.ack(id);
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
    #[allow(clippy::too_many_lines)]
    fn checkpoint_keeps_acked_queue_then_drops_failed_original() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.journal");
        let (mut journal, mut state) = Journal::open(path.clone()).unwrap();
        state.register("%1".into(), "claude".into());
        state.register("%2".into(), "codex".into());
        journal
            .append(&Record::Register {
                pane: "%1".into(),
                name: "claude".into(),
                runtime: None,
                state: AgentState::Busy,
            })
            .unwrap();
        journal
            .append(&Record::Register {
                pane: "%2".into(),
                name: "codex".into(),
                runtime: None,
                state: AgentState::Idle,
            })
            .unwrap();
        let id = enqueue(&mut state, "%1", "failed original");
        journal
            .append(&Record::Enqueue {
                pane: "%1".into(),
                message: state.message(id).unwrap().message.clone(),
                retires: None,
                also_retires: Vec::new(),
            })
            .unwrap();
        journal.append(&Record::Consumed { id }).unwrap();
        state.ack(id);
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
                Origin::new("system", "system", "system"),
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
                retires: None,
                also_retires: Vec::new(),
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
        // 発火条件が「前回 checkpoint 以降の追記数」へ変わったため、2回目の checkpoint に
        // 必要な追記数を 248 → 253 に更新した (Enqueue/Complete/Remove の3件と合わせて256)。
        for _ in 0..253 {
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

    fn noise(journal: &mut Journal, count: usize) {
        for _ in 0..count {
            journal
                .append(&Record::State {
                    pane: "%1".into(),
                    state: AgentState::Busy,
                })
                .unwrap();
        }
    }

    #[test]
    fn checkpoint_writes_the_sequence_marker_last_without_moving_next_id_backwards() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.journal");
        let (mut journal, mut state) = Journal::open(path.clone()).unwrap();
        state.register("%1".into(), "claude".into());
        let id = enqueue(&mut state, "%1", "kept");
        journal
            .append(&Record::Enqueue {
                pane: "%1".into(),
                message: state.message(id).unwrap().message.clone(),
                retires: None,
                also_retires: Vec::new(),
            })
            .unwrap();
        noise(&mut journal, 255);
        journal.checkpoint_if_needed(&mut state).unwrap();
        drop(journal);

        let contents = fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = contents.lines().collect();
        assert!(
            lines.last().unwrap().contains("\"type\":\"sequence\""),
            "sequence marker must be the last record: {contents}"
        );
        assert_eq!(
            lines.iter().filter(|l| l.contains("\"sequence\"")).count(),
            1
        );
        let (journal, replayed) = Journal::open(path).unwrap();
        assert_eq!(journal.appended_since_checkpoint(), 0);
        assert_eq!(replayed.next_id(), state.next_id());
        assert_eq!(replayed.message(id).unwrap().message.brief, "kept");
    }

    #[test]
    fn checkpoint_fires_on_appends_since_checkpoint_across_restarts() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.journal");
        let (mut journal, mut state) = Journal::open(path.clone()).unwrap();
        state.register("%1".into(), "claude".into());
        noise(&mut journal, 256);
        journal.checkpoint_if_needed(&mut state).unwrap();
        assert_eq!(journal.appended_since_checkpoint(), 0);
        drop(journal);

        // checkpoint 直後の再起動では、リクエストだけでは checkpoint が走らない。
        let (mut journal, mut state) = Journal::open(path.clone()).unwrap();
        assert_eq!(journal.appended_since_checkpoint(), 0);
        let before = fs::metadata(&path).unwrap().len();
        journal.checkpoint_if_needed(&mut state).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), before);

        // checkpoint -> 255追記 -> 再起動 -> 1追記 で checkpoint が走る。
        noise(&mut journal, 255);
        journal.checkpoint_if_needed(&mut state).unwrap();
        assert_eq!(journal.appended_since_checkpoint(), 255);
        drop(journal);

        let (mut journal, mut state) = Journal::open(path.clone()).unwrap();
        assert_eq!(journal.appended_since_checkpoint(), 255);
        noise(&mut journal, 1);
        journal.checkpoint_if_needed(&mut state).unwrap();
        assert_eq!(journal.appended_since_checkpoint(), 0);
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 2);
    }

    #[test]
    fn a_large_snapshot_does_not_checkpoint_on_every_request() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.journal");
        let (mut journal, mut state) = Journal::open(path.clone()).unwrap();
        state.register("%1".into(), "claude".into());
        for index in 0..200 {
            let id = enqueue(&mut state, "%1", &index.to_string());
            journal
                .append(&Record::Enqueue {
                    pane: "%1".into(),
                    message: state.message(id).unwrap().message.clone(),
                    retires: None,
                    also_retires: Vec::new(),
                })
                .unwrap();
            journal
                .append(&Record::Complete {
                    pane: "%1".into(),
                    id,
                })
                .unwrap();
            state.complete_delivery("%1", id);
        }
        journal.checkpoint_if_needed(&mut state).unwrap();
        let after_first = fs::read_to_string(&path).unwrap();
        assert!(after_first.lines().count() > 256, "snapshot stays large");
        assert_eq!(journal.appended_since_checkpoint(), 0);

        // 保持中のメッセージが多くても、追記がなければ再圧縮は走らない。
        for _ in 0..10 {
            journal.checkpoint_if_needed(&mut state).unwrap();
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), after_first);
    }

    #[test]
    fn legacy_journal_without_a_trailing_sequence_compacts_once_then_stabilizes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.journal");
        // 旧形式: Sequence が先頭、Consumed（旧意味 = 読了）が存在する。
        let mut legacy = String::from("{\"type\":\"sequence\",\"next_id\":9}\n");
        legacy.push_str(
            "{\"type\":\"register\",\"pane\":\"%1\",\"name\":\"claude\",\"state\":\"idle\"}\n",
        );
        legacy.push_str(
            "{\"type\":\"enqueue\",\"pane\":\"%1\",\"message\":{\"id\":9,\"sender\":\"%2\",\"brief\":\"legacy acked\",\"bell\":\"read 9\",\"target_name\":\"claude\"}}\n",
        );
        legacy.push_str("{\"type\":\"complete\",\"pane\":\"%1\",\"id\":9}\n");
        legacy.push_str("{\"type\":\"consumed\",\"id\":9}\n");
        legacy.push_str(
            "{\"type\":\"enqueue\",\"pane\":\"%1\",\"message\":{\"id\":10,\"sender\":\"%2\",\"brief\":\"legacy pending\",\"bell\":\"read 10\",\"target_name\":\"claude\"}}\n",
        );
        legacy.push_str("{\"type\":\"complete\",\"pane\":\"%1\",\"id\":10}\n");
        for _ in 0..251 {
            legacy.push_str("{\"type\":\"state\",\"pane\":\"%1\",\"state\":\"idle\"}\n");
        }
        fs::write(&path, &legacy).unwrap();

        let (mut journal, mut state) = Journal::open(path.clone()).unwrap();
        // 旧 Consumed は新意味の Acked（即削除対象）として replay される。
        assert!(state.message(9).unwrap().acked);
        assert!(!state.message(10).unwrap().acked);
        assert_eq!(journal.appended_since_checkpoint(), 257);
        assert_eq!(state.next_id(), 11);

        journal.checkpoint_if_needed(&mut state).unwrap();
        assert!(state.message(9).is_none(), "旧 Consumed は即削除対象");
        assert_eq!(state.message(10).unwrap().message.brief, "legacy pending");
        let compacted = fs::read_to_string(&path).unwrap();
        assert!(!compacted.contains("legacy acked"));
        assert!(compacted.contains("legacy pending"));
        assert!(
            compacted
                .lines()
                .last()
                .unwrap()
                .contains("\"type\":\"sequence\"")
        );

        drop(journal);
        let (mut journal, mut state) = Journal::open(path.clone()).unwrap();
        assert_eq!(journal.appended_since_checkpoint(), 0);
        assert_eq!(state.next_id(), 11);
        let stable = fs::read_to_string(&path).unwrap();
        journal.checkpoint_if_needed(&mut state).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), stable);
    }

    #[test]
    fn a_failed_checkpoint_keeps_the_append_count() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.journal");
        let (mut journal, mut state) = Journal::open(path.clone()).unwrap();
        state.register("%1".into(), "claude".into());
        noise(&mut journal, 256);
        // tmp path をディレクトリで塞いで checkpoint を失敗させる。
        fs::create_dir(path.with_extension("journal.tmp")).unwrap();
        assert!(journal.checkpoint_if_needed(&mut state).is_err());
        assert_eq!(journal.appended_since_checkpoint(), 256);

        fs::remove_dir(path.with_extension("journal.tmp")).unwrap();
        journal.checkpoint_if_needed(&mut state).unwrap();
        assert_eq!(journal.appended_since_checkpoint(), 0);
    }
}
