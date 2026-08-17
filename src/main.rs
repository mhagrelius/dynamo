use std::process::ExitCode;

use chrono::{DateTime, Local, Utc};

use dynamo::config::Config;
use dynamo::emporia::{self, Account};
use dynamo::http::{Client, Fault, Pace};
use dynamo::plan::{self, Horizon};
use dynamo::store;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--health") => health(),
        Some("--once") => run(true),
        // The read-only JSON interface Familiar drives. Same shape as
        // `planner agent` and `magpie agent`.
        Some("agent") => agent(&args[1..]),
        None => run(false),
        Some(other) => {
            eprintln!("dynamo: unknown argument {other}");
            eprintln!("usage: dynamo [--once | --health | agent <verb>]");
            ExitCode::FAILURE
        }
    }
}

/// What Container Manager runs, because the image carries no curl.
///
/// It asks the database rather than a socket of our own: this service listens
/// on nothing, and the two ways it fails — Postgres unreachable, refresh token
/// dead — are both visible from the heartbeat row. A green dot on a container
/// that has quietly stopped collecting is the outcome this exists to prevent.
fn health() -> ExitCode {
    let (db, stale) = match Config::health_from_env() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("unhealthy: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut client = match store::connect(&db) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("unhealthy: {e}");
            return ExitCode::FAILURE;
        }
    };
    let h = match store::health(&mut client) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("unhealthy: {e}");
            return ExitCode::FAILURE;
        }
    };
    if !h.ok {
        eprintln!(
            "unhealthy: {}",
            h.last_error.unwrap_or_else(|| "sign-in failed".into())
        );
        return ExitCode::FAILURE;
    }
    match h.last_success {
        // A first run may legitimately take a while before its first pass
        // completes — six thousand requests, paced. No success yet is not yet
        // a failure, and `start_period` in the compose file is the other half
        // of this.
        None => {
            println!("starting: no pass has completed yet");
            ExitCode::SUCCESS
        }
        Some(t) => {
            let age = (Utc::now() - t).num_seconds();
            if age > stale {
                eprintln!("unhealthy: last success was {age}s ago, limit is {stale}s");
                ExitCode::FAILURE
            } else {
                let credential = match h.authenticated_at {
                    Some(a) => format!(", credential {} days old", (Utc::now() - a).num_days()),
                    None => String::new(),
                };
                println!("ok: last success {age}s ago{credential}");
                ExitCode::SUCCESS
            }
        }
    }
}

/// `dynamo agent <verb>` — read, print JSON, exit.
///
/// **Every failure is JSON too, and the exit code is still 0 for a refusal.**
/// Familiar reads stdout and hands it to a model; a non-zero exit with a
/// message on stderr becomes "the tool failed", which is the wrong thing to
/// tell it when the truth is "that circuit does not exist, here is how to list
/// them". A non-zero exit is reserved for the cases where nothing was asked at
/// all — no credentials, no database.
fn agent(args: &[String]) -> ExitCode {
    // The machine's own timezone, because a day means the user's day. Inside
    // the container this is UTC and nothing asks it for a day anyway; on a
    // laptop it is the zone the house is in.
    let zone = *Local::now().offset();
    let request = match dynamo::agent::parse(args, Utc::now(), zone) {
        Ok(r) => r,
        Err(dynamo::agent::Refusal(why)) => {
            println!(
                "{}",
                serde_json::json!({"ok": false, "error": "bad-request", "message": why})
            );
            return ExitCode::SUCCESS;
        }
    };
    let db = match Config::reader_from_env_or_file() {
        Ok(db) => db,
        Err(e) => {
            eprintln!("dynamo: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut client = match store::connect(&db) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dynamo: {e}");
            return ExitCode::FAILURE;
        }
    };
    match dynamo::answer::answer(&mut client, &request, zone) {
        Ok(v) => {
            println!("{v}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!(
                "{}",
                serde_json::json!({"ok": false, "error": "query-failed", "message": e})
            );
            ExitCode::SUCCESS
        }
    }
}

