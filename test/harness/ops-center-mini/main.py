"""Minimal FastAPI app that mounts the real UUDA router. Applies the SQL
migrations in /app/sql at startup so the test stack is self-contained.
"""
from __future__ import annotations

import logging
import pathlib
from contextlib import asynccontextmanager

from fastapi import FastAPI

from database.connection import close_db_pool, get_db_pool
from routers.uuda import router as uuda_router

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


@app.get("/healthz")
async def healthz() -> dict:
    return {"status": "ok"}
