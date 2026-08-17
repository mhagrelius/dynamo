//! Turning a refresh token into an hour of access.
//!
//! The account is a Google-federated identity in Emporia's pool, which means it
//! has no password there and neither `USER_PASSWORD_AUTH` nor `USER_SRP_AUTH`
//! can ever work for it. `REFRESH_TOKEN_AUTH` can, needs no client secret, and
//! is one JSON POST — which is the whole reason this service carries no AWS SDK
//! and no SRP implementation. `DESIGN.md` has the verification.

use serde::Deserialize;

/// Emporia's pool and app client. Public identifiers, visible in the web app's
/// own traffic and hardcoded by PyEmVue too; not secrets.
pub const USER_POOL: &str = "us-east-2_ghlOXVLi1";
pub const CLIENT_ID: &str = "4qte47jbstod8apnfic0bunmrq";
pub const IDP_HOST: &str = "https://cognito-idp.us-east-2.amazonaws.com/";
pub const TARGET: &str = "AWSCognitoIdentityProviderService.InitiateAuth";

pub fn refresh_body(refresh_token: &str) -> String {
    serde_json::json!({
        "AuthFlow": "REFRESH_TOKEN_AUTH",
        "ClientId": CLIENT_ID,
        "AuthParameters": { "REFRESH_TOKEN": refresh_token },
    })
    .to_string()
}

#[derive(Debug, Deserialize)]
pub struct RefreshResponse {
    #[serde(rename = "AuthenticationResult")]
    pub result: Option<AuthResult>,
}

#[derive(Debug, Deserialize)]
pub struct AuthResult {
    /// **The id token, not the access token.** Emporia authorises on the id
    /// token, passed in an `authtoken` header rather than as a bearer. Sending
    /// the access token instead gets a 403 that reads like an expired session.
    #[serde(rename = "IdToken")]
    pub id_token: String,
    #[serde(rename = "ExpiresIn")]
    pub expires_in: i64,
}

/// What came back, in the two shapes worth telling apart.
#[derive(Debug)]
pub enum Refreshed {
    Ok {
        id_token: String,
        expires_in: i64,
    },
    /// Cognito answered, and said no.
    ///
    /// This is the one failure a retry cannot fix. The refresh token is not
    /// rotated by a successful refresh — the response carries no new one — so
    /// the seeded token has a fixed absolute lifetime and when it ends, it ends.
    /// Everything above treats this as terminal and reports it, rather than
    /// looping on it until somebody notices the graphs stopped.
    Rejected {
        kind: String,
        message: String,
    },
}

pub fn parse_refresh(status: u16, body: &str) -> Result<Refreshed, String> {
    if status == 200 {
        let r: RefreshResponse =
            serde_json::from_str(body).map_err(|e| format!("unreadable refresh response: {e}"))?;
        return match r.result {
            Some(a) => Ok(Refreshed::Ok {
                id_token: a.id_token,
                expires_in: a.expires_in,
            }),
            // A 200 with no AuthenticationResult means a challenge — MFA, a
            // forced password change — none of which a headless service can
            // answer. Terminal, and named as such rather than retried.
            None => Ok(Refreshed::Rejected {
                kind: "ChallengeRequired".into(),
                message: "Cognito returned no AuthenticationResult; the sign-in needs a person"
                    .into(),
            }),
        };
    }
    #[derive(Deserialize)]
    struct Err_ {
        #[serde(rename = "__type", default)]
        kind: Option<String>,
        #[serde(default)]
        message: Option<String>,
    }
    let e: Err_ = serde_json::from_str(body).unwrap_or(Err_ {
        kind: None,
        message: None,
    });
    Ok(Refreshed::Rejected {
        kind: e.kind.unwrap_or_else(|| format!("HTTP {status}")),
        message: e
            .message
            .unwrap_or_else(|| body.chars().take(200).collect()),
    })
}

/// When the browser sign-in behind this refresh token happened.
///
/// **The refresh token itself says nothing.** It is a JWE, not a JWS — five
/// segments, `{"cty":"JWT","enc":"A256GCM","alg":"RSA-OAEP"}` in the header and
/// a payload encrypted under a key only Cognito holds. Checked, not assumed.
/// So its expiry cannot be read from it, and `DescribeUserPoolClient` — which
/// would give the pool's `RefreshTokenValidity` — needs AWS credentials for
/// Emporia's account, which we will never have.
///
/// What *is* readable is `auth_time` on the id token that comes back from every
/// refresh. It names the original authentication, not the refresh, so it stays
/// fixed for the life of the credential — which makes it exactly the age of the
/// refresh token. That does not give the ceiling in advance, but it means the
/// service can say how old its credential is on every pass, and that the moment
/// one is finally rejected we learn what the ceiling was, once, for good.
///
/// **Read, never verified, and never used to decide anything.** This claim came
/// out of a token Cognito just handed us over TLS and is used for a log line and
/// a heartbeat column. Nothing is authorised on it, which is why there is no
/// signature check here and must never be a reason to add one and start
/// trusting it.
pub fn auth_time(id_token: &str) -> Option<i64> {
    let payload = id_token.split('.').nth(1)?;
    let json = base64url(payload)?;
    let v: serde_json::Value = serde_json::from_slice(&json).ok()?;
    v.get("auth_time")?.as_i64()
}

