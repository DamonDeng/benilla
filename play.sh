#!/usr/bin/env bash
# Launch the benilla WoW 1.12.1 client.
#
#   ./play.sh            Connect to the local vmangos server — classic 60: NPCs, quests, boats,
#                        and the 3x XP + 5-min hearthstone tweaks. Needs the server up
#                        (cd ../vmangos-deploy && docker compose up -d).
#   ./play.sh explore    Serverless open-world free-roam — instant, no server needed.
#
# Connected-play credentials default to the local GM account; override with env vars, e.g.
#   WOW_USER=someone WOW_PASS=secret ./play.sh
set -euo pipefail
cd "$(dirname "$0")"

mode="${1:-connect}"

case "$mode" in
  explore)
    bin="target/play/warcraft-explorer"
    [ -x "$bin" ] || cargo build --profile play -p warcraft-explorer
    echo "benilla: launching explore mode (serverless) …"
    exec "$bin"
    ;;
  connect|play|"")
    bin="target/play/benilla"
    [ -x "$bin" ] || cargo build --profile play -p benilla
    export WOW_HOST="${WOW_HOST:-127.0.0.1}"
    export WOW_USER="${WOW_USER:-<username>}"
    export WOW_PASS="${WOW_PASS:-<password>}"
    echo "benilla: connecting to ${WOW_HOST} as ${WOW_USER} …"
    exec "$bin"
    ;;
  *)
    echo "usage: $0 [connect|explore]" >&2
    exit 2
    ;;
esac
