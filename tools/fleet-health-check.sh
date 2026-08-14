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
JSON="$JSON,\"docker\":{\"running\":$DOCK_RUNNING,\"floating_tags\":$DOCK_FLOAT}}"

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
    fi
    NOWS=$(date +%s)
    REPEAT=${ALERT_REPEAT_S:-21600}
    SEND=0; SUBJ=""
    if [ "$STATUS" = CRIT ]; then
      if [ "$PREV" != CRIT ]; then
        SEND=1; SUBJ="[fleet-health] CRIT on $HOST"
      elif [ $((NOWS - LAST)) -ge "$REPEAT" ]; then
        SEND=1; SUBJ="[fleet-health] STILL CRIT on $HOST"
      fi
    elif [ "$PREV" = CRIT ]; then
      SEND=1; SUBJ="[fleet-health] recovered on $HOST (now $STATUS)"
    fi

    if [ "$SEND" = 1 ]; then
      EML=$(mktemp 2>/dev/null || echo /tmp/fh-alert.$$)
      {
        printf 'From: Fleet Health <%s>\r\n' "${SMTP_FROM:-$SMTP_USER}"
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
      } > "$EML" 2>/dev/null
      CURLERR=$($T curl -sS --ssl-reqd --url "$SMTP_URL" \
             --user "$SMTP_USER:$SMTP_PASS" \
             --mail-from "${SMTP_FROM:-$SMTP_USER}" --mail-rcpt "$ALERT_EMAIL" \
             -T "$EML" 2>&1 >/dev/null)
      if [ $? -eq 0 ]; then
        printf '%s\t%s\n' "$STATUS" "$NOWS" > "$STATEF" 2>/dev/null || true
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
      printf '%s\t%s\n' "$STATUS" "${LAST:-0}" > "$STATEF" 2>/dev/null || true
    fi
  fi
fi

exit 0
