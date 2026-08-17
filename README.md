# Dynamo

Three Siemens Inhab energy monitors, read out of Emporia's cloud on a schedule
and kept in Postgres on the NAS.

The monitors are white-labelled Emporia Vue 3s and there is no local access to
them — Siemens and Emporia both say so plainly, and the devices offer no API on
the LAN at all. The cloud is the only source. What it will not do is keep
anything: **minute-resolution data is gone after about a week.** This service
exists so that a week's worth becomes a permanent one.

`DESIGN.md` has the evidence for every claim here, all of it measured against
the live account rather than read from documentation.

## What it collects

| Resolution | Kept by the cloud | Kept here |
| --- | --- | --- |
| minute | ~7 days | forever |
| 15 minutes | ~12 months | forever |
| hour | since the account opened | forever |
| day | all of it | forever |

Every channel on all three boxes: the mains, the sixteen branch CTs, and the
merged pseudo-channels that pair the two legs of a 240 V circuit into the thing
a person means by "the dryer".

**Merged channels overlap their branch legs.** `reading.kind` — via the
`channel` table — says which is which, and a query that sums across both counts
the same power twice. To total the house, use the `main` rows.

## Running it

```sh
./test.sh                                   # the gate: fmt, clippy, tests
DYNAMO_REGISTRY=nas.example.ts.net:5050 ./packaging/deploy-server.sh
```

The deploy script runs the tests, builds the image, then proves against a
throwaway Postgres that the schema applies, that TLS to Cognito works, and that
a rejected sign-in is reported rather than swallowed — and only then pushes.

### On the NAS, once

The database has to exist first. In the `postgres` project:

```sql
CREATE ROLE dynamo LOGIN PASSWORD '…';
CREATE DATABASE dynamo OWNER dynamo;
```

The tables are created by the service on every start, so there is no migration
to run by hand.

Then Container Manager → Project → Create at `/volume1/docker/dynamo`, upload
`docker-compose.yml` — **do not type it into the compose editor**, its
auto-indent will nest everything under everything else — and put `.env` beside
it, from `.env.example`.

There is no bind mount to create, because nothing is written to disk.

### Checking it

There is no port to curl; this service listens on nothing.

```sh
sudo docker exec dynamo /usr/local/bin/dynamo --health
sudo docker logs dynamo --tail 20
```

The status dot is worth less than either. A blank Log tab in Container Manager
means nothing at all — go to `sudo docker logs` over SSH.

**The first pass is a backfill and takes roughly half an hour**, several
thousand paced requests covering every channel at every resolution back to the
account's start in January 2025. `--health` says `starting` until it finishes,
which is why the healthcheck has a two-minute `start_period`.

## Seeding the refresh token

This is the one manual step, and it comes back around.

The account signs in with Google, which means it is a *federated* identity in
Emporia's Cognito pool: it has no password there, and no amount of setting one
on the Emporia side gives this identity one. The only sign-in a headless
service can perform is `REFRESH_TOKEN_AUTH`, and that needs a refresh token
minted by a real browser sign-in.

To get one:

1. Sign in at <https://web.emporiaenergy.com> with the Google account.
2. Devtools → Application → IndexedDB → `com.amplify.awsCognitoAuthPlugin` →
   `default.store`.
3. Copy the value of the key ending `.hostedUi.refreshToken` — the long one,
   around 1,700 characters.
4. Put it in `.env` as `DYNAMO_REFRESH_TOKEN` and rebuild the project.

### When it stops working

**A successful refresh does not return a new refresh token.** The one seeded
above is reused until its own absolute expiry, which is set on Emporia's app
client and is not something we can extend.

That expiry is not readable from the token — the refresh token is an *encrypted*
JWE rather than a signed JWT, so unlike the id token it carries no `exp` anyone
but Cognito can see. What the service does instead is report the credential's
**age**, taken from the id token's `auth_time`, on every pass and in `--health`:

```
pass complete: 412 readings written; holding 1MIN 8640, 1H 41200; credential 23 days old
```

Not a countdown, but it climbs, and the day one is finally refused it tells you
exactly how long these last — after which this section can stop guessing.
Cognito's default is 30 days; it may well be longer here.

When it goes, everything stops — no retry helps. What happens then is
deliberate and loud:

- the log says `DYNAMO_REFRESH_TOKEN is no longer accepted`, once, not once a
  minute;
- the process exits, so `restart: unless-stopped` will keep retrying it and
  keep failing, visibly;
- `--health` fails, so Container Manager shows the container unhealthy.

The fix is to repeat the four steps above. Nothing is lost while it is down
beyond the minute-resolution data for the gap — the next run backfills hours
and quarter-hours over it automatically, because catch-up and backfill are the
same mechanism.

## Configuration

Everything comes from the environment. `DYNAMO_REFRESH_TOKEN` and
`DYNAMO_DB_PASSWORD` have no defaults on purpose: the process refuses to start
without them rather than running and collecting nothing.

| Variable | Default | |
| --- | --- | --- |
| `DYNAMO_REFRESH_TOKEN` | — | required |
| `DYNAMO_DB_HOST` | `postgres` | the container name on `postgres_default` |
| `DYNAMO_DB_PORT` | `5432` | its own port, not the 5433 the NAS publishes |
| `DYNAMO_DB_NAME` | `dynamo` | |
| `DYNAMO_DB_USER` | `dynamo` | |
| `DYNAMO_DB_PASSWORD` | — | required |
| `DYNAMO_INTERVAL` | `60` | seconds between passes |
| `DYNAMO_RATE` | `4` | requests per second during a pass |
| `DYNAMO_STALE` | `900` | seconds before `--health` calls a quiet service unwell |

## Querying it

```sql
-- The house, by hour, for the last week.
SELECT r.instant, sum(r.kwh) AS kwh
FROM reading r
JOIN channel c USING (device_gid, channel_num)
WHERE c.kind = 'main' AND r.scale = '1H'
  AND r.instant > now() - interval '7 days'
GROUP BY r.instant ORDER BY r.instant;

-- What each named circuit drew yesterday. Merged channels only, so the two
-- legs of a 240 V circuit are not counted twice.
SELECT c.device_name, c.name, sum(r.kwh) AS kwh
FROM reading r
JOIN channel c USING (device_gid, channel_num)
WHERE c.kind = 'merged' AND r.scale = '1H'
  AND r.instant >= date_trunc('day', now() - interval '1 day')
  AND r.instant <  date_trunc('day', now())
GROUP BY c.device_name, c.name ORDER BY kwh DESC;
```

## What this is not

It does not control anything, and it cannot: the monitors measure and have no
relays. It does not touch the devices on the LAN, because they do not answer
there. And it is not a replacement for the Siemens app, which keeps working
throughout — this reads the same cloud account alongside it.

If local, second-by-second, no-cloud data ever matters more than the app does,
the answer is `emporia-vue-local`, which flashes ESPHome onto the Vue 3's ESP32
and ends the cloud relationship entirely. That is a different project and it
costs the app.
