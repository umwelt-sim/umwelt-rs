<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)"
            srcset="https://raw.githubusercontent.com/umwelt-sim/umwelt-rs/main/assets/logo/umwelt-mark-dark.svg">
    <img src="https://raw.githubusercontent.com/umwelt-sim/umwelt-rs/main/assets/logo/umwelt-mark-light.svg"
         alt="umwelt" width="150">
  </picture>
</p>

<p align="center">
  <a href="https://crates.io/crates/umwelt"><img alt="crates.io"
     src="https://img.shields.io/crates/v/umwelt.svg?logo=rust"></a>
  <a href="https://docs.rs/umwelt"><img alt="docs.rs"
     src="https://img.shields.io/docsrs/umwelt?logo=docsdotrs&amp;label=docs.rs"></a>
  <a href="https://github.com/umwelt-sim/umwelt-rs/actions/workflows/ci.yml"><img alt="build status"
     src="https://img.shields.io/github/actions/workflow/status/umwelt-sim/umwelt-rs/ci.yml?branch=main&amp;logo=github&amp;label=build"></a>
</p>

# Umwelt

**umwelt(n)** - _The specific way in which organisms of a particular species perceive and experience the world, shaped by the capabilities of their sensory organs and perceptual systems_. 

The core idea is that there is no objective reality, only the reality as _derived_ by any one entity's sensory abilities. That's what this library is all about, managing this derived reality for huge crowds at extreme scale.

Some games can become unusable when more than a few hundred players cluster in the same place. EVE Online holds the world record of 6,500 concurrent players in the same region of space for a battle. Supporting 6,500 players is a minumum baseline goal for this library.

## How Much Scale is Extreme?

The following is a set of ballpark figures. These are not maximized performance benchmarks,
they are order of magnitude numbers derived from running tests on a 2020 M1 Macbook, so most
modern equipment should be able to reach much higher numbers.

| | Ballpark |
|---|---|
| What one player sees | a 5×5 box of cells, 640 m across, from a 256 m view radius |
| What reaches that player | the nearest 256 entities, one 1,200-byte packet a tick carrying ~98 updates — about 24 KB/s |
| What one player costs | 0.86 µs of a 20 Hz tick when sparse, 1.9 µs packed into one cell when dense, on 4 consumer-grade cores |
| Entities one region process holds | 50,000, at 7 µs per player watching them |
| Entities one edge connection relays | ~2,000 steady; drops start by 2,500 |

The size of a region and the cells within it is also configurable, so the `640 m across` number can change with region size.

## Architecture

<p align="center">
  <img src="https://raw.githubusercontent.com/umwelt-sim/umwelt-rs/main/assets/diagram/topology.svg" width="760"
       alt="Three region simulations at the center, a NATS ring around them, twelve edge servers outside that, and eighty-four game clients fanned out from the edges. A highlighted path traces one state packet from region R7 through the bus and an edge to a single client.">
</p>

| **Component** | **Scale** | **Transport** | **Description** |
|:--:|:--:|:--:|:--|
| **Region** | Some | NATS | Owns one slice of the world and is **authoritative** over it. Allocates every entity id, and those ids mean nothing in any other region. |
| **Edge** | Many | NATS, QUIC | Relays, and knows nothing about the world. Has **no home region**. Disposable and scalable (_cattle_) processes. |
| **Game** | Tons | QUIC | The only tier that sees more than one region at a time. Owns the "real" map of the universe, sends commands, receives update stream |

This is an opinionated architecture. A common pattern is to run large monolithic processes, each dedicated to a set of clients. The monolith owns both the simulation ticks and the socket communications. Roblox is written this way, spinning up countless services to deal with demand.

Another common pattern is to create a cluster with internal replication. This allows any node in the cluster to handle requests from any client.

In Umwelt, the edge nodes are named deliberately. Their role is to sit as close to a group of game/simulation clients as possible. 

This separation between pure logic simulation management and edge connectivity allows the regions to crash without disconnecting a game client. It also makes it easy to support seamless transition between regions, again without losing any connectivity or packets.

## How it Works

Making a game people want to play is your job. Handling the scale when it goes viral is Umwelt's.

As a game developer using this library, you have 3 core jobs:

* **Define your game's server-side tick loop**. This contains pure rules and logic. Systems like physics and combat belong here.
* **Build your game's Edge**. If you want, you can just deploy an unmodified edge server off the shelf. If you want custom code to run as the edge manages its population, you can implement the edge callbacks.
* **Build your game**. Make a game millions of people want to play. No big deal.

## Defining a Simulation Loop
Defining the simulation loop is as simple as declaring your world parameters like size and simulation parameters like the loop tick rate (typically **20Hz** or **50ms** per tick). 

