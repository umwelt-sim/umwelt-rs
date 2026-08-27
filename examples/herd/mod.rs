//! Shared between `herd-sim` and `herd-edge`.
//!
//! Argument parsing and the world both ends agree on. Kept out of the library:
//! this is what a consumer writes, not what umwelt provides.

// Each example uses a subset: the edge takes its world from the handshake
// rather than building one.
#![allow(dead_code)]

use std::str::FromStr;

use umwelt::WorldConfig;

/// Both ends reach each other through NATS rather than through each other, so
/// this is the only address either needs. See `docs/adr/0001`.
pub const DEFAULT_NATS: &str = "nats://127.0.0.1:4222";

/// `--name value`, anywhere in the arguments.
pub fn arg(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == format!("--{name}") {
            return args.next();
        }
    }
    None
}

pub fn arg_or<T: FromStr>(name: &str, fallback: T) -> T {
    match arg(name) {
        Some(raw) => raw.parse().unwrap_or_else(|_| {
            eprintln!("--{name}: cannot read {raw:?}");
            std::process::exit(2);
        }),
        None => fallback,
    }
}

/// The default world at a chosen tick rate. Wire precision is lossless here, so
/// a position an edge sends comes back exactly.
pub fn world(tick_hz: u32) -> WorldConfig {
    WorldConfig::builder()
        .region_size_m(4096)
        .vertical_extent_m(1024)
        .horizontal_view_radius_m(256)
        .max_horizontal_speed_m_per_sec(40)
        .tick_hz(tick_hz)
        .build()
        .unwrap_or_else(|e| {
            eprintln!("world config: {e}");
            std::process::exit(2);
        })
}
