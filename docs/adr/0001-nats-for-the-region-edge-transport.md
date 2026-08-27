# 0001 — NATS for the region-to-edge transport

Status: Accepted, 2026-08-27. Supersedes the TCP transport in `net::region`.
The subject scheme below is itself superseded by `docs/adr/0004`, which drops
the reply subject, adds a region-wide events subject, and addresses payloads by
entity.

## Context

A region and its edges talk over a TCP connection this crate implements: its own
framing in `wire.rs`, its own handshake in `protocol.rs`, and a bearer-secret
check in `auth.rs`.

An entity has to be able to move from one region to another without its game
client noticing. That means an edge must be able to receive payloads from a
region it may never have spoken to. Over TCP that costs a connect, a
two-round-trip handshake, and a wait of up to one tick for the region to
register the viewer between ticks.

Avoiding that gap led to a list of features that existed only to work around the
transport:

- opening links to neighboring regions before they are needed
- an idle timer to close links whose entities have all left
- a mapping from region id to IP and port, and something to distribute it
- an advertise address in the region config, since the bound address may differ

None of that is about simulation. It is all about the cost of establishing a
connection.

## Measurements

Benchmarked on one M1 laptop over loopback, 1,200-byte payloads, eight edges,
core NATS with `nats-server` 2.14.5. The comparison is against the TCP path
measured under the same conditions in §The smoke test.

| | payloads/s | MB/s |
|---|---|---|
| TCP, batched, at 24,576 observers | 512,627 | ~590 |
| NATS, one publisher, unpaced | 556,650 | 637 |
| NATS, six regions paced at 20 Hz | 983,040 | 1,125 |

Six regions is 49,152 observers at full crowd, held at 19.9 Hz with **zero
loss**. The wall is near 1.07M payloads/s: eight and twelve regions sent
identical totals, and the publishers fell to 16.2 and 10.7 Hz. That is twelve
publisher processes, eight subscriber processes and a server on eight cores, so
past six regions the benchmark is the constraint. `nats-server` reported zero
slow consumers at every level and dropped nothing at any point.

A three-node cluster carrying 491,520 payloads/s matched a single server exactly
— 100% delivered at 20.0 Hz — including the worst topology, every region on one
node and every edge on the others, so that no message stayed local. Cross-server
routing cost nothing measurable.

Reaching a region never spoken to:

| | |
|---|---|
| connect to NATS | 2.88 ms, once at edge startup |
| subscribe to a new region | p50 36 µs, p99 61 µs |

And with a wildcard subject there is no subscribe on migration at all. An edge
subscribing to `umwelt.*.edge.3.payload` at startup receives payloads from every
region, including ones that did not exist when it subscribed.

These are loopback numbers on one machine. They do not measure a network, and
whether a cluster raises the aggregate ceiling needs separate hardware. What
they establish is that NATS carries the load a single region produces, that
routing between servers is not a bottleneck at that load, and that the
connection-establishment gap disappears.

## Decision

Region-to-edge traffic moves to core NATS. Not JetStream: state payloads are
latest-only, lossy and unordered, and persistence would be paid for a guarantee
the design does not want.

Subjects:

| subject | direction | carries |
|---|---|---|
| `umwelt.{region}.info` | request/reply | the region's id, version and world parameters |
| `umwelt.{region}.edge.{edge}.payload` | region to edge | assembled per-viewer payloads |
| `umwelt.{region}.edge.{edge}.reply` | region to edge | answers to commands, such as entities spawned |
| `umwelt.{region}.edge.{edge}.command` | edge to region | spawn, move, despawn |

An edge subscribes once to `umwelt.*.edge.{edge}.payload` and
`umwelt.*.edge.{edge}.reply`, and never subscribes again. A region subscribes to
`umwelt.{region}.edge.*.command` and reads the sender's identity out of the
subject.

`{edge}` is a stable name the edge declares, not the dense per-region `EdgeId`,
which stays internal to a region's own bookkeeping.

