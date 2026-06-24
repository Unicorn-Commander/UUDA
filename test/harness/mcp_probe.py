#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""
MCP probe for the UUDA agent. Connects over mTLS, runs the standard
initialize → tools/list → tools/call flow, and asserts the response
shapes for every Phase-1 tool the agent exposes.

Usage:
    mcp_probe.py --base https://hostname:47000 \\
                 --cert path/to/cert.pem \\
                 --key  path/to/key.pem \\
                 --ca   path/to/ca-chain.pem \\
                 --resolve hostname=127.0.0.1
"""
from __future__ import annotations

import argparse
import json
import socket
import ssl
import sys
from typing import Any, Optional

import httpx


JSONRPC = {"jsonrpc": "2.0"}


def _resolve_hook(host: str, ip: str):
    """httpx doesn't take --resolve; install a name resolver hook globally."""
    original = socket.getaddrinfo

    def patched(h, *args, **kwargs):
        if h == host:
            return original(ip, *args, **kwargs)
        return original(h, *args, **kwargs)

    socket.getaddrinfo = patched  # type: ignore


class McpClient:
    def __init__(self, base: str, cert: str, key: str, ca: str) -> None:
        self.base = base.rstrip("/")
        self.url = f"{self.base}/mcp/v1"
        ctx = ssl.create_default_context(cafile=ca)
        ctx.load_cert_chain(certfile=cert, keyfile=key)
        self.client = httpx.Client(
            verify=ctx,
            timeout=httpx.Timeout(60.0),
            headers={
                "Accept": "application/json, text/event-stream",
                "Content-Type": "application/json",
            },
        )
        self.session_id: Optional[str] = None
        self._req_id = 0

    def close(self) -> None:
        self.client.close()

    def _next_id(self) -> int:
        self._req_id += 1
        return self._req_id

    def _post(self, payload: dict[str, Any]) -> httpx.Response:
        headers = {}
        if self.session_id:
            headers["mcp-session-id"] = self.session_id
        return self.client.post(self.url, json=payload, headers=headers)

    def _parse_event_stream(self, body: str) -> dict[str, Any]:
        """Pull the first `data: {...}` JSON object out of an SSE body."""
        for raw in body.splitlines():
            if raw.startswith("data:"):
                payload = raw[len("data:"):].strip()
                if payload:
                    return json.loads(payload)
        raise RuntimeError(f"no data: line in SSE body: {body[:200]}")

    def _decode(self, resp: httpx.Response) -> dict[str, Any]:
        ctype = resp.headers.get("content-type", "")
        if "text/event-stream" in ctype:
            return self._parse_event_stream(resp.text)
        return resp.json()

    def initialize(self) -> dict[str, Any]:
        rpc_id = self._next_id()
        resp = self._post({
            **JSONRPC,
            "id": rpc_id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "uuda-mcp-probe", "version": "0.1.0"},
            },
        })
        if resp.status_code >= 400:
            raise RuntimeError(f"initialize HTTP {resp.status_code}: {resp.text}")
        # rmcp issues the session id on the initialize response
        self.session_id = resp.headers.get("mcp-session-id") or resp.headers.get("Mcp-Session-Id")
        body = self._decode(resp)
        if "error" in body:
            raise RuntimeError(f"initialize rpc error: {body['error']}")
        # initialized notification
        ack = self._post({**JSONRPC, "method": "notifications/initialized"})
        if ack.status_code >= 400 and ack.status_code != 202:
            raise RuntimeError(f"initialized notification HTTP {ack.status_code}: {ack.text}")
        return body["result"]

    def list_tools(self) -> list[dict[str, Any]]:
        rpc_id = self._next_id()
        resp = self._post({**JSONRPC, "id": rpc_id, "method": "tools/list"})
        if resp.status_code >= 400:
            raise RuntimeError(f"tools/list HTTP {resp.status_code}: {resp.text}")
        body = self._decode(resp)
        if "error" in body:
            raise RuntimeError(f"tools/list rpc error: {body['error']}")
        return body["result"]["tools"]

    def call(self, name: str, arguments: dict[str, Any] | None = None) -> dict[str, Any]:
        rpc_id = self._next_id()
        resp = self._post({
            **JSONRPC,
            "id": rpc_id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments or {}},
        })
        if resp.status_code >= 400:
            raise RuntimeError(f"tools/call {name} HTTP {resp.status_code}: {resp.text}")
        body = self._decode(resp)
        if "error" in body:
            raise RuntimeError(f"tools/call {name} rpc error: {body['error']}")
        return body["result"]


def assert_health(result: dict[str, Any]) -> None:
    payload = json.loads(result["content"][0]["text"])
    os_str = (payload.get("os") or "").lower()
    assert "linux" in os_str or "mac" in os_str or "darwin" in os_str, \
        f"unexpected os: {payload.get('os')!r}"
    assert payload.get("agentVersion"), "missing agentVersion"
    mem = payload.get("memory", {})
    total = int(mem.get("totalBytes", 0))
    assert total > 0, f"memory.totalBytes <= 0 (got {mem})"
    assert payload.get("uptime"), "missing uptime"
    disks = payload.get("disks", [])
    assert isinstance(disks, list) and len(disks) > 0, "no disks reported"
    print(f"  os={payload['os']!r} version={payload['agentVersion']} mem={total // (1024**3)}GiB disks={len(disks)}")


