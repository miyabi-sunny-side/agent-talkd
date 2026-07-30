use std::{
    env,
    io::{self, Read},
};

use anyhow::{Result, bail};

use crate::{
    config::Config,
    lifecycle,
    protocol::{Request, Response, SendOptions},
};

pub async fn run(config: Config, mut command: String, args: Vec<String>) -> Result<i32> {
    let (args, send_options) = if command == "send" {
        parse_send_args(args)?
    } else {
        (args, None)
    };
    if send_options.is_some() {
        command = "send-v2".into();
    }
    let mut stdin = String::new();
    if matches!(command.as_str(), "send" | "send-v2" | "reply") && args.len() <= 1 {
        io::stdin().read_to_string(&mut stdin)?;
        while stdin.ends_with('\n') {
            stdin.pop();
        }
    }
    let request = Request {
        command,
        args,
        stdin,
        pane: env::var("TMUX_PANE").ok(),
        send_options,
    };
    let response = lifecycle::request(&config, &request).await?;
    Ok(print_response(&response))
}

fn parse_send_args(args: Vec<String>) -> Result<(Vec<String>, Option<SendOptions>)> {
    let Some(addr) = args.first().cloned() else {
        return Ok((args, None));
    };
    let mut parsed = vec![addr];
    let mut options = SendOptions::default();
    let mut has_options = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--" => {
                parsed.extend(args[index + 1..].iter().cloned());
                return Ok((parsed, has_options.then_some(options)));
            }
            "--from" | "--skill" => {
                let option = args[index].as_str();
                let Some(value) = args.get(index + 1) else {
                    bail!("{option} には値が必要です");
                };
                let slot = if option == "--from" {
                    &mut options.from
                } else {
                    &mut options.skill
                };
                if slot.replace(value.clone()).is_some() {
                    bail!("{option} は複数指定できません");
                }
                has_options = true;
                index += 2;
            }
            "--no-reply" => {
                if options.no_reply {
                    bail!("--no-reply は複数指定できません");
                }
                options.no_reply = true;
                has_options = true;
                index += 1;
            }
            value if value.starts_with("--") => bail!("不明なsendオプションです: {value}"),
            _ => {
                parsed.extend(args[index..].iter().cloned());
                return Ok((parsed, has_options.then_some(options)));
            }
        }
    }
    Ok((parsed, has_options.then_some(options)))
}

fn print_response(response: &Response) -> i32 {
    if !response.stdout.is_empty() {
        print!("{}", response.stdout);
    }
    if !response.stderr.is_empty() {
        eprint!("{}", response.stderr);
    }
    response.code
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::PathBuf,
    };

    use tempfile::tempdir;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };

    use super::*;

    #[test]
    fn parses_send_options_before_inline_body() {
        let (args, options) = parse_send_args(vec![
            "claude".into(),
            "--from".into(),
            "mobile".into(),
            "--skill".into(),
            "deliver".into(),
            "hello".into(),
            "world".into(),
        ])
        .unwrap();
        assert_eq!(args, ["claude", "hello", "world"]);
        let options = options.unwrap();
        assert_eq!(options.from.as_deref(), Some("mobile"));
        assert_eq!(options.skill.as_deref(), Some("deliver"));
    }

    #[test]
    fn double_dash_preserves_option_like_body() {
        let (args, options) = parse_send_args(vec![
            "claude".into(),
            "--".into(),
            "--skill".into(),
            "literal".into(),
        ])
        .unwrap();
        assert_eq!(args, ["claude", "--skill", "literal"]);
        assert!(options.is_none());
    }

    #[test]
    fn parses_no_reply_with_other_options_and_inline_body() {
        let (args, options) = parse_send_args(vec![
            "claude".into(),
            "--from".into(),
            "mobile".into(),
            "--no-reply".into(),
            "body".into(),
        ])
        .unwrap();
        assert_eq!(args, ["claude", "body"]);
        let options = options.unwrap();
        assert!(options.no_reply);
        assert_eq!(options.from.as_deref(), Some("mobile"));
    }

    #[test]
    fn rejects_duplicate_no_reply_and_preserves_option_like_stdin_body() {
        assert!(
            parse_send_args(vec![
                "claude".into(),
                "--no-reply".into(),
                "--no-reply".into(),
            ])
            .is_err()
        );
        let (args, options) = parse_send_args(vec![
            "claude".into(),
            "--no-reply".into(),
            "--".into(),
            "--literal".into(),
        ])
        .unwrap();
        assert_eq!(args, ["claude", "--literal"]);
        assert!(options.unwrap().no_reply);
    }

    #[tokio::test]
    async fn send_v2_error_does_not_fall_back_to_legacy_send() {
        let dir = tempdir().unwrap();
        let rpc_socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&rpc_socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut line = String::new();
            BufReader::new(reader).read_line(&mut line).await.unwrap();
            let request: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(request.command, "send-v2");
            assert_eq!(
                request.send_options.unwrap().skill.as_deref(),
                Some("deliver")
            );
            let response = Response::error("unknown command");
            writer
                .write_all(&serde_json::to_vec(&response).unwrap())
                .await
                .unwrap();
            writer.write_all(b"\n").await.unwrap();
        });
        let config = Config {
            tmux_socket: String::new(),
            rpc_socket,
            http_socket: PathBuf::new(),
            journal: PathBuf::new(),
            log: PathBuf::new(),
            queue_limit: 1,
            log_level: "info".into(),
            skill_syntax: BTreeMap::new(),
            allowed_skills: None,
            allowed_sources: BTreeSet::new(),
        };
        let code = run(
            config,
            "send".into(),
            vec![
                "claude".into(),
                "--skill".into(),
                "deliver".into(),
                "body".into(),
            ],
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(code, 1);
    }
}