**The library takes a connected client and never makes one.** `RegionServer` and
`RegionClient` are built from an `async_nats::Client` and a Tokio handle the
caller already holds. Flexible topology is a large part of why NATS was chosen —
one server, a cluster, leaf nodes, credentials, TLS, whatever reconnect policy —
and none of it is the library's to pick. What the library decides is the subject scheme
and the message encodings, because both ends have to agree on those or nothing
works. Durations follow the same rule: how long a silent edge survives, and how
long to wait for a region to answer, are parameters rather than constants.

Authorization moves to NATS accounts, JWT and `.creds` files. That replaces
`auth.rs`, whose bearer secret its own documentation describes as second lock
rather than first. `docs/adr/0002` scopes those permissions by role rather than
by instance, and says what that does and does not enforce.

The world parameters still have to reach the edge, since `RecordCodec` derives
from them, so `ServerInfo` survives as the reply on `umwelt.{region}.info`. The
`protocol_hash` check survives with it.

## Consequences

**The crate gains its first dependencies**: `async-nats` and, through it,
`tokio`. §Rust implementation notes said Tokio belongs in `SimulatorEdge`, where
the work is connection-shaped. This puts it in the region process as well. The
tick loop stays a pinned OS thread that never touches it; `Handoff` already
moves payloads onto an I/O thread, and that thread becomes the one holding the
NATS client.

**Deleted rather than built**: warm-on-approach connections, the idle-close
timer, region-to-address discovery, and the advertise address.

**Deleted from what exists**: `wire.rs`, since NATS delivers whole messages and
length-prefixed framing has nothing left to do. `auth.rs`. Most of the handshake
in `protocol.rs`, along with `RegionServer`'s accept loop, `Shutdown`, and the
per-edge `BufWriter`, flush and `send_parts` machinery in `edges.rs`. The
message bodies in `protocol.rs` are unaffected: `SpawnEntities`,
`MoveEntities`, `DespawnEntities`, `EntitiesSpawned` and `PositionUpdates` keep
their encodings.

**NATS becomes required infrastructure.** A region with no reachable NATS server
serves nobody. The TCP transport had no such dependency.

**An edge's death stops being observable.** A closed socket told a region an edge
had gone, and that is what triggers despawning the entities it managed. Publish
and subscribe carries no such signal. A region therefore expires an edge that has
been silent for a set period, and an edge that holds entities but has nothing to
say sends a keepalive on its command subject. An edge under load sends moves
every tick, so silence is already a strong signal; the timeout exists for the
edge that is idle rather than gone.

**Edges become disposable.** An edge holds no region-facing state worth
rebuilding. Starting one is a NATS connection and two wildcard subscriptions,
about three milliseconds, and no region has to be told. Scaling the edge tier out
or in is adding or removing processes. A region learns an edge exists the first
time it speaks and forgets it when it stops.

The limit is worth stating so nobody reads more into it. Disposability is about
the region-facing side. The game clients attached to an edge hold connections to
that process, so losing it disconnects them, and the entities it managed are
despawned once it is expired. A replacement edge, even one taking the same name,
inherits nothing: it spawns its clients' entities afresh. What the transport
removes is negotiated session state, not the client connections themselves.

**Tests need a broker.** Integration tests require a running `nats-server`, and
`cargo test` will fail without one. No transport seam is introduced to avoid
this: an in-memory transport that exists only for tests would be a second
implementation of the thing under test. Unit tests stay transport-free by
exercising message encoding directly, which is what most of them already do.

**The batching work survives in a different form.** The measured reason delivery
was capped at 170,000 payloads a second was three syscalls per payload. The NATS
client buffers and the region flushes once per drain pass, which is the same
shape: `PayloadSink::flush` keeps its meaning.

## Open questions

**Whether a cluster raises the aggregate ceiling.** Untested, and untestable on
one machine.

**Whether region-to-region traffic uses the same transport.** Likely, but that
belongs to the record on entity migration.

**What the edge speaks to game clients.** Not this. Game clients do not speak
NATS, and the client-facing protocol is unaffected by this record.
