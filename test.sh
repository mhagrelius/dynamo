#!/usr/bin/env bash
#
# The gate. Run this, not bare `cargo test`.
#
# No Xvfb and no private D-Bus, unlike the sibling apps: there is no GTK here
# and nothing to draw. No test touches the network or a database either — the
# planner, the URL builder and the response readers are pure, which is what
# makes that possible and is the reason the seam is where it is.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

echo "==> cargo fmt --check"
cargo fmt --all -- --check

echo "==> cargo clippy"
cargo clippy --all-targets -- -D warnings

echo "==> cargo test"
cargo test --all-targets
