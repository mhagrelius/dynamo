//! What to ask for next.
//!
//! There is no separate first-run path. For every `(device, channel, scale)`
//! the store knows the newest instant it holds; this turns that, a clock and
//! the account's own start date into a list of windows to fetch. An empty
//! database makes the start the earliest the scale serves, which *is* the
//! one-time historical load; a four-minute restart makes it four minutes. One
//! mechanism, so the awkward case is the ordinary case and gets exercised on
//! every run.
//!
//! Pure on purpose. Every decision worth arguing about — how far back to go,
//! how much to overlap, what to do when the vendor no longer keeps a
//! resolution — is settled here against a fixed clock, with tests, rather than
//! inside a loop that also opens sockets.

use chrono::{DateTime, Duration, Utc};

use crate::scale::{Scale, SCALES};

/// One channel at one resolution. The unit the watermark is kept per.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Series {
    pub device_gid: i64,
    pub channel_num: String,
    pub scale: Scale,
}

/// One request. `start` and `end` are inclusive of the points they bound in
/// the sense the API means, which is why chunks are stepped by the ceiling
/// exactly and overlap is spelled out rather than left to rounding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetch {
    pub series: Series,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// What the store already holds for one channel, per scale.
#[derive(Clone, Debug, Default)]
pub struct Watermark {
    pub newest: Option<DateTime<Utc>>,
}

/// A channel as the device list describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Channel {
    pub device_gid: i64,
    pub channel_num: String,
}

/// Everything the planner needs that is not a channel list.
#[derive(Clone, Debug)]
pub struct Horizon {
    /// The customer's `createdAt`. Nothing exists before it, so no scale is
    /// asked to look further back however generous its retention is — that
    /// would spend the first run on windows of nulls.
    pub account_created: DateTime<Utc>,
    pub now: DateTime<Utc>,
}

/// One step of overlap when resuming.
///
/// The watermark is the newest instant *stored*, and asking from exactly there
/// re-fetches one point we already have. That is deliberate and it is cheaper
/// than the alternative: the upsert makes a repeat free, while an off-by-one
/// in the other direction is a permanent one-point hole that nothing would
/// ever go back for. It also lets the cloud revise the most recent interval,
/// which it does as late readings arrive from the device.
fn resume_from(newest: DateTime<Utc>, scale: Scale) -> DateTime<Utc> {
    newest - scale.step()
}

/// The earliest instant worth asking a scale for.
fn floor(scale: Scale, h: &Horizon) -> DateTime<Utc> {
    match scale.retained() {
        Some(window) => {
            let rolling = h.now - window;
            // Whichever is later: there is no data before the account existed,
            // and none older than the vendor keeps.
            if rolling > h.account_created {
                rolling
            } else {
                h.account_created
            }
        }
        None => h.account_created,
    }
}

/// Build the work list.
///
/// Ordered by scale, minutes first. That ordering is not cosmetic: minute data
/// is the perishable half and the only reason this service exists, so on a
/// first run it should land before an hour of daily history that will still be
/// there tomorrow.
pub fn plan(
    channels: &[Channel],
    watermarks: &dyn Fn(&Series) -> Watermark,
    h: &Horizon,
) -> Vec<Fetch> {
    let mut out = Vec::new();
    for scale in SCALES {
        for channel in channels {
            let series = Series {
                device_gid: channel.device_gid,
                channel_num: channel.channel_num.clone(),
                scale,
            };
            let start = match watermarks(&series).newest {
                Some(newest) => {
                    let resumed = resume_from(newest, scale);
                    let f = floor(scale, h);
                    // A watermark older than the retention floor is a gap the
                    // vendor can no longer fill — a fortnight down means the
                    // minutes are gone. Start at the floor and take what is
                    // there; the hole stays a hole, honestly, rather than
                    // being papered over by requests that answer nulls.
                    if resumed < f {
                        f
                    } else {
                        resumed
                    }
                }
                None => floor(scale, h),
            };
            if start >= h.now {
                continue;
            }
            out.extend(chunks(series, start, h.now, scale));
        }
    }
    out
}