fn run(once: bool) -> ExitCode {
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dynamo: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut db = match store::connect(&cfg.db) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dynamo: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = store::migrate(&mut db) {
        eprintln!("dynamo: {e}");
        return ExitCode::FAILURE;
    }

    let mut api = Client::new(cfg.refresh_token.clone());

    loop {
        match pass(&mut api, &mut db, &cfg) {
            Ok(written) => {
                let signed_in = api
                    .authenticated_at()
                    .and_then(|t| DateTime::from_timestamp(t, 0));
                let _ = store::record_success(&mut db, signed_in);
                let held = store::counts(&mut db).unwrap_or_default();
                let summary = held
                    .iter()
                    .map(|(s, n)| format!("{} {n}", s.api_name()))
                    .collect::<Vec<_>>()
                    .join(", ");
                // The credential's age, every pass. It cannot be turned into a
                // countdown — the refresh token is an encrypted JWE and its
                // expiry is not readable from it, nor from any call we can make
                // — but an age that keeps climbing is the only warning
                // available, and it is the number that answers "how long do
                // these last" the day one is finally refused.
                let age = match signed_in {
                    Some(t) => format!("; credential {} days old", (Utc::now() - t).num_days()),
                    None => String::new(),
                };
                println!("pass complete: {written} readings written; holding {summary}{age}");
            }
            Err(Fault::Unauthorised(m)) => {
                // Terminal. The refresh token is not rotated by use, so it has
                // a fixed lifetime and when it ends no retry brings it back —
                // a person has to paste a new one into `.env`. Saying so once,
                // loudly, and going unhealthy beats a log that fills with the
                // same line every minute.
                eprintln!("dynamo: {m}");
                eprintln!("dynamo: DYNAMO_REFRESH_TOKEN is no longer accepted. See README.md,");
                eprintln!("dynamo: 'Re-seeding the refresh token'. Nothing will be collected");
                eprintln!("dynamo: until it is replaced.");
                let _ = store::record_failure(&mut db, &format!("sign-in rejected: {m}"), true);
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("dynamo: pass failed: {e}");
                let _ = store::record_failure(&mut db, &e.to_string(), false);
            }
        }

        if once {
            return ExitCode::SUCCESS;
        }
        std::thread::sleep(std::time::Duration::from_secs(cfg.interval_secs));
    }
}

fn pass(api: &mut Client, db: &mut postgres::Client, cfg: &Config) -> Result<usize, Fault> {
    let body = api.get(&emporia::devices_url())?;
    let account: Account = serde_json::from_str(&body)
        .map_err(|e| Fault::Transient(format!("unreadable device list: {e}")))?;

    let channels = emporia::channels_of(&account);
    store::save_channels(db, &channels).map_err(Fault::Transient)?;

    let marks = store::watermarks(db).map_err(Fault::Transient)?;
    let horizon = Horizon {
        account_created: account.created_at,
        now: Utc::now(),
    };
    let plan_channels: Vec<_> = channels.iter().map(|c| c.as_plan_channel()).collect();
    let work = plan::plan(
        &plan_channels,
        &|s| marks.get(s).cloned().unwrap_or_default(),
        &horizon,
    );

    if work.len() > 200 {
        // A first run, or a long outage. Worth one line rather than silence
        // for the half hour it takes.
        println!(
            "backfilling: {} windows across {} channels",
            work.len(),
            channels.len()
        );
    }

    let mut pace = Pace::per_second(cfg.rate);
    let mut written = 0usize;
    for fetch in &work {
        pace.wait();
        let url = emporia::chart_url(fetch);
        let body = match api.get(&url) {
            Ok(b) => b,
            // Terminal faults end the pass; the caller decides what that means.
            Err(e @ Fault::Unauthorised(_)) => return Err(e),
            // A 400 means our chunking asked for something the API refuses,
            // which is a bug in `scale.rs` rather than weather. Named, skipped,
            // and the pass carries on so one bad scale does not stop the rest.
            Err(Fault::Refused { status, message }) => {
                eprintln!(
                    "dynamo: refused ({status}) for {} {} {}: {message}",
                    fetch.series.device_gid,
                    fetch.series.channel_num,
                    fetch.series.scale.api_name()
                );
                continue;
            }
            Err(e) => {
                eprintln!("dynamo: {e}");
                continue;
            }
        };
        let usage: emporia::ChartUsage = match serde_json::from_str(&body) {
            Ok(u) => u,
            Err(e) => {
                eprintln!("dynamo: unreadable usage body: {e}");
                continue;
            }
        };
        let readings = emporia::readings(&usage, fetch.series.scale);
        match store::save_readings(db, &fetch.series, &readings) {
            Ok(n) => written += n,
            Err(e) => eprintln!("dynamo: {e}"),
        }
    }
    Ok(written)
}
