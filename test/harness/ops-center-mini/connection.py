"""asyncpg pool helper, mirroring the signature ops-center's real
database/connection.py exposes (`async def get_db_pool() -> asyncpg.Pool`).
"""
from __future__ import annotations

import os
from typing import Optional

import asyncpg

_POOL: Optional[asyncpg.Pool] = None


async def get_db_pool() -> asyncpg.Pool:
    global _POOL
    if _POOL is None:
        _POOL = await asyncpg.create_pool(
            host=os.environ["POSTGRES_HOST"],
            port=int(os.environ.get("POSTGRES_PORT", "5432")),
            user=os.environ["POSTGRES_USER"],
            password=os.environ["POSTGRES_PASSWORD"],
            database=os.environ["POSTGRES_DB"],
            min_size=1,
            max_size=4,
        )
    return _POOL


async def close_db_pool() -> None:
    global _POOL
    if _POOL is not None:
        await _POOL.close()
        _POOL = None
