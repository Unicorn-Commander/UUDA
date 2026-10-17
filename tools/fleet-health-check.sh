#!/bin/bash
# fleet-health-check.sh - read-only fleet health probe.
# Emits ONE line of JSON on stdout. ALWAYS exits 0 (status lives in the JSON)
# so cron/systemd never spam. Every external command is wrapped in `timeout`.
#
# Doctrine: SMART overall-health PASSED/FAILED is IGNORED entirely. Only RAW
# attribute counters are trusted. (Seagate REDACTED_HARDWARE_ID reported PASSED while
# carrying 2776 pending + 2776 uncorrectable sectors.)
#
# Read-only: never starts/stops/reconfigures a service, never touches a zpool
# beyond `zpool list`.

export LC_ALL=C
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

CONF=${FLEET_HEALTH_CONF:-/etc/fleet-health.conf}
[ -r "$CONF" ] && . "$CONF"

LOGFILE=${LOGFILE:-/var/log/fleet-health.json}
T="timeout 20"

# ---------- privilege ----------
SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  if timeout 5 sudo -n true 2>/dev/null; then SUDO="sudo -n"; else SUDO="NONE"; fi
fi

# ---------- state ----------
STATUS=OK           # OK < WARN < CRIT
MSGS=""             # human-readable, "; " separated
declare -a JDISK

bump() { # bump LEVEL
  case "$1" in
    CRIT) STATUS=CRIT ;;
    WARN) [ "$STATUS" = CRIT ] || STATUS=WARN ;;
  esac
}
note() { # note LEVEL "text"
  bump "$1"
  if [ -z "$MSGS" ]; then MSGS="$1: $2"; else MSGS="$MSGS; $1: $2"; fi
}
jesc() { # JSON-escape stdin-free arg
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/\t/ /g' | tr -d '\000-\037'
}
num() { # normalize to an integer, default 0
  case "$1" in
    ''|*[!0-9]*) printf '0' ;;
    *) printf '%s' "$1" ;;
  esac
}

# =====================================================================
# 1. SMART - per physical disk, RAW counters only
# =====================================================================
SMART_STATE=ok
if ! command -v smartctl >/dev/null 2>&1; then
  SMART_STATE=missing_smartctl
elif [ "$SUDO" = NONE ]; then
  SMART_STATE=no_privilege
else
  DISKS=$($T lsblk -dn -o NAME,TYPE 2>/dev/null | awk '$2=="disk"{print $1}' \
          | grep -Ev '^(loop|ram|zram|sr|fd|dm-|md)' )
  [ -z "$DISKS" ] && SMART_STATE=no_disks
  for d in $DISKS; do
    dev="/dev/$d"
    out=$($T $SUDO smartctl -i -A -H -d auto "$dev" 2>/dev/null)
    if [ -z "$out" ]; then
      JDISK+=("{\"dev\":\"$(jesc "$d")\",\"state\":\"unreadable\"}")
      note WARN "$d SMART unreadable"
      continue
    fi

    serial=$(printf '%s\n' "$out" | awk -F: '/^Serial Number/{gsub(/^[ \t]+|[ \t]+$/,"",$2);print $2;exit}')
    model=$(printf '%s\n' "$out"  | awk -F: '/^(Device Model|Model Number|Product)/{gsub(/^[ \t]+|[ \t]+$/,"",$2);print $2;exit}')
    [ -z "$serial" ] && serial="unknown-$d"

    if printf '%s\n' "$out" | grep -q 'SMART/Health Information (NVMe'; then
      # ---- NVMe ----
      pct=$(printf '%s\n' "$out"  | awk -F: '/Percentage Used/{gsub(/[^0-9]/,"",$2);print $2;exit}')
      mderr=$(printf '%s\n' "$out"| awk -F: '/Media and Data Integrity Errors/{gsub(/[^0-9]/,"",$2);print $2;exit}')
      spare=$(printf '%s\n' "$out"| awk -F: '/^Available Spare:/{gsub(/[^0-9]/,"",$2);print $2;exit}')
      pct=$(num "$pct"); mderr=$(num "$mderr"); spare=$(num "$spare")
      dstat=ok
      if [ "$mderr" -gt 0 ]; then note CRIT "$serial NVMe media/data-integrity errors=$mderr"; dstat=crit; fi
      if [ "$pct" -gt 85 ]; then note WARN "$serial NVMe wear ${pct}%"; [ "$dstat" = ok ] && dstat=warn; fi
      JDISK+=("{\"dev\":\"$(jesc "$d")\",\"serial\":\"$(jesc "$serial")\",\"model\":\"$(jesc "$model")\",\"kind\":\"nvme\",\"state\":\"$dstat\",\"pct_used\":$pct,\"media_errors\":$mderr,\"avail_spare\":$spare}")
    else
      # ---- SATA/SAS ----
      attr() { printf '%s\n' "$out" | awk -v n="$1" '$2==n {print $10; exit}'; }
      pend=$(num "$(attr Current_Pending_Sector)")
      unc=$(num "$(attr Offline_Uncorrectable)")
      realloc=$(num "$(attr Reallocated_Sector_Ct)")
      crc=$(num "$(attr UDMA_CRC_Error_Count)")
      # SAS fallback: no ATA attribute table at all
      if ! printf '%s\n' "$out" | grep -q 'ID# ATTRIBUTE_NAME'; then
        gerr=$(printf '%s\n' "$out" | awk -F: '/Elements in grown defect list/{gsub(/[^0-9]/,"",$2);print $2;exit}')
        realloc=$(num "$gerr")
      fi
      dstat=ok
      if [ "$pend" -gt 0 ] || [ "$unc" -gt 0 ]; then
        note CRIT "$serial pending=$pend uncorrectable=$unc"; dstat=crit
      elif [ "$realloc" -gt 0 ] || [ "$crc" -gt 0 ]; then
        note WARN "$serial realloc=$realloc crc=$crc"; dstat=warn
      fi
      JDISK+=("{\"dev\":\"$(jesc "$d")\",\"serial\":\"$(jesc "$serial")\",\"model\":\"$(jesc "$model")\",\"kind\":\"sata\",\"state\":\"$dstat\",\"pending\":$pend,\"uncorrectable\":$unc,\"realloc\":$realloc,\"crc\":$crc}")
    fi
  done
