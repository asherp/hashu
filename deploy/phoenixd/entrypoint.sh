#!/bin/sh
#
# Provision the wallet seed (optional), then launch phoenixd.
# Adapted from fathom-x/overpay docker/phoenixd/entrypoint.sh.
#
# - If seed.dat already exists in the datadir volume, it is used as is ("disk
#   wins"): a later-changed secret can never replace a funded wallet.
# - Else, if PHOENIXD_SEED_FILE (preferred) or PHOENIXD_SEED holds a 12/24-word
#   mnemonic, it is written to seed.dat before the first boot (restore).
# - Else phoenixd creates a brand-new wallet. BACK UP seed.dat right away
#   (see deploy/README.md).
set -eu

DATADIR="${PHOENIXD_DATADIR:-/phoenix/.phoenix}"
SEED_PATH="$DATADIR/seed.dat"
log() { printf 'phoenixd-entrypoint: %s\n' "$*" >&2; }

if [ -f "$SEED_PATH" ]; then
  log "seed.dat present, using the existing wallet."
else
  raw=""; src=""
  if [ -n "${PHOENIXD_SEED_FILE:-}" ]; then
    [ -s "$PHOENIXD_SEED_FILE" ] || { log "FATAL: PHOENIXD_SEED_FILE missing or empty: $PHOENIXD_SEED_FILE"; exit 1; }
    raw="$(cat "$PHOENIXD_SEED_FILE")"; src="file"
  elif [ -n "${PHOENIXD_SEED:-}" ]; then
    raw="$PHOENIXD_SEED"; src="PHOENIXD_SEED"
  fi
  if [ -n "$raw" ]; then
    seed="$(printf '%s' "$raw" | tr '\r\n\t' '   ' | tr -s ' ' | sed 's/^ *//; s/ *$//' | tr 'A-Z' 'a-z')"
    words="$(printf '%s' "$seed" | wc -w | tr -d ' ')"
    case "$words" in 12|24) ;; *) log "FATAL: seed has $words words (expected 12 or 24)."; exit 1 ;; esac
    if printf '%s' "$seed" | grep -q '[^a-z ]'; then log "FATAL: seed contains non-letter characters."; exit 1; fi
    mkdir -p "$DATADIR"
    ( umask 077; printf '%s' "$seed" > "$SEED_PATH" )
    log "imported $words-word seed from $src (first boot)."
  else
    log "no seed.dat and no seed provided: phoenixd will create a NEW wallet."
    log "Back it up now: docker compose exec phoenixd cat /phoenix/.phoenix/seed.dat"
  fi
fi
unset PHOENIXD_SEED 2>/dev/null || true

if [ "$#" -gt 0 ]; then exec "$@"; fi

# shellcheck disable=SC2086
set -- ${PHOENIXD_EXTRA_ARGS:-}
exec /phoenix/phoenixd \
  --agree-to-terms-of-service \
  --http-bind-ip=0.0.0.0 \
  --http-bind-port=9740 \
  --http-password="${PHOENIXD_PASSWORD:?PHOENIXD_PASSWORD is required}" \
  --http-password-limited-access="${PHOENIXD_LIMITED_PASSWORD:?PHOENIXD_LIMITED_PASSWORD is required}" \
  "$@"
