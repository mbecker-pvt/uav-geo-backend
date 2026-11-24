# Implementation progress

Working notes for implementing the README feature list (roadmap weeks 2–6).
Full plan: milestones M0–M5 (housekeeping → ingestion → PostGIS/API →
spatial/terrain → missions/robustness → tests/compose/docs).

Committed on branch `feat/weeks-2-6` (see "Commits" below).

## Status

| Milestone | Scope | Status |
|---|---|---|
| M0 | `.gitignore` fix, `.gitattributes`, README badge, workspace deps | ✅ |
| M1 | MAVLink UDP ingestion (`telemetry`) | ✅ |
| M2 | PostGIS schema + storage, axum REST, WebSocket | ✅ |
| M3 | UTM, bbox, simplification, DEM/AGL | ✅ |
| M4 | Missions, geofence, upload, link loss, buffer, shutdown | ✅ |
| M5 | DB/e2e tests, Dockerfile, compose, CI, README | ✅ 54 tests green, strict clippy clean, `docker build` ✅, manual run against real PX4 SITL ✅ (2026-11-23, see below). CI not yet run (nothing pushed). |

## Crates

| Crate | State |
|---|---|
| `geo-core` | Done. `Position`, `BBox`, `utm` (Krüger n³, tested vs PROJ < 1 mm), `simplify_track` (RDP in UTM metres, returns indices), `Geofence` (geo `Covers`, boundary counts as inside), `Mission::validate` (fence per waypoint + per leg, terrain sampled along legs), `TerrainSource` trait, `FlatTerrain`. |
| `telemetry` | Done. `decode()` (pure), `MavLink` (async `mavlink::connect_async`, `run()` → `mpsc<TelemetryEvent>`, `subscribe()` raw broadcast, `send()` with GCS ids 255/190), `upload_mission` (COUNT → REQUEST_INT → ITEM_INT → ACK, timeout + resend), `LinkMonitor` (pure, clock passed in). Loopback UDP tests incl. lost item, timeout, rejection. |
| `terrain` | Done. `GeoTiffDem` (pure-Rust `tiff`, EPSG:4326/4258, PixelIsArea/Point, GDAL nodata, bilinear), `TerrainModel::load(file or dir)`. |
| `storage` | Done, 5 DB tests pass (`--features db-tests`). Migrations `0001_telemetry.sql`, `0002_missions.sql`; `Db::connect_lazy`, `migrate`, `upsert_vehicle`, `insert_positions` (tx, auto-creates `UNKNOWN` vehicle, chunked), `list_vehicles`, `track`, `positions_in_bbox`, geofence + mission CRUD, `begin_mission_upload` (atomic). |
| `server` | Done (lib + bin). `config` (env), `state` (`AppState`, `LiveMessage`), `ingest` (events → broadcast + buffer + link state; vehicle upsert in background, rate-limited), `buffer` (bounded FIFO, drop-oldest, backoff, final flush; `PositionSink` trait for tests), `geo_json`, `api/*` (routes below), `app` (`start()` → `Running{http_addr, shutdown}`; migrations retried in background so the service starts with DB down). |

### API

`GET /health`, `GET /vehicles`, `GET /vehicles/{id}/track?from&to&simplify_m&limit`,
`GET /positions?bbox&system_id&from&to&limit`, `GET /convert/utm?lat&lon`,
`GET /convert/wgs84?zone&hemisphere&easting&northing`, `GET /terrain?lat&lon`,
`POST /geofences`, `GET /geofences/active`, `POST /missions` (`{system_id, waypoints: GeoJSON}` → 201 / 422 with violations),
`GET /missions/{id}`, `POST /missions/{id}/upload` (202, background), `WS /live?system_id&bbox`.

## Decisions / gotchas (learned while implementing)

