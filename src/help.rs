pub struct CommandHelp {
    pub command: &'static str,
    pub text: &'static str,
}

pub const GLOBAL: &str = r"agent-talk: tmux 上の対話エージェント同士の連絡係。

  agent-talk --version
  agent-talk <command> --help
  agent-talk update
  agent-talk ensure-daemon
  agent-talk daemon-status
  agent-talk run <name> <executable> [args...]
  agent-talk reply <original-id> [body]
  agent-talk mailbox-list <mailbox> [--after <id>] [--limit <n>]
  agent-talk register <name>
  agent-talk unregister
  agent-talk busy | idle
  agent-talk turn-end
  agent-talk who
  agent-talk gc
  agent-talk resolve <addr>
  agent-talk send <addr> [--from <source>] [--skill <name>] [--] [message]
  agent-talk send <addr> [--no-reply] [--] [message]
  agent-talk read <id>
  agent-talk ack-message <id>
";

pub const COMMANDS: &[CommandHelp] = &[
    CommandHelp {
        command: "update",
        text: "usage: agent-talk update\n\n公開GitHub Releaseから安全に更新します。",
    },
    CommandHelp {
        command: "ensure-daemon",
        text: "usage: agent-talk ensure-daemon\n\n対象tmux serverのdaemonを現在のbinaryへ合わせます。",
    },
    CommandHelp {
        command: "daemon-status",
        text: "usage: agent-talk daemon-status\n\n対象daemonのversionとready状態を表示します。",
    },
    CommandHelp {
        command: "run",
        text: "usage: agent-talk run <name> <executable> [args...]\n\n実行中だけ現在のpaneをagent名で登録します。",
    },
    CommandHelp {
        command: "register",
        text: "usage: agent-talk register <name>\n\n現在のpaneをagent名で登録します。",
    },
    CommandHelp {
        command: "unregister",
        text: "usage: agent-talk unregister\n\n現在のpaneのagent登録を解除します。",
    },
    CommandHelp {
        command: "busy",
        text: "usage: agent-talk busy\n\n現在のpaneをbusy状態にします。",
    },
    CommandHelp {
        command: "idle",
        text: "usage: agent-talk idle\n\n現在のpaneをidle状態にします。",
    },
    CommandHelp {
        command: "turn-end",
        text: "usage: agent-talk turn-end\n\n現在のpaneのturn終了を通知します。",
    },
    CommandHelp {
        command: "who",
        text: "usage: agent-talk who\n\n登録中のagent一覧を表示します。",
    },
    CommandHelp {
        command: "gc",
        text: "usage: agent-talk gc\n\n互換用のno-opです。",
    },
    CommandHelp {
        command: "watch",
        text: "usage: agent-talk watch\n\n互換用のno-opです。",
    },
    CommandHelp {
        command: "resolve",
        text: "usage: agent-talk resolve [scope/]<name> | %pane\n\n宛先agentをpaneへ解決します。",
    },
    CommandHelp {
        command: "send",
        text: "usage: agent-talk send [scope/]<name> [--from <source>] [--skill <name>] [--no-reply] [--] [message]\n\nagentへ依頼または一方向連絡を送信します。--from/--skill/--no-replyを指定できます。",
    },
    CommandHelp {
        command: "read",
        text: "usage: agent-talk read <id>\n\n現在のpane宛の依頼本文を確認します。",
    },
    CommandHelp {
        command: "send-message",
        text: "usage: agent-talk send-message <addr>\n\n登録済みagent paneからの送信結果をJSONで返します (MCP adapter用)。",
    },
    CommandHelp {
        command: "read-message",
        text: "usage: agent-talk read-message <id>\n\n依頼本文をJSONで返します (MCP adapter用)。配達未完了は拒否します。",
    },
    CommandHelp {
        command: "ack-message",
        text: "usage: agent-talk ack-message <id>\n\n受領報告を送りmessageを削除対象にします。存在しないIDは冪等成功です。",
    },
    CommandHelp {
        command: "list-peers",
        text: "usage: agent-talk list-peers\n\n登録agentと両方向の未受領IDをJSONで返します (MCP adapter用)。",
    },
    CommandHelp {
        command: "reply",
        text: "usage: agent-talk reply <original-id> [body]\n\n外部mailboxの依頼へ返信します。",
    },
    CommandHelp {
        command: "mailbox-list",
        text: "usage: agent-talk mailbox-list <mailbox> [--after <id>] [--limit <n>]\n\nmailbox eventをJSONで非consume取得します。--afterは排他、--limitは1〜500です。",
    },
    CommandHelp {
        command: "daemon",
        text: "usage: agent-talk daemon\n\n内部daemonを起動します。",
    },
    CommandHelp {
        command: "internal-daemon-status",
        text: "usage: agent-talk internal-daemon-status\n\n内部daemon status RPCです。",
    },
    CommandHelp {
        command: "internal-daemon-shutdown",
        text: "usage: agent-talk internal-daemon-shutdown\n\n内部daemon shutdown RPCです。",
    },
    CommandHelp {
        command: "internal-pane-exited",
        text: "usage: agent-talk internal-pane-exited <pane>\n\npane退出を内部通知します。",
    },
    CommandHelp {
        command: "internal-reconcile",
        text: "usage: agent-talk internal-reconcile\n\n内部状態を再照合します。",
    },
];

pub fn command(command: &str) -> Option<&'static str> {
    COMMANDS
        .iter()
        .find(|entry| entry.command == command)
        .map(|entry| entry.text)
}

pub fn usage(name: &str) -> &'static str {
    command(name)
        .and_then(|text| text.lines().next())
        .unwrap_or("usage: agent-talk <command>")
}

pub fn is_known(name: &str) -> bool {
    command(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_table_is_the_command_acceptance_set() {
        let expected = [
            "update",
            "ensure-daemon",
            "daemon-status",
            "run",
            "register",
            "unregister",
            "busy",
            "idle",
            "turn-end",
            "who",
            "gc",
            "watch",
            "resolve",
            "send",
            "read",
            "send-message",
            "read-message",
            "ack-message",
            "list-peers",
            "reply",
            "mailbox-list",
            "daemon",
            "internal-daemon-status",
            "internal-daemon-shutdown",
            "internal-pane-exited",
            "internal-reconcile",
        ];
        assert_eq!(COMMANDS.len(), expected.len());
        for command in expected {
            assert!(is_known(command), "missing help entry: {command}");
        }
    }
}
