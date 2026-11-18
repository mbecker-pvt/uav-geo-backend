//! Entry point. Week 3 replaces this with an `axum` server.

fn main() {
    match telemetry::from_global_position_int(481_086_000, 112_939_000, 560_250) {
        Ok(p) => println!(
            "uav-geo-backend {}: sample position {:.4}, {:.4} @ {:.1} m",
            env!("CARGO_PKG_VERSION"),
            p.lat,
            p.lon,
            p.alt_m
        ),
        Err(e) => eprintln!("error: {e}"),
    }
}