fi
# SMART_OPTIONAL=1 (set in /etc/fleet-health.conf) for hosts where SMART is
# genuinely inapplicable - virtio/cloud disks, SD cards - so a blind host does
# not sit in permanent WARN. Everywhere else, blindness IS a finding.
if [ "${SMART_OPTIONAL:-0}" != 1 ]; then
  case "$SMART_STATE" in
    missing_smartctl) note WARN "smartctl not installed - disks unmonitored" ;;
    no_privilege)     note WARN "no root/sudo - SMART unmonitored" ;;
  esac
fi
[ "$SUDO" = NONE ] && SUDO=""
[ "$SUDO" = NONE ] && SUDO=""

# =====================================================================
# 2. ZFS - read-only
# =====================================================================
ZFS_STATE=absent
JZPOOL=""
if command -v zpool >/dev/null 2>&1; then
  zout=$($T zpool list -H -o name,health 2>/dev/null)
  rc=$?
  if [ $rc -ne 0 ] && [ $rc -ge 124 ]; then
    ZFS_STATE=timeout
    note CRIT "zpool list timed out (>20s) - likely wedged I/O"
  elif [ -z "$zout" ]; then
    ZFS_STATE=no_pools
  else
    # ZFS_STATE must degrade with the worst pool. It used to be pinned to "ok"
    # here and never lowered, so a machine consumer asserting zfs.state == "ok"
    # would pass while a pool was SUSPENDED - the same trap as smartctl's
    # overall PASSED verdict. The per-pool health array and the CRIT notes were
    # always right; only this summary field lied.
    ZFS_STATE=ok
    while IFS=$'\t ' read -r pname phealth; do
      [ -z "$pname" ] && continue
      [ -n "$JZPOOL" ] && JZPOOL="$JZPOOL,"
      JZPOOL="$JZPOOL{\"pool\":\"$(jesc "$pname")\",\"health\":\"$(jesc "$phealth")\"}"
      case "$phealth" in
        ONLINE) ;;
        SUSPENDED) ZFS_STATE=suspended; note CRIT "zpool $pname SUSPENDED" ;;
        *) [ "$ZFS_STATE" = suspended ] || ZFS_STATE=degraded
           note CRIT "zpool $pname $phealth" ;;
      esac
    done <<< "$zout"
  fi
fi

# =====================================================================
# 3. Update freshness / dpkg lock
# =====================================================================
NOW=$(date +%s)
APT_AGE=-1
STAMP=/var/lib/apt/periodic/update-success-stamp
[ -f "$STAMP" ] || STAMP=/var/lib/apt/periodic/update-stamp
if [ -f "$STAMP" ]; then
  m=$(stat -c %Y "$STAMP" 2>/dev/null)
  [ -n "$m" ] && APT_AGE=$(( (NOW - m) / 3600 ))
elif [ -d /var/lib/apt/lists ]; then
  m=$(stat -c %Y /var/lib/apt/lists 2>/dev/null)
  [ -n "$m" ] && APT_AGE=$(( (NOW - m) / 3600 ))
