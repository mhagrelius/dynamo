//! `dynamo agent <verb>` — the read-only JSON interface Familiar drives.
//!
//! The same shape as `planner agent` and `magpie agent`, because that is the
//! shape Familiar already knows how to gate, spawn and read. Two things differ,
//! and both are simplifications:
//!
//! **Every verb reads.** Planner's gating asks whether a verb mutates, and
//! anything unrecognised is gated because the answer might be yes. Here the
//! answer is no for all of them and cannot become yes: this binary's writes come
//! from the collector loop, and nothing reachable from `agent` opens a socket to
//! Emporia or writes a row. An unknown verb is still refused rather than
//! guessed at, but it is refused for being unknown, not for being dangerous.
//!
//! **There is no running instance to forward to.** Planner has to ride its own
//! command line because its store lives in the memory of the running app and a
//! second writer loses. Dynamo's store is Postgres, which is built for more than
//! one reader, so `agent` connects directly and the collector on the NAS neither
//! knows nor cares.
//!
//! **No `--flags`, ever** — the same rule as the siblings, for consistency
//! rather than for GOption's sake. Arguments are positional words and
//! `key=value` pairs.

use chrono::{DateTime, Datelike, Duration, FixedOffset, TimeZone, Utc};
use serde_json::{json, Value};

/// How many rows any verb will return before it starts saying so.
///
/// A `series` over a year of minutes is half a million points and the context
/// window is shared with the conversation. Truncation is reported in the
/// response — `truncated` and `matched` — because Familiar's `note_for` turns
/// that into "this is a page, not the whole list", and a silently short answer
/// is the one failure a model reports as fact.
pub const MAX_ROWS: usize = 400;

/// A parsed invocation. Pure: no clock of its own and no database.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Describe,
    Channels,
    /// The most recent reading for every channel.
    Now,
    /// Energy by circuit over a window.
    Usage {
        span: Span,
        kind: Kind,
    },
    /// One channel's readings over a window.
    Series {
        query: String,
        span: Span,
        scale: crate::scale::Scale,
    },
}

/// Which family of channels a question is about.
///
/// Defaults to `merged`, and that default is the whole reason this exists: the
/// merged pseudo-channels are the circuits a person names, and summing them
/// together with the branch legs they are made of double-counts every 240 V
/// appliance in the house.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Main,
    Branch,
    Merged,
    /// Everything a person would call a circuit: merged where a merge exists,
    /// and the branch legs that are not part of one.
    Circuits,
}

impl Kind {
    fn parse(word: &str) -> Option<Kind> {
        match word {
            "main" | "mains" | "total" => Some(Kind::Main),
            "branch" | "branches" | "legs" => Some(Kind::Branch),
            "merged" => Some(Kind::Merged),
            "circuits" | "circuit" | "all" => Some(Kind::Circuits),
            _ => None,
        }
    }
}

/// A window of time, resolved against a clock the caller supplies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// What the user typed, echoed back so a reply can name the period the same
    /// way the question did.
    pub named: &'static str,
}

/// What went wrong, in words a model can act on.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal(pub String);

