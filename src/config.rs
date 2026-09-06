use std::{env, net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use tracing::level_filters::LevelFilter;

pub struct Config {
    pub http_addr: SocketAddr,
    pub home: PathBuf,
    pub log_level: LevelFilter,
}

impl Config {
    pub fn discover() -> Result<Self> {
        let home = PathBuf::from(env::var_os("HOME").context("HOME is required")?);
        let http_addr = env::var("AGENT_TALK_HTTP_ADDR")
            .context("set AGENT_TALK_HTTP_ADDR (for example 127.0.0.1:5002)")?
            .parse()
            .context("AGENT_TALK_HTTP_ADDR must be an IP address and port")?;
        Ok(Self {
            http_addr,
            home,
            log_level: match env::var("LOG_LEVEL").as_deref() {
                Ok("off") => LevelFilter::OFF,
                Ok("error") => LevelFilter::ERROR,
                Ok("warn") => LevelFilter::WARN,
                Ok("debug") => LevelFilter::DEBUG,
                Ok("trace") => LevelFilter::TRACE,
                _ => LevelFilter::INFO,
            },
        })
    }
}