fi
# APT_AGE_OPTIONAL=1 for hosts where apt metadata age is meaningless. piguy1
# live-boots a read-only SD image with a tmpfs overlay: apt-daily is disabled and
# unattended-upgrades is masked on purpose, the writable layer is discarded on
# every reboot, and `apt-get update` would spend ~40% of the remaining overlay
# headroom to produce a number that resets to the image build date anyway. A
# permanently-unfixable WARN is noise that trains you to ignore the check.
if [ "${APT_AGE_OPTIONAL:-0}" = 1 ]; then
  :
else
  [ "$APT_AGE" -gt 168 ] 2>/dev/null && note WARN "apt metadata ${APT_AGE}h old (>7d)"
fi

UU_AGE=-1
for f in /var/lib/apt/periodic/unattended-upgrades-success-stamp \
         /var/log/unattended-upgrades/unattended-upgrades.log; do
  if [ -e "$f" ]; then
    m=$(stat -c %Y "$f" 2>/dev/null)
    [ -n "$m" ] && UU_AGE=$(( (NOW - m) / 3600 )) && break
  fi
done

# ---- dpkg/apt lock actually HELD, and for how long ----
# NOTE: `unattended-upgrade-shutdown --wait-for-signal` is a normal, permanently
# running daemon on every Ubuntu box. Matching it by comm (`unattended-upgr`)
# reports a multi-week "stuck lock" on a perfectly healthy host. Match on full
# argv and exclude it explicitly.
LOCK_AGE=0
LOCK_PID=""
LOCK_CMD=""
LOCK_SRC=none
for lk in /var/lib/dpkg/lock-frontend /var/lib/dpkg/lock; do
  [ -e "$lk" ] || continue
  p=$($T $SUDO fuser "$lk" 2>/dev/null | tr -s ' ' '\n' | grep -E '^[0-9]+$' | head -1)
  if [ -n "$p" ]; then LOCK_PID=$p; LOCK_SRC=fuser; break; fi