/// Turn the words after `dynamo agent` into a request.
///
/// `now` is passed in rather than read, so every case below is testable against
/// a fixed clock — "yesterday" is otherwise a different answer every day.
pub fn parse(args: &[String], now: DateTime<Utc>, zone: FixedOffset) -> Result<Request, Refusal> {
    let words: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|w| !w.trim().is_empty())
        .collect();
    let mut words = words.as_slice();
    // A leading `agent` is tolerated: the prefix is fixed, and every character
    // of it a model has to reproduce is a character it can get wrong.
    if words.first() == Some(&"agent") {
        words = &words[1..];
    }
    if words.iter().any(|w| w.starts_with("--")) {
        return Err(Refusal(
            "`dynamo` takes no `--flags`. Arguments are positional words and key=value pairs, \
             like `usage yesterday kind=circuits` or `series 'Water Heater' today scale=1H`."
                .into(),
        ));
    }
    let Some((verb, rest)) = words.split_first() else {
        return Err(Refusal(
            "`dynamo` needs a verb. `describe` lists them, `channels` says what is measured, \
             `now` is the current draw, `usage <period>` totals energy by circuit."
                .into(),
        ));
    };

    let pairs = |key: &str| -> Option<String> {
        rest.iter()
            .find_map(|w| w.strip_prefix(&format!("{key}=")))
            .map(str::to_string)
    };
    let positional: Vec<&str> = rest.iter().copied().filter(|w| !w.contains('=')).collect();

    match *verb {
        "describe" | "help" => Ok(Request::Describe),
        "channels" | "circuits" => Ok(Request::Channels),
        "now" | "current" | "live" => Ok(Request::Now),
        "usage" | "energy" => {
            let span = span_of(positional.first().copied().unwrap_or("today"), now, zone)?;
            let kind = match pairs("kind") {
                Some(k) => Kind::parse(&k).ok_or_else(|| {
                    Refusal(format!(
                        "`kind={k}` is not one of `circuits`, `merged`, `branch` or `main`."
                    ))
                })?,
                None => Kind::Circuits,
            };
            Ok(Request::Usage { span, kind })
        }
        "series" | "history" => {
            let Some(query) = positional.first() else {
                return Err(Refusal(
                    "`series` needs a circuit to look at — a name like `Water Heater`, or a \
                     channel like `422818/4`. `channels` lists them."
                        .into(),
                ));
            };
            let span = span_of(positional.get(1).copied().unwrap_or("today"), now, zone)?;
            let scale = match pairs("scale") {
                Some(s) => scale_of(&s)?,
                // An hour a point keeps a day inside `MAX_ROWS`, where minutes
                // would truncate a day at a third of it.
                None => crate::scale::Scale::Hour,
            };
            Ok(Request::Series {
                query: (*query).to_string(),
                span,
                scale,
            })
        }
        other => Err(Refusal(format!(
            "`{other}` is not a dynamo verb. `describe` lists them."
        ))),
    }
}

fn scale_of(word: &str) -> Result<crate::scale::Scale, Refusal> {
    crate::scale::SCALES
        .iter()
        .copied()
        .find(|s| s.api_name().eq_ignore_ascii_case(word))
        .ok_or_else(|| {
            Refusal(format!(
                "`scale={word}` is not one of `1MIN`, `15MIN`, `1H` or `1D`."
            ))
        })
}

/// Resolve a period word.
///
/// **Days are boundaries in the user's own day, not multiples of 24 hours.**
/// "Yesterday" ending at this time yesterday would put half of this morning in
/// it, which is not what anybody means and is exactly the sort of wrong answer
/// that reads as right.
///
/// **And they are boundaries in the user's own *timezone*.** A first version
/// took midnight in UTC, which for anybody east or west of Greenwich is not
/// midnight: on this machine, in EDT, `today` began at eight o'clock the
/// previous evening and every daily figure quietly included four hours of the
/// day before. The offset is a parameter rather than read from the machine so
/// that these cases are testable somewhere other than one timezone.
fn span_of(word: &str, now: DateTime<Utc>, zone: FixedOffset) -> Result<Span, Refusal> {
    let local = now.with_timezone(&zone);
    let midnight = |d: DateTime<FixedOffset>| {
        zone.with_ymd_and_hms(d.year(), d.month(), d.day(), 0, 0, 0)
            .single()
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or(now)
    };
    let today = midnight(local);
    Ok(match word {
        "today" => Span {
            from: today,
            to: now,
            named: "today",
        },
        "yesterday" => Span {
            from: today - Duration::days(1),
            to: today,
            named: "yesterday",
        },
        "week" | "7d" => Span {
            from: today - Duration::days(6),
            to: now,
            named: "the last 7 days",
        },
        "month" | "30d" => Span {
            from: today - Duration::days(29),
            to: now,
            named: "the last 30 days",
        },
        "year" | "365d" => Span {
            from: today - Duration::days(364),
            to: now,
            named: "the last year",
        },
        "all" | "ever" => Span {
            // Before the account existed, so the query is bounded by the data.
            from: Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).single().unwrap(),
            to: now,
            named: "all of it",
        },
        other => {
            return Err(Refusal(format!(
                "`{other}` is not a period. Use `today`, `yesterday`, `week`, `month`, `year` \
                 or `all`."
            )))
        }
    })
}

