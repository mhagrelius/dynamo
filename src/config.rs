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

/// `~/.config/dynamo/config.json`, if it is there.
///
/// The same shape as the sibling clients' config: a small JSON object, read
/// rather than generated, so it can be written by hand once and forgotten.
fn read_config_file() -> Result<Option<Db>, String> {
    let Some(home) = env::var_os("HOME") else {
        return Ok(None);
    };
    let path = std::path::Path::new(&home).join(".config/dynamo/config.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        // Absent is the ordinary case on a machine that uses the environment,
        // and not an error. Unreadable *is*, and says which file.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    let string = |key: &str, fallback: &str| {
        v.get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or(fallback)
            .to_string()
    };
    let password = v
        .get("password")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{} has no \"password\"", path.display()))?;
    Ok(Some(Db {
        host: string("host", "nas.example.ts.net"),
        port: v
            .get("port")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(5433) as u16,
        name: string("database", "dynamo"),
        user: string("user", "dynamo_read"),
        password: password.to_string(),
    }))
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

    /// The database, for a reader rather than the collector.
    ///
    /// `agent` runs on a laptop, not in the container, so it reads
    /// `~/.config/dynamo/config.json` first and falls back to the same
    /// environment variables the service uses. The file is the ordinary case:
    /// nobody wants to export five variables to ask what the dryer used.
    ///
    /// The credentials in it should be the **read-only** role, not the
    /// collector's. Nothing reachable from `agent` writes, and the grant is
    /// what makes that a fact rather than a promise.
    pub fn reader_from_env_or_file() -> Result<Db, String> {
        if let Some(db) = read_config_file()? {
            return Ok(db);
        }
        Ok(Db {
            host: or("DYNAMO_DB_HOST", "nas.example.ts.net"),
            port: number("DYNAMO_DB_PORT", 5433)?,
            name: or("DYNAMO_DB_NAME", "dynamo"),
            user: or("DYNAMO_DB_USER", "dynamo_read"),
            password: required("DYNAMO_DB_PASSWORD").map_err(|_| {
                "no database credentials. Write ~/.config/dynamo/config.json with \
                 {\"host\":…,\"port\":…,\"database\":…,\"user\":…,\"password\":…}, or set \
                 DYNAMO_DB_PASSWORD. See README.md."
                    .to_string()
            })?,
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
