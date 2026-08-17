//! The Emporia API, as URLs and as response shapes. Nothing here opens a
//! socket — `http.rs` does that, and it is the only file that does.
//!
//! The seam is the same one the sibling apps keep between `model/source/*` and
//! `ui/http.rs`, and it buys the same thing: every URL this service can build
//! and every body it can be handed are testable against fixtures, with no
//! network and no account.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::plan::{Channel, Fetch};
use crate::scale::Scale;

pub const HOST: &str = "https://api.emporiaenergy.com";

/// Which of the three kinds of channel a number names.
///
/// This distinction is not decoration. A merged channel is the *sum* of two
/// branch legs — the dryer is legs 11 and 12, and merged channel 101 is "Clothes
/// Dryer" — so a query that adds branches and merged channels together reports
/// the house using twice the power it uses. Storing the kind is what lets a
/// query be written that cannot make that mistake by accident.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChannelKind {
    /// The mains, which arrive as the single channel `1,2,3`.
    Main,
    /// A physical CT, `1`–`16`.
    Branch,
    /// Two legs of one 240 V circuit, presented as one, from `97` up.
    Merged,
}

impl ChannelKind {
    pub fn of(channel_num: &str) -> ChannelKind {
        if channel_num.contains(',') {
            return ChannelKind::Main;
        }
        match channel_num.parse::<u32>() {
            // The boundary is the API's, not ours: branch CTs are 1–16 and the
            // merged pseudo-channels observed on these devices start at 97.
            // Anything between is unseen rather than impossible, and calling it
            // a branch is the reading that cannot silently double-count.
            Ok(n) if n >= 97 => ChannelKind::Merged,
            Ok(_) => ChannelKind::Branch,
            Err(_) => ChannelKind::Main,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Main => "main",
            ChannelKind::Branch => "branch",
            ChannelKind::Merged => "merged",
        }
    }
}

pub fn devices_url() -> String {
    format!("{HOST}/customers/devices")
}

/// `getChartUsage` is the only call that takes a time range, which is why
/// backfill and catch-up both go through it one channel at a time.
pub fn chart_url(f: &Fetch) -> String {
    format!(
        "{HOST}/AppAPI?apiMethod=getChartUsage&deviceGid={gid}&channel={ch}&start={start}&end={end}&scale={scale}&energyUnit=KilowattHours",
        gid = f.series.device_gid,
        ch = encode(&f.series.channel_num),
        start = encode(&rfc3339(f.start)),
        end = encode(&rfc3339(f.end)),
        scale = f.series.scale.api_name(),
    )
}

/// The API spells an instant without fractional seconds and rejects an offset
/// that is not `Z`. `chrono`'s default RFC 3339 renders `+00:00`, and PyEmVue
/// carries a note about the API disliking timezone data on the same parameter —
/// so this is written out rather than left to a formatter's default.
fn rfc3339(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Percent-encoding for the handful of characters that actually occur here:
/// `,` and `:` in channel numbers and instants. A general-purpose encoder is a
/// dependency for two match arms.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---- responses ----

#[derive(Debug, Deserialize)]
pub struct Account {
    #[serde(rename = "customerGid")]
    pub customer_gid: i64,
    #[serde(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub devices: Vec<Device>,
}

#[derive(Debug, Deserialize)]
pub struct Device {
    #[serde(rename = "deviceGid")]
    pub device_gid: i64,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub firmware: Option<String>,
    #[serde(rename = "locationProperties", default)]
    pub location: Option<Location>,
    #[serde(default)]
    pub channels: Vec<ApiChannel>,
    /// The branch channels hang off a nested device rather than the `VUE003`
    /// itself — a `WAT001` carrying the same `deviceGid`. Flattening is
    /// therefore safe and is what `channels_of` does.
    #[serde(default)]
    pub devices: Vec<Device>,
}

#[derive(Debug, Deserialize)]
pub struct Location {
    #[serde(rename = "deviceName", default)]
    pub device_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ApiChannel {
    #[serde(rename = "channelNum")]
    pub channel_num: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "channelMultiplier", default)]
    pub multiplier: Option<f64>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    /// Which merged pseudo-channel this leg was folded into, if any.
    ///
    /// This is what makes "every circuit, counted once" answerable without a
    /// heuristic: a merged channel plus the branch legs that belong to no merge
    /// is exactly the set of things a person would call a circuit. Guessing it
    /// from matching names would break on the two channels here already named
    /// the same thing on purpose.
    #[serde(rename = "mergedChannelId", default)]
    pub merged_into: Option<String>,
}

