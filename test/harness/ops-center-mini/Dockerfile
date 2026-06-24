# SPDX-License-Identifier: LicenseRef-Magic-Unicorn-Proprietary
# Minimal ops-center surrogate for UUDA harness. Hosts the real uuda.py router.

FROM python:3.12-slim

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1

RUN apt-get update && apt-get install -y --no-install-recommends curl && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY requirements.txt /app/requirements.txt
RUN pip install --no-cache-dir -r /app/requirements.txt

# Stub modules the real ops-center router expects to find on the import path.
COPY server_stub.py /app/server.py
COPY connection.py /app/database/connection.py
RUN touch /app/database/__init__.py
RUN mkdir -p /app/routers && touch /app/routers/__init__.py

COPY main.py /app/main.py

EXPOSE 8000
CMD ["uvicorn", "main:app", "--host", "0.0.0.0", "--port", "8000", "--log-level", "info"]
