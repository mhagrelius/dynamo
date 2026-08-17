//! The only file that opens a socket.
//!
//! Everything it fetches is described somewhere else — `emporia.rs` builds the
//! URLs and reads the bodies, `cognito.rs` builds the auth request and reads
//! its answer. This performs them, holds the id token, and renews it before it
//! lapses.

use std::time::{Duration as StdDuration, Instant};

use crate::cognito::{self, Refreshed};

/// A failure worth telling apart from the others.
#[derive(Debug)]
pub enum Fault {
    /// The refresh token will not do it any more. Terminal: no retry helps,
    /// and the operator has to paste a new one in.
    Unauthorised(String),
    /// The API refused the request itself — the chunking asked for more than a
    /// scale's ceiling, say. A bug here, not a blip, so it is not retried
    /// silently either.
    Refused { status: u16, message: String },
    /// Everything else: DNS, a reset, a 502, a body that would not parse.
    /// Worth another attempt later.
    Transient(String),
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fault::Unauthorised(m) => write!(f, "sign-in rejected: {m}"),
            Fault::Refused { status, message } => write!(f, "refused ({status}): {message}"),
            Fault::Transient(m) => write!(f, "{m}"),
        }
    }
}

pub struct Client {
    agent: ureq::Agent,
    refresh_token: String,
    token: Option<Token>,
    authenticated_at: Option<i64>,
}

struct Token {
    id_token: String,
    got_at: Instant,
    lifetime: StdDuration,
}

impl Token {
    /// Renewed a minute early. An hour-long token that expires mid-backfill
    /// turns one long run into a 403 storm; a minute of margin costs one extra
    /// refresh an hour.
    fn tired(&self) -> bool {
        self.got_at.elapsed() + StdDuration::from_secs(60) >= self.lifetime
    }
}

impl Client {
    pub fn new(refresh_token: String) -> Client {
        Client {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(StdDuration::from_secs(10))
                .timeout(StdDuration::from_secs(60))
                .build(),
            refresh_token,
            token: None,
            authenticated_at: None,
        }
    }

    /// When the browser sign-in behind this credential happened, as Unix
    /// seconds, once a refresh has been performed.
    ///
    /// The refresh token cannot be asked how long it has left — it is an
    /// encrypted JWE — but this says how long it has been alive, and that is
    /// the number that answers the question the moment one is rejected.
    pub fn authenticated_at(&self) -> Option<i64> {
        self.authenticated_at
    }

    fn id_token(&mut self) -> Result<String, Fault> {
        if let Some(t) = &self.token {
            if !t.tired() {
                return Ok(t.id_token.clone());
            }
        }
        let resp = self
            .agent
            .post(cognito::IDP_HOST)
            .set("Content-Type", "application/x-amz-json-1.1")
            .set("X-Amz-Target", cognito::TARGET)
            .send_string(&cognito::refresh_body(&self.refresh_token));

        let (status, body) = match resp {
            Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
            Err(ureq::Error::Status(s, r)) => (s, r.into_string().unwrap_or_default()),
            Err(e) => return Err(Fault::Transient(format!("cognito unreachable: {e}"))),
        };

        match cognito::parse_refresh(status, &body).map_err(Fault::Transient)? {
            Refreshed::Ok {
                id_token,
                expires_in,
            } => {
                // `auth_time` names the original sign-in, not this refresh, so
                // it does not move as the hour-long id tokens roll over.
                self.authenticated_at = cognito::auth_time(&id_token).or(self.authenticated_at);
                self.token = Some(Token {
                    id_token: id_token.clone(),
                    got_at: Instant::now(),
                    lifetime: StdDuration::from_secs(expires_in.max(0) as u64),
                });
                Ok(id_token)
            }
            Refreshed::Rejected { kind, message } => {
                Err(Fault::Unauthorised(format!("{kind}: {message}")))
            }
        }
    }

    /// A GET against the Emporia API, authorised and read as text.
    pub fn get(&mut self, url: &str) -> Result<String, Fault> {
        let token = self.id_token()?;
        let resp = self.agent.get(url).set("authtoken", &token).call();
        match resp {
            Ok(r) => r
                .into_string()
                .map_err(|e| Fault::Transient(format!("unreadable body: {e}"))),
            Err(ureq::Error::Status(401 | 403, r)) => Err(Fault::Unauthorised(
                r.into_string().unwrap_or_else(|_| "401/403".into()),
            )),
            Err(ureq::Error::Status(400, r)) => Err(Fault::Refused {
                status: 400,
                message: r.into_string().unwrap_or_default(),
            }),
            Err(ureq::Error::Status(s, r)) => Err(Fault::Transient(format!(
                "HTTP {s}: {}",
                r.into_string().unwrap_or_default()
            ))),
            Err(e) => Err(Fault::Transient(e.to_string())),
        }
    }
}

/// A plain pace-setter for the one-time backfill.
///
/// Six thousand requests is what a first run costs, and firing them as fast as
/// the network allows is how a free unofficial API stops being available. There
/// is no published rate limit; this is politeness rather than compliance.
pub struct Pace {
    every: StdDuration,
    last: Option<Instant>,
}

impl Pace {
    pub fn per_second(rate: f64) -> Pace {
        Pace {
            every: StdDuration::from_secs_f64(if rate > 0.0 { 1.0 / rate } else { 0.0 }),
            last: None,
        }
    }

    pub fn wait(&mut self) {
        if let Some(last) = self.last {
            let since = last.elapsed();
            if since < self.every {
                std::thread::sleep(self.every - since);
            }
        }
        self.last = Some(Instant::now());
    }
}
