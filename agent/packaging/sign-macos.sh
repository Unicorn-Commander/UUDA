#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Codesigns, notarizes, staples and verifies the macOS uc-agent binary. This is
# what docs/PHASE1_PLAN.md §14 and PHASE1_NOTES.md describe in prose: real
# Developer ID signing, no ad-hoc identity, no xattr workarounds.
#
# Usage:
#   ./sign-macos.sh --binary <path>
#
# Required env:
#   UC_SIGN_IDENTITY      codesigning identity, exactly as `security
#                         find-identity -v -p codesigning` prints it. For this
#                         project that is:
#                           Developer ID Application: AARON MAZZONE STRANSKY (69CT7K6PWR)
#
# Notary credentials, one of:
#   UC_NOTARY_PROFILE     notarytool keychain profile name, created once with
#                           xcrun notarytool store-credentials <name> \
#                             --apple-id <id> --team-id 69CT7K6PWR --password <app-specific>
#   UC_NOTARY_APPLE_ID + UC_NOTARY_TEAM_ID + UC_NOTARY_PASSWORD
#                         app-specific password, read from the environment only.
#
# Optional env:
#   UC_SIGN_BUNDLE_ID     signing identifier (default tech.magicunicorn.uc-agent)
#   UC_SIGN_SKIP_NOTARIZE set to 1 to sign and verify without submitting
#
# Nothing is hardcoded and nothing is written outside a temp dir plus the
# binary's own signature. With nothing configured the script fails in preflight
# and changes nothing.

set -euo pipefail

BINARY="${UC_AGENT_BINARY:-}"
IDENTITY="${UC_SIGN_IDENTITY:-}"
BUNDLE_ID="${UC_SIGN_BUNDLE_ID:-tech.magicunicorn.uc-agent}"
NOTARY_PROFILE="${UC_NOTARY_PROFILE:-}"
NOTARY_APPLE_ID="${UC_NOTARY_APPLE_ID:-}"
NOTARY_TEAM_ID="${UC_NOTARY_TEAM_ID:-}"
NOTARY_PASSWORD="${UC_NOTARY_PASSWORD:-}"
SKIP_NOTARIZE="${UC_SIGN_SKIP_NOTARIZE:-0}"

log()  { printf "\033[1;36m[sign-macos]\033[0m %s\n" "$*"; }
warn() { printf "\033[1;33m[sign-macos]\033[0m %s\n" "$*" >&2; }
fail() { printf "\033[1;31m[sign-macos]\033[0m %s\n" "$*" >&2; exit 1; }

usage() {
    sed -n '3,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --binary)        BINARY="$2"; shift 2 ;;
        --identity)      IDENTITY="$2"; shift 2 ;;
        --skip-notarize) SKIP_NOTARIZE=1; shift ;;
        -h|--help)       usage 0 ;;
        *) fail "unknown arg: $1 (see --help)" ;;
    esac
done

# ---- preflight: everything that can be wrong is checked before anything is signed

[ "$(uname -s)" = Darwin ] || fail "macOS only (codesign/notarytool live here)"

[ -n "$BINARY" ] || fail "no binary: pass --binary or set UC_AGENT_BINARY"
[ -f "$BINARY" ] || fail "binary not found: $BINARY"

command -v codesign >/dev/null 2>&1 || fail "codesign not found; install the Xcode command line tools"
command -v xcrun    >/dev/null 2>&1 || fail "xcrun not found; install the Xcode command line tools"
command -v ditto    >/dev/null 2>&1 || fail "ditto not found"

if [ -z "$IDENTITY" ]; then
    fail "UC_SIGN_IDENTITY is unset. Expected:
    export UC_SIGN_IDENTITY='Developer ID Application: AARON MAZZONE STRANSKY (69CT7K6PWR)'
  List what this machine actually holds with:
    security find-identity -v -p codesigning"
fi

security find-identity -v -p codesigning 2>/dev/null | grep -qF "$IDENTITY" \
    || fail "identity not in any keychain on this host: $IDENTITY
  Check with: security find-identity -v -p codesigning"

NOTARY_ARGS=()
if [ "$SKIP_NOTARIZE" != "1" ]; then
    if [ -n "$NOTARY_PROFILE" ]; then
        NOTARY_ARGS=(--keychain-profile "$NOTARY_PROFILE")
    elif [ -n "$NOTARY_APPLE_ID" ] && [ -n "$NOTARY_TEAM_ID" ] && [ -n "$NOTARY_PASSWORD" ]; then
        NOTARY_ARGS=(--apple-id "$NOTARY_APPLE_ID" --team-id "$NOTARY_TEAM_ID" --password "$NOTARY_PASSWORD")
    else
        fail "no notary credentials. Set UC_NOTARY_PROFILE, or all of
    UC_NOTARY_APPLE_ID / UC_NOTARY_TEAM_ID / UC_NOTARY_PASSWORD.
  Create a profile once with:
    xcrun notarytool store-credentials <name> --apple-id <id> --team-id 69CT7K6PWR --password <app-specific-password>
  Or re-run with --skip-notarize to sign and verify only."
    fi
    xcrun notarytool --help >/dev/null 2>&1 \
        || fail "notarytool unavailable; needs Xcode 13+ command line tools"
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/uc-agent-sign.XXXXXX")"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

# ---- sign

# --options runtime (hardened runtime) and a secure timestamp are both
# prerequisites for notarization; --force makes a re-run idempotent by
# replacing an existing signature instead of erroring.
log "signing as: $IDENTITY"
codesign --force --timestamp --options runtime \
         --identifier "$BUNDLE_ID" \
         --sign "$IDENTITY" \
         "$BINARY"

# ---- notarize

if [ "$SKIP_NOTARIZE" = "1" ]; then
    warn "UC_SIGN_SKIP_NOTARIZE=1: skipping submission. The binary is signed but NOT notarized."
else
    ZIP="$WORK/uc-agent.zip"
    # notarytool takes a zip, pkg or dmg, never a bare Mach-O.
    ditto -c -k --keepParent "$BINARY" "$ZIP"

    log "submitting to the notary service (this blocks until Apple answers)"
    xcrun notarytool submit "$ZIP" "${NOTARY_ARGS[@]}" --wait

    # A bare Mach-O has nowhere to hold a stapled ticket; stapling only works on
    # bundles, .pkg and .dmg. Gatekeeper falls back to an online ticket lookup,
    # so this failing is expected for a standalone binary and is not fatal.
    log "stapling"
    if xcrun stapler staple "$BINARY"; then
        log "ticket stapled"
    else
        warn "could not staple $BINARY. Expected for a standalone executable:"
        warn "the ticket lives online instead. Ship a .pkg or .dmg if an offline"
        warn "first launch has to work."
    fi
fi

# ---- verify

log "codesign --verify --strict"
codesign --verify --strict --verbose=2 "$BINARY"

log "codesign -dv"
codesign -dv --verbose=4 "$BINARY" 2>&1 | grep -E 'Identifier|Authority|TeamIdentifier|Timestamp|flags' || true

# spctl on a non-bundle CLI tool is informational: it can report "rejected"
# purely because the artifact is not an app, and without a stapled ticket it
# needs network to reach the notary service. codesign --verify above is the gate.
log "spctl --assess"
if spctl --assess --type execute --verbose=4 "$BINARY" 2>&1; then
    log "spctl accepted"
else
    warn "spctl did not accept the binary. For a standalone executable this is"
    warn "not conclusive; check the codesign output above and confirm the"
    warn "notary submission was Accepted."
fi

log "done: $BINARY"
