# dynamo

Reads three Siemens Inhab energy monitors out of Emporia's cloud into Postgres
on the NAS, because the cloud keeps a week of minutes and this keeps all of
them.

## How this differs from the siblings, and why

Every other project in `~/Projects` is a GTK 4 / libadwaita desktop application,
three of which (`brain`, `planner`, `armory`) carry a `server/` crate deployed
to the NAS. **This repo is only the service.** There is no GTK, no `examples/`
preview harness, no `install.sh`, no Flatpak, and no `model/` versus `ui/`
split, because there is no user interface to split from anything.

Two deliberate partings from the house pattern, both justified where they
appear:

- **It takes an HTTP client.** `brain-server`, `planner-server` and
  `armory-server` all say "no HTTP crate" and mean it — they serve plain HTTP
  on the tailnet and open no outbound connection at all. Dynamo is the mirror
  image: it serves nothing and its whole job is outbound HTTPS to two public
  hosts. `ureq` with rustls, argued in `Cargo.toml`. Hand-rolling TLS is the
  one thing in this stack nobody should hand-roll.
- **The Containerfile and compose file are at the root**, not under `server/`.
  There is no shell for a `server/` folder to distinguish them from.

Everything else follows the house: `test.sh` is the gate,
`packaging/deploy-server.sh` is test → build → smoke-test → push with a
`date-shortsha` tag, and the compose file carries `user: "0:0"`,
`read_only: true`, no `logging:` block and an upload-don't-type warning.

## Commands

- `./test.sh` — fmt check, clippy with `-D warnings`, then `cargo test`. This
  is the gate; run it, not bare `cargo test`. No Xvfb and no private D-Bus,
  unlike the siblings — there is nothing to draw.
- `./packaging/deploy-server.sh` — the full path to the NAS. Its smoke test
  starts a throwaway Postgres and proves the schema applies, that TLS to
  Cognito works, and that a rejected sign-in is reported as terminal, before
  anything is pushed.

**No test touches the network or a database.** The planner, the URL builder and
the response readers are pure, which is what makes that true; keep it true for
anything new. The database and the network are exercised by the deploy script's
smoke test instead, against real instances rather than mocks.

## Layout

`scale.rs`, `plan.rs` and `emporia.rs` are pure — what to ask for, what a URL
looks like, what a body means. `http.rs` is the only file that opens a socket
and `store.rs` the only one that talks to Postgres. This is the same seam the
siblings keep between `model/source/*` and `ui/http.rs`, and it buys the same
thing: every decision worth arguing about is tested with no account and no
network.

`DESIGN.md` carries the evidence. Every retention figure and every per-request
ceiling in `scale.rs` was measured against the live account on 2026-08-17 —
there is no documentation for this API and none of it is guessed.

## Things that will bite

- **The account is a Google-federated identity, so it has no password in the
  pool.** `USER_PASSWORD_AUTH` and `USER_SRP_AUTH` cannot work for it, and no
  amount of setting a password on the Emporia side changes that.
  `REFRESH_TOKEN_AUTH` is the only flow available. Anybody proposing to
  "simplify" auth by taking a username and password is proposing something that
  cannot work.

- **A successful refresh returns no new refresh token.** The seeded one has a
  fixed absolute lifetime and when it ends nothing retries its way out. That is
  why `Fault::Unauthorised` is terminal: the process says so once, records it,
  exits, and `--health` goes red. Treating it as transient would fill a log
  with one line a minute while nothing was collected.

- **The refresh token is a JWE, not a JWT, so it has no readable expiry.** Five
  segments, `alg: RSA-OAEP`, `enc: A256GCM`, payload encrypted under Cognito's
  own key — checked, not assumed. Anybody reaching for `exp` on it is thinking
  of the *id* token, which is a signed JWS and does carry one. The credential's
  age comes from `auth_time` on the id token instead, which names the original
  sign-in and stays fixed across refreshes. `cognito::auth_time` reads it
  **without verifying the signature and nothing is authorised on it** — it
  feeds a log line and a heartbeat column, and that must stay true.

- **Emporia authorises on the *id* token, in an `authtoken` header.** Not the
  access token, and not a bearer. Sending the access token gets a 403 that
  reads exactly like an expired session.

- **A merged channel is the sum of two branch legs.** Channels from 97 up pair
  the two legs of a 240 V circuit — the dryer is legs 11 and 12, and 101 is
  "Clothes Dryer". Summing branches and merged channels together reports the
  house using twice the power it uses. `channel.kind` exists so a query cannot
  make that mistake silently.

- **A null in `usageList` is not a zero.** It means the cloud has nothing for
  that interval — the device was offline, or the resolution has aged out.
  Writing zeros would turn "we do not know" into "the house used no power",
  which every average and every bill downstream would believe.

- **A 400 and a window of nulls are different and must stay different.** A 400
  means the chunking asked for more than a scale's ceiling and is a bug in
  `scale.rs`; a 200 full of nulls is the honest edge of what the vendor keeps.
  Conflating them makes the end of history look like a crash, or a crash look
  like the end of history.

- **The per-request ceilings are exact and the API quotes them at you.**
  `PT13H20M` for minutes, `PT168H` for quarter-hours, `PT800H` for hours,
  `PT12000H` for days. Exceeding one is a refusal, never a truncated answer, so
  `chunks` respects them exactly rather than approximately.

- **`instant_at` is arithmetic over a bare array.** `getChartUsage` returns
  `usageList` and a single `firstUsageInstant`; every other timestamp is
  derived. Getting the step wrong shifts a whole window silently rather than
  failing, which is why it has its own test.

- **Backfill and catch-up are one mechanism, deliberately.** There is no
  first-run path to keep working. An empty watermark makes the start the
  earliest a scale serves; a four-minute restart makes it four minutes. Adding
  a separate "initial import" would create a path that runs once and is never
  exercised again.

- **The planner overlaps by one point on resume, on purpose.** The upsert makes
  a repeat free, an off-by-one the other way is a permanent hole nothing goes
  back for, and the cloud revises the most recent interval as late readings
  arrive.

- **`CREATE TABLE IF NOT EXISTS` never adds a column** to a table that already
  exists. A new column needs its own `ALTER TABLE … ADD COLUMN IF NOT EXISTS`
  in `migrate`. A sibling lost months of silently unwritten data to this.

- **`ON CONFLICT DO UPDATE`, not `DO NOTHING`.** A recent interval gets revised
  as more data reaches the cloud, and the later answer is the better one.

- **No `ca-certificates` in the runtime image, and that is checked rather than
  assumed.** `ureq`'s `tls` feature resolves to rustls with `webpki-roots`, so
  the root set is compiled into the binary — confirmed with `cargo tree`. The
  consequence: roots are updated by rebuilding, not by `apt upgrade`.

- **Nothing listens.** No port, no `EXPOSE`, no `ports:` block, no token to
  protect. `--health` asks Postgres instead of a socket of its own, which
  covers both ways this fails — database unreachable, and sign-in dead — and
  needed no HTTP server to do it.

## Conventions

- Use the `deploying-to-nas` skill for anything touching the NAS, the
  registry or Container Manager rather than deriving it again.
- Never rewrite sources through `python3 - <<PY` heredocs or `sed -i`.
- The sibling projects share `test.sh`, `packaging/` and the compose
  conventions; a pattern established in one is the pattern here, except where
  this file says otherwise and says why.
