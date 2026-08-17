//! Postgres: the schema, the upserts, and the watermarks the planner runs on.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use postgres::{Client, NoTls};

use crate::config::Db;
use crate::emporia::{NamedChannel, Reading};
use crate::plan::{Series, Watermark};
use crate::scale::{Scale, SCALES};

pub fn connect(db: &Db) -> Result<Client, String> {
    let url = format!(
        "host={} port={} dbname={} user={} password={} connect_timeout=10",
        db.host, db.port, db.name, db.user, db.password
    );
    Client::connect(&url, NoTls).map_err(|e| format!("cannot reach Postgres: {e}"))
}

/// Created on every start, which is what makes a fresh database and an
/// existing one the same case.
///
/// `CREATE TABLE IF NOT EXISTS` never adds a column to a table that already
/// exists — a lesson from a sibling that cost months of silently unwritten
/// data. A new column goes in its own `ALTER TABLE … IF NOT EXISTS` below, not
/// into the `CREATE`.
pub fn migrate(c: &mut Client) -> Result<(), String> {
    c.batch_execute(
        r#"
        CREATE TABLE IF NOT EXISTS channel (
            device_gid   BIGINT NOT NULL,
            channel_num  TEXT   NOT NULL,
            device_name  TEXT,
            name         TEXT,
            kind         TEXT   NOT NULL,
            multiplier   DOUBLE PRECISION,
            seen_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
            PRIMARY KEY (device_gid, channel_num)
        );

        -- One reading is a channel, a resolution and an instant. The key is
        -- over all four so re-fetching an overlapping window is free, which is
        -- what lets the planner overlap deliberately instead of trusting a
        -- boundary it cannot verify.
        CREATE TABLE IF NOT EXISTS reading (
            device_gid   BIGINT NOT NULL,
            channel_num  TEXT   NOT NULL,
            scale        TEXT   NOT NULL,
            instant      TIMESTAMPTZ NOT NULL,
            kwh          DOUBLE PRECISION NOT NULL,
            PRIMARY KEY (device_gid, channel_num, scale, instant)
        );

        -- The watermark query runs once per series on every pass; without this
        -- it is a sequential scan of the whole table each time.
        CREATE INDEX IF NOT EXISTS reading_series_instant
            ON reading (device_gid, channel_num, scale, instant DESC);

        -- One row, and the only thing `--health` reads. `ok` is false when the
        -- refresh token has been rejected, which is the failure no retry fixes.
        CREATE TABLE IF NOT EXISTS heartbeat (
            id           BOOLEAN PRIMARY KEY DEFAULT TRUE,
            last_success TIMESTAMPTZ,
            last_error   TEXT,
            ok           BOOLEAN NOT NULL DEFAULT TRUE,
            -- The original browser sign-in behind the refresh token, from the
            -- id token's `auth_time`. The refresh token's own expiry cannot be
            -- read — it is an encrypted JWE — so this is how the credential's
            -- age is known, and what will finally answer how long one lasts.
            authenticated_at TIMESTAMPTZ,
            CONSTRAINT heartbeat_single_row CHECK (id)
        );
        INSERT INTO heartbeat (id) VALUES (TRUE) ON CONFLICT (id) DO NOTHING;

        -- `CREATE TABLE IF NOT EXISTS` reaches new databases only, so a column
        -- added to the definition above never arrives on one that already
        -- exists. Every added column needs a line here as well.
        ALTER TABLE heartbeat ADD COLUMN IF NOT EXISTS authenticated_at TIMESTAMPTZ;
        "#,
    )
    .map_err(|e| format!("migration failed: {e}"))
}

pub fn save_channels(c: &mut Client, channels: &[NamedChannel]) -> Result<(), String> {
    for ch in channels {
        c.execute(
            "INSERT INTO channel (device_gid, channel_num, device_name, name, kind, multiplier, seen_at)
             VALUES ($1,$2,$3,$4,$5,$6, now())
             ON CONFLICT (device_gid, channel_num) DO UPDATE SET
               device_name = EXCLUDED.device_name,
               name        = EXCLUDED.name,
               kind        = EXCLUDED.kind,
               multiplier  = EXCLUDED.multiplier,
               seen_at     = now()",
            &[
                &ch.device_gid,
                &ch.channel_num,
                &ch.device_name,
                &ch.name,
                &ch.kind.as_str(),
                &ch.multiplier,
            ],
        )
        .map_err(|e| format!("cannot record channel {}: {e}", ch.channel_num))?;
    }
    Ok(())
}

