-- Geofences and missions.
CREATE TABLE geofences (
    id         serial      PRIMARY KEY,
    name       text        NOT NULL,
    area       geometry(Polygon, 4326) NOT NULL,
    active     boolean     NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- At most one active geofence.
CREATE UNIQUE INDEX geofences_one_active_idx ON geofences (active) WHERE active;

CREATE TABLE missions (
    id            uuid        PRIMARY KEY,
    system_id     smallint    NOT NULL CHECK (system_id BETWEEN 0 AND 255),
    created_at    timestamptz NOT NULL DEFAULT now(),
    -- MultiPointZ keeps the waypoint order and allows single-waypoint missions.
    waypoints     geometry(MultiPointZ, 4326) NOT NULL,
    status        text        NOT NULL
                  CHECK (status IN ('validated', 'uploading', 'uploaded', 'failed')),
    status_detail text
);
