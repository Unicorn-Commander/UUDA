#!/usr/bin/env bash
# Alert when a registered UUDA device stops heartbeating.
#
# Why this exists: every other check in the system reasons about a signal that
# ARRIVED. The drift scheduler explicitly scans only devices whose
# last_heartbeat_at is within 90 seconds, so a host that dies completely is not
# reported as unhealthy -- it silently drops out of the fleet and every
# remaining check goes green. Absence of signal looks identical to success.
#
# The per-host fleet-health check cannot cover this either: it runs ON the host,
# so a dead host simply stops sending. Only something central can notice.
#
# Read-only against Postgres. Edge-triggered like the other alerting: one mail
# when a device goes stale, one when it comes back.

set -euo pipefail

CONF="${LIVENESS_CONF:-/home/muut/.config/uuda-liveness.conf}"
[ -r "$CONF" ] && . "$CONF"

CONTAINER="${PG_CONTAINER:-unicorn-postgresql}"
PGUSER="${PG_USER:-unicorn}"
PGDB="${PG_DB:-unicorn_db}"
STALE_MINUTES="${STALE_MINUTES:-10}"
STATE_DIR="${STATE_DIR:-/home/muut/.local/state/uuda-liveness}"
STATE="$STATE_DIR/stale-devices"
LOG="${LOG_FILE:-$STATE_DIR/watchdog.log}"

mkdir -p "$STATE_DIR"
log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >> "$LOG"; }

# One hostname per line, sorted, so a plain diff against the previous run gives
# the transitions. Devices are only "stale" if they registered and then went
# quiet; a device that never heartbeated at all is still reported.
CURRENT=$(timeout 30 docker exec "$CONTAINER" psql -U "$PGUSER" -d "$PGDB" -tAc \
    "select hostname from uuda_devices
      where revoked_at is null
        and (last_heartbeat_at is null
             or last_heartbeat_at < now() - interval '$STALE_MINUTES minutes')
      order by hostname;" 2>/dev/null | sed '/^$/d' || true)

if ! timeout 30 docker exec "$CONTAINER" psql -U "$PGUSER" -d "$PGDB" -tAc "select 1" >/dev/null 2>&1; then
    # Could not reach the database. Say nothing rather than declaring the whole
    # fleet dead: a false "everything is down" at 3am is worse than a gap.
    log "SKIP database unreachable"
    exit 0
fi

PREVIOUS=""
[ -r "$STATE" ] && PREVIOUS=$(cat "$STATE")

NEWLY_STALE=$(comm -13 <(printf '%s\n' "$PREVIOUS" | sed '/^$/d') <(printf '%s\n' "$CURRENT" | sed '/^$/d') || true)
RECOVERED=$(comm -23 <(printf '%s\n' "$PREVIOUS" | sed '/^$/d') <(printf '%s\n' "$CURRENT" | sed '/^$/d') || true)

printf '%s\n' "$CURRENT" | sed '/^$/d' > "$STATE"

send() { # send SUBJECT BODY
    [ -n "${ALERT_EMAIL:-}" ] && [ -n "${SMTP_URL:-}" ] || return 0
    command -v curl >/dev/null 2>&1 || return 0
    eml=$(mktemp)
    {
        printf 'From: Fleet Liveness <%s>\r\n' "${SMTP_FROM:-$SMTP_USER}"
        printf 'To: %s\r\n' "$ALERT_EMAIL"
        printf 'Subject: %s\r\n' "$1"
        printf 'Date: %s\r\n' "$(date -R)"
        printf 'Content-Type: text/plain; charset=utf-8\r\n\r\n'
        # Every line CRLF: Apache James rejects bare LF in DATA and curl only
        # reports "Weird server reply".
        printf '%s' "$2" | sed -e 's/$/\r/'
    } > "$eml"
    if timeout 30 curl -sS --ssl-reqd --url "$SMTP_URL" \
        --user "$SMTP_USER:$SMTP_PASS" \
        --mail-from "${SMTP_FROM:-$SMTP_USER}" --mail-rcpt "$ALERT_EMAIL" \
        -T "$eml" >/dev/null 2>&1; then
        log "mailed: $1"
    else
        log "MAIL FAILED: $1"
    fi
    rm -f "$eml"
}

if [ -n "$NEWLY_STALE" ]; then
    body="These UUDA devices stopped heartbeating (no contact for >${STALE_MINUTES}m):

$NEWLY_STALE

They are now invisible to the drift scanner, so their disk, ZFS and update
checks are NOT running. Check whether the host is down or just the agent:
  systemctl status uc-agent
"
    send "[uuda] device(s) stopped reporting: $(printf '%s' "$NEWLY_STALE" | tr '\n' ' ')" "$body"
    log "newly stale: $(printf '%s' "$NEWLY_STALE" | tr '\n' ' ')"
fi

if [ -n "$RECOVERED" ]; then
    send "[uuda] device(s) reporting again: $(printf '%s' "$RECOVERED" | tr '\n' ' ')" \
         "These UUDA devices are heartbeating again:

$RECOVERED
"
    log "recovered: $(printf '%s' "$RECOVERED" | tr '\n' ' ')"
fi

exit 0