/// What `describe` prints. The authority on this interface, the way
/// `planner agent describe` is for Planner's.
pub fn describe() -> Value {
    json!({
        "ok": true,
        "tool": "dynamo",
        "summary": "Household electricity, measured per circuit by three panel monitors and \
                    kept minute by minute. Read-only.",
        "reads_only": true,
        "verbs": [
            {"verb": "describe", "args": "", "does": "this"},
            {"verb": "channels", "args": "",
             "does": "every circuit that is measured, with the name it was given and which \
                      monitor it is on"},
            {"verb": "now", "args": "",
             "does": "what each circuit is drawing right now, in watts, newest reading"},
            {"verb": "usage", "args": "<period> [kind=circuits|merged|branch|main]",
             "does": "energy by circuit over a period, in kWh, biggest first"},
            {"verb": "series", "args": "<circuit> <period> [scale=1MIN|15MIN|1H|1D]",
             "does": "one circuit's readings over a period"}
        ],
        "periods": ["today", "yesterday", "week", "month", "year", "all"],
        "notes": [
            "A merged channel is the sum of two branch legs — the two halves of a 240 V \
             circuit. kind=circuits is the default and counts each circuit once; adding \
             merged and branch figures together double-counts every large appliance.",
            "Only one of the three monitors has mains CTs, so a whole-house total from \
             kind=main covers that panel and not the others.",
            "Minute resolution goes back about a week before this was installed and \
             indefinitely after; hourly and daily reach back to January 2025."
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    fn clock() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-17T19:30:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// EDT, the timezone this house is in, stated rather than inherited from
    /// whatever machine runs the suite.
    fn edt() -> FixedOffset {
        FixedOffset::west_opt(4 * 3600).unwrap()
    }

    /// Greenwich, for the cases that are about the words rather than the clock.
    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    #[test]
    fn the_fixed_prefix_is_tolerated_but_not_required() {
        assert_eq!(
            parse(&args("channels"), clock(), utc()),
            Ok(Request::Channels)
        );
        assert_eq!(
            parse(&args("agent channels"), clock(), utc()),
            Ok(Request::Channels)
        );
    }

    #[test]
    fn yesterday_is_a_whole_day_and_stops_at_midnight() {
        // Not "24 hours ago until now", which would fold half of this morning
        // into yesterday and give a confidently wrong number.
        let Ok(Request::Usage { span, .. }) = parse(&args("usage yesterday"), clock(), utc())
        else {
            panic!("usage should parse");
        };
        assert_eq!(span.from.to_rfc3339(), "2026-08-16T00:00:00+00:00");
        assert_eq!(span.to.to_rfc3339(), "2026-08-17T00:00:00+00:00");
    }

    #[test]
    fn today_runs_to_the_current_moment_rather_than_to_midnight_tonight() {
        let Ok(Request::Usage { span, .. }) = parse(&args("usage today"), clock(), utc()) else {
            panic!("usage should parse");
        };
        assert_eq!(span.from.to_rfc3339(), "2026-08-17T00:00:00+00:00");
        assert_eq!(span.to, clock());
    }

    #[test]
    fn usage_counts_each_circuit_once_by_default() {
        let Ok(Request::Usage { kind, .. }) = parse(&args("usage today"), clock(), utc()) else {
            panic!("usage should parse");
        };
        // The default that stops a 240 V appliance being counted twice.
        assert_eq!(kind, Kind::Circuits);
    }

    #[test]
    fn a_day_begins_at_local_midnight_and_not_at_utc_midnight() {
        // The bug this exists for. In EDT, UTC midnight is eight o'clock the
        // previous evening, so `today` silently carried four hours of
        // yesterday and every daily figure was wrong by however much the house
        // drew overnight. Nothing failed; the numbers were just not the ones
        // anybody asked for.
        let Ok(Request::Usage { span, .. }) = parse(&args("usage today"), clock(), edt()) else {
            panic!("usage should parse");
        };
        // 2026-08-17T19:30Z is 15:30 EDT, so today began at 04:00Z.
        assert_eq!(span.from.to_rfc3339(), "2026-08-17T04:00:00+00:00");
    }

    #[test]
    fn yesterday_is_a_local_day_end_to_end() {
        let Ok(Request::Usage { span, .. }) = parse(&args("usage yesterday"), clock(), edt())
        else {
            panic!("usage should parse");
        };
        assert_eq!(span.from.to_rfc3339(), "2026-08-16T04:00:00+00:00");
        assert_eq!(span.to.to_rfc3339(), "2026-08-17T04:00:00+00:00");
        // Exactly one day, not 24 hours that happen to look like one.
        assert_eq!((span.to - span.from).num_hours(), 24);
    }

    #[test]
    fn a_zone_east_of_greenwich_works_too() {
        // The other direction, because an off-by-one in the sign is easy and
        // invisible from one timezone.
        let berlin = FixedOffset::east_opt(2 * 3600).unwrap();
        let Ok(Request::Usage { span, .. }) = parse(&args("usage today"), clock(), berlin) else {
            panic!("usage should parse");
        };
        assert_eq!(span.from.to_rfc3339(), "2026-08-16T22:00:00+00:00");
    }

    #[test]
    fn a_period_is_optional_and_defaults_to_today() {
        let Ok(Request::Usage { span, .. }) = parse(&args("usage"), clock(), utc()) else {
            panic!("usage should parse");
        };
        assert_eq!(span.named, "today");
    }

    #[test]
    fn series_defaults_to_hourly_so_a_day_fits_in_one_answer() {
        let Ok(Request::Series { scale, query, .. }) =
            parse(&args("series Dryer today"), clock(), utc())
        else {
            panic!("series should parse");
        };
        assert_eq!(scale, crate::scale::Scale::Hour);
        assert_eq!(query, "Dryer");
    }

    #[test]
    fn a_flag_is_refused_with_what_to_write_instead() {
        let Err(Refusal(why)) = parse(&args("usage --period yesterday"), clock(), utc()) else {
            panic!("a flag should be refused");
        };
        assert!(why.contains("key=value"), "{why}");
    }

    #[test]
    fn an_unknown_verb_names_describe_rather_than_guessing() {
        let Err(Refusal(why)) = parse(&args("obliterate"), clock(), utc()) else {
            panic!("should refuse");
        };
        assert!(why.contains("describe"), "{why}");
    }

    #[test]
    fn an_unknown_period_lists_the_ones_that_work() {
        let Err(Refusal(why)) = parse(&args("usage since-tuesday"), clock(), utc()) else {
            panic!("should refuse");
        };
        assert!(why.contains("yesterday"), "{why}");
    }

    #[test]
    fn series_without_a_circuit_says_how_to_find_one() {
        let Err(Refusal(why)) = parse(&args("series"), clock(), utc()) else {
            panic!("should refuse");
        };
        assert!(why.contains("channels"), "{why}");
    }

    #[test]
    fn describe_declares_itself_read_only_and_warns_about_double_counting() {
        let d = describe();
        assert_eq!(d["reads_only"], json!(true));
        let notes = d["notes"].as_array().unwrap();
        assert!(
            notes
                .iter()
                .any(|n| n.as_str().unwrap().contains("double-count")),
            "the merged/branch trap has to be in describe, it is the one way to \
             misread this data badly"
        );
    }
}
