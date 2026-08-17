# Dynamo

Three Siemens Inhab Energy Monitors, read out of Emporia's cloud on a schedule
and kept in Postgres on the NAS, so the history outlives what the vendor
retains.

## What the hardware actually is

The Siemens Inhab Energy Monitor is a white-labelled Emporia Vue 3. Siemens
announced it with Emporia in May 2024; Emporia designed the hardware and runs
the backend, the app store listing says "Powered by Emporia", and the iOS
listing's privacy policy link points at `emporiaenergy.com`. The account here
confirms it from the other direction: all three devices report `"model":
"VUE003"` and firmware `Vue3-812`, and the Siemens login authenticates against
Emporia's own Cognito pool.

There is no local access. Siemens' FAQ says so — "there is no other way to
obtain the measurement data other than through the Inhab Energy application and
cloud services" — and Emporia's help centre says the same for every Vue: no
local API, no local network interface, and the app cannot read the device even
when both sit on the same LAN with the internet down. The devices are ESP32s
and the `emporia-vue-local` project flashes ESPHome onto a Vue 3, which would
make them fully local at the cost of the Siemens app. That is a different
project. **This one treats the cloud as the only source, because it is.**

## Authentication

Verified against the live account on 2026-08-17, from inside the web app so no
token had to leave the browser.

The account is a **Google-federated identity** in Emporia's pool: the access
token carries `cognito:groups: ["us-east-2_ghlOXVLi1_Google"]` and username
`Google_…`. Pool `us-east-2_ghlOXVLi1`, client `4qte47jbstod8apnfic0bunmrq` —
the same two constants PyEmVue hardcodes.

**A federated identity has no password in the pool**, so `USER_PASSWORD_AUTH`
and `USER_SRP_AUTH` are both unavailable and no amount of setting a password on
the Emporia side changes that for *this* identity. What works, and what this
service uses:

```
POST https://cognito-idp.us-east-2.amazonaws.com/
X-Amz-Target: AWSCognitoIdentityProviderService.InitiateAuth
{"AuthFlow":"REFRESH_TOKEN_AUTH","ClientId":"4qte47jbstod8apnfic0bunmrq",
 "AuthParameters":{"REFRESH_TOKEN":"…"}}
```

Confirmed working with a hosted-UI refresh token: 200, `ExpiresIn: 3600`, and
the returned `IdToken` was accepted by `api.emporiaenergy.com` on the next call.
No client secret, no SRP, no AWS SDK — one JSON POST, which is why this service
carries no `aws-sdk-*` dependency.

**The response does not include a new refresh token.** The seeded one is reused
until its own absolute expiry.

That expiry cannot be read from the token, and it is worth saying exactly why,
because the obvious assumption is that it can. The id and access tokens are
JWSs — three segments, `RS256`, a readable payload with `exp`, `iat` and
`auth_time`. **The refresh token is not one.** It is a JWE: five segments, a
header of `{"cty":"JWT","enc":"A256GCM","alg":"RSA-OAEP"}`, and a payload
encrypted under a key only Cognito holds. The header decodes and says nothing
about time; the rest does not decode at all. The pool's `RefreshTokenValidity`
would answer it, but `DescribeUserPoolClient` needs AWS credentials for
Emporia's account.

What *is* readable is **`auth_time` on the id token**, which names the original
browser sign-in rather than the refresh, and therefore does not move as hourly
id tokens roll over. It is the age of the refresh token. That is not a
countdown, but it is the next best thing: the service reports the credential's
age on every pass and records it in `heartbeat.authenticated_at`, so the day one
is finally refused we learn what the ceiling was — once, for good.

Cognito's default is 30 days and Emporia may have set it longer. So: 

- `DYNAMO_REFRESH_TOKEN` seeds it, from the `.env` beside the compose file.
- The id token is cached in memory and renewed a few minutes before its hour is
  up. Nothing token-shaped is ever written to Postgres or to disk.
- **A failed refresh is the one failure that cannot be retried out of.** It is
  recorded as such, `--health` starts failing, and the container goes unhealthy
  in Container Manager rather than sitting green and silently collecting
  nothing. Re-seeding means pasting a new refresh token into `.env` and
  rebuilding — the path is in `README.md`.

## What the API will and will not give you

Every figure below was measured against this account, not read from
documentation. The per-request ceilings are quoted verbatim from the API's own
400 responses.