- **Versions** (Nov 2026): mavlink 0.21, sqlx 0.9, axum 0.8, tower-http 0.7, geo 0.33, geojson 1.0, tiff 0.11, Rust 1.100.
- **mavlink 0.21**: features are `dialect-common`, `transport-udp`, `tokio`; dialect at `mavlink::dialects::common`; `MavHeader.system_id` is `u32` → we keep `u8` and drop ids > 255. `udpin` replies to the last sender, so mission upload can reuse the receive socket. Without `mav2-message-extensions`, mission messages have no `mission_type` field (defaults to mission).
- **sqlx 0.9**: runtime-built SQL strings are rejected (`SqlSafeStr`). Use literals (`concat!` + macro for shared column lists) or `QueryBuilder`. No compile-time `query!` macros, so builds need no DB.
- Missions stored as `MultiPointZ` (keeps order, allows single waypoint). Only one active geofence (partial unique index).
- Clippy pedantic is on; add domain words to `clippy.toml` `doc-valid-idents`.
- Workspace prints "unused workspace dependency" warnings until every dep is used by a crate; these are cargo warnings, not clippy.
- **Windows + Docker Desktop: use `127.0.0.1`, not `localhost`, in `DATABASE_URL`.** `localhost` may resolve to `::1`, and connects through Docker's IPv6 forward can hang (seen as `PoolTimedOut`).
- Hung test processes on Windows can become unkillable and lock `target/.../e2e-*.exe` (linker error LNK1104). Workaround: rename the locked `.exe`/`.pdb` (renaming a running exe is allowed), then rebuild.
- e2e tests create/drop their own database (`e2e_<uuid>`) via the admin `DATABASE_URL`; the outage test starts the app *before* creating its DB to exercise buffer + migration retry.
- Vehicle upsert is rate-limited only after success (bug found by e2e: first heartbeat before migrations kept type `UNKNOWN`).
- API maps "DB unreachable" and "DB not set up yet" (SQLSTATE `3D000`, `42P01`) to 503 `database_unavailable`.

## Manual PX4 SITL run (2026-11-23, profile `full`)

Docker disk moved to `D:\Docker_wsl\...` (the C: blocker of 2026-11-20 is resolved). Verified with real PX4 1.17
in containers: vehicle shows up as PX4 `QUADROTOR`, takeoff, `POST /missions` → 201, upload → `uploaded` (PX4
accepted it, "Executing Mission"), bbox query finds positions at waypoint 3, simplified track (7288 → 5 points),
WS `/live` at ~50 Hz, `docker compose stop postgis` for 20 s → 1000 rows buffered, 0 dropped, replayed after
restart, `docker compose stop app` → clean shutdown with final flush, exit code 0.

Gotchas found:

- PX4 image entrypoint: **one** argument only sets the 14540 (API) target; 14550 still goes to the host.
  Compose `px4-app` therefore uses `command: ["app", "app"]`.
- Arming needs `NAV_DLL_ACT 0` + `COM_RC_IN_MODE 4` (backend sends no GCS heartbeat). Commands via
  `$FIRMWARE_DIR/build/bin/px4-param` / `px4-commander` (see README).
- **Windows: binding TCP on `0.0.0.0` hangs** (2026-11-24, reproducible after reboot). `TcpListener::bind`
  never returns, 0 % CPU, and the process becomes unkillable while still holding its sockets (and locking its
  `.exe`). Binding `127.0.0.1` works; UDP on `0.0.0.0` works. Most likely Windows Firewall / a security tool
  holding `listen()` until someone answers its "allow network access" prompt. Fix: `HTTP_ADDR` now defaults to
  `127.0.0.1:8080` (the Docker image sets `0.0.0.0:8080` itself). The old "unkillable test processes" were
  probably the same thing.
- Host mode (`sim` profile + `cargo run -p server`) verified on 2026-11-24 with the loopback default: PX4 in
  Docker reaches the Windows backend, vehicle `connected` with live positions. (Run on port 14541 through an
  extra `px4-mavlink start ... -o 14541` because zombies held 14550/14540; a reboot frees them.)

## Commits

Branch `feat/weeks-2-6` (pushed, not merged), 8 layered commits + fixes. Each commit passes
`cargo clippy --workspace --all-targets --all-features -- -D warnings` on its own (checked in a worktree).
CI only runs on pull requests and pushes to `main`, so it runs once a PR is opened.

## Next steps

1. Open the PR (`https://github.com/mbecker-pvt/uav-geo-backend/pull/new/feat/weeks-2-6`) and check CI
   (`gh` is not installed here). Merge when green.
2. Reboot to free the zombie processes (UDP 14550/14540).
3. Optional: demo GIF / small web map, OpenAPI (utoipa), GCS heartbeat from the backend (so PX4 arms without param changes).

## Infra

- `docker-compose.yml`: `postgis` (always), profile `sim` → `px4` (PX4 SITL 1.17.0, sends to `host.docker.internal:14550`), profile `full` → `app` (Dockerfile) + `px4-app` (sends to `app`). Home position via `PX4_HOME_LAT/LON/ALT`.
- PX4 image entrypoint: args `[HOST]` (sets only 14540!) or `[HOST_API HOST_QGC]`, hostnames resolved with `getent`; no args on Docker Desktop → `host.docker.internal` for both.
- `Dockerfile`: `rust:1.100-slim-trixie` builder (BuildKit cache mounts) → `debian:trixie-slim`, non-root user.
- CI: `test` job has a PostGIS service + `DATABASE_URL`; new `docker` job builds the image.

## Verify

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
docker compose up -d postgis && DATABASE_URL=postgres://uav:uav@127.0.0.1:5432/uav cargo test --workspace --all-features
```
