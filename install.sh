#!/usr/bin/env bash
#
# Install the `dynamo` command line under ~/.local.
#
# This is the *client* half. The collector runs in a container on the NAS and is
# deployed by `packaging/deploy-server.sh`; what this puts on your PATH is the
# same binary invoked as `dynamo agent <verb>`, which reads the database and
# prints JSON. Familiar spawns it that way, and so can you.
#
# ./uninstall.sh reverses it.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

PREFIX="${PREFIX:-$HOME/.local}"

echo "==> cargo build --release"
cargo build --release --locked

install -Dm755 target/release/dynamo "$PREFIX/bin/dynamo"
echo "    installed $PREFIX/bin/dynamo"

CONFIG="$HOME/.config/dynamo/config.json"
if [ -f "$CONFIG" ]; then
    echo "    $CONFIG is already there, left alone"
else
    # Written rather than left absent, because the failure without it is a
    # connection refused against a default host, which reads like the NAS being
    # down rather than like a file nobody has filled in.
    mkdir -p "$(dirname "$CONFIG")"
    cat > "$CONFIG" <<'EOF'
{
  "host": "nas.example.ts.net",
  "port": 5433,
  "database": "dynamo",
  "user": "dynamo_read",
  "password": ""
}
EOF
    chmod 600 "$CONFIG"
    echo "    wrote $CONFIG — put the dynamo_read password in it"
fi

case ":$PATH:" in
    *":$PREFIX/bin:"*) ;;
    *) echo "    note: $PREFIX/bin is not on your PATH, so Familiar will not find it" ;;
esac

cat <<EOF

Check it:
  dynamo agent describe
  dynamo agent channels

The user in that config should be **dynamo_read**, which is granted SELECT and
nothing else. The collector's own role can write, and nothing reachable from
\`agent\` needs to.
EOF
