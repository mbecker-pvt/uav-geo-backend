-- Vehicles and their position history.
CREATE EXTENSION IF NOT EXISTS postgis;

CREATE TABLE vehicles (
    system_id    smallint    PRIMARY KEY CHECK (system_id BETWEEN 0 AND 255),
    component_id smallint    NOT NULL,
    vehicle_type text        NOT NULL,
    autopilot    text        NOT NULL,
    first_seen   timestamptz NOT NULL,
    last_seen    timestamptz NOT NULL
);

CREATE TABLE positions (
    id             bigserial   PRIMARY KEY,
    system_id      smallint    NOT NULL REFERENCES vehicles (system_id),
    time           timestamptz NOT NULL,
    time_boot_ms   bigint      NOT NULL,
    geom           geometry(PointZ, 4326) NOT NULL,
    relative_alt_m real        NOT NULL,
    vn             real        NOT NULL,
    ve             real        NOT NULL,
    vd             real        NOT NULL,
    heading_deg    real
);

CREATE INDEX positions_vehicle_time_idx ON positions (system_id, time);
CREATE INDEX positions_geom_idx ON positions USING GIST (geom);
