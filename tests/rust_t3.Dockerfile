FROM node:22-bookworm-slim AS node
FROM ubuntu:24.04@sha256:a853f94d226358a79c740cfc7bce0c289748f3fe3488d921d038ccd752c61b60
COPY --from=node /usr/local/bin/node /usr/local/bin/node
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-venv ca-certificates libstdc++6 \
    && rm -rf /var/lib/apt/lists/* \
    && python3 -m venv /opt/t3-venv \
    && /opt/t3-venv/bin/pip install --no-cache-dir websockets==15.0.1
