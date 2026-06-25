# SPDX-License-Identifier: Apache-2.0
# uc-agent test container. Bare Ubuntu 24.04 + the cross-compiled binary +
# minimal helpers for entrypoint.sh.

FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl jq sudo systemd-coredump iproute2 \
    && rm -rf /var/lib/apt/lists/*

RUN install -d -m 0750 /etc/uc-agent /var/lib/uc-agent /var/lib/uc-agent/certs \
    && install -d -m 0755 /usr/local/libexec

# Cross-compiled Linux binary baked in at build time.
COPY uc-agent /usr/local/libexec/uc-agent
RUN chmod 0755 /usr/local/libexec/uc-agent

COPY config.toml.template /etc/uc-agent/config.toml.template
COPY entrypoint.sh /entrypoint.sh
RUN chmod 0755 /entrypoint.sh

ENV UC_AGENT_CONFIG=/etc/uc-agent/config.toml

EXPOSE 47000

ENTRYPOINT ["/entrypoint.sh"]
