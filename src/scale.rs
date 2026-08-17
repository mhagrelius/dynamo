//! The five resolutions the API serves, and the two numbers that matter about
//! each: how much of one a single request may ask for, and how far back it
//! still answers.
//!
//! Both were measured against the live account on 2026-08-17 rather than read
//! from documentation — there is none. The ceilings are quoted by the API
//! itself in the 400 it returns when you exceed one, which is where these came
//! from. `DESIGN.md` has the table and the evidence.

use chrono::Duration;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scale {
    Minute,
    QuarterHour,
    Hour,
    Day,
}

/// `1S` is deliberately absent.
///
/// It exists and it works — 4000 seconds a request, about a week of history —
/// and storing it would be 86,400 rows per channel per day, which across
/// seventy-odd channel-series is some six million rows a day to answer
/// questions nobody has asked. Minute resolution is what the app itself charts
/// and it is already finer than the vendor keeps. If a second-resolution
/// question ever turns up, it is a variant here and a row in `SCALES`, not a
/// redesign.
pub const SCALES: [Scale; 4] = [Scale::Minute, Scale::QuarterHour, Scale::Hour, Scale::Day];

impl Scale {
    /// What the `scale` query parameter has to say. These spellings are the
    /// API's and are not derivable from the variant names — `1MIN` and `1MON`
    /// differ by one letter and mean a minute and a month.
    pub fn api_name(self) -> &'static str {
        match self {
            Scale::Minute => "1MIN",
            Scale::QuarterHour => "15MIN",
            Scale::Hour => "1H",
            Scale::Day => "1D",
        }
    }

    /// How long one point covers. Used to step through a `usageList`, which
    /// arrives as a bare array with only its first instant given.
    pub fn step(self) -> Duration {
        match self {
            Scale::Minute => Duration::minutes(1),
            Scale::QuarterHour => Duration::minutes(15),
            Scale::Hour => Duration::hours(1),
            Scale::Day => Duration::days(1),
        }
    }

    /// The most one request may span, verbatim from the API's own refusal.
    ///
    /// Asking for more is a 400 naming the limit, never a truncated answer, so
    /// this is the number chunking has to respect exactly rather than
    /// approximately.
    pub fn ceiling(self) -> Duration {
        match self {
            // "larger than the allowed limit of PT13H20M"
            Scale::Minute => Duration::minutes(800),
            // "larger than the allowed limit of PT168H"
            Scale::QuarterHour => Duration::hours(168),
            // "larger than the allowed limit of PT800H"
            Scale::Hour => Duration::hours(800),
            // "larger than the allowed limit of PT12000H"
            Scale::Day => Duration::hours(12000),
        }
    }

    /// How far back the cloud still answers, as measured.
    ///
    /// `None` means "as far as the account goes" — hourly and daily were
    /// non-null back to the customer's `createdAt` and there is no evidence of
    /// a floor.
    ///
    /// These are the reason the first run is worth doing at all: minute data is
    /// present at 7 days and null at 7.5, so the vendor keeps a week of the
    /// resolution anybody actually wants. The margin below is deliberate — a
    /// retention edge is a rolling one and asking for a little less than the
    /// measurement avoids spending the whole first pass on windows of nulls.
    pub fn retained(self) -> Option<Duration> {
        match self {
            Scale::Minute => Some(Duration::days(6)),
            // Non-null at 365 days, null at 400. Where exactly it starts is the
            // backfill's business to discover, not this table's to guess: a
            // year is the conservative floor and `plan` stops early when a
            // window comes back empty.
            Scale::QuarterHour => Some(Duration::days(360)),
            Scale::Hour => None,
            Scale::Day => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceilings_are_whole_numbers_of_points() {
        // A ceiling that is not a multiple of the step would make the last
        // chunk of a range ask for a fraction of a point, and the API's
        // refusal is exact.
        for s in SCALES {
            assert_eq!(
                s.ceiling().num_seconds() % s.step().num_seconds(),
                0,
                "{:?} ceiling is not a whole number of steps",
                s
            );
        }
    }

    #[test]
    fn every_scale_can_hold_its_own_retention_in_a_few_chunks() {
        // Not a property of the API — a sanity check on the pair of numbers.
        // If a retention window ever needed thousands of chunks at its own
        // ceiling, the first run would be an afternoon of requests and that
        // would be worth noticing here rather than on the NAS.
        for s in SCALES {
            if let Some(r) = s.retained() {
                let chunks = r.num_seconds() / s.ceiling().num_seconds();
                assert!(chunks < 100, "{:?} needs {} chunks", s, chunks);
            }
        }
    }

    #[test]
    fn api_names_are_distinct() {
        let mut names: Vec<_> = SCALES.iter().map(|s| s.api_name()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len());
    }
}
