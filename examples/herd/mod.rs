//! Shared between `herd-sim` and `herd-edge`.
//!
//! Argument parsing and the world both ends agree on. Kept out of the library:
//! this is what a consumer writes, not what umwelt provides.

// Each example uses a subset: the edge takes its world from the handshake
// rather than building one.
#![allow(dead_code)]

use std::str::FromStr;

use umwelt::WorldConfig;

pub const DEFAULT_ADDR: &str = "127.0.0.1:7777";

/// Both ends default to this so a smoke test is one command each. A deployment
/// passes `--secret`, and §The region link says what a bearer secret is worth.
pub const DEFAULT_SECRET: &str = "herd-smoke-test-key";

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
