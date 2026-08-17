//! Running an [`crate::agent::Request`] against Postgres and shaping the JSON
//! Familiar reads.
//!
//! The split is the usual one: `agent.rs` decides *what* was asked, with a fixed
//! clock and no database; this asks it. Every query here is a `SELECT`, and the
//! role the CLI connects as has no other grant.

use chrono::{DateTime, Utc};
use postgres::Client;
use serde_json::{json, Value};

use crate::agent::{Kind, Request, Span, MAX_ROWS};

/// A database failure, with the bit that says what actually went wrong.
///
/// `postgres::Error`'s own `Display` is the word "db error" and nothing else —
/// the column name, the constraint, the syntax position all live on the source
/// underneath it. A message that says `cannot read channels: db error` costs a
/// debugging session; this one names the missing column.
fn db(context: &str, e: postgres::Error) -> String {
    match std::error::Error::source(&e) {
        Some(cause) => format!("{context}: {cause}"),
        None => format!("{context}: {e}"),
    }
}

/// A label for a channel, in the words a person would use.
///
/// The name a circuit was given, falling back to the monitor and channel number
/// when nobody has named it — which is most of one box. Never an empty string:
/// an unnamed circuit that reads as `""` is one a model will describe as
/// unnamed *and then forget to mention which one it was*.
const LABEL: &str = "coalesce(nullif(c.name, ''), c.device_name || ' ch' || c.channel_num)";

/// The filter for a `kind`.
///
/// `Circuits` is the one worth reading twice: merged channels, plus branch legs
/// that belong to no merge. That is every circuit exactly once. Merged and
/// branch together would count both halves of every 240 V appliance and its
/// merged total, which inflates a house by however much the big loads draw.
fn kind_sql(kind: Kind) -> &'static str {
    match kind {
        Kind::Main => "c.kind = 'main'",
        Kind::Branch => "c.kind = 'branch'",
        Kind::Merged => "c.kind = 'merged'",
        Kind::Circuits => "(c.kind = 'merged' OR (c.kind = 'branch' AND c.merged_into IS NULL))",
    }
}

/// How many points per circuit a `usage` sum will scan before it moves to a
/// coarser scale.
///
/// **This is not `MAX_ROWS` and the difference matters.** `usage` groups by
/// circuit, so the *answer* is sixty-odd rows however long the period is; what
/// grows with the span is the number of rows summed to produce it. A year of
/// minutes across sixty circuits is thirty million rows scanned to print sixty.
/// A first version of this conflated the two and picked quarter-hours for
/// "today", which is a worse answer for no reason.
const SCAN_BUDGET: i64 = 2_000;

/// The finest stored scale that answers a span within [`SCAN_BUDGET`].
///
/// Each scale accumulates energy independently, so summing kWh at any of them
/// gives the same total for the same window — coarser is cheaper and not less
/// correct. That is what makes choosing here better than refusing a long
/// period.
fn scale_for(span: &Span) -> crate::scale::Scale {
    use crate::scale::Scale;
    let seconds = (span.to - span.from).num_seconds().max(1);
    // Coarsest last: take the first that fits, and fall through to `Day`, which
    // is the coarsest there is and so is the answer for anything longer.
    for scale in [Scale::Minute, Scale::QuarterHour, Scale::Hour] {
        if seconds / scale.step().num_seconds() <= SCAN_BUDGET {
            return scale;
        }
    }
    Scale::Day
}

pub fn answer(c: &mut Client, request: &Request) -> Result<Value, String> {
    match request {
        Request::Describe => Ok(crate::agent::describe()),
        Request::Channels => channels(c),
        Request::Now => now(c),
        Request::Usage { span, kind } => usage(c, span, *kind),
        Request::Series { query, span, scale } => series(c, query, span, *scale),
    }
}

fn channels(c: &mut Client) -> Result<Value, String> {
    let rows = c
        .query(
            &format!(
                "SELECT {LABEL} AS label, c.device_name, c.channel_num, c.kind, c.name IS NOT NULL
                 FROM channel c
                 WHERE {}
                 ORDER BY c.device_name, c.kind, length(c.channel_num), c.channel_num",
                kind_sql(Kind::Circuits)
            ),
            &[],
        )
        .map_err(|e| db("cannot read channels", e))?;
    let list: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "circuit": r.get::<_, String>(0),
                "monitor": r.get::<_, Option<String>>(1),
                "channel": r.get::<_, String>(2),
                "kind": r.get::<_, String>(3),
                "named": r.get::<_, bool>(4),
            })
        })
        .collect();
    Ok(json!({"ok": true, "count": list.len(), "circuits": list}))
}