| Scale | Max window per request | How far back data exists |
| --- | --- | --- |
| `1S` | `PT1H6M40S` (4000 s) | ~7 days |
| `1MIN` | `PT13H20M` (800 min) | ~7 days |
| `15MIN` | `PT168H` (7 days) | ~12 months |
| `1H` | `PT800H` (~33 days) | to account creation, 2025-01-29 |
| `1D` | `PT12000H` (500 days) | all of it |

Two things fall out of this and they shape the whole service.

**Sub-hourly data expires in about a week.** Minute data is present at 7 days
and null at 7.5; second data is present at 7 days and null at 14. Everything
finer than an hour is a rolling window. This is the reason the project exists:
poll minutes forward and they accumulate here forever, where the vendor keeps
seven days.

**Hourly reaches back to the account and fifteen-minute reaches back about a
year.** So "as far back as possible" is not one number. Hourly is available from
2025-01-29; fifteen-minute data is non-null at 365 days but null at 400, so it
starts somewhere in mid-2025 — the devices were installed, or the tier rolled
off, and either way the backfill discovers the edge rather than assuming it.

A request past the ceiling fails with 400 and a message naming the limit; it
does not silently truncate. A request for a window with no data succeeds with
200 and a `usageList` of nulls. **Those two are different and the code must not
conflate them** — a 400 is a bug in our chunking, a null run is the honest edge
of what exists.

## The shape of an account

`GET /customers/devices` returns three devices, each a `VUE003` with a nested
`WAT001` carrying the branch channels:

- one `Main` channel, numbered `1,2,3` (the three mains CTs summed)
- sixteen branch legs, `1`–`16`, typed `FiftyAmp` or `FiftyAmpBidirectional`
- **merged pseudo-channels from `97` up**, which pair the two legs of a 240 V
  circuit into the thing a person means: legs `11`/`12` are the dryer, and `101`
  is "Clothes Dryer".

**A merged channel is the sum of its legs, so storing both and adding them
double-counts.** They are all stored, because throwing away either would be
lossy, and the schema marks which is which so a query cannot accidentally sum
across the two kinds. `channel_kind` is `main`, `branch` or `merged`, and
nothing but a deliberate query mixes them.

## Polling forward

`getDeviceListUsages` takes every device gid at once and answers a nested tree —
one request covers all three boxes and every channel on them. That is the live
poll: once a minute, one request, at `1MIN` scale.

`getChartUsage` is per-device-per-channel and is what backfill and catch-up use,
because it is the only call that takes a time range.

## Backfill and catch-up are one mechanism

There is no separate "first run" path. For each `(device, channel, scale)` the
store knows the newest instant it holds; the planner asks for everything between
that and now, chunked to the scale's ceiling. On an empty database the watermark
is absent and the start becomes the earliest the scale serves, which is the
one-time historical load. After a four-minute restart it is four minutes. After
a fortnight down it is a fortnight of hours, because minutes that old no longer
exist to be fetched.

The planner is a pure function — watermarks and a clock in, a list of requests
out — so the awkward cases have tests rather than a note saying they were
considered.

Rough one-time cost, at roughly 75 channel-series across three devices: ~18
hourly chunks, ~52 fifteen-minute chunks and ~13 minute chunks per series, so on
the order of six thousand requests. Throttled, that is half an hour and it
happens once. `DYNAMO_RATE` bounds it.

## Storage

Postgres, in the `postgres` project already on the NAS, reached over that
project's own network at `postgres:5432` the way `planner-server` does — so the
password never crosses the tailnet.

One reading is `(device_gid, channel_num, scale, instant) → kwh`, with the
primary key over the first four. Re-fetching an overlapping window is therefore
free and idempotent, which matters because the catch-up deliberately overlaps by
one interval rather than trusting a boundary.

`ON CONFLICT DO UPDATE` rather than `DO NOTHING`: a recent interval can be
re-reported by the cloud as more data arrives from the device, and the later
answer is the better one.

## What this service does not do

**It publishes no port and serves nothing.** Every sibling on this NAS is a
server; this one is only a client, so there is no listener, no token to protect,
nothing on the LAN, and no `ports:` block. `--health` — which Container Manager
runs, and which cannot use `curl` because the image has none — connects to
Postgres and checks that the last successful poll is recent. That covers the
database being unreachable and the refresh token having expired, which are the
two ways this fails, and it needed no HTTP server to do it.
