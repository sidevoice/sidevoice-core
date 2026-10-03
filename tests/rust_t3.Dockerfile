FROM node:22-bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-venv ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && python3 -m venv /opt/t3-venv \
    && /opt/t3-venv/bin/pip install --no-cache-dir websockets==15.0.1