fn now(c: &mut Client) -> Result<Value, String> {
    // The newest minute each circuit has. `DISTINCT ON` rather than a window
    // function because it is the one Postgres does with the index this table
    // already has.
    let rows = c
        .query(
            &format!(
                "SELECT DISTINCT ON (r.device_gid, r.channel_num)
                        {LABEL} AS label, r.kwh, r.instant
                 FROM reading r JOIN channel c USING (device_gid, channel_num)
                 WHERE r.scale = '1MIN' AND {} AND r.instant > now() - interval '30 minutes'
                 ORDER BY r.device_gid, r.channel_num, r.instant DESC",
                kind_sql(Kind::Circuits)
            ),
            &[],
        )
        .map_err(|e| db("cannot read current usage", e))?;

    let mut list: Vec<(String, f64, DateTime<Utc>)> = rows
        .iter()
        .map(|r| {
            (
                r.get::<_, String>(0),
                // kWh over one minute back to watts, which is the unit anybody
                // asking "what is it drawing" means.
                r.get::<_, f64>(1) * 60_000.0,
                r.get::<_, DateTime<Utc>>(2),
            )
        })
        .collect();
    list.sort_by(|a, b| b.1.total_cmp(&a.1));

    let latest = list.iter().map(|(_, _, t)| *t).max();
    let circuits: Vec<Value> = list
        .iter()
        .map(|(label, watts, at)| {
            json!({"circuit": label, "watts": watts.round(), "at": at.to_rfc3339()})
        })
        .collect();
    Ok(json!({
        "ok": true,
        "unit": "W",
        "as_of": latest.map(|t| t.to_rfc3339()),
        // Said out loud because it is the difference between "the house is
        // idle" and "the collector stopped an hour ago", which look identical
        // in a list of small numbers.
        "note": "Newest reading per circuit within the last 30 minutes. A circuit absent from \
                 this list has not reported recently.",
        "count": circuits.len(),
        "circuits": circuits
    }))
}

fn usage(c: &mut Client, span: &Span, kind: Kind) -> Result<Value, String> {
    let scale = scale_for(span);
    let rows = c
        .query(
            &format!(
                "SELECT {LABEL} AS label, sum(r.kwh) AS kwh
                 FROM reading r JOIN channel c USING (device_gid, channel_num)
                 WHERE r.scale = $1 AND {} AND r.instant >= $2 AND r.instant < $3
                 GROUP BY label
                 HAVING sum(r.kwh) > 0
                 ORDER BY kwh DESC",
                kind_sql(kind)
            ),
            &[&scale.api_name(), &span.from, &span.to],
        )
        .map_err(|e| db("cannot read usage", e))?;

    let matched = rows.len();
    let total: f64 = rows.iter().map(|r| r.get::<_, f64>(1)).sum();
    let circuits: Vec<Value> = rows
        .iter()
        .take(MAX_ROWS)
        .map(|r| {
            json!({
                "circuit": r.get::<_, String>(0),
                "kwh": (r.get::<_, f64>(1) * 1000.0).round() / 1000.0,
            })
        })
        .collect();

    Ok(json!({
        "ok": true,
        "period": span.named,
        "from": span.from.to_rfc3339(),
        "to": span.to.to_rfc3339(),
        "resolution": scale.api_name(),
        "kind": match kind {
            Kind::Main => "main", Kind::Branch => "branch",
            Kind::Merged => "merged", Kind::Circuits => "circuits",
        },
        "total_kwh": (total * 1000.0).round() / 1000.0,
        "unit": "kWh",
        "count": circuits.len(),
        "matched": matched,
        "truncated": matched > circuits.len(),
        "circuits": circuits
    }))
}

