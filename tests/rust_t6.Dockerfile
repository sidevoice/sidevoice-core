FROM ubuntu:24.04@sha256:a853f94d226358a79c740cfc7bce0c289748f3fe3488d921d038ccd752c61b60
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-venv ca-certificates libstdc++6 \
    && rm -rf /var/lib/apt/lists/* \
    && python3 -m venv /opt/t6-venv \
    && /opt/t6-venv/bin/pip install --no-cache-dir python-socketio==5.17.0 uvicorn==0.53.0 aiohttp==3.13.5