/// One flattened channel, with the name a person gave it.
#[derive(Clone, Debug, PartialEq)]
pub struct NamedChannel {
    pub device_gid: i64,
    pub device_name: Option<String>,
    pub channel_num: String,
    pub name: Option<String>,
    pub kind: ChannelKind,
    pub multiplier: Option<f64>,
    /// The merged channel this leg belongs to, if any. `None` on a merged
    /// channel itself and on a branch that stands alone.
    pub merged_into: Option<String>,
}

/// Every channel in the account, nested devices flattened in.
pub fn channels_of(account: &Account) -> Vec<NamedChannel> {
    let mut out = Vec::new();
    for d in &account.devices {
        walk(
            d,
            d.location.as_ref().and_then(|l| l.device_name.clone()),
            &mut out,
        );
    }
    // The same `deviceGid` appears twice — once as the VUE003 and once as its
    // nested WAT001 — so a channel could be collected twice if the two ever
    // listed one in common. Keyed dedup rather than trust.
    out.sort_by(|a, b| {
        (a.device_gid, a.channel_num.as_str()).cmp(&(b.device_gid, b.channel_num.as_str()))
    });
    out.dedup_by(|a, b| a.device_gid == b.device_gid && a.channel_num == b.channel_num);
    out
}

fn walk(d: &Device, inherited_name: Option<String>, out: &mut Vec<NamedChannel>) {
    let name = d
        .location
        .as_ref()
        .and_then(|l| l.device_name.clone())
        .or(inherited_name.clone());
    for c in &d.channels {
        out.push(NamedChannel {
            device_gid: d.device_gid,
            device_name: name.clone(),
            channel_num: c.channel_num.clone(),
            name: c.name.clone(),
            kind: ChannelKind::of(&c.channel_num),
            multiplier: c.multiplier,
            merged_into: c.merged_into.clone(),
        });
    }
    for nested in &d.devices {
        walk(nested, name.clone(), out);
    }
}

