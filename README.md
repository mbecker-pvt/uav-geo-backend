# uav-geo-backend

[![CI](https://github.com/mbecker-pvt/uav-geo-backend/actions/workflows/ci.yml/badge.svg)](https://github.com/mbecker-pvt/uav-geo-backend/actions/workflows/ci.yml)
![Rust](https://img.shields.io/badge/rust-2024_edition-orange)
![License](https://img.shields.io/badge/license-MIT-blue)

An async Rust backend that ingests MAVLink telemetry from a (simulated) UAV,
stores flight tracks in PostGIS and serves them as GeoJSON over a REST and
WebSocket API, with mission planning, geofence and terrain checks.

<!-- TODO: GIF of the live map here -->

## Features

- [x] Receive `HEARTBEAT` / `GLOBAL_POSITION_INT` from PX4 SITL over UDP
- [x] Persist positions in PostgreSQL/PostGIS (`geometry(PointZ, 4326)`)
- [x] REST API: vehicles, flight tracks as GeoJSON `LineString`
- [x] WebSocket live positions
- [x] Spatial queries (bounding box), WGS84 ↔ UTM, track simplification
- [x] Height above ground from DEM raster data
- [x] Mission upload with geofence and minimum-AGL validation
- [x] Buffering and replay on link loss, graceful shutdown

## Architecture

```text
                    ┌──────────────────────────── server ────────────────────────────┐
 PX4 SITL ─UDP────▶ │ MavLink::run ──mpsc──▶ ingest ──broadcast──▶ WS /live           │
 (MAVLink 14550)    │      ▲                   │  └──▶ LinkMonitor (heartbeat timeout) │
                    │      │ raw broadcast     ▼                                       │
                    │ upload_mission     write buffer ──batched INSERT──▶ PostGIS      │
                    │      ▲             (bounded, replay)                    ▲        │
                    │      └───── REST (axum) ── tracks, bbox, missions ──────┘        │
                    │                    │                                             │
                    │                    └── geo-core (UTM, RDP, geofence) + DEM       │
                    └─────────────────────────────────────────────────────────────────┘
```

| Crate | Purpose |
|---|---|
| [`geo-core`](crates/geo-core) | Pure domain logic, no I/O: `Position`, `BBox`, WGS84 ↔ UTM, track simplification, geofence, mission validation, `TerrainSource` trait |
| [`telemetry`](crates/telemetry) | Async MAVLink connection, decoding, mission upload protocol, link-loss monitor |
| [`terrain`](crates/terrain) | GeoTIFF DEM reader (pure Rust) with bilinear interpolation |
| [`storage`](crates/storage) | PostGIS persistence with `sqlx`, embedded migrations |
| [`server`](crates/server) | Ingest pipeline, write buffer, REST/WebSocket API, binary `uav-geo-backend` |

## Getting started

### Prerequisites

- Rust 1.85+ (edition 2024)
- Docker + Docker Compose

### Run against PX4 SITL

```bash
cp .env.example .env              # DATABASE_URL etc.
docker compose up -d postgis      # PostGIS on 127.0.0.1:5432
docker compose --profile sim up -d px4   # PX4 SITL, sends MAVLink to the host:14550
cargo run -p server               # API on http://localhost:8080
```

Or everything in containers (backend image built from the `Dockerfile`):

```bash
docker compose --profile full up --build
```

Let the drone fly. The backend is not a full ground station (it sends no GCS
heartbeat), so first allow arming without one, then take off. Use service `px4`
for the `sim` profile and `px4-app` for `full`:

```bash
docker compose exec px4 bash -c 'cd $FIRMWARE_DIR/build/bin &&
  ./px4-param set NAV_DLL_ACT 0 && ./px4-param set COM_RC_IN_MODE 4 &&
  ./px4-commander takeoff'
# after POST /missions and POST /missions/{id}/upload:
docker compose exec px4 bash -c 'cd $FIRMWARE_DIR/build/bin && ./px4-commander mode auto:mission'
```

Then watch it:

```bash
curl localhost:8080/vehicles
curl "localhost:8080/vehicles/1/track?simplify_m=2"    # paste into geojson.io
websocat ws://localhost:8080/live                      # live positions with agl_m
```

> **Windows:** use `127.0.0.1` rather than `localhost` in `DATABASE_URL`.
> `localhost` can resolve to `::1`, and connections through Docker Desktop's
> IPv6 port forwarding may hang.

### Terrain data (optional)

Height above ground and the minimum-AGL mission check need a DEM in
geographic WGS84/ETRS89 coordinates, e.g. a
[Copernicus GLO-30](https://spacedata.copernicus.eu/collections/copernicus-digital-elevation-model)
tile (`Copernicus_DSM_COG_10_N48_00_E011_00_DEM.tif` covers Munich). Put one or
more `.tif` files into `data/dem/` (git-ignored) and set `DEM_PATH=data/dem`.

### Configuration

| Variable | Default | Description |
|---|---|---|
| `DATABASE_URL` | *(required)* | PostgreSQL/PostGIS connection string |
| `MAVLINK_ADDR` | `udpin:0.0.0.0:14550` | MAVLink endpoint (`udpin`, `udpout`, `tcpout`, `serial`) |
| `HTTP_ADDR` | `127.0.0.1:8080` | HTTP listen address (the Docker image uses `0.0.0.0:8080`) |
| `DEM_PATH` | – | GeoTIFF file or directory of tiles |
| `GEOFENCE_PATH` | – | GeoJSON polygon used when no geofence is active in the database |
| `MIN_AGL_M` | `30` | Minimum height above ground for missions |
| `TERRAIN_SAMPLE_M` | `25` | Terrain sample spacing along mission legs |
| `BUFFER_CAPACITY` | `100000` | Positions kept in memory while the database is down |
| `LINK_TIMEOUT_S` | `3` | Heartbeat timeout before a vehicle counts as lost |
| `SHUTDOWN_FLUSH_S` | `5` | Deadline for the final buffer flush on shutdown |
| `RUST_LOG` | `info` | Log filter |

### Test

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace                       # unit + UDP loopback tests, no Docker needed

docker compose up -d postgis                 # database + end-to-end tests
DATABASE_URL=postgres://uav:uav@127.0.0.1:5432/uav cargo test --workspace --all-features
```

The `db-tests` feature enables tests against a real PostGIS. Each test creates
its own database. The end-to-end tests run the whole service against a fake
vehicle over UDP. One flies a track, checks REST and WebSocket, validates and
uploads a mission, and detects link loss. The other starts the service
*before* its database exists and checks that all buffered positions are
replayed in order.

## API

All coordinates are GeoJSON order `[lon, lat, alt]`, with altitudes in metres
above mean sea level. Errors are JSON:
`{"error": "<code>", "message": "...", "details": ...}`.

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Liveness, database status, write-buffer counters |
| `GET` | `/vehicles` | Vehicles with live status (link, armed, last position) |
| `GET` | `/vehicles/{id}/track?from&to&simplify_m&limit` | Track as GeoJSON `LineString` feature, optional per-point `agl_m` |
| `GET` | `/positions?bbox=minLon,minLat,maxLon,maxLat&system_id&from&to&limit` | Positions in a bounding box (`FeatureCollection`) |
| `GET` | `/convert/utm?lat&lon` | WGS84 → UTM |
| `GET` | `/convert/wgs84?zone&hemisphere&easting&northing` | UTM → WGS84 |
| `GET` | `/terrain?lat&lon` | DEM elevation |
| `POST` | `/geofences` | Set the active geofence (GeoJSON `Polygon` or `Feature`) |
| `GET` | `/geofences/active` | Active geofence |
| `POST` | `/missions` | Validate and store a mission: `201`, or `422` with all violations |
| `GET` | `/missions/{id}` | Mission with status (`validated`, `uploading`, `uploaded`, `failed`) |
| `POST` | `/missions/{id}/upload` | Upload to the vehicle via the MAVLink mission protocol (`202`) |
| `WS` | `/live?system_id&bbox` | Live `position` (with `agl_m`) and `link` messages |

Example: a mission that cuts across terrain and leaves the fence. The response
below is abbreviated, and its values are illustrative: they depend on the
loaded DEM and the active geofence.

```bash
curl -X POST localhost:8080/missions -H 'content-type: application/json' -d '{
  "system_id": 1,
  "waypoints": {"type": "LineString", "coordinates": [[11.29, 48.10, 620], [11.40, 48.20, 620]]}
}'
```

```json
{
  "error": "mission_invalid",
  "message": "mission violates 2 rule(s)",
  "details": {
    "violations": [
      {"kind": "outside_geofence", "waypoint": 1},
      {"kind": "below_min_agl", "segment": 0, "at": {"lat": 48.13, "lon": 11.32, "alt_m": 620.0}, "agl_m": 12.4, "min_agl_m": 30.0}
    ],
    "checks": {"geofence": "field", "terrain": true, "min_agl_m": 30.0}
  }
}
```

## Design decisions & trade-offs

- **Pure `geo-core` crate.** Geodesy, geofence and mission rules have no I/O,
  so they are unit-tested exhaustively and reused by the API and the tests. The
  trade-off is a few more crates and types to map between layers.
- **Own UTM implementation (Krüger series) instead of PROJ.** PROJ needs a
  native library, which makes Windows and CI builds harder. The n³ series is
  checked against PROJ reference values to < 1 mm. Norway/Svalbard zone
  exceptions are not implemented.
- **Pure-Rust GeoTIFF reader instead of GDAL.** Same reason: no native
  dependencies. It supports single-band, north-up, EPSG:4326/4258 rasters
  (Copernicus, SRTM). Projected DEMs must be reprojected first.
- **Terrain is checked along mission legs, not only at waypoints.** Legs are
  sampled every `TERRAIN_SAMPLE_M` metres in UTM, so a ridge between two
  valid waypoints is caught. Missing terrain data counts as a violation
  (fail safe).
- **Write buffer with drop-oldest policy.** Positions leave the in-memory
  queue only after a successful insert, so a database outage loses nothing up
  to `BUFFER_CAPACITY`. Beyond that the oldest positions are dropped and
  counted: recent data matters more for a live system. Retries back off
  exponentially. Rows carry their receive time, so the replayed track stays
  in order.
- **The service starts without a database.** The pool connects lazily and
  migrations are retried in the background. Telemetry is buffered meanwhile,
  and the API answers `503 database_unavailable`.
- **The live WebSocket skips messages for slow clients.** It uses a `tokio`
  broadcast channel: a lagging client misses intermediate positions instead
  of slowing down ingestion or being disconnected.
- **Runtime-checked SQL (no `sqlx::query!` macros).** Building and CI need no
  database. Queries are covered by the PostGIS integration tests instead.
- **Missions are stored as `MultiPointZ`.** This keeps the waypoint order and
  also allows single-waypoint missions, which a `LineString` cannot hold.
- **Mission upload runs in the background.** `POST /missions/{id}/upload`
  returns `202`, and the result is visible in the mission status. Only one
  upload per mission can run at a time (atomic status transition).
- **One MAVLink socket for everything.** The receive loop broadcasts raw
  messages, and the mission protocol subscribes to them. `udpin` replies to
  the last sender, so no second port is needed.

## Roadmap

| Week | Milestone | Status |
|---|---|---|
| 1 | Workspace, CI, README | ✅ |
| 2 | MAVLink telemetry ingestion with `tokio` | ✅ |
| 3 | PostGIS + `axum` API (MVP) | ✅ |
| 4 | Spatial queries, coordinate transforms, terrain | ✅ |
| 5 | Mission planning, robustness | ✅ |
| 6 | Integration tests, Docker Compose, demo | ✅ (demo GIF pending) |

## License

Licensed under the [MIT license](LICENSE-MIT).