/// `DO UPDATE`, not `DO NOTHING`.
///
/// The cloud revises a recent interval as late readings arrive from the device,
/// so the second answer for an instant is the better one. For an interval that
/// has settled the update writes the same number, which costs nothing worth
/// measuring at this volume.
pub fn save_readings(
    c: &mut Client,
    series: &Series,
    readings: &[Reading],
) -> Result<usize, String> {
    if readings.is_empty() {
        return Ok(0);
    }
    let mut tx = c
        .transaction()
        .map_err(|e| format!("cannot open transaction: {e}"))?;
    let stmt = tx
        .prepare(
            "INSERT INTO reading (device_gid, channel_num, scale, instant, kwh)
             VALUES ($1,$2,$3,$4,$5)
             ON CONFLICT (device_gid, channel_num, scale, instant)
             DO UPDATE SET kwh = EXCLUDED.kwh",
        )
        .map_err(|e| format!("cannot prepare insert: {e}"))?;
    let scale = series.scale.api_name();
    for r in readings {
        tx.execute(
            &stmt,
            &[
                &series.device_gid,
                &series.channel_num,
                &scale,
                &r.instant,
                &r.kwh,
            ],
        )
        .map_err(|e| format!("cannot write reading: {e}"))?;
    }
    tx.commit().map_err(|e| format!("cannot commit: {e}"))?;
    Ok(readings.len())
}

/// Every watermark in one query rather than one per series.
///
/// Seventy-odd channels times four scales is nearly three hundred round trips a
/// pass done the obvious way, against one `GROUP BY` done this way.
pub fn watermarks(c: &mut Client) -> Result<HashMap<Series, Watermark>, String> {
    let rows = c
        .query(
            "SELECT device_gid, channel_num, scale, MAX(instant)
             FROM reading GROUP BY device_gid, channel_num, scale",
            &[],
        )
        .map_err(|e| format!("cannot read watermarks: {e}"))?;
    let mut out = HashMap::new();
    for row in rows {
        let scale_name: String = row.get(2);
        let Some(scale) = SCALES.iter().copied().find(|s| s.api_name() == scale_name) else {
            // A scale this build does not know. Left alone rather than
            // guessed at: it belongs to a version that stored something we no
            // longer plan for, and inventing a mapping would make its rows
            // look like ours.
            continue;
        };
        out.insert(
            Series {
                device_gid: row.get(0),
                channel_num: row.get(1),
                scale,
            },
            Watermark {
                newest: row.get::<_, Option<DateTime<Utc>>>(3),
            },
        );
    }
    Ok(out)
}

pub fn record_success(
    c: &mut Client,
    authenticated_at: Option<DateTime<Utc>>,
) -> Result<(), String> {
    c.execute(
        "UPDATE heartbeat SET last_success = now(), last_error = NULL, ok = TRUE,
                authenticated_at = COALESCE($1, authenticated_at) WHERE id",
        &[&authenticated_at],
    )
    .map(|_| ())
    .map_err(|e| format!("cannot write heartbeat: {e}"))
}

/// `fatal` is the difference between "the network wobbled" and "nobody is
/// collecting anything until a person acts". Only the second turns the
/// container red.
pub fn record_failure(c: &mut Client, message: &str, fatal: bool) -> Result<(), String> {
    c.execute(
        "UPDATE heartbeat SET last_error = $1, ok = NOT $2 AND ok WHERE id",
        &[&message, &fatal],
    )
    .map(|_| ())
    .map_err(|e| format!("cannot write heartbeat: {e}"))
}

pub struct Health {
    pub last_success: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub ok: bool,
    pub authenticated_at: Option<DateTime<Utc>>,
}

pub fn health(c: &mut Client) -> Result<Health, String> {
    let row = c
        .query_one(
            "SELECT last_success, last_error, ok, authenticated_at FROM heartbeat WHERE id",
            &[],
        )
        .map_err(|e| format!("cannot read heartbeat: {e}"))?;
    Ok(Health {
        last_success: row.get(0),
        last_error: row.get(1),
        ok: row.get(2),
        authenticated_at: row.get(3),
    })
}

/// How many rows are held, for the line printed at the end of a pass.
pub fn counts(c: &mut Client) -> Result<Vec<(Scale, i64)>, String> {
    let rows = c
        .query("SELECT scale, count(*) FROM reading GROUP BY scale", &[])
        .map_err(|e| format!("cannot count readings: {e}"))?;
    let mut out = Vec::new();
    for row in rows {
        let name: String = row.get(0);
        if let Some(s) = SCALES.iter().copied().find(|s| s.api_name() == name) {
            out.push((s, row.get(1)));
        }
    }
    out.sort_by_key(|(s, _)| *s);
    Ok(out)
}
