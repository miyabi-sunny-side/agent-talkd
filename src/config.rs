use std::{
    env,
    net::{Ipv4Addr, SocketAddr},
    num::NonZeroU16,
    path::PathBuf,
};

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
        let port = parse_port(env::var("PORT"))?;
        let http_addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
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

fn parse_port(value: Result<String, env::VarError>) -> Result<u16> {
    match value {
        Err(env::VarError::NotPresent) => Ok(5002),
        value => {
            let value = value.context("PORT must be a number from 1 to 65535")?;
            anyhow::ensure!(
                !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()),
                "PORT must be a number from 1 to 65535"
            );
            value
                .parse::<NonZeroU16>()
                .map(NonZeroU16::get)
                .context("PORT must be a number from 1 to 65535")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_defaults_only_when_absent_and_accepts_the_full_range() {
        assert_eq!(parse_port(Err(env::VarError::NotPresent)).unwrap(), 5002);
        for port in [1, 5002, 65535] {
            assert_eq!(parse_port(Ok(port.to_string())).unwrap(), port);
        }
    }
}
