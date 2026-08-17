#!/usr/bin/env bash
#
# Build dynamo, prove it starts and refuses what it should, and push it to the
# NAS registry.
#
#     DYNAMO_REGISTRY=nas.example.ts.net:5050 ./packaging/deploy-server.sh
#
# Tests first, then build, then a smoke test of the actual image, and only
# then a push. A registry is a place other machines pull from; getting a
# broken tag out of one is more work than not putting it there.
#
# The tag is today's date and the commit. `:latest` means a restart can quietly
# change what is collecting, which for anything accumulating history is the
# wrong kind of surprise.

set -euo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

REGISTRY="${DYNAMO_REGISTRY:-nas.example.ts.net:5050}"

# Asked before the tag is built rather than after. Without a commit the
# substitution below fails under `set -e` and the script dies on git's own
# `fatal: Needed a single revision`, which says nothing about deployment to
# somebody reading it.
if ! git rev-parse --verify --quiet HEAD >/dev/null; then
    echo "this repository has no commits, so nothing can say what would be in" >&2
    echo "the image. Commit first." >&2
    exit 1
fi

# The date says when, the commit says what. A date alone cannot answer "which
# commit is running on the NAS", which is the question actually asked when
# something is behaving oddly.
TAG="${DYNAMO_TAG:-$(date +%Y-%m-%d)-$(git rev-parse --short HEAD)}"
IMAGE="$REGISTRY/dynamo:$TAG"

# A tag naming a commit has to mean it. Untracked files are fine — CLAUDE.md
# and a local .env live beside this — but a tracked change that is not in the
# commit would make the tag a lie.
if ! git diff-index --quiet HEAD --; then
    echo "the working tree has uncommitted changes, so $TAG would not describe" >&2
    echo "what is in the image. Commit them, or set DYNAMO_TAG to say so." >&2
    exit 1
fi

echo "==> ./test.sh"
./test.sh

echo "==> podman build $IMAGE"
# --format docker, not podman's OCI default: HEALTHCHECK has no place in the
# OCI image spec, so an OCI build drops it with a warning that is easy to miss.
podman build --format docker -f Containerfile -t "$IMAGE" .

echo "==> smoke test"

# Every check below runs the deployed configuration — `--read-only`, and
# `--user 0:0` because that is what the compose file says. Testing a
# convenient configuration rather than the deployed one is how a container
# passes here and fails on the NAS.
#
# **Container arguments go after the image name.** Putting `--health` in with
# podman's own flags gets `unknown flag: --health` from podman, which reads
# exactly like the binary not having the flag.
run() {
    local env_args=()
    while [ "${1:-}" != "--" ]; do env_args+=("$1"); shift; done
    shift
    podman run --rm --read-only --user 0:0 "${env_args[@]}" "$IMAGE" "$@"
}

# No credential and no database needed for these two: the image must refuse to
# start half-configured rather than run and collect nothing.
missing_token="$(run -e DYNAMO_DB_PASSWORD=x -- 2>&1 || true)"
if grep -q "DYNAMO_REFRESH_TOKEN" <<<"$missing_token"; then
    echo "    refuses a missing refresh token"
else
    echo "    the image did not refuse a missing refresh token, it said:" >&2
    echo "$missing_token" >&2
    exit 1
fi

missing_pw="$(run -e DYNAMO_REFRESH_TOKEN=x -- 2>&1 || true)"
if grep -q "DYNAMO_DB_PASSWORD" <<<"$missing_pw"; then
    echo "    refuses a missing database password"
else
    echo "    the image did not refuse a missing database password, it said:" >&2
    echo "$missing_pw" >&2
    exit 1
fi

