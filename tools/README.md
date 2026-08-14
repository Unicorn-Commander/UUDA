# tools/

## `fleet-health-check.sh`

The agent-independent fallback health check. Deployed to every fleet host on a
15-minute systemd timer (or cron where there is no sudo), it reads SMART **raw
counters**, ZFS pool health including SUSPENDED, apt/update freshness, dpkg lock
duration, D-state count, filesystem fullness and Docker counts, then emits one
JSON line and sends edge-triggered email on CRIT.

It deliberately duplicates part of what `uc-agent` does. That is the point:

- it runs on hosts that cannot take the agent (no passwordless sudo)
- it keeps working if the control plane, the gateway, or the agent is down
- it needed no deploy pipeline, so it was protecting the fleet the same day

Config lives in `/etc/fleet-health.conf` (mode **600**, it holds an SMTP
password). Useful switches: `SMART_OPTIONAL=1` on hosts with virtualised disks
that expose no SMART, and `APT_AGE_OPTIONAL=1` on appliance nodes that live-boot
a read-only image where apt age can never improve.

Copy it as-is; do not fork per host. Divergent copies of a monitoring script are
how you end up trusting a check that no longer does what you think.

## `uuda-liveness-watchdog.sh`

Runs on commander every 15 minutes. Alerts when a registered UUDA device stops
heartbeating.

Every other check in this system reasons about a signal that **arrived**. The
drift scheduler deliberately scans only devices whose `last_heartbeat_at` is
inside 90 seconds, so a host that dies completely does not go red -- it drops
out of the fleet and every remaining check reads green. Absence of signal looks
exactly like success. The per-host `fleet-health-check.sh` cannot cover this
either, because it runs on the host that just died. Only something central can.

Edge-triggered: one mail when a device goes quiet, one when it returns. If the
database itself is unreachable it logs and exits rather than declaring the whole
fleet dead, because a false "everything is down" is worse than a gap.

## `pg-backup-unicorn.sh`

Nightly `pg_dump` of every database in commander's `unicorn-postgresql`
instance, with sha256 sums and 30-day retention.

It replaces a crontab entry that pointed at `ops-center/scripts/backup-database.sh`,
a script that no longer exists (it appears to have been renamed to
`automated-backup.sh`) and redirected to a `/var/log` path the user cannot
write. It therefore failed instantly and silently every night. When this was
found on 2026-08-14 the newest `unicorn_db` dump on disk was from 2025-10, and
**14 production databases had no working backup** -- including `unicorn_db`
(which now also holds all the UUDA tables), `majiks_online`, `msrl_portal_db`
and `loopnet_leads_db`.

It enumerates databases rather than hardcoding them, so a newly created database
is not silently left unprotected the way `unicorn_db` was.
