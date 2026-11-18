# uav-geo-backend

[![CI](https://github.com/<user>/uav-geo-backend/actions/workflows/ci.yml/badge.svg)](https://github.com/<user>/uav-geo-backend/actions/workflows/ci.yml)
![Rust](https://img.shields.io/badge/rust-2024_edition-orange)
![License](https://img.shields.io/badge/license-MIT-blue)

An async Rust backend that ingests MAVLink telemetry from a (simulated) UAV,
stores flight tracks in PostGIS and serves them as GeoJSON over a REST and
WebSocket API, with mission planning, geofence and terrain checks.

> **Status:** 🚧 Work in progress. See [Roadmap](#roadmap).

<!-- TODO: GIF of the live map here (week 6) -->

## Features

- [ ] Receive `HEARTBEAT` / `GLOBAL_POSITION_INT` from PX4 SITL over UDP
- [ ] Persist positions in PostgreSQL/PostGIS (`geometry(PointZ, 4326)`)
- [ ] REST API: vehicles, flight tracks as GeoJSON `LineString`
- [ ] WebSocket live positions
- [ ] Spatial queries (bounding box), WGS84 ↔ UTM, track simplification
- [ ] Height above ground from DEM raster data
- [ ] Mission upload with geofence and minimum-AGL validation
- [ ] Buffering and replay on link loss, graceful shutdown

## Architecture

```text
 PX4 SITL ──UDP/MAVLink──▶ telemetry ──channel──▶ server (axum) ──▶ REST / WebSocket
                                                     │
                                                     ▼
                                               PostGIS (sqlx)
```

<!-- TODO: replace with a proper diagram (docs/architecture.svg) -->

| Crate | Purpose |
|---|---|
| [`geo-core`](crates/geo-core) | Domain types: `Position`, vehicles, missions |
| [`telemetry`](crates/telemetry) | MAVLink ingestion and decoding |
| [`server`](crates/server) | HTTP/WebSocket API, binary `uav-geo-backend` |

## Getting started

### Prerequisites

- Rust (stable, edition 2024)
- Docker + Docker Compose *(from week 3)*

### Run

```bash
cargo run -p server
# later: docker compose up   # simulator + PostGIS + service
```

### Test

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## API

<!-- TODO: link to OpenAPI docs (utoipa) once available -->

| Method | Path | Description |
|---|---|---|
| `GET` | `/vehicles` | List all vehicles |
| `GET` | `/vehicles/{id}/track` | Flight track as GeoJSON |
| `POST` | `/missions` | Create a mission from GeoJSON waypoints |
| `WS` | `/live` | Live position stream |

## Design decisions & trade-offs

<!-- TODO: fill in as decisions are made. Suggested format per entry: -->
<!-- **Decision** – why – alternatives considered – trade-off -->

- …

## Roadmap

| Week | Milestone | Status |
|---|---|---|
| 1 | Workspace, CI, README | ✅ |
| 2 | MAVLink telemetry ingestion with `tokio` | ⬜ |
| 3 | PostGIS + `axum` API (MVP) | ⬜ |
| 4 | Spatial queries, coordinate transforms, terrain | ⬜ |
| 5 | Mission planning, robustness | ⬜ |
| 6 | Integration tests, Docker Compose, demo | ⬜ |

## License

Licensed under the [MIT license](LICENSE-MIT).