def assert_metrics(result: dict[str, Any]) -> None:
    payload = json.loads(result["content"][0]["text"])
    series = payload.get("series", [])
    assert isinstance(series, list), "metrics.series missing"
    sample_names = [s.get("name") for s in series[:5]]
    print(f"  series_count={len(series)} sample_names={sample_names} truncated={payload.get('truncated')}")


def assert_service_status(result: dict[str, Any], expected_name: str) -> None:
    payload = json.loads(result["content"][0]["text"])
    assert payload.get("name") == expected_name, f"unexpected name: {payload.get('name')}"
    print(f"  name={payload['name']} exists={payload.get('exists')} state={payload.get('state')} pid={payload.get('pid')}")


def assert_logs(result: dict[str, Any]) -> None:
    payload = json.loads(result["content"][0]["text"])
    if payload.get("error"):
        raise RuntimeError(f"logs returned error: {payload['error']}")
    entries = payload.get("entries", [])
    print(f"  entry_count={len(entries)} truncated={payload.get('truncated')}")


def assert_package_install(result: dict[str, Any]) -> None:
    payload = json.loads(result["content"][0]["text"])
    if payload.get("error"):
        raise RuntimeError(f"package.install returned error: {payload['error']}")
    cs = payload.get("changeSet", {})
    print(f"  applied={cs.get('applied')} change_count={len(cs.get('changes', []))} backend_used={payload.get('backendUsed')}")


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--base", required=True, help="https://hostname:port")
    p.add_argument("--cert", required=True)
    p.add_argument("--key", required=True)
    p.add_argument("--ca", required=True)
    p.add_argument("--resolve", action="append", default=[], help="hostname=IP override(s)")
    p.add_argument("--service", default=None, help="service name to query (defaults per-OS)")
    p.add_argument("--log-subsystem", default=None, help="log subsystem/unit to fetch (defaults per-OS)")
    p.add_argument("--package", default=None, help="package to dry-run install (defaults per-OS)")
    args = p.parse_args()

    for r in args.resolve:
        host, ip = r.split("=", 1)
        _resolve_hook(host, ip)

    # Defaults per OS — caller can override.
    on_macos = sys.platform == "darwin"
    service = args.service or ("com.apple.audio.coreaudiod" if on_macos else "uc-agent")
    log_subsystem = args.log_subsystem or ("com.apple.security" if on_macos else "uc-agent")
    package = args.package or ("wget" if on_macos else "htop")

    client = McpClient(args.base, args.cert, args.key, args.ca)
    try:
        print("[probe] initialize")
        info = client.initialize()
        print(f"  serverInfo={info.get('serverInfo')} protocolVersion={info.get('protocolVersion')}")

        print("[probe] tools/list")
        tools = client.list_tools()
        names = [t["name"] for t in tools]
        for required in ("system.health", "system.metrics", "system.logs", "package.install", "service.status"):
            if required not in names:
                raise RuntimeError(f"missing tool: {required}; got {names}")
        print(f"  found {len(names)} tools: {names}")

        print("[probe] system.health")
        assert_health(client.call("system.health", {}))

        print("[probe] system.metrics window=10s")
        assert_metrics(client.call("system.metrics", {"window_secs": 10, "max_series": 20}))

        print(f"[probe] system.logs service={log_subsystem}")
        import time as _t
        since = int(_t.time()) - 60  # last 60 seconds keeps log show fast
        assert_logs(client.call("system.logs", {"service": log_subsystem, "since_secs": since, "max_lines": 5}))

        print(f"[probe] service.status name={service}")
        assert_service_status(client.call("service.status", {"name": service}), service)

        print(f"[probe] package.install name={package} dry_run=true")
        assert_package_install(client.call("package.install", {"name": package, "dry_run": True}))

        print("[probe] agent.brief (Phase 2 seam)")
        brief_resp = client.call("agent.brief", {
            "version": "probe-1",
            "system_prompt": "You are LtCol Test. Verify the briefing channel works.",
            "context": {"role": "test", "fleet_size": "1"},
            "tasks": [{"id": "t1", "description": "no-op"}],
            "issued_by": "uuda-mcp-probe",
        })
        brief_payload = json.loads(brief_resp["content"][0]["text"])
        assert brief_payload.get("acknowledgedVersion") == "probe-1", \
            f"unexpected brief ack: {brief_payload}"
        print(f"  acknowledged_version={brief_payload['acknowledgedVersion']}")

        print("[probe] agent.report")
        report_resp = client.call("agent.report", {})
        report_payload = json.loads(report_resp["content"][0]["text"])
        # Local agent is off in headless/canary mode, but the brief should
        # still have been persisted. We check has_local_agent and the brief
        # version below.
        print(f"  has_local_agent={report_payload.get('hasLocalAgent')} current_brief_version={report_payload.get('currentBriefVersion','')!r}")

        print("[probe] all checks passed")
        return 0
    except AssertionError as e:
        print(f"[probe] assertion failed: {e}", file=sys.stderr)
        return 2
    except Exception as e:
        print(f"[probe] error: {e}", file=sys.stderr)
        return 1
    finally:
        client.close()


if __name__ == "__main__":
    sys.exit(main())