impl NamedChannel {
    pub fn as_plan_channel(&self) -> Channel {
        Channel {
            device_gid: self.device_gid,
            channel_num: self.channel_num.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ChartUsage {
    #[serde(rename = "firstUsageInstant")]
    pub first_usage_instant: Option<DateTime<Utc>>,
    #[serde(rename = "usageList", default)]
    pub usage_list: Vec<Option<f64>>,
}

/// One stored reading.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    pub instant: DateTime<Utc>,
    pub kwh: f64,
}

/// Turn a response into readings, dropping the nulls.
///
/// **A null is not a zero.** It means the cloud has nothing for that interval —
/// either the device was offline or the resolution has aged out — and writing
/// zeros would turn "we do not know" into "the house used no power", which is
/// a lie that averages and bills would both believe.
pub fn readings(usage: &ChartUsage, scale: Scale) -> Vec<Reading> {
    let Some(first) = usage.first_usage_instant else {
        return Vec::new();
    };
    usage
        .usage_list
        .iter()
        .enumerate()
        .filter_map(|(i, v)| {
            v.map(|kwh| Reading {
                instant: crate::plan::instant_at(first, i, scale),
                kwh,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Series;

    #[test]
    fn channel_kinds_follow_the_numbering_observed_on_these_devices() {
        assert_eq!(ChannelKind::of("1,2,3"), ChannelKind::Main);
        assert_eq!(ChannelKind::of("1"), ChannelKind::Branch);
        assert_eq!(ChannelKind::of("16"), ChannelKind::Branch);
        assert_eq!(ChannelKind::of("97"), ChannelKind::Merged);
        assert_eq!(ChannelKind::of("104"), ChannelKind::Merged);
    }

    #[test]
    fn a_chart_url_encodes_the_comma_in_a_mains_channel_and_the_colons_in_an_instant() {
        let f = Fetch {
            series: Series {
                device_gid: 415375,
                channel_num: "1,2,3".into(),
                scale: Scale::Hour,
            },
            start: DateTime::parse_from_rfc3339("2025-01-29T12:34:40Z")
                .unwrap()
                .with_timezone(&Utc),
            end: DateTime::parse_from_rfc3339("2025-03-03T20:34:40Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        let u = chart_url(&f);
        assert!(u.contains("channel=1%2C2%2C3"), "{u}");
        assert!(u.contains("start=2025-01-29T12%3A34%3A40Z"), "{u}");
        assert!(u.contains("scale=1H"), "{u}");
        assert!(!u.contains("+00:00"), "an offset the API rejects: {u}");
    }

    // Trimmed from the live response on 2026-08-17. The nesting is the point:
    // the branch channels are on a WAT001 inside the VUE003, sharing its gid.
    const DEVICES: &str = r#"{
      "customerGid": 279350,
      "createdAt": "2025-01-29T12:34:40Z",
      "devices": [{
        "deviceGid": 415375,
        "model": "VUE003",
        "firmware": "Vue3-812",
        "locationProperties": {"deviceName": "basement (black)"},
        "channels": [{"channelNum": "1,2,3", "name": null, "type": "Main", "channelMultiplier": 1}],
        "devices": [{
          "deviceGid": 415375,
          "model": "WAT001",
          "channels": [
            {"channelNum": "11", "name": "Dryer", "type": "FiftyAmp", "channelMultiplier": 1},
            {"channelNum": "12", "name": "Dryer", "type": "FiftyAmp", "channelMultiplier": 1},
            {"channelNum": "101", "name": "Clothes Dryer", "type": "Merged", "channelMultiplier": 1}
          ]
        }]
      }]
    }"#;

    #[test]
    fn the_device_tree_flattens_to_every_channel_with_its_kind() {
        let a: Account = serde_json::from_str(DEVICES).unwrap();
        let cs = channels_of(&a);
        assert_eq!(cs.len(), 4);
        let dryer = cs.iter().find(|c| c.channel_num == "101").unwrap();
        assert_eq!(dryer.kind, ChannelKind::Merged);
        assert_eq!(dryer.name.as_deref(), Some("Clothes Dryer"));
        // The nested WAT001 has no name of its own; the channel belongs to the
        // box a person named.
        assert_eq!(dryer.device_name.as_deref(), Some("basement (black)"));
    }

    #[test]
    fn the_account_start_is_read_because_it_bounds_every_backfill() {
        let a: Account = serde_json::from_str(DEVICES).unwrap();
        assert_eq!(a.created_at.to_rfc3339(), "2025-01-29T12:34:40+00:00");
    }

    #[test]
    fn nulls_are_dropped_rather_than_stored_as_zero() {
        let u: ChartUsage = serde_json::from_str(
            r#"{"firstUsageInstant":"2026-08-17T12:00:00Z","usageList":[1.5,null,2.5]}"#,
        )
        .unwrap();
        let r = readings(&u, Scale::Hour);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].instant.to_rfc3339(), "2026-08-17T12:00:00+00:00");
        // The third point keeps its own hour — the gap is not closed up.
        assert_eq!(r[1].instant.to_rfc3339(), "2026-08-17T14:00:00+00:00");
    }

    #[test]
    fn a_response_with_no_first_instant_yields_nothing_rather_than_guessing_one() {
        let u: ChartUsage = serde_json::from_str(r#"{"usageList":[1.0,2.0]}"#).unwrap();
        assert!(readings(&u, Scale::Hour).is_empty());
    }

    #[test]
    fn a_window_of_pure_nulls_is_an_empty_answer_not_an_error() {
        // This is what the far edge of retention looks like: 200, a first
        // instant, and nothing in it. Distinguishing it from a 400 is what
        // stops the backfill treating the end of history as a bug.
        let u: ChartUsage = serde_json::from_str(
            r#"{"firstUsageInstant":"2025-01-01T00:00:00Z","usageList":[null,null]}"#,
        )
        .unwrap();
        assert!(readings(&u, Scale::Minute).is_empty());
    }
}
