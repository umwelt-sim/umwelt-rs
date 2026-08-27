//! Watches the region tier.
//!
//! Subscribes to every region's heartbeat and prints one line each. The payload
//! is bytes, so `nats sub` alone shows nothing readable; this is the decoder.
//!
//! ```text
//! cargo run --release --example herd-watch
//! cargo run --release --example herd-watch -- --nats nats://host:4222 --creds ops.creds
//! ```
//!
//! It decides nothing. Rebalancing, draining and placing regions belong to a
//! control plane that is a separate program; see `docs/adr/0002`.

#[path = "herd/mod.rs"]
mod herd;

use std::time::Instant;

use futures::StreamExt;
use umwelt::net::{Heartbeat, control};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let url: String = herd::arg_or("nats", herd::DEFAULT_NATS.to_string());
    let client = herd::connect(&url, herd::arg("creds")).await.unwrap_or_else(|e| {
        eprintln!("nats {url}: {e}");
        std::process::exit(1);
    });

    let mut beats = client.subscribe(control::all_subjects()).await.unwrap_or_else(|e| {
        eprintln!("subscribing: {e}");
        std::process::exit(1);
    });
    println!("herd-watch: {}", control::all_subjects());

    let started = Instant::now();
    while let Some(message) = beats.next().await {
        match Heartbeat::decode(&message.payload) {
            Ok(beat) => println!("{:>6.1}s  {beat}", started.elapsed().as_secs_f64()),
            Err(e) => eprintln!("herd-watch: undecodable heartbeat: {e}"),
        }
    }
}