done
# fuser is authoritative when we are root: no holder means the lock is FREE.
# Only fall back to process inspection when we could not ask the kernel.
if [ -z "$LOCK_PID" ] && { [ -z "$SUDO" ] && [ "$(id -u)" -ne 0 ]; }; then
  LOCK_PID=$($T ps -eo pid=,etimes=,args= 2>/dev/null | awk '
    $2 > 1800 &&
    $0 ~ /(\/|^)(apt|apt-get|aptitude|dpkg|unattended-upgrade)([ ]|$)/ &&
    $0 !~ /unattended-upgrade-shutdown/ &&
    $0 !~ /fleet-health/ { print $1; exit }')
  [ -n "$LOCK_PID" ] && LOCK_SRC=ps
fi
if [ -n "$LOCK_PID" ]; then
  LOCK_AGE=$(num "$($T ps -o etimes= -p "$LOCK_PID" 2>/dev/null | tr -d ' ')")
  LOCK_CMD=$($T ps -o comm= -p "$LOCK_PID" 2>/dev/null | tr -d '\n')
  if [ "$LOCK_AGE" -gt 3600 ]; then
    note CRIT "dpkg/apt lock held ${LOCK_AGE}s by $LOCK_CMD (pid $LOCK_PID)"
  elif [ "$LOCK_AGE" -gt 1800 ]; then
    note WARN "dpkg/apt lock held ${LOCK_AGE}s by $LOCK_CMD (pid $LOCK_PID)"
  fi
fi

# ---- apt/unattended-upgrade systemd unit wedged mid-run ----
# This is the precise signature of the 2026-08-11 bigboy failure: a oneshot unit
# stuck in `activating` for hours because its ExecStart D-stated on a suspended
# zpool. A healthy unattended-upgrades.service sits in `active` forever, so only
# `activating` counts.
UNIT_STUCK=""
UNIT_STUCK_AGE=0
for u in apt-daily.service apt-daily-upgrade.service unattended-upgrades.service; do
  ustate=$($T systemctl show "$u" -p ActiveState --value 2>/dev/null)
  [ "$ustate" = activating ] || continue
  uts=$($T systemctl show "$u" -p InactiveExitTimestamp --value 2>/dev/null)
  [ -n "$uts" ] || continue
  us=$(date -d "$uts" +%s 2>/dev/null) || continue
  [ -n "$us" ] || continue
  uage=$(( NOW - us ))
  [ "$uage" -gt "$UNIT_STUCK_AGE" ] && { UNIT_STUCK="$u"; UNIT_STUCK_AGE=$uage; }
done
if [ -n "$UNIT_STUCK" ]; then
  if [ "$UNIT_STUCK_AGE" -gt 3600 ]; then
    note CRIT "$UNIT_STUCK stuck activating ${UNIT_STUCK_AGE}s"
  elif [ "$UNIT_STUCK_AGE" -gt 1800 ]; then
    note WARN "$UNIT_STUCK stuck activating ${UNIT_STUCK_AGE}s"
  fi
fi

# =====================================================================
# 4. D-state processes (cheap proxy for wedged I/O)
# =====================================================================
DSTATE=$(num "$($T ps -eo stat= 2>/dev/null | grep -c '^D')")
DPROCS=""
if [ "$DSTATE" -gt 2 ]; then
  DPROCS=$($T ps -eo stat=,comm= 2>/dev/null | awk '$1 ~ /^D/{print $2}' | sort -u | head -5 | tr '\n' ',' | sed 's/,$//')
  note WARN "$DSTATE processes in uninterruptible sleep (${DPROCS})"
fi

# =====================================================================
# 5. Disk space
# =====================================================================
JFS=""
dfout=$($T df -P -l 2>/dev/null | tail -n +2)
while read -r fs blocks used avail pcent mnt; do
  [ -z "$mnt" ] && continue
  case "$fs" in
    tmpfs|devtmpfs|udev|none|overlay|shm) continue ;;
  esac
  case "$mnt" in
    /snap/*|/var/lib/docker/*|/run/*|/dev|/dev/*|/sys/*) continue ;;
  esac
  p=$(num "$(printf '%s' "$pcent" | tr -d '%')")
  [ "$p" -lt 85 ] && continue
  [ -n "$JFS" ] && JFS="$JFS,"
  JFS="$JFS{\"mount\":\"$(jesc "$mnt")\",\"pct\":$p}"
  if   [ "$p" -ge 95 ]; then note CRIT "$mnt ${p}% full"
  elif [ "$p" -ge 90 ]; then note WARN "$mnt ${p}% full"
  fi
done <<< "$dfout"

# =====================================================================
# 6. Docker (optional) - weak image-currency proxy
# =====================================================================
DOCK_RUNNING=-1
DOCK_FLOAT=-1
if command -v docker >/dev/null 2>&1; then
  imgs=$($T docker ps --format '{{.Image}}' 2>/dev/null)
  if [ -n "$imgs" ]; then
    DOCK_RUNNING=$(printf '%s\n' "$imgs" | grep -c .)
    DOCK_FLOAT=$(printf '%s\n' "$imgs" | awk -F/ '{print $NF}' \
      | awk '{ if ($0 !~ /:/ || $0 ~ /:latest$/) c++ } END{print c+0}')
  elif $T docker info >/dev/null 2>&1; then
    DOCK_RUNNING=0; DOCK_FLOAT=0
  fi
fi

# =====================================================================
# 7. Containers - restart loops and wedged creations
# =====================================================================
# Added 2026-09-15. A restart loop is how one dead dependency silently takes
# down everything that needs it: a corrupt Redis AOF put thirteen containers
# into Restarting for 27 hours, across a clean reboot, and the only outward
# symptom was one hostname serving Traefik's default 404. `docker ps` alone
# looks fine, because a restarting container is not "running" and simply
# vanishes from the default listing.
CONT_RUNNING=-1
CONT_BAD=0
CONT_RESTARTING=0
JCONT=""
CONT_NAMES=""
if command -v docker >/dev/null 2>&1; then
  crun=$($T docker ps -q 2>/dev/null | grep -c .)
  CONT_RUNNING=$(num "$crun")
  cbad=$($T docker ps -a --format '{{.Names}}|{{.Status}}' 2>/dev/null \
         | grep -E '\|(Restarting|Created)' | head -25)
  if [ -n "$cbad" ]; then
    while IFS='|' read -r cname cstat; do
      [ -z "$cname" ] && continue
      CONT_BAD=$((CONT_BAD + 1))
      case "$cstat" in Restarting*) CONT_RESTARTING=$((CONT_RESTARTING + 1)) ;; esac
      [ -n "$JCONT" ] && JCONT="$JCONT,"
      JCONT="$JCONT{\"name\":\"$(jesc "$cname")\",\"state\":\"$(jesc "$cstat")\"}"
      if [ -z "$CONT_NAMES" ]; then CONT_NAMES="$cname"
      else CONT_NAMES="$CONT_NAMES,$cname"; fi
    done <<< "$cbad"
  fi
  if [ "$CONT_RESTARTING" -gt 0 ]; then
    note CRIT "$CONT_RESTARTING container(s) restarting: $CONT_NAMES"
  elif [ "$CONT_BAD" -gt 0 ]; then
    # A container wedged in Created is a deploy that never started - it needs a
    # human, but nothing is flapping, so it is not an outage.
    note WARN "$CONT_BAD container(s) stuck in Created: $CONT_NAMES"
  fi
fi
[ -z "$JCONT" ] && JCONT="[]"

# =====================================================================
# 8. TLS certificates in the local ACME store (routing-aware)
# =====================================================================
# Added 2026-09-15 after ten certificates were found expired across the fleet,
# one of them by 281 days, on customer-facing hostnames - while every page
# still looked perfect. Cloudflare terminates TLS with its OWN valid edge
# certificate and every zone is "Full" rather than "Full (strict)", so it never
# validates the origin certificate at all. Checking the public URL proves
# nothing; the only way to see this is to read the origin's own store.
#
# ROUTING-AWARE ON PURPOSE: a certificate only matters if this host still
# serves that hostname. An ACME store accumulates entries for names that moved
# to another origin long ago and Traefik retries them forever; on bigboy every
# single failing certificate was one of those eight. Raising them would teach
# everyone to ignore this check, so unrouted names are counted separately and
# never alert.
#
# The helper emits shell-safe KEY=VALUE lines rather than JSON: parsing JSON
# with sed in bash breaks on the first embedded quote or bracket, and a monitor
# that misparses its own input is worse than one that does not run.
CERT_STATE=absent
CERT_TOTAL=0
CERT_EXPIRED=0
CERT_SOON=0
CERT_STALE=0
CERT_HTTP01=0
CERT_MSG=""
JCERT="[]"
# Helper location differs by install mode: root hosts get /usr/local/lib,
# while commander and centerdeep run this as `muut` from ~/.local and cannot
# write there. Search rather than assume - a hardcoded path would silently
# disable the whole cert check on exactly the host that had the expired one.
if [ -z "${CERT_HELPER:-}" ]; then
  for _ch in /usr/local/lib/fleet-health-certs.py "$HOME/.local/lib/fleet-health-certs.py"; do
    [ -r "$_ch" ] && { CERT_HELPER="$_ch"; break; }
  done
fi
CERT_HELPER=${CERT_HELPER:-/usr/local/lib/fleet-health-certs.py}
if command -v docker >/dev/null 2>&1 && command -v python3 >/dev/null 2>&1 \
   && [ -r "$CERT_HELPER" ]; then
  if $T docker ps --format '{{.Names}}' 2>/dev/null | grep -qi traefik; then
    certout=$(CERT_WARN_DAYS="${CERT_WARN_DAYS:-21}" timeout 60 python3 "$CERT_HELPER" 2>/dev/null)
    if [ -n "$certout" ]; then
      while IFS='=' read -r ck cv; do
        case "$ck" in
          CERT_STATE)    CERT_STATE="$cv" ;;
          CERT_TOTAL)    CERT_TOTAL=$(num "$cv") ;;
          CERT_EXPIRED)  CERT_EXPIRED=$(num "$cv") ;;
          CERT_SOON)     CERT_SOON=$(num "$cv") ;;
          CERT_STALE)    CERT_STALE=$(num "$cv") ;;
          CERT_HTTP01)   CERT_HTTP01=$(num "$cv") ;;
          CERT_MSG_B64)  CERT_MSG=$(printf '%s' "$cv" | base64 -d 2>/dev/null) ;;
          CERT_ITEMS_B64) JCERT=$(printf '%s' "$cv" | base64 -d 2>/dev/null) ;;
        esac
      done <<< "$certout"
      [ -z "$JCERT" ] && JCERT="[]"
      if [ "$CERT_EXPIRED" -gt 0 ]; then
        note CRIT "$CERT_EXPIRED routed TLS cert(s) EXPIRED: $CERT_MSG"
      elif [ "$CERT_SOON" -gt 0 ]; then
        note WARN "$CERT_SOON routed TLS cert(s) expire within ${CERT_WARN_DAYS:-21}d: $CERT_MSG"
      fi
      # HTTP-01 behind the Cloudflare proxy renews nothing, silently, until
      # certificates start expiring. Worth saying out loud even while every
      # routed certificate is still valid.
      [ "$CERT_HTTP01" -gt 0 ] 2>/dev/null && \
        note WARN "$CERT_HTTP01 ACME resolver(s) still on httpChallenge (cannot validate behind the CF proxy)"
    else
      CERT_STATE=unreadable
      note WARN "traefik present but ACME store unreadable - certs unmonitored"
    fi
  fi
fi

# =====================================================================
# 9. Public endpoint probe (opt-in, one host)
# =====================================================================
# ENDPOINT_PROBE=1 plus ENDPOINT_LIST=/path/to/hostnames in the conf. Off by
# default and deliberately enabled on ONE host: probing the same 50 hostnames
# from twelve hosts every 15 minutes is hundreds of requests an hour to prove
# one fact. This is the end-to-end view - it goes through Cloudflare, so it
# sees what a customer sees, which is exactly what section 8 cannot tell you.
EP_CHECKED=0
EP_BAD=0
EP_NAMES=""
JEP="[]"
if [ "${ENDPOINT_PROBE:-0}" = 1 ] && [ -n "${ENDPOINT_LIST:-}" ] && [ -r "${ENDPOINT_LIST}" ] \
   && command -v curl >/dev/null 2>&1; then
  eplist=$(grep -vE '^[[:space:]]*(#|$)' "$ENDPOINT_LIST" 2>/dev/null | head -"${ENDPOINT_MAX:-60}")
  if [ -n "$eplist" ]; then
    epres=$(printf '%s\n' "$eplist" | timeout 120 xargs -r -P "${ENDPOINT_PAR:-10}" -I{} \
      sh -c 'printf "%s %s\n" "$1" "$(curl -s -o /dev/null -m 6 -w "%{http_code}" "https://$1/" 2>/dev/null || echo 000)"' _ {} 2>/dev/null)
    JEPB=""
    while read -r ephost epcode; do
      [ -z "$ephost" ] && continue
      EP_CHECKED=$((EP_CHECKED + 1))
      case "$epcode" in
        000|502|503|504)
          EP_BAD=$((EP_BAD + 1))
          [ -n "$JEPB" ] && JEPB="$JEPB,"
          JEPB="$JEPB{\"host\":\"$(jesc "$ephost")\",\"code\":\"$(jesc "$epcode")\"}"
          if [ -z "$EP_NAMES" ]; then EP_NAMES="$ephost($epcode)"
          else EP_NAMES="$EP_NAMES,$ephost($epcode)"; fi
          ;;
      esac
    done <<< "$epres"
    [ -n "$JEPB" ] && JEP="[$JEPB]"
    [ "$EP_BAD" -gt 0 ] && note CRIT "$EP_BAD/$EP_CHECKED public endpoint(s) not serving: $EP_NAMES"
  fi
fi

# =====================================================================
# 9b. Internal service probe (opt-in, one host)
# =====================================================================
# SERVICE_LIST=/path/to/file in the conf: one "name url" per line, probed over
# the tailnet. Section 9 only sees public hostnames through Cloudflare, so a
# GPU box that drops off the LAN (2026-10-17: midboy1+2 knocked onto the wrong
# VLAN, every house STT path dead for an evening) was invisible to it.
SVC_CHECKED=0
SVC_BAD=0
SVC_NAMES=""
if [ -n "${SERVICE_LIST:-}" ] && [ -r "${SERVICE_LIST}" ] && command -v curl >/dev/null 2>&1; then
  while read -r svname svurl; do
    [ -z "$svname" ] && continue
    case "$svname" in \#*) continue ;; esac
    SVC_CHECKED=$((SVC_CHECKED + 1))
    svcode=$(curl -s -o /dev/null -m 8 -w "%{http_code}" "$svurl" 2>/dev/null)
    case "$svcode" in
      2*|3*|401|403|404|405) ;;
      *)
        SVC_BAD=$((SVC_BAD + 1))
        if [ -z "$SVC_NAMES" ]; then SVC_NAMES="$svname(${svcode:-000})"
        else SVC_NAMES="$SVC_NAMES,$svname(${svcode:-000})"; fi
        ;;
    esac
  done < <(grep -vE '^[[:space:]]*(#|$)' "$SERVICE_LIST" 2>/dev/null)
  [ "$SVC_BAD" -gt 0 ] && note CRIT "internal service down: $SVC_NAMES"
fi

# =====================================================================
# Emit
# =====================================================================
HOST=$(hostname)
TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
[ -z "$MSGS" ] && MSGS="all checks nominal"

DISKS_JSON=$(IFS=,; printf '%s' "${JDISK[*]}")

JSON="{\"host\":\"$(jesc "$HOST")\",\"ts\":\"$TS\",\"status\":\"$STATUS\",\"msg\":\"$(jesc "$MSGS")\""
JSON="$JSON,\"smart\":{\"state\":\"$SMART_STATE\",\"disks\":[$DISKS_JSON]}"
JSON="$JSON,\"zfs\":{\"state\":\"$ZFS_STATE\",\"pools\":[$JZPOOL]}"
JSON="$JSON,\"updates\":{\"apt_age_h\":$APT_AGE,\"unattended_age_h\":$UU_AGE,\"lock_age_s\":$LOCK_AGE,\"lock_cmd\":\"$(jesc "$LOCK_CMD")\",\"lock_src\":\"$LOCK_SRC\",\"stuck_unit\":\"$(jesc "$UNIT_STUCK")\",\"stuck_unit_age_s\":$UNIT_STUCK_AGE}"
JSON="$JSON,\"dstate\":{\"count\":$DSTATE,\"procs\":\"$(jesc "$DPROCS")\"}"
JSON="$JSON,\"fs_alerts\":[$JFS]"
JSON="$JSON,\"docker\":{\"running\":$DOCK_RUNNING,\"floating_tags\":$DOCK_FLOAT}"
JSON="$JSON,\"containers\":{\"running\":$CONT_RUNNING,\"bad\":$CONT_BAD,\"restarting\":$CONT_RESTARTING,\"items\":$JCONT}"
JSON="$JSON,\"certs\":{\"state\":\"$(jesc "$CERT_STATE")\",\"total\":$CERT_TOTAL,\"expired\":$CERT_EXPIRED,\"soon\":$CERT_SOON,\"stale_unrouted\":$CERT_STALE,\"http01_resolvers\":$CERT_HTTP01,\"items\":$JCERT}"
JSON="$JSON,\"endpoints\":{\"checked\":$EP_CHECKED,\"bad\":$EP_BAD,\"items\":$JEP}}"

printf '%s\n' "$JSON"

# Local sinks (best-effort, never fatal)
if [ "${NO_SINK:-0}" != 1 ]; then
  ( printf '%s\n' "$JSON" > "$LOGFILE".tmp 2>/dev/null && mv -f "$LOGFILE".tmp "$LOGFILE" 2>/dev/null ) || true
  command -v logger >/dev/null 2>&1 && logger -t fleet-health -p daemon.info -- "$STATUS $MSGS" 2>/dev/null

  # Uptime-Kuma push transport (only if wired up in /etc/fleet-health.conf)
  if [ -n "${KUMA_PUSH_URL:-}" ] && command -v curl >/dev/null 2>&1; then
    case "$STATUS" in
      OK)   kstatus=up ;;
      WARN) kstatus=${KUMA_WARN_STATUS:-up} ;;
      *)    kstatus=down ;;
    esac
    kmsg=$(printf '%s' "$STATUS: $MSGS" | cut -c1-450)
    $T curl -fsS -G --max-time 15 -o /dev/null \
        --data-urlencode "status=$kstatus" \
        --data-urlencode "msg=$kmsg" \
        --data-urlencode "ping=" \
        "$KUMA_PUSH_URL" 2>/dev/null || \
      logger -t fleet-health -p daemon.warning -- "kuma push failed" 2>/dev/null
  fi

  # ---- Email sink (edge-triggered) ----------------------------------
  # Fires on the TRANSITION into CRIT, not on every 15-minute tick, plus one
  # recovery notice on the way back out. A level-triggered alert on a condition
  # like a dying disk would send 96 identical mails a day and get filtered,
  # which is the same as having no alerting at all. ALERT_REPEAT_S re-nags on a
  # condition that stays CRIT, so it can't be silently forgotten either.
  if [ -n "${ALERT_EMAIL:-}" ] && [ -n "${SMTP_URL:-}" ] && command -v curl >/dev/null 2>&1; then
    STATEF=${ALERT_STATE_FILE:-/var/lib/fleet-health/alert-state}
    mkdir -p "$(dirname "$STATEF")" 2>/dev/null || true
    PREV=""; LAST=0
    if [ -r "$STATEF" ]; then
      PREV=$(cut -f1 "$STATEF" 2>/dev/null)
      LAST=$(num "$(cut -f2 "$STATEF" 2>/dev/null)")
      PREVSIG=$(cut -f3 "$STATEF" 2>/dev/null)
    fi
    # Signature of WHICH CRIT conditions are present, with counts masked so a
    # climbing sector count is not "new". A host that sits in CRIT for weeks
    # (bigboy's dying disks) used to swallow every new CRIT until the re-nag;
    # now a new reason mails on its own.
    SIG=$(printf '%s' "$MSGS" | tr ';' '\n' | sed -n 's/^ *CRIT: *//p' \
          | sed -E 's/=[0-9]+/=N/g; s/[0-9]+\/[0-9]+/N\/N/g; s/[0-9]+%/N%/g; s/\([0-9]{3}\)//g' \
          | sort -u | md5sum | cut -c1-12)
    NOWS=$(date +%s)
    REPEAT=${ALERT_REPEAT_S:-21600}
    SEND=0; SUBJ=""
    if [ "$STATUS" = CRIT ]; then
      if [ "$PREV" != CRIT ]; then
        SEND=1; SUBJ="[fleet-health] CRIT on $HOST"
      elif [ -n "$PREVSIG" ] && [ "$SIG" != "$PREVSIG" ]; then
        SEND=1; SUBJ="[fleet-health] CRIT changed on $HOST"
      elif [ $((NOWS - LAST)) -ge "$REPEAT" ]; then
        SEND=1; SUBJ="[fleet-health] STILL CRIT on $HOST"
      fi
    elif [ "$PREV" = CRIT ]; then
      SEND=1; SUBJ="[fleet-health] recovered on $HOST (now $STATUS)"
    fi

    if [ "$SEND" = 1 ]; then
      # Default the queue next to the LOGFILE rather than to /var/lib: on
      # commander and centerdeep this runs as `muut`, which cannot write
      # /var/lib/fleet-health, and the append would fail silently - leaving the
      # two hosts that most need triage with an empty queue.
      QUEUE=${TRIAGE_QUEUE:-$(dirname "$LOGFILE")/triage-queue.jsonl}
      mkdir -p "$(dirname "$QUEUE")" 2>/dev/null || true
      printf '{"ts":"%s","host":"%s","status":"%s","subj":"%s","msg":"%s"}\n' \
        "$TS" "$(jesc "$HOST")" "$STATUS" "$(jesc "$SUBJ")" "$(jesc "$MSGS")" \
        >> "$QUEUE" 2>/dev/null || true
      chmod 0644 "$QUEUE" 2>/dev/null || true
      # Keep the queue bounded; The Colonel only ever needs the recent tail.
      if [ "$(wc -l < "$QUEUE" 2>/dev/null || echo 0)" -gt 500 ]; then
        tail -200 "$QUEUE" > "$QUEUE.tmp" 2>/dev/null && mv -f "$QUEUE.tmp" "$QUEUE" 2>/dev/null
      fi
      EML=$(mktemp 2>/dev/null || echo /tmp/fh-alert.$$)
      {
        printf 'From: The Colonel (fleet watch) <%s>\r\n' "${SMTP_FROM:-$SMTP_USER}"
        printf 'To: %s\r\n' "$ALERT_EMAIL"
        printf 'Subject: %s\r\n' "$SUBJ"
        printf 'Date: %s\r\n' "$(date -R)"
        printf 'Content-Type: text/plain; charset=utf-8\r\n'
        printf '\r\n'
        printf 'host:   %s\r\n' "$HOST"
        printf 'time:   %s\r\n' "$TS"
        printf 'status: %s\r\n\r\n' "$STATUS"
        # Every DATA line must end CRLF. `tr ';' '\n'` emits bare LF, which
        # Apache James rejects outright (curl reports only "Weird server reply"),
        # so re-terminate each line explicitly.
        printf '%s' "$MSGS" | tr ';' '\n' | sed -e 's/^ *//' -e 's/$/\r/'
        printf '\r\nFull JSON: %s on %s\r\n' "$LOGFILE" "$HOST"
        printf 'Reported by The Colonel, the standing fleet watch. This is a\r\n'
        printf 'sensor reading, not a diagnosis, and nothing has been changed.\r\n'
      } > "$EML" 2>/dev/null
      CURLERR=$($T curl -sS --ssl-reqd --url "$SMTP_URL" \
             --user "$SMTP_USER:$SMTP_PASS" \
             --mail-from "${SMTP_FROM:-$SMTP_USER}" --mail-rcpt "$ALERT_EMAIL" \
             -T "$EML" 2>&1 >/dev/null)
      if [ $? -eq 0 ]; then
        printf '%s\t%s\t%s\n' "$STATUS" "$NOWS" "$SIG" > "$STATEF" 2>/dev/null || true
        logger -t fleet-health -p daemon.notice -- "alert mailed: $SUBJ" 2>/dev/null
      else
        # Do NOT record state on failure, so the next run retries the alert.
        # Log WHY: a bare "FAILED" gives the next person nothing to work with.
        logger -t fleet-health -p daemon.err -- \
          "alert mail FAILED: $SUBJ :: $(printf '%s' "$CURLERR" | tr '\n' ' ' | cut -c1-300) :: bytes=$(wc -c < "$EML" 2>/dev/null)" 2>/dev/null
        cp -f "$EML" "${LOGFILE}.failed-alert" 2>/dev/null || true
      fi
      rm -f "$EML" 2>/dev/null || true
    else
      printf '%s\t%s\t%s\n' "$STATUS" "${LAST:-0}" "$SIG" > "$STATEF" 2>/dev/null || true
    fi
  fi
fi

exit 0