/// Split a range into windows the API will accept.
///
/// The ceiling is exact and exceeding it is a 400, so the last chunk is short
/// rather than the first being long.
fn chunks(series: Series, start: DateTime<Utc>, end: DateTime<Utc>, scale: Scale) -> Vec<Fetch> {
    let mut out = Vec::new();
    let ceiling = scale.ceiling();
    let mut cursor = start;
    while cursor < end {
        let stop = if end - cursor > ceiling {
            cursor + ceiling
        } else {
            end
        };
        out.push(Fetch {
            series: series.clone(),
            start: cursor,
            end: stop,
        });
        cursor = stop;
    }
    out
}

/// The instant a point at `index` covers, given what the response said its
/// first one was.
///
/// `getChartUsage` returns a bare array and a single `firstUsageInstant`; the
/// rest is arithmetic, and getting it wrong shifts a whole window silently
/// rather than failing.
pub fn instant_at(first: DateTime<Utc>, index: usize, scale: Scale) -> DateTime<Utc> {
    first + Duration::seconds(scale.step().num_seconds() * index as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn horizon() -> Horizon {
        Horizon {
            account_created: at("2025-01-29T12:34:40Z"),
            now: at("2026-08-17T18:00:00Z"),
        }
    }

    fn one_channel() -> Vec<Channel> {
        vec![Channel {
            device_gid: 415375,
            channel_num: "1,2,3".into(),
        }]
    }

    fn empty(_: &Series) -> Watermark {
        Watermark { newest: None }
    }

    #[test]
    fn an_empty_store_asks_hourly_all_the_way_back_to_the_account() {
        let h = horizon();
        let p = plan(&one_channel(), &empty, &h);
        let hourly: Vec<_> = p.iter().filter(|f| f.series.scale == Scale::Hour).collect();
        assert_eq!(hourly.first().unwrap().start, h.account_created);
        assert_eq!(hourly.last().unwrap().end, h.now);
    }

    #[test]
    fn an_empty_store_does_not_ask_minutes_for_more_than_the_vendor_keeps() {
        let h = horizon();
        let p = plan(&one_channel(), &empty, &h);
        let first = p
            .iter()
            .find(|f| f.series.scale == Scale::Minute)
            .expect("minutes planned");
        // Six days, not nineteen months. Asking further back is not an error —
        // it is worse, a 200 with a list of nulls — so the floor is the only
        // thing keeping the first run from spending most of itself on nothing.
        assert_eq!(first.start, h.now - Duration::days(6));
    }

    #[test]
    fn no_chunk_exceeds_the_ceiling_the_api_quoted() {
        let h = horizon();
        for f in plan(&one_channel(), &empty, &h) {
            assert!(
                f.end - f.start <= f.series.scale.ceiling(),
                "{:?} spans {:?}, ceiling is {:?}",
                f.series.scale,
                f.end - f.start,
                f.series.scale.ceiling()
            );
        }
    }

    #[test]
    fn chunks_are_contiguous_and_cover_the_whole_range() {
        let h = horizon();
        let p = plan(&one_channel(), &empty, &h);
        for scale in SCALES {
            let s: Vec<_> = p.iter().filter(|f| f.series.scale == scale).collect();
            for pair in s.windows(2) {
                assert_eq!(
                    pair[0].end, pair[1].start,
                    "{:?} left a gap between chunks",
                    scale
                );
            }
            assert_eq!(s.last().unwrap().end, h.now);
        }
    }

    #[test]
    fn resuming_overlaps_by_one_point_rather_than_risking_a_hole() {
        let h = horizon();
        let newest = h.now - Duration::minutes(30);
        let marks = |s: &Series| Watermark {
            newest: if s.scale == Scale::Minute {
                Some(newest)
            } else {
                None
            },
        };
        let p = plan(&one_channel(), &marks, &h);
        let first = p.iter().find(|f| f.series.scale == Scale::Minute).unwrap();
        assert_eq!(first.start, newest - Duration::minutes(1));
    }

    #[test]
    fn a_watermark_older_than_retention_starts_at_the_floor_not_at_the_watermark() {
        // Down for a fortnight. The minutes in that gap no longer exist
        // anywhere, so asking for them is a long run of 200s full of nulls.
        let h = horizon();
        let stale = h.now - Duration::days(14);
        let marks = |s: &Series| Watermark {
            newest: if s.scale == Scale::Minute {
                Some(stale)
            } else {
                None
            },
        };
        let p = plan(&one_channel(), &marks, &h);
        let first = p.iter().find(|f| f.series.scale == Scale::Minute).unwrap();
        assert_eq!(first.start, h.now - Duration::days(6));
    }

    #[test]
    fn a_watermark_at_now_plans_nothing() {
        let h = horizon();
        let marks = |_: &Series| Watermark {
            newest: Some(h.now),
        };
        let p = plan(&one_channel(), &marks, &h);
        // The overlap still pulls the start one step back, so a single chunk
        // per scale is correct and expected — what must not happen is an
        // unbounded list or a chunk running past `now`.
        assert!(p.iter().all(|f| f.end <= h.now));
        assert!(p.len() <= SCALES.len());
    }

    #[test]
    fn minutes_are_planned_before_the_coarser_scales() {
        let h = horizon();
        let p = plan(&one_channel(), &empty, &h);
        let first_minute = p.iter().position(|f| f.series.scale == Scale::Minute);
        let first_day = p.iter().position(|f| f.series.scale == Scale::Day);
        assert!(first_minute.unwrap() < first_day.unwrap());
    }

    #[test]
    fn nothing_is_planned_before_the_account_existed() {
        let h = horizon();
        for f in plan(&one_channel(), &empty, &h) {
            assert!(f.start >= h.account_created, "{:?} predates the account", f);
        }
    }

    #[test]
    fn a_young_account_is_not_asked_for_a_year_of_quarter_hours() {
        // The retention floor is a rolling window; the account start is a hard
        // one. Whichever is later wins, and for an account opened last week
        // that is the account.
        let h = Horizon {
            account_created: at("2026-08-10T00:00:00Z"),
            now: at("2026-08-17T18:00:00Z"),
        };
        let p = plan(&one_channel(), &empty, &h);
        let q = p
            .iter()
            .find(|f| f.series.scale == Scale::QuarterHour)
            .unwrap();
        assert_eq!(q.start, h.account_created);
    }

    #[test]
    fn instants_step_by_the_scale() {
        let first = Utc.with_ymd_and_hms(2026, 8, 17, 12, 0, 0).unwrap();
        assert_eq!(
            instant_at(first, 3, Scale::QuarterHour),
            Utc.with_ymd_and_hms(2026, 8, 17, 12, 45, 0).unwrap()
        );
        assert_eq!(
            instant_at(first, 0, Scale::Minute),
            first,
            "the first point is the instant the response named"
        );
    }

    #[test]
    fn every_channel_gets_every_scale() {
        let h = horizon();
        let channels = vec![
            Channel {
                device_gid: 1,
                channel_num: "1,2,3".into(),
            },
            Channel {
                device_gid: 2,
                channel_num: "97".into(),
            },
        ];
        let p = plan(&channels, &empty, &h);
        for c in &channels {
            for s in SCALES {
                assert!(
                    p.iter().any(|f| f.series.device_gid == c.device_gid
                        && f.series.channel_num == c.channel_num
                        && f.series.scale == s),
                    "{:?} at {:?} was never planned",
                    c,
                    s
                );
            }
        }
    }
}
