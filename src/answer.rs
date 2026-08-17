//! Running an [`crate::agent::Request`] against Postgres and shaping the JSON
//! Familiar reads.
//!
//! The split is the usual one: `agent.rs` decides *what* was asked, with a fixed
//! clock and no database; this asks it. Every query here is a `SELECT`, and the
//! role the CLI connects as has no other grant.

use chrono::{DateTime, FixedOffset, Utc};
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

/// One candidate for a name somebody typed.
#[derive(Clone, Debug, PartialEq)]
pub struct Match {
    pub device_gid: i64,
    pub channel_num: String,
    pub label: String,
    pub kind: String,
    pub merged_into: Option<String>,
}

/// Narrow a set of matches to the one a person meant, if there is one.
///
/// Two rules, both found by running the real thing against the real house
/// rather than reasoned out in advance.
///
/// **An exact name beats a substring.** `GeoThermal` matched `GeoThermal`,
/// `GeoThermal Blower` *and* `GeoThermal Aux Heat`, and reporting that as
/// ambiguous asks somebody to disambiguate a name they typed exactly.
///
/// **A merged circuit beats its own legs.** `Water Heater` matched legs 7 and 8
/// and merged channel 99 — which is not ambiguity, it is one circuit described
/// twice. Nobody asking about the water heater means one leg of it. Genuine
/// ambiguity, two circuits of the same name on different monitors, survives
/// both rules and is still returned as a question.
pub fn narrow(matches: &[Match], query: &str) -> Vec<Match> {
    let exact: Vec<Match> = matches
        .iter()
        .filter(|m| m.label.eq_ignore_ascii_case(query.trim()))
        .cloned()
        .collect();
    let pool = if exact.is_empty() {
        matches.to_vec()
    } else {
        exact
    };

    // Only collapse legs into a merge when every branch in the pool is part of
    // *some* merge and they all sit on the same monitor as the single merged
    // candidate. Two unmerged circuits sharing a name must stay ambiguous.
    let merged: Vec<&Match> = pool.iter().filter(|m| m.kind == "merged").collect();
    if merged.len() == 1 {
        let gid = merged[0].device_gid;
        let legs_of_it = pool
            .iter()
            .filter(|m| m.kind != "merged")
            .all(|m| m.kind == "branch" && m.merged_into.is_some() && m.device_gid == gid);
        if legs_of_it {
            return vec![merged[0].clone()];
        }
    }
    pool
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

/// Render an instant in the caller's own timezone.
///
/// **Every timestamp that leaves here is local, not UTC.** A model handed
/// `2026-08-17T22:44:00+00:00` will say "at 22:44", which is four hours wrong
/// for the person reading it — and wrong in a way that looks like a fact rather
/// than a units mistake. The offset is on the string, so anything that wants
/// UTC can still recover it.
fn at(t: DateTime<Utc>, zone: FixedOffset) -> String {
    t.with_timezone(&zone).to_rfc3339()
}

pub fn answer(c: &mut Client, request: &Request, zone: FixedOffset) -> Result<Value, String> {
    match request {
        Request::Describe => Ok(crate::agent::describe()),
        Request::Channels => channels(c),
        Request::Now => now(c, zone),
        Request::Usage { span, kind } => usage(c, span, *kind, zone),
        Request::Series { query, span, scale } => series(c, query, span, *scale, zone),
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

fn now(c: &mut Client, zone: FixedOffset) -> Result<Value, String> {
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
        .map(|(label, watts, moment)| {
            json!({"circuit": label, "watts": watts.round(), "at": at(*moment, zone)})
        })
        .collect();
    Ok(json!({
        "ok": true,
        "unit": "W",
        "as_of": latest.map(|t| at(t, zone)),
        // Said out loud because it is the difference between "the house is
        // idle" and "the collector stopped an hour ago", which look identical
        // in a list of small numbers.
        "note": "Newest reading per circuit within the last 30 minutes. A circuit absent from \
                 this list has not reported recently.",
        "count": circuits.len(),
        "circuits": circuits
    }))
}

fn usage(c: &mut Client, span: &Span, kind: Kind, zone: FixedOffset) -> Result<Value, String> {
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
        "from": at(span.from, zone),
        "to": at(span.to, zone),
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
    zone: FixedOffset,
) -> Result<Value, String> {
    // Resolve the circuit first, and say so when it is ambiguous rather than
    // picking one. The same posture as Planner's `ambiguous`: two circuits here
    // are genuinely named the same thing on different monitors.
    let rows = c
        .query(
            &format!(
                "SELECT c.device_gid, c.channel_num, {LABEL} AS label, c.kind, c.merged_into
                 FROM channel c
                 WHERE ({LABEL}) ILIKE $1
                    OR c.device_gid || '/' || c.channel_num = $2
                 ORDER BY label",
            ),
            &[&format!("%{query}%"), &query],
        )
        .map_err(|e| db("cannot look up a circuit", e))?;
    let found = narrow(
        &rows
            .iter()
            .map(|r| Match {
                device_gid: r.get(0),
                channel_num: r.get(1),
                label: r.get(2),
                kind: r.get(3),
                merged_into: r.get(4),
            })
            .collect::<Vec<_>>(),
        query,
    );

    if found.is_empty() {
        return Ok(json!({
            "ok": false, "error": "no-such-circuit",
            "message": format!("Nothing here is called {query:?}. `channels` lists them.")
        }));
    }
    if found.len() > 1 {
        let candidates: Vec<Value> = found
            .iter()
            .map(|m| {
                json!({
                    "circuit": m.label,
                    "channel": format!("{}/{}", m.device_gid, m.channel_num),
                })
            })
            .collect();
        return Ok(json!({
            "ok": false, "error": "ambiguous",
            "message": format!("{} circuits match {query:?}.", candidates.len()),
            "candidates": candidates
        }));
    }

    let gid = found[0].device_gid;
    let num = found[0].channel_num.clone();
    let label = found[0].label.clone();

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
                "at": at(r.get::<_, DateTime<Utc>>(0), zone),
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

    fn m(gid: i64, num: &str, label: &str, kind: &str, merged: Option<&str>) -> Match {
        Match {
            device_gid: gid,
            channel_num: num.into(),
            label: label.into(),
            kind: kind.into(),
            merged_into: merged.map(str::to_string),
        }
    }

    #[test]
    fn a_merged_circuit_beats_its_own_legs() {
        // The real case: "Water Heater" matched legs 7 and 8 and merged 99.
        // That is one circuit described twice, not a question for the user.
        let matches = vec![
            m(415375, "7", "Water Heater", "branch", Some("Merged_99")),
            m(415375, "8", "Water Heater", "branch", Some("Merged_99")),
            m(415375, "99", "Water Heater", "merged", None),
        ];
        let narrowed = narrow(&matches, "Water Heater");
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].channel_num, "99");
    }

    #[test]
    fn an_exact_name_beats_a_substring() {
        // "GeoThermal" also matched "GeoThermal Blower" and "GeoThermal Aux
        // Heat". Asking somebody to disambiguate a name they typed exactly is
        // the wrong kind of careful.
        let matches = vec![
            m(415375, "97", "GeoThermal", "merged", None),
            m(415375, "100", "GeoThermal Blower", "merged", None),
            m(415375, "104", "GeoThermal Aux Heat", "merged", None),
        ];
        let narrowed = narrow(&matches, "GeoThermal");
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].label, "GeoThermal");
    }

    #[test]
    fn two_real_circuits_of_the_same_name_stay_a_question() {
        // The same name on two different monitors is genuine ambiguity and
        // must survive both rules — guessing here is a wrong answer about the
        // wrong half of the house.
        let matches = vec![
            m(415375, "97", "GeoThermal", "merged", None),
            m(422778, "97", "GeoThermal", "merged", None),
        ];
        assert_eq!(narrow(&matches, "GeoThermal").len(), 2);
    }

    #[test]
    fn an_unmerged_leg_sharing_a_name_with_a_merge_stays_a_question() {
        // A branch that belongs to no merge is its own circuit, so collapsing
        // it into a similarly-named merge would silently answer about
        // something else.
        let matches = vec![
            m(415375, "99", "Water Heater", "merged", None),
            m(422818, "4", "Water Heater", "branch", None),
        ];
        assert_eq!(narrow(&matches, "Water Heater").len(), 2);
    }

    #[test]
    fn a_substring_match_is_still_offered_when_nothing_matches_exactly() {
        let matches = vec![m(415375, "100", "GeoThermal Blower", "merged", None)];
        assert_eq!(narrow(&matches, "blower").len(), 1);
    }

    #[test]
    fn an_unnamed_circuit_still_gets_a_label_that_identifies_it() {
        assert!(LABEL.contains("device_name"));
        assert!(LABEL.contains("channel_num"));
    }
}
