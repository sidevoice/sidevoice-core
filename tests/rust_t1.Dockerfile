FROM ubuntu:24.04@sha256:a853f94d226358a79c740cfc7bce0c289748f3fe3488d921d038ccd752c61b60
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-venv ca-certificates util-linux libopus0 \
    && rm -rf /var/lib/apt/lists/* \
    && python3 -m venv /opt/t1-venv \
    && /opt/t1-venv/bin/pip install --no-cache-dir cryptography==46.0.2 loguru==0.7.3 websockets==15.0.1
ENV PYTHONPATH=/workspace/src
