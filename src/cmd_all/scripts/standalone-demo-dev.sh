#!/usr/bin/env bash

set -euo pipefail

RUST_BACKTRACE=1 \
cargo run -p risingwave_cmd_all \
          --profile "${RISINGWAVE_BUILD_PROFILE}" \
          ${RISINGWAVE_FEATURE_FLAGS} \
          -- standalone \
               --meta-opts=" \
                  --listen-addr 127.0.0.1:5690 \
                  --advertise-addr 127.0.0.1:5690 \
                  --dashboard-host 127.0.0.1:5691 \
                  --prometheus-host 127.0.0.1:1250 \
                  --connector-rpc-endpoint 127.0.0.1:50051 \
                  --backend mem \
                  --state-store hummock+memory \
                  --data-directory hummock_001 \
                  --config-path src/config/standalone-example.toml" \
               --compute-opts=" \
                  --config-path src/config/standalone-example.toml \
                  --listen-addr 127.0.0.1:5688 \
                  --prometheus-listener-addr 127.0.0.1:1222 \
                  --advertise-addr 127.0.0.1:5688 \
                  --metrics-level 1 \
                  --async-stack-trace verbose \
                  --connector-rpc-endpoint 127.0.0.1:50051 \
                  --parallelism 4 \
                  --total-memory-bytes 8589934592 \
                  --role both \
                  --meta-address http://127.0.0.1:5690" \
               --frontend-opts=" \
                 --config-path src/config/standalone-example.toml \
                 --listen-addr 127.0.0.1:4566 \
                 --advertise-addr 127.0.0.1:4566 \
                 --prometheus-listener-addr 127.0.0.1:2222 \
                 --health-check-listener-addr 127.0.0.1:6786 \
                 --metrics-level 1 \
                 --meta-addr http://127.0.0.1:5690"