```rust,no_run
use std::sync::Arc;
use std::time::Duration;

use umwelt::net::{EdgeSink, Edges, Inbound};
use umwelt::{
    ClientLimits, Fixed, Flow, Game, Handoff, Pacing, RegionId, RegionServer, Step, WorldConfig,
    WorldSimulation,
};

// Everything the valley simulates: crops, livestock, and the farmers walking
// between them.
struct MildewValley {
    /// What the edges have asked for, applied inside the tick.
    inbound: Arc<Inbound>,
}

// Implement the region sim's callback, the core responsibility of
// a region server.
impl Game for MildewValley {
    fn step(&mut self, world: &mut Step<'_>) {
        // Spawns, moves and despawns your edges sent since the last tick.
        self.inbound.apply(world);

        // .. your logic here: grow the crops, wander the goats, spread the rot.
        let (xs, _, _) = world.positions_mut();
        for x in xs {
            *x = x.saturating_add(Fixed::from_raw(4));
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    let nats = runtime.block_on(async_nats::connect("nats://127.0.0.1:4222"))?;

    let region = RegionId::from_raw(7);
    let config = WorldConfig::default();

    let edges = Arc::new(Edges::new());
    let inbound = Arc::new(Inbound::new(Arc::clone(&edges)));
    let sink = EdgeSink::new(region, nats.clone(), runtime.handle().clone(), edges);

    let _server = RegionServer::new(
        nats,
        runtime.handle().clone(),
        region,
        config,
        Arc::clone(&inbound),
        Duration::from_secs(10),
    )?;

    // MildewValley is your type, WorldSimulation belongs to umwelt.
    let game = MildewValley { inbound: Arc::clone(&inbound) };
    let mut sim = WorldSimulation::new(config, game).with_sink(Handoff::new(sink.clone()));

    // umwelt owns the loop, because it owns the tick rate.
    sim.run(Pacing::default(), |_report, sim| {
        // Between ticks is the only place a viewer can be added or dropped.
        inbound.settle(sim, &sink, ClientLimits::default());
        Flow::Continue
    });
    Ok(())
}
```

Now that you've got a binary that launches a `WorldSimulation`, you're ready to build an edge.

## Building an Edge Server
You can choose to add no code at all and run the bare edge process. By default, it will relay the right messages and manage game clients and simulation communications and even manage things like
transferring entities from one region to another.

If you want to inject your own custom code, just create a new binary with your own implementation
of the edge callback:

```rust,no_run
use std::net::SocketAddr;

use umwelt::{ClientId, EdgeGame, EdgeServer};

/// Relaying needs no code at all. This only says who came and went.
struct Gatehouse;

impl EdgeGame for Gatehouse {
    fn connected(&mut self, client: ClientId, from: SocketAddr) {
        println!("{client} walked in from {from}");
    }

    fn disconnected(&mut self, client: ClientId) {
        // Everything this client held is already despawned. Nothing to clean up.
        println!("{client} went home");
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    let nats = runtime.block_on(async_nats::connect("nats://127.0.0.1:4222"))?;

    // Bound by you, so certificates and the crypto provider stay yours.
    let quic: quinn::Endpoint = todo!(".. your endpoint here");

    let edge = EdgeServer::new(nats, runtime.handle().clone(), quic, |_handle| Gatehouse)?;
    println!("{} is open", edge.name());
    std::thread::park();
    Ok(())
}
```

With a simulation and an edge, the only thing left to do is make the game client. That should be super easy, right?

## Creating a Game
To create a game, just implement the methods in the `ClientGame` trait, connect to an edge
via QUIC, and create your `ClientHandle`.

```rust,no_run
use umwelt::{
    ClientGame, ClientHandle, EdgeClient, EntityHandle, EntityId, EntityKind, PacketReader, Pos3,
    RegionId,
};

/// The player's side, called when the edge has something to say.
struct Farm {
    sending: ClientHandle,
}

// ClientGame has the umwelt callbacks you need to implement.
impl ClientGame for Farm {
    fn spawned(&mut self, handle: EntityHandle, region: RegionId, entity: EntityId) {
        println!("{handle} is {entity} in {region}");
    }

    // Umwelt will call this whenever state changes, at a fixed interval of your choosing
    fn state(&mut self, _handle: EntityHandle, _region: RegionId, state: &PacketReader<'_>) {
        for gone in state.despawns() {
            // .. your logic here: the neighbor's goat wandered out of sight
            let _ = gone;
        }
        for (id, at) in state.updates() {
            // .. and here: draw whatever `id` is at `at`
            let _ = (id, at);
        }
    }

    // Lost connectivity with the edge. This is to give a game a chance to 
    // react, not diagnose a root cause for the connection failure
    fn disconnected(&mut self) {
        println!("the valley went quiet");
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Runtime::new()?;
    let conn: quinn::Connection = todo!(".. your connection here");

    let client = EdgeClient::new(conn, runtime.handle().clone(), |sending| Farm { sending })?;
    let sending = client.handle();

    // Usable at once: a move sent under this handle is held at the edge until
    // the region answers with an id.
    let farmer = sending.spawn(
        RegionId::from_raw(7),
        Pos3::from_meters(2048, 2048, 0),
        // This entity is either an avatar that can see things, or a prop that
        // just takes up space (that avatars can see).
        EntityKind::Observer,
    )?;
    sending.move_entity(farmer, Pos3::from_meters(2049, 2048, 0))?;
    Ok(())
}
```