# The rest needs a real database, because the two things most worth proving
# cannot be proved without one: that the schema in `store::migrate` actually
# applies to a live Postgres, and that the binary reaches Cognito over TLS.
#
# A throwaway Postgres on a throwaway network, in the spirit of armory's
# `sync-check.sh` — the alternative is finding out on the NAS that a `CREATE
# TABLE` has a typo in it.
NET="dynamo-smoke-$$"
PG="dynamo-smoke-pg-$$"
cleanup() {
    podman rm -f "$PG" >/dev/null 2>&1 || true
    podman network rm -f "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT

podman network create "$NET" >/dev/null
podman run -d --name "$PG" --network "$NET" \
    -e POSTGRES_PASSWORD=smoke -e POSTGRES_USER=dynamo -e POSTGRES_DB=dynamo \
    docker.io/library/postgres:16-alpine >/dev/null

ready=""
for _ in $(seq 1 60); do
    if podman exec "$PG" pg_isready -U dynamo -q 2>/dev/null; then ready=yes; break; fi
    sleep 1
done
if [ -z "$ready" ]; then
    echo "    the throwaway Postgres never came up:" >&2
    podman logs "$PG" >&2 || true
    exit 1
fi

with_db() { run --network "$NET" -e DYNAMO_DB_HOST="$PG" -e DYNAMO_DB_PASSWORD=smoke "$@"; }

# One pass with a deliberately invalid refresh token. Three things at once:
# the schema applies, TLS to Cognito works with no ca-certificates package in
# the image, and a rejected sign-in is reported as terminal rather than retried.
#
# The expected output is Cognito's own `NotAuthorizedException`. A TLS or DNS
# error here would mean the runtime stage is missing something, which is the
# failure this whole check exists to catch before the NAS does.
pass="$(with_db -e DYNAMO_REFRESH_TOKEN=not-a-real-token -- --once 2>&1 || true)"
if grep -q "NotAuthorizedException" <<<"$pass"; then
    echo "    reaches Cognito over TLS with no ca-certificates package"
    echo "    treats a rejected refresh token as terminal"
else
    echo "    the image did not get a real answer from Cognito, it said:" >&2
    echo "$pass" >&2
    exit 1
fi

tables="$(podman exec "$PG" psql -U dynamo -d dynamo -tAc \
    "SELECT string_agg(tablename, ',' ORDER BY tablename) FROM pg_tables WHERE schemaname='public'")"
# Spelled out rather than counted, so adding a table is a deliberate edit here
# and a dropped one is a failure rather than a smaller number nobody reads.
if [ "$tables" = "channel,heartbeat,probe,reading" ]; then
    echo "    creates its schema on a fresh database"
else
    echo "    the schema is not what it should be, it is: $tables" >&2
    exit 1
fi

# `--health` is what Container Manager runs every minute, and it has to say why
# rather than merely exiting non-zero — an unhealthy container whose healthcheck
# printed nothing is the case that costs an evening.
health="$(with_db -e DYNAMO_REFRESH_TOKEN=x -- --health 2>&1 || true)"
if grep -qi "unhealthy" <<<"$health" && grep -qi "sign-in rejected" <<<"$health"; then
    echo "    --health reports the rejected sign-in it recorded"
else
    echo "    --health did not report the failed pass, it said:" >&2
    echo "$health" >&2
    exit 1
fi

# Everything above ran with `--read-only`, so reaching here is the proof that a
# read-only root is enough. The compose file claims it; this is where the claim
# is checked, rather than on the NAS where the failure looks exactly like the
# Synology ACL problem and is not.
echo "    read-only root is enough"

cleanup
trap - EXIT

echo "==> podman push $IMAGE"
# --tls-verify=false because the registry speaks plain HTTP. It is reachable
# only over the tailnet, which is what makes that acceptable.
podman push --tls-verify=false "$IMAGE"

cat <<EOF

Pushed $IMAGE

Next, on the NAS:
  1. The database has to exist before this can start. In the \`postgres\`
     project, once:

       CREATE ROLE dynamo LOGIN PASSWORD '…';
       CREATE DATABASE dynamo OWNER dynamo;

     The tables are created by the service itself on every start.
  2. Container Manager → Project → dynamo, pointed at /volume1/docker/dynamo
  3. Set DYNAMO_IMAGE in its .env to:

       localhost:5050/dynamo:$TAG

     localhost, not the tailnet name — a registry stores repositories by name
     rather than by hostname, so it is the same image, and Docker accepts a
     registry reached over localhost on plain HTTP without being configured
     to. Naming the NAS means editing the daemon config over SSH for no gain.
  4. Build.

There is no port to curl, so check it the way the healthcheck does:
  sudo docker exec dynamo /usr/local/bin/dynamo --health
  sudo docker logs dynamo --tail 20

The first pass is a backfill of several thousand paced requests and takes
roughly half an hour. \`--health\` reports "starting" until it finishes.
EOF