/// Base64url, no padding — the JWT alphabet.
///
/// Twenty lines rather than a dependency, for one field on one token.
fn base64url(s: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.bytes() {
        // Padding is optional in a JWT and terminates the payload either way.
        if c == b'=' {
            break;
        }
        let v = TABLE.iter().position(|t| *t == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_names_the_flow_and_carries_no_secret_of_ours() {
        let b = refresh_body("tok");
        assert!(b.contains("REFRESH_TOKEN_AUTH"));
        assert!(b.contains(CLIENT_ID));
        // No SECRET_HASH: this app client has no secret, which is what makes
        // the whole flow a plain POST.
        assert!(!b.contains("SECRET_HASH"));
    }

    #[test]
    fn a_good_refresh_yields_the_id_token_and_its_hour() {
        // Shape taken from the live 200 on 2026-08-17.
        let r = parse_refresh(
            200,
            r#"{"AuthenticationResult":{"IdToken":"eyJabc","AccessToken":"eyJdef","ExpiresIn":3600,"TokenType":"Bearer"}}"#,
        )
        .unwrap();
        match r {
            Refreshed::Ok {
                id_token,
                expires_in,
            } => {
                assert_eq!(id_token, "eyJabc");
                assert_eq!(expires_in, 3600);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_expired_refresh_token_is_reported_rather_than_looked_like_a_network_blip() {
        let r = parse_refresh(
            400,
            r#"{"__type":"NotAuthorizedException","message":"Refresh Token has expired"}"#,
        )
        .unwrap();
        match r {
            Refreshed::Rejected { kind, message } => {
                assert_eq!(kind, "NotAuthorizedException");
                assert!(message.contains("expired"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_200_with_a_challenge_is_terminal_too() {
        let r = parse_refresh(200, r#"{"ChallengeName":"SMS_MFA"}"#).unwrap();
        assert!(matches!(r, Refreshed::Rejected { .. }));
    }

    /// Header and payload of a real id token's shape, re-signed with nothing —
    /// the signature is never checked, so a placeholder is honest here.
    /// `auth_time` is the value observed on the live account on 2026-08-17.
    fn id_token_with(payload: &str) -> String {
        fn enc(b: &[u8]) -> String {
            const T: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in b.chunks(3) {
                let mut n = (chunk[0] as u32) << 16;
                if chunk.len() > 1 {
                    n |= (chunk[1] as u32) << 8;
                }
                if chunk.len() > 2 {
                    n |= chunk[2] as u32;
                }
                let take = chunk.len() + 1;
                for i in 0..take {
                    out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
                }
            }
            out
        }
        format!(
            "{}.{}.signature-not-checked",
            enc(br#"{"alg":"RS256"}"#),
            enc(payload.as_bytes())
        )
    }

    #[test]
    fn the_credentials_age_is_readable_from_the_id_token_even_though_the_refresh_token_is_opaque() {
        let t = id_token_with(r#"{"token_use":"id","auth_time":1786991247,"exp":1786994847}"#);
        assert_eq!(auth_time(&t), Some(1786991247));
    }

    #[test]
    fn a_token_without_the_claim_is_absent_rather_than_zero() {
        // Zero would render as 1970 and read as a credential fifty years old,
        // which is a worse answer than not knowing.
        let t = id_token_with(r#"{"token_use":"id","exp":1786994847}"#);
        assert_eq!(auth_time(&t), None);
    }

    #[test]
    fn nothing_shaped_like_a_token_panics() {
        assert_eq!(auth_time(""), None);
        assert_eq!(auth_time("not.a.token"), None);
        assert_eq!(auth_time("one-segment"), None);
        // A JWE, which is what the *refresh* token is: five segments and an
        // encrypted payload. Asking this function about one must answer "no"
        // rather than producing something.
        assert_eq!(auth_time("aaa.bbb.ccc.ddd.eee"), None);
    }
}