fn series(
    c: &mut Client,
    query: &str,
    span: &Span,
    scale: crate::scale::Scale,
) -> Result<Value, String> {
    // Resolve the circuit first, and say so when it is ambiguous rather than
    // picking one. The same posture as Planner's `ambiguous`: two circuits here
    // are genuinely named the same thing on different monitors.
    let found = c
        .query(
            &format!(
                "SELECT c.device_gid, c.channel_num, {LABEL} AS label
                 FROM channel c
                 WHERE ({LABEL}) ILIKE $1
                    OR c.device_gid || '/' || c.channel_num = $2
                 ORDER BY label",
            ),
            &[&format!("%{query}%"), &query],
        )
        .map_err(|e| db("cannot look up a circuit", e))?;

    if found.is_empty() {
        return Ok(json!({
            "ok": false, "error": "no-such-circuit",
            "message": format!("Nothing here is called {query:?}. `channels` lists them.")
        }));
    }
    if found.len() > 1 {
        let candidates: Vec<Value> = found
            .iter()
            .map(|r| {
                json!({
                    "circuit": r.get::<_, String>(2),
                    "channel": format!("{}/{}", r.get::<_, i64>(0), r.get::<_, String>(1)),
                })
            })
            .collect();
        return Ok(json!({
            "ok": false, "error": "ambiguous",
            "message": format!("{} circuits match {query:?}.", candidates.len()),
            "candidates": candidates
        }));
    }

    let gid: i64 = found[0].get(0);
    let num: String = found[0].get(1);
    let label: String = found[0].get(2);

    let rows = c
        .query(
            "SELECT instant, kwh FROM reading
             WHERE device_gid = $1 AND channel_num = $2 AND scale = $3
               AND instant >= $4 AND instant < $5
             ORDER BY instant",
            &[&gid, &num, &scale.api_name(), &span.from, &span.to],
        )
        .map_err(|e| db("cannot read a series", e))?;

    let matched = rows.len();
    let per_hour = 3_600_000.0 / scale.step().num_seconds() as f64;
    let points: Vec<Value> = rows
        .iter()
        .take(MAX_ROWS)
        .map(|r| {
            let kwh: f64 = r.get(1);
            json!({
                "at": r.get::<_, DateTime<Utc>>(0).to_rfc3339(),
                "kwh": (kwh * 1000.0).round() / 1000.0,
                "watts": (kwh * per_hour).round(),
            })
        })
        .collect();
    let total: f64 = rows.iter().map(|r| r.get::<_, f64>(1)).sum();

    Ok(json!({
        "ok": true,
        "circuit": label,
        "channel": format!("{gid}/{num}"),
        "period": span.named,
        "resolution": scale.api_name(),
        "total_kwh": (total * 1000.0).round() / 1000.0,
        "count": points.len(),
        "matched": matched,
        "truncated": matched > points.len(),
        "points": points
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scale::Scale;
    use chrono::{Duration, TimeZone};

    fn span(days: i64) -> Span {
        let to = Utc.with_ymd_and_hms(2026, 8, 17, 12, 0, 0).unwrap();
        Span {
            from: to - Duration::days(days),
            to,
            named: "test",
        }
    }

    #[test]
    fn a_day_is_answered_from_minutes_and_a_year_is_not() {
        // Today and yesterday are the common questions and they get the real
        // resolution; a year does not need it and could not afford it.
        assert_eq!(scale_for(&span(1)), Scale::Minute);
        assert_eq!(scale_for(&span(7)), Scale::QuarterHour);
        assert_eq!(scale_for(&span(30)), Scale::Hour);
        assert_eq!(scale_for(&span(365)), Scale::Day);
    }

    #[test]
    fn every_span_stays_inside_the_scan_budget_or_is_already_at_the_coarsest_scale() {
        for days in [1, 2, 6, 7, 21, 30, 90, 365, 700, 9000] {
            let s = span(days);
            let scale = scale_for(&s);
            let points = (s.to - s.from).num_seconds() / scale.step().num_seconds();
            assert!(
                points <= SCAN_BUDGET || scale == Scale::Day,
                "{days} days at {scale:?} scans {points} points a circuit"
            );
        }
    }

    #[test]
    fn the_finest_scale_that_fits_is_the_one_chosen() {
        // Not merely "within budget" — the *finest* within budget, or the
        // thresholds could drift coarse without a test noticing.
        for days in [1, 2, 6, 7, 21, 30, 90, 365] {
            let s = span(days);
            let chosen = scale_for(&s);
            let seconds = (s.to - s.from).num_seconds();
            for finer in crate::scale::SCALES.iter().copied() {
                if finer.step() < chosen.step() {
                    assert!(
                        seconds / finer.step().num_seconds() > SCAN_BUDGET,
                        "{days} days chose {chosen:?} when {finer:?} would have fitted"
                    );
                }
            }
        }
    }

    #[test]
    fn circuits_counts_merged_channels_and_unmerged_legs_and_nothing_twice() {
        let sql = kind_sql(Kind::Circuits);
        assert!(sql.contains("'merged'"));
        assert!(sql.contains("merged_into IS NULL"));
        // The trap: branch legs that *are* part of a merge must be excluded, or
        // every 240 V appliance is counted twice.
        assert!(!sql.contains("c.kind = 'branch')"), "{sql}");
    }

    #[test]
    fn an_unnamed_circuit_still_gets_a_label_that_identifies_it() {
        assert!(LABEL.contains("device_name"));
        assert!(LABEL.contains("channel_num"));
    }
}
