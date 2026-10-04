FROM ubuntu:24.04@sha256:a853f94d226358a79c740cfc7bce0c289748f3fe3488d921d038ccd752c61b60
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-venv ca-certificates libstdc++6 libopus0 \
    && rm -rf /var/lib/apt/lists/* \
    && python3 -m venv /opt/route-venv \
    && /opt/route-venv/bin/pip install --no-cache-dir websockets==15.0.1 pydantic==2.13.5
ENV PYTHONPATH=/workspace/src
