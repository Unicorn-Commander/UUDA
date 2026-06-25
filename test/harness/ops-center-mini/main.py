"""Minimal FastAPI app that mounts the real UUDA control-plane routers. Applies
the SQL migrations in /app/sql at startup so the test stack is self-contained.

Mounts the FULL governed surface so the harness exercises the whole agentic loop
against the real agent, not just Phase-1 register/heartbeat:

  * uuda            — register / heartbeat / devices / tokens          (Phase 1)
  * uuda_mcp        — gateway call / tools / fanout / approvals         (Phase 1.5 + 3)
  * uuda_drift_api  — desired-state checks / scan / remediate           (Phase 4)

uuda_mcp transitively imports uuda_gateway / uuda_policy / uuda_tenancy, and
uuda_drift_api imports uuda_drift — all are volume-mounted into /app/routers by
docker-compose.yml.
"""
from __future__ import annotations

import logging
import pathlib
from contextlib import asynccontextmanager

from fastapi import FastAPI

from database.connection import close_db_pool, get_db_pool
from routers.uuda import router as uuda_router
from routers.uuda_mcp import router as uuda_mcp_router
from routers.uuda_drift_api import router as uuda_drift_router

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s %(message)s")
logger = logging.getLogger("ops-center-mini")


@asynccontextmanager
async def lifespan(_app: FastAPI):
    pool = await get_db_pool()
    sql_dir = pathlib.Path("/app/sql")
    files = sorted(sql_dir.glob("*.sql"))
    for f in files:
        logger.info("applying %s", f.name)
        async with pool.acquire() as conn:
            await conn.execute(f.read_text())
    yield
    await close_db_pool()


app = FastAPI(title="ops-center-mini (UUDA test harness)", lifespan=lifespan)
app.include_router(uuda_router)
app.include_router(uuda_mcp_router)
app.include_router(uuda_drift_router)


@app.get("/healthz")
async def healthz() -> dict:
    return {"status": "ok"}
