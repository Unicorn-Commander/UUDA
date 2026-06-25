"""Stub for the ops-center `server` module the real uuda routers import.

Provides `get_current_user` / `require_admin`. Both succeed (the harness does not
run Keycloak — see README), but the acting identity is taken from an optional
`X-Uuda-Actor` request header so the E2E driver can act as distinct operators.
That is what lets the harness exercise SEPARATION OF DUTIES for real: the actor
who requests a held mutation must not be the one who approves it (uuda_mcp
forbids self-approval), and an approver/remediator acts under its own id.

In production this identity comes from the real Keycloak-authenticated principal;
here a header stands in for it. Role is always admin (fleet ops are admin-gated).
"""
from fastapi import Request


def _actor(request: Request) -> dict:
    actor = request.headers.get("x-uuda-actor", "harness-admin")
    return {"user_id": actor, "role": "admin"}


async def get_current_user(request: Request) -> dict:
    return _actor(request)


async def require_admin(request: Request) -> dict:
    return _actor(request)
