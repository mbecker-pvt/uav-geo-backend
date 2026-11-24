# syntax=docker/dockerfile:1
# Multi-stage build of the `uav-geo-backend` binary.

FROM rust:1.100-slim-trixie AS build
WORKDIR /src
COPY . .
# Cache the registry and target dir between builds (BuildKit).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p server \
    && cp target/release/uav-geo-backend /usr/local/bin/uav-geo-backend

FROM debian:trixie-slim
RUN useradd --system --uid 10001 --no-create-home app
COPY --from=build /usr/local/bin/uav-geo-backend /usr/local/bin/uav-geo-backend
USER app
ENV HTTP_ADDR=0.0.0.0:8080 \
    MAVLINK_ADDR=udpin:0.0.0.0:14550 \
    RUST_LOG=info
EXPOSE 8080 14550/udp
# The binary handles SIGTERM (docker stop) with a graceful shutdown.
ENTRYPOINT ["uav-geo-backend"]
