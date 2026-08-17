//! Everything this service is told, and the refusals that keep it from
//! starting half-configured.
//!
//! No default invents a credential. A missing token or password stops the
//! process with a message naming the variable, which is the same posture the
//! compose file takes with `${VAR:?…}` — better to fail at start than to run
//! and collect nothing.

use std::env;

pub struct Config {
    pub refresh_token: String,
    pub db: Db,
    /// How long to wait between passes.
    pub interval_secs: u64,
    /// Requests per second during a pass.
    pub rate: f64,
    /// How old the last success may be before `--health` calls it unwell.
    pub stale_secs: i64,
}

pub struct Db {
    pub host: String,
    pub port: u16,
    pub name: String,
    pub user: String,
    pub password: String,
}

fn required(key: &str) -> Result<String, String> {
    match env::var(key) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        _ => Err(format!("{key} is not set")),
    }
}

fn or(key: &str, fallback: &str) -> String {
    env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn number<T: std::str::FromStr>(key: &str, fallback: T) -> Result<T, String> {
    match env::var(key) {
        Ok(v) if !v.trim().is_empty() => v
            .trim()
            .parse()
            .map_err(|_| format!("{key} is not a number: {v}")),
        _ => Ok(fallback),
    }
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        Ok(Config {
            refresh_token: required("DYNAMO_REFRESH_TOKEN")?,
            db: Db {
                // `postgres`, because that is the container name on the
                // `postgres_default` network this joins — and 5432, its own
                // port, not the 5433 the NAS publishes it on. Going through
                // the published port would put the database password on the
                // tailnet for no reason.
                host: or("DYNAMO_DB_HOST", "postgres"),
                port: number("DYNAMO_DB_PORT", 5432)?,
                name: or("DYNAMO_DB_NAME", "dynamo"),
                user: or("DYNAMO_DB_USER", "dynamo"),
                password: required("DYNAMO_DB_PASSWORD")?,
            },
            // A minute, because that is the finest resolution the cloud
            // reports and asking more often returns the same point again.
            interval_secs: number("DYNAMO_INTERVAL", 60)?,
            rate: number("DYNAMO_RATE", 4.0)?,
            // Fifteen minutes. Long enough that one failed pass and a retry do
            // not turn the container red, short enough that a service which
            // has genuinely stopped is visible within a quarter of an hour.
            stale_secs: number("DYNAMO_STALE", 900)?,
        })
    }

    /// The subset `--health` needs. It runs in a separate process inside the
    /// same container and must not require the refresh token — handing the
    /// credential to Docker as well, to answer a question about the database,
    /// would be the wrong trade.
    pub fn health_from_env() -> Result<(Db, i64), String> {
        Ok((
            Db {
                host: or("DYNAMO_DB_HOST", "postgres"),
                port: number("DYNAMO_DB_PORT", 5432)?,
                name: or("DYNAMO_DB_NAME", "dynamo"),
                user: or("DYNAMO_DB_USER", "dynamo"),
                password: required("DYNAMO_DB_PASSWORD")?,
            },
            number("DYNAMO_STALE", 900)?,
        ))
    }
}
