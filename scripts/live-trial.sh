#!/bin/sh
# A trial with fake money and real jobs.
#
# Starts a throwaway local mint whose Lightning backend is fake, then runs
# ci-pot as if it were the pot of a coordinator you name. It funds one
# repository's pot with fake sats and charges that coordinator's real Job
# Results from the public relay. It only reads public events: nothing is
# published and no real money moves.
#
# Usage:
#   COORDINATOR=<coordinator public key, hex or npub> \
#   REPO=30617:<maintainer key in hex>:<repository name> \
#   scripts/live-trial.sh
#
# Optional:
#   PROVIDERS=<key>,<key>   compute providers the coordinator uses, if they are
#                           separate keys from the coordinator
#   REPLAY_HOURS=<n>        also charge results from the last n hours (at most
#                           24), as if the pot had been funded that long ago.
#                           Without it, only jobs that finish while the trial
#                           runs are charged, and a quiet hour shows nothing.
#   MINUTES=<n>             how long to watch for new jobs (default 2)
#
# Any Job Result (kind 9841) on wss://relay.ngit.dev shows the values to use:
# the coordinator is the key in its `q` tag, the provider is the key that
# signed it, and the repository is its `a` tag.
#
# Needs Docker, curl, python3 and sqlite3.
set -eu

if [ -z "${COORDINATOR:-}" ] || [ -z "${REPO:-}" ]; then
  echo "Set COORDINATOR and REPO first. See the comment at the top of this file." >&2
  exit 2
fi

BIN="${CI_POT:-target/release/ci-pot}"
MINUTES="${MINUTES:-2}"
REPLAY_HOURS="${REPLAY_HOURS:-0}"
WORK="$(mktemp -d)"
MINT=http://localhost:3338
POT=http://127.0.0.1:4050
REPO_PATH=$(printf '%s' "$REPO" | sed 's/:/%3A/g')

cleanup() {
  kill "$SERVICE" 2>/dev/null || true
  wait 2>/dev/null || true
  docker rm -f ci-pot-trial-mint >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

step() { printf '\n== %s\n' "$1"; }

# The mint image ships a command-line wallet. It plays the funder.
wallet() { docker exec -e MINT_URL="$MINT" ci-pot-trial-mint poetry run cashu -t "$@" 2>/dev/null; }
token() { wallet send "$1" | grep -o 'cashu[AB][A-Za-z0-9_=-]*' | head -1; }

pay() {
  curl -s -X POST "$POT/pots/$REPO_PATH" -H 'content-type: application/json' \
    -d "{\"token\":\"$1\"}" | python3 -I -c "
import sys, json
body = json.load(sys.stdin)
if 'error' in body:
    print('refused:', body['error'])
else:
    print('accepted: the pot now holds %d sat' % (body['pot']['balance_msat'] // 1000))"
}

show_pot() {
  curl -s "$POT/pots/$REPO_PATH" | python3 -I -c "
import sys, json
body = json.load(sys.stdin)
pot = body['pot']
print('balance %d sat   spent %d sat   jobs charged %d   served now: %s' % (
    pot['balance_msat'] // 1000, pot['charged_msat'] // 1000, pot['jobs'],
    'yes' if pot['admitted'] else 'no'))
for entry in body['history'][:5]:
    print('  %-7s %8.1f sat   %s' % (entry['kind'], entry['msat'] / 1000, entry['reference'][:16]))"
}

step "Start a local mint with a fake Lightning backend"
SERVICE=
docker run -d --name ci-pot-trial-mint -p 127.0.0.1:3338:3338 \
  -e MINT_LISTEN_HOST=0.0.0.0 -e MINT_LISTEN_PORT=3338 \
  -e MINT_PRIVATE_KEY=ci-pot-trial-only -e MINT_BACKEND_BOLT11_SAT=FakeWallet \
  cashubtc/nutshell:0.17.0 poetry run mint >/dev/null
until curl -sf "$MINT/v1/info" >/dev/null; do sleep 1; done
wallet invoice 5000 >/dev/null
echo "mint ready at $MINT, funder holds 5000 fake sat"

step "Start ci-pot for the coordinator"
set -- --coordinator "$COORDINATOR" --mint "$MINT" --sat-per-vcpu-hour 50 \
  --repos-file "$WORK/repos.env" --poll-seconds 10 --listen 127.0.0.1:4050
if [ -n "${PROVIDERS:-}" ]; then
  set -- "$@" --provider "$PROVIDERS"
fi
"$BIN" --data-dir "$WORK/data" serve "$@" 2>"$WORK/service.log" &
SERVICE=$!
sleep 3
curl -s "$POT/info" | python3 -I -c "
import sys, json
info = json.load(sys.stdin)
print('price: %d sat per vCPU-hour   accepted mint: %s' % (info['sat_per_vcpu_hour'], info['mints'][0]))"

step "A funder pays 1000 sat into the repository's pot"
FIRST="$(token 1000)"
pay "$FIRST"

step "The same token is paid in again"
pay "$FIRST"

step "The repository is now on the coordinator's list"
cut -c1-110 "$WORK/repos.env"

if [ "$REPLAY_HOURS" -gt 0 ]; then
  step "Replay: treat the pot as funded $REPLAY_HOURS hours ago"
  sqlite3 "$WORK/data/ledger.sqlite" \
    "UPDATE pots SET created_at = created_at - $REPLAY_HOURS * 3600"
  echo "results from the last $REPLAY_HOURS hours will now be charged"
fi

step "Charge the coordinator's real jobs"
elapsed=0
while [ "$elapsed" -lt "$((MINUTES * 60))" ]; do
  sleep 20
  elapsed=$((elapsed + 20))
done
show_pot

step "What the operator has earned"
"$BIN" --data-dir "$WORK/data" wallet --mint "$MINT" balance

step "Withdrawing more than was earned is refused"
"$BIN" --data-dir "$WORK/data" wallet --mint "$MINT" withdraw 5000 || true
