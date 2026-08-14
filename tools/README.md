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
