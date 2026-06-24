"""Stub for the ops-center `server` module the real uuda router imports.
Provides `get_current_user` and `require_admin` that always succeed for
test purposes; production behavior comes from real ops-center.
"""
from fastapi import Request


async def get_current_user(_request: Request) -> dict:
    return {"user_id": "harness-operator", "role": "admin"}


async def require_admin(_request: Request) -> dict:
    return {"user_id": "harness-admin", "role": "admin"}
