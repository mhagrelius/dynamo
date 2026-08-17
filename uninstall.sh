#!/usr/bin/env bash
#
# Reverse ./install.sh. Leaves ~/.config/dynamo/config.json alone — it holds a
# password somebody typed, and removing it on an uninstall is the kind of
# helpfulness nobody asks for twice.

set -euo pipefail
PREFIX="${PREFIX:-$HOME/.local}"
rm -f "$PREFIX/bin/dynamo" && echo "removed $PREFIX/bin/dynamo"
echo "left $HOME/.config/dynamo/config.json in place"
