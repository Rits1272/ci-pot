#!/bin/sh
# A live trial with fake money and real jobs.
#
# Starts a throwaway local mint whose Lightning backend is fake, then runs
# ci-pot as if it were the pot of a coordinator you name. It funds one
# repository's pot with fake sats and shows that coordinator's real jobs being
# charged as they finish on the public relay. It only reads public events:
# nothing is published and no real money moves.
#
# Usage:
#   COORDINATOR=<coordinator public key, hex or npub> \
#   REPO=30617:<maintainer key in hex>:<repository name> \
#   scripts/live-trial.sh [minutes to watch, default 12]
#
# Pick a coordinator that runs its own jobs and a repository it serves. Any
# Job Result (kind 9841) on wss://relay.ngit.dev shows both: the coordinator
# is the key in its `q` tag, the repository is its `a` tag.
set -eu

if [ -z "${COORDINATOR:-}" ] || [ -z "${REPO:-}" ]; then
  echo "Set COORDINATOR and REPO first. See the comment at the top of this file." >&2
  exit 2
fi
BIN="${CI_POT:-target/release/ci-pot}"
MINUTES="${1:-12}"
WORK="$(mktemp -d)"
MINT=http://localhost:3338
REPO_PATH=$(printf '%s' "$REPO" | sed 's/:/%3A/g')

cleanup() {
  kill "$POT" 2>/dev/null || true
  wait 2>/dev/null || true
  docker rm -f ci-pot-trial-mint >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "== Starting a local mint with a fake Lightning backend"
docker run -d --name ci-pot-trial-mint -p 127.0.0.1:3338:3338 \
  -e MINT_LISTEN_HOST=0.0.0.0 -e MINT_LISTEN_PORT=3338 \
  -e MINT_PRIVATE_KEY=ci-pot-trial-only -e MINT_BACKEND_BOLT11_SAT=FakeWallet \
  cashubtc/nutshell:0.17.0 poetry run mint >/dev/null
until curl -sf "$MINT/v1/info" >/dev/null; do sleep 1; done
wallet() { docker exec -e MINT_URL="$MINT" ci-pot-trial-mint poetry run cashu -t "$@" 2>/dev/null; }
token() { wallet send "$1" | grep -o 'cashu[AB][A-Za-z0-9_=-]*' | head -1; }
wallet invoice 1000 >/dev/null

"$BIN" --data-dir "$WORK/data" serve --coordinator "$COORDINATOR" --mint "$MINT" \
  --sat-per-vcpu-hour 50 --repos-file "$WORK/repos.env" --poll-seconds 30 \
  --listen 127.0.0.1:4050 2>"$WORK/pot.log" & POT=$!
sleep 3

pay() { curl -s -X POST "http://127.0.0.1:4050/pots/$REPO_PATH" \
  -H 'content-type: application/json' -d "{\"token\":\"$1\"}"; echo; }
balance() { curl -s "http://127.0.0.1:4050/pots/$REPO_PATH" | python3 -I -c "
import sys, json
p = json.load(sys.stdin)['pot']
print('balance %d sat, spent %d sat, %d jobs, served now: %s' % (p['balance_msat']//1000, p['charged_msat']//1000, p['jobs'], p['admitted']))"; }

echo "== The price and mints the pot states"
curl -s http://127.0.0.1:4050/info; echo

echo "== Paying 300 fake sats into the repository's pot"
FIRST="$(token 300)"
pay "$FIRST"

echo "== Paying the same token again"
pay "$FIRST"

echo "== The admission list written for the coordinator"
cat "$WORK/repos.env"

echo "== Watching real jobs for $MINUTES minutes"
i=0
while [ "$i" -lt "$MINUTES" ]; do
  sleep 60
  i=$((i + 1))
  printf 'minute %2d: ' "$i"; balance
done

echo "== What the pot service logged"
cat "$WORK/pot.log"
echo "== The admission list at the end"
cat "$WORK/repos.env"
echo "== The wallet"
"$BIN" --data-dir "$WORK/data" wallet --mint "$MINT" balance
echo "== Withdrawing more than was earned is refused"
"$BIN" --data-dir "$WORK/data" wallet --mint "$MINT" withdraw 300 || true
