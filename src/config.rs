use std::{env, net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};

pub struct Config {
    pub herdr_socket: PathBuf,
    pub http_addr: SocketAddr,
    pub home: PathBuf,
    pub log_level: String,
}

impl Config {
    pub fn discover() -> Result<Self> {
        let home = PathBuf::from(env::var_os("HOME").context("HOME is required")?);
        let herdr_socket = env::var_os("AGENT_TALK_HERDR_SOCKET")
            .or_else(|| env::var_os("HERDR_SOCKET_PATH"))
            .filter(|value| !value.is_empty())
            .context("set AGENT_TALK_HERDR_SOCKET to the Herdr socket")?;
        let http_addr = env::var("AGENT_TALK_HTTP_ADDR")
            .context("set AGENT_TALK_HTTP_ADDR (for example 127.0.0.1:5002)")?
            .parse()
            .context("AGENT_TALK_HTTP_ADDR must be an IP address and port")?;
        Ok(Self {
            herdr_socket: herdr_socket.into(),
            http_addr,
            home,
            log_level: env::var("AGENT_TALK_LOG_LEVEL").unwrap_or_else(|_| "info".into()),
        })
    }
}
