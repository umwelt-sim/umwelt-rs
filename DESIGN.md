# umwelt design

Interest management for real-time simulation servers. Rust. Solo project.

Companion project: `herd`, a minimal game used only to generate load. Repo name
only, never published to crates.io.

This document supersedes `chat_pre_context.md`, which contained three errors
serious enough to mislead: it placed per-client work on the edge tier, it
proposed starting as a single process, and it described a TRIBES priority
mechanism that is not in the paper. All three are corrected here.

---

## Architecture decision records

Decisions that would otherwise be re-argued live in `docs/adr/`, one file each.
This document holds the design and its measurements; those hold the decisions
and what they cost. `docs/adr/0001` moves the region-to-edge transport to NATS
and supersedes the TCP transport described under §The region link.

## Working agreements

Rules for anyone, human or model, writing code or prose for this project.

- **Never state a number without verifying it or labeling it.** Mark every
  figure as measured, computed from stated assumptions, or a guess.
- **Neutral register in code comments.** State what a thing is and what
  constraints hold. No justification narrative, no rhetorical framing, no
  second person.
- **Explanation belongs in the blog post, not the doc comment.**
- **No florid filler.**
- **American spelling.**
- **Name things for their role, not their shape.**
- **If a better option exists, propose that one.** Do not present a worse option
  and mention the better one afterward.
- **Show code before writing it to the repo.**
- **A summary written by a model is not a source.** Verify against the code, the
  paper, or the person who made the decision.

---

## Architecture

Two data-plane tiers and a control plane.

**Simulation.** One process per region. Owns authoritative state. Fixed
timestep tick loop. Applies inputs, steps the world, publishes a snapshot into
its own memory. Then does all per-client work against that snapshot:
subscription, gather, priority scoring, budget selection, packet assembly. Emits
finished per-client payloads.

**Edge.** Dispatcher. Holds client connections and relays finished payloads to
sockets. Holds no world state and contains no proximity logic. Exists to
terminate connections and to spread packet I/O across machines.

**Control plane.** Maps edges to simulations. Region ownership, membership,
rebalancing, drain. Runs at human timescales. Should be built over etcd or
similar rather than implementing consensus. Elixir/BEAM is the preferred choice
for this tier.

Edges are always separate processes from simulations. There is no monolith
phase.

Simulation and edge communicate over a socket, which may be local. Shared memory
and memory mapping are excluded.

### Why per-client work stays in the simulation

Budget selection is a filter that discards most of its input. Computed at the
default config a viewer's subscription walks roughly 185 entities, of which
**95 survive the view-radius test** (measured), and 98 records fit in an
MTU-sized packet with no events pending, 77 under a full event backlog. So about
half of what the gather examines fails the radius test, and under a full backlog
about a fifth of what survives loses to the budget. Computed, half to three
fifths of what is examined never reaches a client, and the budget binds only
under a backlog or under crowding.

Running that filter before the network hop means the bytes leaving the
simulation are exactly the bytes clients receive. That is the floor; no byte
crosses the internal hop that is not needed downstream.

The alternative, shipping the snapshot to edges and gathering there, puts
entity data on the wire that the budget then discards, and puts the same full
snapshot on the wire once per edge regardless of whether any of that edge's
clients can see those entities.

The scaling makes it decisive. Snapshot-on-the-wire costs
`entities x record size x tick rate x edges`: it grows with the world and with
the fleet, and is independent of how many clients are connected.
Payload-on-the-wire costs `clients x packet size x tick rate` and is bounded by
consumption. Entity count is the term intended to run away here, and the first
arrangement multiplies it by fleet size.

### Consequences of keeping the work in the simulation

- The snapshot is a process-internal structure. It has no wire format, and none
  of the questions about byte order, alignment, or zero-copy decode apply to it.
  Those return only for the checkpoint and standby path, which is a different
  payload.
- Per-client work is read-only against a frozen snapshot, so it parallelizes
  across threads within the simulation process.
- Adding an edge adds socket and packet-I/O capacity, not gather capacity.

---

## Library boundary

`umwelt` owns the hard parts. The consumer writes two thin binaries and supplies
hooks into their game.

**The consumer builds two binaries.** A simulation server that creates and
configures a `WorldSimulation`, and an edge server that creates and configures a
`SimulatorEdge`. Both link this crate. Beyond that the consumer writes a
`WorldConfig`, their game logic as hooks, and two `main` functions.

**umwelt owns:** the tick loop and the clock, entity position storage, entity id
allocation and lifetime (via `LiveSet`), the cell-ordered snapshot, subscription, gather,
priority scoring, budget selection, payload assembly, the wire protocol between
`WorldSimulation` and `SimulatorEdge`, the `PayloadSink` implementation that
speaks it, and client connection handling on the edge.

**The consumer owns:** game state that is not position (health, velocity, AI,
inventory) keyed by `EntityId`, and the logic that mutates it.

Handing umwelt a foreign engine's world was never a design goal. A consumer
writes their game inside umwelt's storage model, not alongside it.

### Consequences of owning the loop

**Position storage.** The game step receives mutable slices and moves
entities in place. There is no per-tick marshaling pass copying positions from
the consumer's representation into umwelt's.

**Entity lifetime.** Spawn and despawn go through the library, so id
density, reuse, and generation are handled where the snapshot's assumptions live
rather than being a rule the consumer has to honor.

**Sim-to-edge protocol.** Both ends are umwelt types, so the
format never has to be stable for anyone outside the crate and the consumer
never sees a socket, a frame, or a serialization decision.

**Tick budget.** Owning the loop means the library
times each phase (game hook, snapshot update, gather, scoring, assembly) and
can act when a phase overruns its share of the tick.

What "act" can mean is constrained. Safe Rust cannot preempt a synchronous
callback mid-execution; there is no way to cancel a game hook that is already
running without abandoning a thread that is still mutating state. What is
achievable:

- Measure every phase and attribute overrun to the phase that caused it.
- Degrade rather than cancel: on overrun, shed replication work for the current
  tick by dropping some clients to a lower send rate or skipping non-essential
  phases, so the tick still completes on schedule.
- Cooperative deadlines: hand the game hook a deadline it can check inside its
  own loops.
- A watchdog thread that detects a hung hook and fails the process loudly rather
  than silently missing every subsequent tick.

Recording this now so nobody later promises preemptive cancellation the language
cannot deliver.

### Pacing the loop

A `WorldConfig` carries `tick_hz`, so the library knows the rate and the library
paces it. `run` is not built; this is what it has to do, written down before the
code exists because a consumer already wrote the loop by hand and measuring it
turned up something that changes the design. See §Idle costs speed, measured.

**An absolute schedule against a monotonic clock.** The next deadline is
`start + n * period`, never `now + period`. A relative sleep accumulates its own
overshoot and the clock drifts; an absolute one does not.

**Sleep short, then hold the core for the last stretch.** No timer is exact.
Every platform's sleep overshoots by roughly its granularity, which is why game
and audio loops sleep to within about a millisecond of the deadline and busy-wait
the rest. This is a fix for timer granularity and nothing else; it does not
address the state the core is in, which is a separate problem below.

**Clamped catch-up.** A tick that overruns must not be made up by running extra
ticks. That is the spiral of death: the loop falls behind, runs more steps to
catch up, takes longer, falls further behind. The shipped options are to drop
simulated ticks and say so, or to let simulated time dilate, which is what Eve
Online does under fleet-fight load so that a tick keeps its shape rather than
degrading unpredictably. Either is defensible; silently running extra ticks is
not.

**Report the schedule, not just the work.** How long a tick took and how late it
started are different questions, and only the second says whether the machine is
keeping up. Tick duration percentiles are also one of the two items §Still to
instrument still lists, and they cannot live anywhere but the loop.

**A hold-the-core mode, off by default.** Never yielding the core is what
low-latency messaging does and it costs a full core per loop. It belongs behind a
switch for someone who has measured that they need it, not in the default path.

**What stays out of the crate.** Thread pinning, scheduling class, CPU governor,
C-state limits and quality-of-service hints are per-deployment, are mostly kernel
flags on Linux, and would need FFI here since there is no `unsafe` and no
dependency in `src`. They belong in an operations note.

**The real answer to an idle server is not to have one.** A host running one
region at one percent duty is a misconfigured host. Operators pack zones per
machine until the cores are busy, which is also what makes the measured idle
penalty disappear.

### What umwelt stores

**Rule: umwelt stores only what umwelt's own code reads.**

Position is stored because the gather reads it. Liveness is stored because the
sort needs it. Accumulated displacement is stored because priority scoring reads
it, which `select` does; see §Odometer. Health, inventory, and AI state never
enter, because nothing in the replication pipeline reads them.

The rule is testable against any proposed field: name the umwelt code that reads
it, or it belongs to the consumer.

### ECS drift is a tracked risk

umwelt is not an entity component system and is not to become one.

It has struct-of-arrays storage and a dense entity id, which is the substrate an
ECS is built on, but none of what defines one: no components, no arbitrary
composition, no type-erased storage, no archetypes, no queries. That machinery is
where most of an ECS's complexity lives, and none of it is present.

Owning the tick loop made umwelt an entity registry, which is the step before a
component registry. The failure mode is drift rather than a decision: one field
here, one query there, until there is a half-ECS that is worse than either a
focused library or a real one.

**Standing requirement: any change that moves the implementation toward ECS
territory is flagged at review and justified in this document before it lands.**

What counts as movement:

- A second per-entity field stored by umwelt.
- Any means for a consumer to attach arbitrary data to an entity.
- Any query over entities by which data they carry.
- Type erasure or dynamic dispatch anywhere in entity storage.
- Storage that varies per entity rather than being uniform across all of them.

**Justified: the odometer.** `Odometer` stores accumulated displacement per
entity slot, plus the previous positions the accumulation is computed from. That
is the second per-entity field the list above flags, recorded here as the rule
requires.

- Priority scoring is the only thing that reads it, which is the test in §What
  umwelt stores. `select` is that caller, so the field passes on an existing
  reader rather than on intent.
- It is derived from positions umwelt already owns. Nothing the consumer writes
  reaches it, and nothing can be attached beside it. The type is public, as
  `CellSnapshot` and `Subscription` are, so the piece stays benchmarkable; no
  consumer path hands one out.
- Only a monotone total is exposed and direction never leaves the type. A caller
  keeping its own history could difference two readings and recover speed
  magnitude; it could not recover a vector.
- Private per-entity-scale scratch is not new. `CellSnapshot` holds
  `scratch_ids`, `scratch_xs`, `scratch_ys`, and `scratch_zs`, sized to the
  largest dense cell, which in the worst case is the whole population.

Computed: 16 bytes per entity, 4 for the total and 12 for the previous position,
so 800 KB at 50,000 entities. Measured: one pass costs 2.30 ns per slot, 0.23%
of a 50 ms tick at that population.

The failure mode the rule guards against is arbitrary composition. This adds one
fixed scalar plus a copy of a field already stored, and no means to add another.

The accepted cost of owning storage: a consumer already running an ECS holds two
entity registries and maps between them. Bevy interop was never a design goal, so
this is a known price rather than a surprise. The way back, if it ever becomes
intolerable, is to take position slices again instead of owning the arrays, at
the price of a per-tick marshaling pass and the despawn problem moving to the
consumer.

### Events go through umwelt, not around it

State and events have different delivery semantics. Positions are latest-only,
lossy, and unordered, which is what the gather and the budget are built for.
Death, chat, loot, and damage are reliable, ordered, and low volume.

Both go to the same client over the same socket, and umwelt owns that socket. A
consumer forced to build a second reliable channel alongside it has gained
nothing from the tier split, so event delivery is umwelt's responsibility.

```rust
impl Step<'_> {
    /// Queue a reliable, ordered message. The payload is opaque.
    fn notify(&mut self, target: EventTarget, payload: &[u8]);
}

pub enum EventTarget {
    /// The client controlling this entity.
    Entity(EntityId),
    /// Every client that currently has a ghost of this entity.
    Observers(EntityId),
    /// Every client within a radius of a point.
    Near(Pos3, Fixed),
}
```

The payload is opaque bytes. umwelt reads the target and the length; the game
serializes the rest. That follows the storage rule above and keeps events from
growing into a component system.

**`Observers` is the target that justifies putting this in the library.** Which
clients currently hold a ghost of a given entity is a fact only the replication
state knows. A game emitting that itself would have to reconstruct the interest
set, which is the work umwelt already does and has been optimized to do
quickly.
`Entity` and `Near` a consumer could plausibly build; `Observers` they could not.

Delivery machinery is umwelt's and is not small: sequence numbers, a sliding
window per connection, retransmit on loss, and ordered delivery. The TRIBES event
manager is a worked design for exactly this and the paper describes it.

**Ordering constraint.** An event naming an entity is meaningless to a client
that has not been told the entity exists. Spawn notifications and events on the
same entity must be ordered relative to each other. TRIBES gave ghost creation
the same guaranteed delivery path as events.

~~**The event reserve is a fixed cut and should be a floor.**
`state_budget_bytes` is computed once as `payload - header - reserve`, so 256 of
1,200 bytes are unavailable to state even when no events are pending.~~ It is a
floor now. `PacketBudget::state_bytes_available` takes the bytes actually queued
and holds back `min(queued, reserve)`, so state takes the whole packet when
nothing is pending, and a backlog past the reserve waits its turn rather than
starving state. At a 12-byte record: 98 records idle, 90 with 100 bytes queued,
77 under a full backlog.

~~**Blocked on client registration.** `Entity(EntityId)` means "the client
controlling this entity" and nothing maps a connection to an avatar yet.~~
Registration is built: `register_viewer` names a viewer's avatar and `avatar_of`
reads it back.

**What the entity-named targets still need is the reverse direction.**
`avatar_of` maps viewer to entity; nothing maps entity to viewer, so `Entity`
has no lookup. `Observers` is the harder one. Ghost state is stored per viewer,
a `GhostTable` keyed by entity id, so "every client holding a ghost of entity N"
is answerable only by scanning every viewer's table or by maintaining a reverse
index that pays on every `sent` and `evict`, both of which are in the per-viewer
hot path. Undecided. It should be decided before the ghost record grows for
acknowledgment, so that struct is touched once rather than twice.

**Blocked on what a session carries.** The delivery machinery above is per
connection. The link is built and its handshake works (§The region link), but a
session carries nothing yet, so sequence numbers, a window and retransmit have
nothing to run over, and the ordering constraint is still an open wire-format
question rather than an answered one.

### Payloads leave through a `PayloadSink`

`WorldSimulation` does not own a socket. It writes finished per-client payloads
into a `PayloadSink` the consumer supplies at construction.

umwelt ships the implementation that speaks the sim-to-edge protocol, so wiring
it is one line and the consumer still writes no transport code. Supplying it
explicitly rather than having `WorldSimulation` construct one keeps the seam
open: a test drives the simulation with an in-memory sink and no network, and a
consumer with an unusual deployment can substitute their own.

The binding constraint is that a sink must not block the tick thread.
`WorldSimulation` calls it from inside the tick, so an implementation that waits
on a socket turns a slow edge into a missed tick. The shipped implementation
hands off to an I/O thread and drops rather than queues, since payloads are
latest-only and a stale one has no value.

### Shape

```rust
// consumer's sim server binary
let sink = /* umwelt's implementation, speaking the sim-to-edge protocol */;
let sim = WorldSimulation::new(world_cfg, view_cfg, MyGame::new(), sink);
sim.run();           // from the consumer's own run(), by composition

// consumer's game hook
impl Game for MyGame {
    fn step(&mut self, world: &mut Step<'_>) {
        let (xs, ys, zs) = world.positions_mut();
        // move entities, spawn, despawn
    }
}

// consumer's edge server binary
let edge = SimulatorEdge::new(...);
edge.run();
```

Both types expose a single-tick entry point alongside `run`, so a deterministic
test or a harness that controls time can drive them without a clock.

The primitives underneath (`CellSnapshot`, `Subscription`, `gather`) stay
public. Not for the typical consumer, but so the pieces can be benchmarked
individually.

---

## Numeric representation

### `Fixed`

`#[repr(transparent)] struct Fixed(i32)` with 10 fractional bits. One meter is
1024 units. One unit is 1/1024 m, approximately 0.98 mm. The unit has no name.

`Fixed` is a **scalar**, one axis. `Pos2` and `Pos3` are two and three of them.
State this explicitly wherever bit layouts are discussed.

Why integer:

- Checkpoint-replay requires bit-identical results. Float computation varies
  across implementations via FMA contraction, x87 80-bit intermediates, and libm
  differences.
- `sqrt` **is** required by IEEE 754 to be correctly rounded, so it is portable.
  Only the transcendentals (`sin`, `cos`, `exp`, `log`, `pow`) vary.
- Keeps every config derivation const-evaluable, since `sqrt` and `ceil` are not
  `const fn`.

Computed: `i32` at shift 10 gives roughly +/-2,097,152 m of range against a
4,096 m region. Over-provisioned, but the type is 4 bytes regardless.

**Contingent decision:** if crash recovery moves to hot-standby replicas instead
of checkpoint-replay, determinism is not required and `f32` becomes viable. The
entire fixed-point apparatus depends on this choice.

### Operator semantics

- `Mul<Fixed>` shifts right by 10 after widening to `i64`. `Mul<i32>` does not
  shift. These are different operations and the separate impls are the main
  reason the newtype exists.
- `Div<Fixed>` shifts left by 10 before dividing.
- **Known inconsistency:** `Mul` truncates toward negative infinity (arithmetic
  shift), `Div` toward zero. Deterministic but they disagree for negative
  values. Unresolved.

### `DistSq`

Wraps `u64`. Squaring doubles the fractional bits and overflows `i32`.
computed, the largest squared separation across the region is about 5.3e13.
Ordering is preserved by squaring, so priority comparison never needs a square
root. `sqrt_approx` exists for display only.

---

## Spatial layout

`x` and `y` horizontal, sharing an extent, cell size, and wire precision. `z`
vertical with its own of each.

Consequences of a 2D cell grid over a 3D world:

- Vertical speed is unconstrained. Falling never crosses a cell boundary.
- A subscription is a vertical cylinder, not a sphere.
- Height affects relevance but not membership. Hence `Pos3::dist_sq` (3D,
  scoring) and `Pos3::horizontal_dist_sq` (2D, subscription and the gather's
  range test). **They are not interchangeable.**

Power-of-two cell size is enforced by `build()`, not merely preferred. The
compiler converts division-by-constant into multiply-shift automatically, but
cell size is runtime config, so it cannot. Verified anchor: an 8-bit `DIV` on
Coffee Lake measures 25 cycles (uops.info). A shift is one cycle.

Cell size is half the view radius. Computed over-subscription versus the ideal
circle:

| cell size | grid | area | ratio |
|---|---|---|---|
| r | 3x3 | 9r^2 | 2.86x |
| r/2 | 5x5 | 6.25r^2 | 1.99x |
| r/3 | 7x7 | 5.44r^2 | 1.73x |

At r/2 the subscribed area is about twice the view circle, so roughly half the
entities a gather walks are out of range and are dropped by its distance test.

### Bit layout (default config, one axis)

Region 4096 m = 4,194,304 units = 2^22. Cell 128 m = 131,072 units = 2^17.

```
 22 bits total, one axis:
   5 bits cell index | 7 bits whole meters in cell | 10 bits fraction
```

`cell_shift` is 17 = 7 + 10. The 7 is not arbitrary: 128 m is 2^7.

**Naming trap:** "22" means two unrelated things: 22 integer bits in the `i32`
container, and 22 total bits of a position within a 4096 m region. Avoid
Q-notation entirely; say "an `i32` with 10 fractional bits."

---

## Configuration

Runtime, not constants. Forced by the library goal, and required for parameter
sweeps.

**`WorldConfig`** is protocol-critical. Every simulation and client on a region
must agree or positions decode into plausible garbage. Guarded by
`protocol_hash` (FNV-1a, chosen for being a `const fn`, stable across platforms
unlike `DefaultHasher`, and dependency-free). Deliberately excludes speed and
tick rate, which affect simulation but not decoding.

**Everything else is derived.** The builder takes five values, all of which a
game developer knows:

```rust
WorldConfig::builder()
    .region_size_m(4096)
    .vertical_extent_m(1024)
    .horizontal_view_radius_m(256)
    .max_horizontal_speed_m_per_sec(40)
    .tick_hz(20)
    .build()?
```

Cell size is `1 << (view_radius_raw / 2).ilog2()`, the largest power of two at
or below half the view radius, which the cell-size sweep measured as the fastest
range. Both extents are already required to be powers of two, so
`region % cell == 0` holds for free, and flooring loses at most a factor of two
from `radius / 2`, which bounds `cell_radius` at 4. Neither needs checking.

Wire precision is lossless, so `horizontal_bits` is `log2(region_raw)`,
`vertical_bits` is `log2(vertical_raw)`, and both quantization shifts are zero.
A single global precision has to serve the nearest entity, and at arm's length
sub-pixel error is sub-millimeter, so there is nothing to trade away. The cost
is bounded: a record is 12 bytes at the default region and 16 at the largest one
`Fixed` can express.

`WorldConfig::with_cell_size_m` overrides the derived cell size and recomputes
the grid. It exists so the cell-size sweep can be re-run and panics rather than
returning an error, since it is a benchmarking tool and not a consumer path.

**There is no `ViewConfig`.** An earlier revision had one holding payload size,
header size, event reserve, send rate, dwell, and hysteresis. Nothing used it,
and only send rate had a plausible consumer story. Payload size is MTU
discovery, header size is our protocol, and dwell and hysteresis are subscription
tuning a consumer has no basis to pick. Per-connection policy belongs to
`SimulatorEdge`, constructed from what a client declares at connect plus what the
connection is measurably doing, which is the model TRIBES used.

`ConfigError` went from 15 variants to 6 as a result. Nine became impossible
rather than merely unchecked.

### Records per packet

The record is **12 bytes**, measured rather than assumed: 4 for an `EntityId`
and 8 for a position packed at 22, 22, and 20 bits. `RecordCodec::record_bytes`
computes it from config.

The packet figures below were computed against constants that were on
`ViewConfig`. They live on `PacketBudget` now, as `DEFAULT_PAYLOAD_BYTES`,
`DEFAULT_HEADER_BYTES` and `DEFAULT_EVENT_RESERVE_BYTES`, and a connection's
actual payload size arrives with `ClientLimits`. What follows is the arithmetic;
`PacketBudget` is the API.

The event reserve is a floor for events, not a subtraction from every packet, so
there are two figures:

```
max_state_bytes = payload_bytes - header_bytes
                = 1200 - 16 = 1184
idle records    = 1184 / 12 = 98

min_state_bytes = payload_bytes - (header_bytes + event_reserve_bytes)
                = 1200 - (16 + 256) = 928
records under a full backlog = 928 / 12 = 77
```

`state_bytes_available(pending_event_bytes)` gives the figure between them.

**A uniform viewer gathers 95 candidates and 98 fit in a packet, so it is not
oversubscribed in the calm case.** Earlier revisions of this document, and three
published posts, said it was. That rested on an assumed 16-byte record. The
budget binds under crowding and not otherwise.

Of the remaining inputs, `payload_bytes` at 1200 is well founded: Ethernet MTU is
1500, an IPv6 header takes 40 and UDP 8, leaving 1452, and 1200 survives tunnels,
VPNs, and PPPoE without fragmenting, which is what QUIC mandates as its minimum
datagram size.

`header_bytes` at 16 is no longer a guess. `PacketHeader` is exactly that: a
tick identifier, a per-client sequence number, an ack and a 32-bit ack bitfield,
and the two record counts. The acknowledgment fields are carried but unpopulated
until there is a transport to acknowledge over.

`event_reserve_bytes` at 256 is still a guess. Without a reserve a dense crowd's
position updates fill every packet and a client can stand in a mob without
learning it died. The number itself is arbitrary.

Record size varies with region and precision, since bits per axis is
`log2(extent / precision)`. A 16 km region costs 13 bytes. Coarsening precision
to 1/16 m costs 10, saving 2 bytes at the price of quantized motion below
`precision * tick_hz`, which is 1.25 m/s at 20 Hz.

---|---|
| region | 4096 m |
| vertical extent | 1024 m |
| cell size | 128 m (derived from view radius) |
| horizontal view radius | 256 m |
| max horizontal speed | 40 m/s |
| tick rate | 20 Hz |
| `cell_shift` | 17 |
| cells per axis | 32 |
| cells per region | 1024 |
| `cell_radius` | 2 |
| subscription grid | 5x5, 25 cells |
| tick duration | 50 ms |
| max move per tick | 2 m |
| horizontal bits | 22 (derived) |
| horizontal precision | 1/1024 m, lossless (fixed) |
| vertical bits | 20 (derived) |
| wire steps per cell | 131,072 |
| record bytes | 12 |

Only region size, vertical extent, view radius, max speed, and tick rate are
authored. Everything else in this table derives from those five.

---

## What is built

`fixed`, `pos`, `config`, `subscription`, `entity`, `snapshot`, `gather`,
`odometer`, `ghost`, `select`, `budget`, `codec`, `packet`, `sim`, `net`, and
both halves of `net`: the region link and the edge server. 302 library tests
pass, and three integration tests run against a live broker: one drives a region
and three edges end to end, one moves an entity from one region into another, and
one takes a game client through an edge into a region and out again.

A tick runs end to end: the game moves entities, the odometer observes how far
they went, the snapshot is rebuilt in cell order, and every due viewer is
subscribed, gathered, scored and selected against it, then handed a payload
assembled from that selection and passed to its sink. Viewers are partitioned
across worker threads.

Not built: events, and anything a session carries. The clock is built; see
§Pacing the loop. Both links are built: §The region link and §The edge server.

### Subscription

`Subscription` is a bounding box of four `i32` edges, 16 bytes. The set of cells
within `cell_radius` of a viewer's cell is always a rectangle. Membership is
four comparisons, independent of cell count.

`Option<Subscription>` for viewer state; no `NONE` sentinel. Consequence: the
delta function must handle `None -> Some` as its initial case, which is the full
cell list rather than a strip.

**Speed cap invariant:** computed, 40 m/s at 20 Hz is 2 m per tick against 128 m
cells, so at most one boundary per axis per tick. `build()` rejects configs where
this fails. Verified by test across a snake path covering all 1024 cells: bounds
shift by at most 1 per axis.

**Strip delta: canceled.** Rebuilding a box measured 1.5 ns. A strip version
might reach 0.5 ns, saving ~10 microseconds per tick at 10k viewers, 0.02% of
budget, against two edge cases and an oracle test suite. The delta *function* is
still required, since entered and exited cells drive client spawn and despawn.
This is the first decision in the project reversed by measurement.

### Snapshot

`CellSnapshot` holds entities in cell order: one flat run of ids with positions
stored alongside, plus `starts`, an offset table of length `cells + 1` where cell
`c` occupies `starts[c]..starts[c + 1]`.

The layout exists because the simulation writes entity data indexed by entity id
and the gather reads by cell. An entity's id records when it spawned; its cell
records where it is. Resolving an id into a position while reading by cell is a
scattered access per entity per viewer.

Offsets replace a `Vec` per cell: one allocation instead of `cells_per_region`,
no indirection before reaching entity data, consecutive cells adjacent in memory,
and no growth inside a tick. An empty cell costs one repeated number.

`CellSorter` produces a snapshot by counting sort: tally, running total,
scatter. Three linear passes, no comparisons. Held separately from the snapshot
because it owns single-writer scratch while the snapshot is read concurrently by
many threads. That is the tick-thread-to-worker-threads handoff.

The scatter walks the source in ascending id order, so each cell's range is
ascending by id. This is a byproduct of the algorithm, not a goal, and costs
nothing.

Its value is unproven. Entity id is the only key stable across a tick boundary,
since position, distance, and cell all change, so it is the natural key for
reconciling per-(viewer, entity) state between ticks. Whether the ordering beats
a small per-viewer hash map is untested: at the uniform case's ~95 candidates
such a map fits in L1 and probes cheaply. The ordering would only matter at large
candidate counts, which is the crowded case, which is the case a walk cap is
meant to eliminate. It also conflicts with distance ordering within a cell, so if
a cap is built the two cannot both come from one walk.

Rebuild rather than incremental maintenance: a full sort is
`O(entities + cells)` regardless of how many entities moved, incremental is
`O(movers)` but leaves cells unordered and needs a per-entity back-reference.
Under continuous movement most entities are movers. Not measured.

### Gather

```rust
pub fn gather(
    cfg: &WorldConfig,
    snapshot: &CellSnapshot,
    viewer: Pos3,
    sub: Subscription,
    out: &mut DiscoveredEntities,
)
```

Walks the subscription's cells, tests horizontal distance against the view
radius, and appends survivors with their full 3D distance. Horizontal decides
membership because that is what matches the cell grid; 3D is recorded because
height affects relevance and the grid cannot express it.

Appends rather than clearing, so the reuse pattern stays visible at the call
site and a viewer near a region boundary can gather from more than one snapshot.
The caller is `WorldSimulation`, not the consumer.

`DiscoveredEntities` is held per worker thread and reused across viewers, so it
allocates once at startup rather than per viewer. `clear()` retains the
allocation.

Known inefficiency: the horizontal terms are computed twice, once in the range
test and once inside the 3D distance. Fusing them is a candidate if a benchmark
shows it matters.

`DiscoveredEntity` carries the entity's index into the snapshot's entity arrays
alongside its id, so reading a selected entity's position needs no id lookup.
`CellSnapshot::pos_at` resolves it. The field occupies padding that `DistSq`'s
alignment already required, so the record is still 16 bytes, which a test
asserts. Whether any gather figure moved is unverified: no before-and-after was
run on one machine.

### Odometer

`Odometer` holds one `u32` per entity slot: the running sum of how far that
entity moved between consecutive calls to `accumulate`, in raw `Fixed` units.
The difference between an entity's reading now and its reading when a client was
last told about it bounds how far that client's copy has fallen behind.

Consequences of scoring on displacement rather than elapsed time:

- An entity that has not moved generates no score and is never resent. That is
  the property §What the paper actually contains credits to the TRIBES state
  mask, reached without a mask or a component system.
- Starvation is structural rather than tuned, given one constraint on the weight
  table: every band inside the view radius must carry a non-zero weight. Under
  that, a moving entity's score grows without bound so it wins a slot
  eventually, and a still one has no reason to.
- Boundary churn separates by cause, within the grace period. An entity that
  left a viewer's candidate set because the viewer moved, and returns having
  walked very little, scores near zero; one that returns because it walked
  scores high and should be sent. Elapsed time cannot tell those apart. Past the
  grace period the ghost is gone and a returning entity is sent regardless.
  ~~Unverified: no measurement of churn exists.~~ Measured: 0.49 first sightings
  per packet for a walking viewer and 2.92 for one crossing a crowd at 30 m/s,
  against 98 records. Grace trades that count against ghosts held past their
  usefulness, and one tick is the best of that trade; see §Quality harness.

Displacement is `|dx| + |dy| + |dz|`. Computed: that over-estimates the
Euclidean step by up to 1.73x, direction-dependent, and needs no square root.
Under movement that changes direction it is noise rather than bias against any
one entity; a permanent diagonal is the case that would show it. Unmeasured.

The total wraps and the per-call step saturates. Opposite on purpose: a
saturating total would pin at `u32::MAX` and freeze that entity's score forever,
which is the starvation this prevents, while a wrapping step would turn
an absurd single-tick jump into a small number. Computed: the total wraps after
4,194,304 m of path, about 29 hours at 40 m/s. Differences below 2^32 units stay
exact across a wrap.

Dead slots are skipped rather than updated, so a reading freezes on despawn. The
cost is that reusing a slot for a different entity would charge the newcomer for
its separation from the previous occupant. `LiveSet` already says a freed slot is
not safe to reuse without compaction or a quarantine; this gives that condition
teeth.

Nothing distinguishes a step from a teleport. Both are measured, since both
leave a client's copy equally wrong.

### Ghost table

`GhostTable` holds, per viewer, what that client has been told about each
entity: the odometer reading at the last send, and the tick the entity was last
in the viewer's ghost set. An entity with no entry is one the client does not
know exists, and only a send creates one.

Open addressing, linear probing, power-of-two slots, held at or below half full.
Computed: 12 bytes per slot, so 24 bytes per ghost. The hash is fixed rather
than seeded so a replay reconstructs identical probe orders; entity ids are
server-assigned, so there is nothing to defend against.

Deletion is Knuth's backward shift, leaving no tombstone. Eviction is a single
forward sweep: backward shift moves an entry either to a slot at or after the
removal point, which the cursor has yet to reach, or, once the probe scan has
wrapped the end of the table, between two slots the cursor already passed and
kept. Staleness is fixed for the duration of a call, so an entry the cursor kept
once it would keep again. Measured with temporary instrumentation: the eviction
test's 128 configurations wrap the probe scan 24 times, so the case that
argument turns on is exercised rather than assumed.

Nothing leaves the table unreported. `evict` appends every dropped id, so a
caller can always tell a client what it no longer holds.

**The mark advances on send, not on acknowledgment.** A lost packet therefore
leaves a client's copy of a since-idle entity permanently wrong: the entity has
stopped moving, so its drift stays zero and it is never resent. This is what the
paper's Most Recent State handles. It cannot be fixed before there is a protocol
with sequence numbers, and fixing it will need a pending mark beside the
acknowledged one, growing a record from 12 bytes to 16. Unresolved.

### Selection

`select` scores a viewer's ghost set, ranks it, and commits the result.

**Relevance and staleness do different jobs.** Distance decides what a client
knows about, because it changes slowly and so the ghost set holds still. Drift
decides what is worth sending, because it resets every time something is sent.
The ghost set is the nearest `ghost_cap` candidates, which the gather already
delivers in order, so choosing it costs nothing. Everything in it is stamped, so
only an entity that has left the set ages out.

Score is `drift x weight(band)` where `band` is `dist_sq.raw().ilog2()`. One
multiply, no divide, no square root, integer throughout, so replay stays
bit-identical. Ties break on candidate index, which the gather's walk order
fixes.

An entity the client has never seen scores as a ghost that has drifted by
`unseen_drift`, defaulting to the view radius, on the same scale as any other
score. A near stranger still beats a barely stale neighbor; a distant stranger
loses to a badly stale one.

An update that scored zero consumes no slot. That is the whole point of scoring
on displacement: an entity whose client copy is already correct has nothing to
say. Under it, a still world sends nothing at all.

Starvation is structural rather than tuned, given one constraint on the weight
table: every band inside the view radius must carry a non-zero weight. `Weights`
refuses a zero.

### Packet budget

`PacketBudget` turns a connection's declared payload size into the record count
selection is given. Per-connection, built from what a client declared plus this
protocol's overheads.

The event reserve is a floor rather than a subtraction:
`state = payload - header - min(pending, reserve)`. Measured against the default
config, 98 records fit an idle packet and 77 under a backlog at or past the
reserve.

### Payload assembly

A payload is a header, then despawns, then state records. `PacketWriter` builds
it into a buffer it keeps, so a worker allocates once; `PacketReader` reads it
back, which is what a client does and what makes the format testable by round
trip.

A despawn is four bytes, an `EntityId` alone: a client already holds the
position it is being told to forget. There is no separate spawn record. An
update for an entity a client does not hold is how it learns of one, and
anything a spawn would carry beyond position is the consumer's opaque payload,
which is not built.

Despawns are written first, capped at half the payload, so a viewer whose ghost
set turned over cannot spend a whole packet forgetting things. They also lag by
a tick: departures are found by the eviction at the end of `select`, after that
tick's records were already chosen, so they ride the next packet from a
per-viewer queue.

This is bytes in a buffer and nothing else. No socket, no framing to an edge, no
transport.

### Payloads leave through a sink

`PayloadSink` is a trait the consumer supplies at construction. `send` takes
`&self` and the trait is `Sync`, because it is called once per served viewer per
tick from every worker thread at once. An implementation must not block: the
tick is waiting on it.

Static dispatch with a defaulted type parameter, `WorldSimulation<G, S =
NullSink>`, which is the shape `HashMap<K, V, S = RandomState>` uses for the
same reason. There is one sink per simulation, chosen at startup and never
swapped, which is the case generics are for. It also keeps a sink inspectable:
`sim.sink()` returns the concrete type, so a test reads what was sent without an
`Arc` held alongside. A boxed trait object would type-erase that away.

Attaching one is consuming, so the type is inferred and no other constructor has
to name it:

```rust
let sim = WorldSimulation::new(cfg, game).with_sink(EdgeSink::connect(addr)?);
```

`NullSink` discards, costs nothing to hold, and is what a benchmark measuring
the simulation rather than the transport wants. `RecordingSink` keeps the latest
payload per viewer for tests and examples; it allocates and locks, so it is not
a production path.

**A sink is the one part of a tick the library does not control**, and
`DESIGN.md`'s own note on owning the loop applies: safe Rust cannot preempt a
synchronous callback, so a sink that blocks cannot be canceled. What is done
instead:

- **Attributed.** `TickStats::sink_nanos` reports time spent inside `send`,
  summed across workers. Measured, the timing costs about 1% of a tick, so it is
  always on rather than sampled. Divide by the worker count for what it cost in
  wall clock.
- **Loud on panic.** A panicking sink propagates out of `tick` rather than being
  swallowed. Viewers served before it keep their advanced state, which is safe
  because per-viewer state is independent, and a test pins the behavior.

Measured, why this matters: 8,192 viewers on an evenly spread region take
6.02 ms with `NullSink` and 10.22 ms with `RecordingSink`, which allocates and
takes one lock per send. **A sink that merely copies its payload under a shared
mutex is 70% of the tick**, at 513 ns per send, and before `sink_nanos` existed
nothing would have said so.

### Handoff

`Handoff` wraps any sink so that sending from a tick cannot wait on it. One slot
per viewer, an I/O thread draining them, and **the worst a wrapped sink can do
is cost a client one frame.**

One slot per viewer rather than one shared queue. A queue delivers stale frames
when the consumer falls behind, which is wrong for latest-only payloads, and a
bounded one drops whatever arrives after it fills, which is the tail of each
worker's chunk and so the same viewers every tick. Per-slot supersedes instead,
and degrades evenly.

**`PayloadSink::send` takes an owned `Vec<u8>` and returns one.** Ownership
rather than a borrow so nothing copies: a sink that reads the bytes returns the
buffer it was given, one that keeps them returns whatever it displaced, and the
buffers cycle so a tick allocates nothing. The lock a slot holds is then held
for a pointer swap rather than for a payload-sized copy.

Measured, 8,192 viewers, against the copying version it replaced:

| | copying | ownership |
|---|---|---|
| `NullSink` direct | 6.07 ms | 6.10 ms |
| `RecordingSink` direct | 10.15 ms | 7.55 ms |
| `Handoff(NullSink)` | 7.03 ms | 6.60 ms |
| `Handoff(RecordingSink)` | 7.35 ms | 6.75 ms |

The handoff costs 61 ns per send, down from 117. `RecordingSink` got a quarter
faster without being touched, because a sink that keeps payloads now swaps
rather than copies. A handoff around a contending sink is **faster than calling
that sink directly**, since the tick stops paying its lock.

Two mistakes worth recording, both found by tests rather than by reading:

- The drain held the `slots` read lock across the call into the wrapped sink, so
  a viewer served for the first time waited on that sink from the tick thread to
  take the write lock and grow. Neither lock may be held across the inner call.
- The fill state was an atomic outside the slot's mutex. A producer stashing
  between the drain's scan and its swap left it set after the payload was gone,
  so the next drain shipped an empty spare as a frame. State describing a buffer
  belongs under the same lock as the buffer.

**Not lock-free.** A fully lock-free version means triple buffering with
`UnsafeCell`, and there is no `unsafe` anywhere in `src`. Estimated, not
measured: it would remove the slot's `try_lock` and the `slots` read lock,
around two of the four atomics left, so roughly 0.2 ms of a 6.6 ms tick.
Three percent is not worth the crate's first `unsafe`.

Not built: a watchdog that fails the process loudly when a sink hangs rather
than silently missing every subsequent tick. That is a process-level policy and
belongs with `run`, which is also not built. Nothing currently defends against a
sink that blocks forever.

Not built: the sink that writes into a region link, which is the one that
would hand off to an I/O thread and drop rather than queue. `net` carries the
handshake; nothing yet carries payloads. See §The region link.

### Simulation and viewers

`WorldSimulation` owns positions, liveness, the odometer, the snapshot and
per-viewer state, and calls a consumer's `Game` once per tick. `Step` hands the
game mutable position slices, spawn and despawn.

Registration is logical. `register_viewer(avatar, ClientLimits)` names the
entity a client controls and what its connection declared; nothing in the
simulation sees a socket. Viewer ids are reusable, unlike entity ids, because a
recycled viewer is a different client with an empty ghost set and there is
nothing for a stale reference to alias.

`TickStats` counts viewers served, candidates gathered, records sent, first
sightings, departures, despawn records written and payload bytes assembled,
which covers all but two items of §Still to instrument.

Viewers are partitioned across `thread_count` workers with scoped threads, and
each worker holds its own gather, selection and packet buffers, so the only
thing they share for writing is the cache line at a chunk boundary.

Not built: events. `run` and its clock are built, and §Pacing the loop is what
they do.

### The region link

**Superseded in part by `docs/adr/0001` and `docs/adr/0004`.** The link was a
TCP connection this crate implemented: its own framing, its own handshake, and a
bearer secret. It is NATS now, 0001 holds the measurements and the reasoning,
and 0004 revises the subjects: no reply subject, a per-edge presence subject,
and payloads addressed by entity rather than by viewer. What follows is what
survived.

`net` holds two protocols, and keeping them apart is what the module is for.

**Region to edge**, `net::region`, is built. One region simulation and the
edges relaying for it: few peers, mutually trusted, deployed together.

**Edge to game client**, `net::edge`, is built, over QUIC. Many peers, none of
them trusted, deployed on someone else's machine and updated on their schedule.
Game clients do not speak NATS, so that link was unaffected by the transport
change and is a separate protocol sharing no types with this one. See
`docs/adr/0006` and §The edge server.

**Neither end connects to anything.** Both take a connected
`async_nats::Client` and a Tokio handle, so the broker address, credentials,
TLS and cluster membership stay with whoever is deploying. The library decides
the subjects and the encodings, because both ends have to agree on those or
nothing works, and nothing else.

**Two ends, and deliberately not one type.** `RegionServer` is a region's side.
`RegionClient` is an edge's: its own name plus a connection the caller made,
through which it talks to any number of regions. An edge server will hold a
`RegionClient` and run its own client-facing protocol on the other side of
itself; it is not built and it is not `RegionClient`. Collapsing the two would put per-client work back on the edge
tier, which is what §Why per-client work stays in the simulation is about.

**Messages are named for what they carry, not for who sends them.** A message
belongs to the protocol, not to whichever end speaks it. `ServerInfo` describes
a region, `SpawnEntities` asks for entities, `MoveEntities` carries positions.

**A region reports, it does not answer.** `Presence` says an entity was added
or removed, and it is published on the owning edge's `presence` subject
whatever caused the change: the edge asked, the consumer's game despawned, or
the edge was expired and its entities orphaned. An edge told only about what it
asked for cannot know when something it owns has gone, which is the gap
`docs/adr/0004` exists to close, and `WorldSimulation::despawned` is how a
despawn inside `Game::step` reaches the reporting at all.

An `Added` carries a `u64` token the edge chose on the `Spawn` and the region
echoes without looking inside it. The subject says which edge owns the entity
and nothing more, so an edge that asked for three avatars in one message has no
other way to tell which arrival belongs to which of its game clients. In
practice it is the handle the edge already holds for that client.

**The config crosses as the five authored values, not the struct.** A
`WorldConfig` is mostly derived, so `WorldParams` carries region size, vertical
extent, view radius and max speed in whole meters, plus tick rate and
`protocol_hash`. The edge rebuilds through the builder and rejects on a digest
mismatch. That is one derivation rather than two, and it makes the digest do
real work: a config that did not come through the builder is caught instead of
approximated. A region using `with_cell_size_m` fails it, and a test pins that.

Three versions are checked and all three move independently. `PROTOCOL_VERSION`
is the shape of the messages and must match exactly, since region and edge
deploy together and a skew is a deployment mistake rather than a condition to
tolerate. `ServerVersion` is the crate build, reported so an operator can see
what a region is running. `protocol_hash` is the world's wire layout.

**Authorization is the broker's.** NATS accounts, JWT and `.creds` files
replaced the bearer secret this crate used to check, whose own documentation
called it a second lock rather than a first.

### Edges, and what each one manages

`Edges` is the set of edges relaying for one region: dense reusable ids, the
name each edge calls itself, and the entities each one manages. An edge becomes
known the first time it sends a command. Ids are reusable on the same terms as
`ViewerId`: a recycled `EdgeId` names a different edge carrying nothing over, so
there is nothing for a stale reference to alias.

**An edge's death stopped being observable when the socket went.** A closed
connection used to say an edge had gone, which is what triggered despawning the
entities it managed. Publish and subscribe carries no such signal, so silence
does: an edge is dropped after the timeout `RegionServer` was built with, without a
word. An edge under load
sends moves every tick and an idle one sends a keepalive, so the timeout only
decides how long a quiet edge survives. Measured end to end: an edge killed with
no warning is dropped about five seconds later, and its 640 entities despawn
with it while the slots they occupied stay allocated.

**Why an edge owns entities.** A game client connects to an edge, and that edge
registers a viewer with the region for the client's avatar. The region then
builds a payload per viewer per tick and hands each to a `PayloadSink`, which has
to send it to the edge holding that client's connection and to no other. Which
edge that is, is a fact only this set knows. That is the same argument §Events go
through umwelt, not around it makes for `Observers`: the replication tier holds
the mapping, so the routing question belongs to it.

An edge claims the entities it manages, and `Edges::edge_for` answers the routing
question. A sink resolves a served `ViewerId` to its avatar through `avatar_of`,
then that avatar to an edge through `edge_for`. The avatar is also what it
writes into the message, so a `ViewerId` never leaves the region and an edge
keeps one map rather than two.

**The lookup sits on the sink's path, which is on the tick's path.** Every served
viewer costs one, from every worker thread at once. So `edge_for` takes a shared
lock and one relaxed atomic load and never waits on a claim; claiming and
releasing take the exclusive lock, and they happen when a game client arrives or
leaves rather than per tick. **Not measured.** §Payloads leave through a sink is
why the shape was chosen that way regardless: a sink that merely copied its
payload under a shared mutex was 70% of a tick at 513 ns per send, so a routing
lookup that serialized every worker would be the same mistake twice.

**An entity has at most one edge.** A second claim on one already held is a
`ClaimError` rather than a last-writer-wins, because two edges managing one
avatar means two clients being sent one viewer's packets, which is a bug to
surface rather than a race to resolve quietly.

Detaching an edge releases everything it managed. The entities are not despawned:
they stop being routable, which is the truth, since there is no longer a
connection to send their clients anything over.

Not benchmarked, and none of it is on the tick path today, since nothing yet
calls `edge_for`. What is established is that the link runs: six edges attach,
complete the handshake and stay attached while the region counts them, and 67
tests cover the framing, the messages, the handshake, the refusals, the deadline,
the shutdown, and the ownership and routing rules. No number here is a
performance claim.

### The session

Past the handshake, an edge asks for entities, moves them, gives them back, and
is sent each of its viewers' payloads. `net::region::session` is both halves.

**Inbound work happens in three places, because what it needs is available at
three different moments.** `Inbound::accept` runs on the NATS reader and only
queues, since the simulation is mid-tick as often as not. `Inbound::apply`
runs inside `Game::step`, the only place a `Step` exists and therefore the only
place an entity can be spawned, despawned or moved. `Inbound::settle` runs
between ticks, the only place `&mut WorldSimulation` exists and therefore the
only place a viewer can be registered or dropped.

The split is required rather than stylistic. Registering a viewer mid-tick would
change the set the workers are iterating over, and spawning between ticks would
write the position arrays after the snapshot had been built from them.

**An entity is not a viewer, and the spawn says which is being asked for.** An
entity is a thing with a position: it can be seen, and it costs 12 bytes of
snapshot and a visit during a gather walk. A viewer is an observer: an avatar
entity plus the replication state kept for it, and it costs a subscription, a
gather, a score, a selection and a packet every tick it is served, plus a ghost
table of its own. Every viewer has an avatar; most entities need no viewer.

`EntityKind` carries that on the wire. `Observer` has a game client behind it and
gets a viewer. `Unattended` has nothing behind it: simulated, replicated to
whoever can see it, and sent nothing. Projectiles, wildlife, NPCs, a vehicle with
no driver.

**Static scenery has no kind, because it is never spawned.** A rock that never
moves is already in the client's content package. Putting it in a region would
cost 12 bytes of snapshot and a gather-walk visit every tick for the life of the
region, to replicate a position the client already has. What belongs in a region
is state that is authoritative and changes. The protocol therefore has no way to
describe scenery, which is deliberate.

**The region allocates every entity id.** An edge asks for entities by position
and is told which ids it got. Two edges cannot choose colliding ids because
neither chooses any, so dispatch cannot go ambiguous through the id scheme. What
an edge does with those ids on its own side — mapping them to game client
sockets — is its business and no part of this protocol.

Ids are unique within a region, as §What umwelt stores has always said. An edge
relaying for several regions sees the same numbers from each and has to key by
`(RegionId, EntityId)`. Today a `RegionClient` is one link to one region, so
that falls out; it stops being automatic the moment an edge holds several.

**Every command naming an entity is checked against who manages it.** Entity ids
are region-wide and an edge can name any of them, so the ownership record is
the only check separating one edge from another. A move or despawn for an
entity the sender does
not manage is counted and dropped, not applied. That check is the authorization
boundary here, not the id scheme.

**An edge's population is not fixed.** Game clients connect and disconnect, so
`DespawnEntities` gives entities back: the region despawns them, drops the
viewer watching each, and frees the ownership record. An edge that goes away
gives up everything it held without sending anything, because a region that kept
simulating entities with nobody behind them would be replicating for nobody.
Expiry runs on its own timer and despawning needs a `Step`, so the orphans are
recorded and the next tick clears them.

**Tearing a viewer down requires entity to viewer, which the simulation does not
hold.** `Inbound` keeps that map itself. §Open items records the same gap.

**Payloads route by ownership.** `EdgeSink` holds viewer to avatar, filled when
a viewer registers, and asks `Edges::edge_for` which edge manages that avatar.
It is wrapped in `Handoff`, so the tick's side of a send is a memory copy and a
broker that stops accepting cannot stall the region.

**A known deviation, now half closed.** State is latest-only, lossy and
unordered, and this link is reliable and ordered. It reaches the client on a
datagram now — §The edge server — so what remains is one reliable hop in the
middle, between peers in the same datacenter. What that costs has not been
measured under loss, which is the only condition where it costs anything.

### Moving an entity between regions

`docs/adr/0003` decides how, and decides that nothing in the library changes to
allow it. The edge asks the destination for the entity, waits for the add
carrying the id the destination allocated, and only then gives the origin's copy
back. Spawn first, despawn second: ordered the other way there is a window where
the entity exists nowhere, and nothing needs that window.

**Reaching a second region costs no connect and no subscribe.** An edge's two
subscriptions are wildcards over the region, so a destination it has never dealt
with is already reachable. `tests/region_migration.rs` runs two regions in one
process with one edge talking to both through a single `RegionClient`.

**Measured**, at 100 Hz, with both regions, the edge and the broker on one M1,
over ten runs. Asking the destination to the add coming back: 1.4 to 9.4 ms. The
add to the first packet the destination addressed to that entity: 8.7 to 13.5 ms,
which is the tick it takes to register the viewer and serve it. End to end: 10.5
to 22.9 ms. Against a transport where a new destination meant a connect and a
handshake, that difference is the case `docs/adr/0001` made.

**The id is the destination's, and the origin's is refused there.** Ids are
unique within a region, so the destination allocates its own and the consumer
rekeys whatever state it holds against it. A move sent under the origin's id is
counted and dropped rather than applied. That is the ownership check catching an
edge's own stale ids, not an authorization decision.

**Bystanders are told what happened, and nothing special-cases them.** Whoever
could see the entity in the origin is sent a despawn for it, because that is what
happened there. Whoever can see it in the destination is sent it as an arrival,
because that is what happened here. Both fall out of the ghost table on each
side. It is also why the seamless case is a separate record: crossing a field
boundary this way would make every bystander drop the entity and re-add it, which
is a visible flicker for people who did not move.

**An edge stops moving an entity when it gives it back, not when the removal is
reported.** A move already in flight for an entity just despawned is refused, and
the round trip is a tick or more. Migrating 16 observers a second without this
cost 16 refused moves a second, one per migration, which is how it was found.

### The edge server

`net::edge` is the other half, and it shares no types with `net::region`. One
edge, and the game clients connected to it: many peers, none of them trusted,
running on someone else's machine and updated on someone else's schedule. See
`docs/adr/0006`.

**QUIC, not TCP plus a second UDP socket.** A reliable ordered stream carries
spawn, despawn and the consumer's own messages; datagrams carry the state stream
down and moves up. Both of those are latest-only, so a lost one is superseded
within a tick, and a lost spawn is not recoverable by anything. Two sockets would
mean two handshakes, NAT traversal on the second, and two congestion controllers
over one path.

**The caller supplies a bound endpoint.** Same rule as the NATS client:
certificates, the crypto provider and what the edge listens on are deployment
decisions, and the library installs no process-global default.

**The region ships final packets and the edge relays them without decoding.**
The edge routes on the four-byte avatar in front of a packet and replaces those
bytes with the region it came from. Authoritative state does not lose authority
by passing through a relay, and where the edge has something of its own to say it
has its own channel to say it on.

**An edge has no home region.** It reaches every region through one wildcard
subscription and has no way to know which regions exist, so a client's spawn
names the region it is for. Which region a player belongs in is the game's, kept
out of band — see `docs/adr/0003` — and the game is what told that client where
it is. Nor does an edge check a position against the region's bounds: the region
refuses one, and checking here would mean holding every region's world.

**The identifiers live outside `net`.** `RegionId`, `ClientId` and `EntityKey`
name a region, a connection and an entity an edge is holding; none of that is
networking, and a program that never opened a socket would name the same things.
They are in `id`, so the traits a consumer implements can be written without
reaching into a networking module. `EntityId` stays with `LiveSet` in `entity`
for the one reason the others do not share: it is also the index of a slot in
the simulation's position arrays. Every one of them carries `from_raw` and `raw`
and nothing else — the bytes are the protocol's business.

**Three identifier spaces, and only one of them crosses a tier.** A client names
entities by a `u32` handle it chose, so it can move one the instant it asks for
it. The edge maps that to an `EntityKey`, which is never reused and doubles as
the correlation token on `SpawnEntities` — so there is no fourth space. The
region allocates the `EntityId`. `ClientId` names one live connection and is
never reused either: unlike an `EdgeId`, a consumer holds one freely in its own
tables and timers, and a recycled one would send one player's packets to another.

**The edge names itself.** `docs/adr/0004` makes a fresh name per incarnation a
correctness requirement, and a correctness requirement is not the consumer's to
remember, so there is no way to supply one.

**Sending is a handle, not a callback argument.** `EdgeHandle` is cheap to clone
and callable from anywhere. `Step` earns its shape because spawning is valid only
inside `Game::step`; nothing an edge does is moment-scoped, and an edge that could
only speak from inside a callback would force a consumer to queue its own work
until some unrelated event fired.

**Both halves are built, and a game developer touches neither transport nor a
receive loop.** `EdgeServer` is the edge's side; `EdgeClient` is the game
client's. Sending is four commands on a `ClientHandle` — spawn, move, despawn,
and the game's own opaque bytes. Receiving is a `ClientGame`, called with what
the edge said.

**A game is told nothing about how a region is configured.** It never sees a
`WorldConfig`, a `RecordCodec`, or a packet: `ClientGame::state` is handed a
decoded reader. It has no say in a region's tick rate, view radius or speed cap,
and being given them would only invite it to act as though it did.

What it does see is the `RegionId`, because a game is the only tier that watches
more than one region at once — which is how seamless movement will work when it
is built — and because the entity ids inside a packet are that region's and mean
nothing in another one.

**The client link has its own info type.** `EdgeInfo` carries a region's two
extents and nothing else, because those are the whole of the wire layout:
horizontal bits come from the region size, vertical bits from the extent, and a
test in `codec` pins that view radius, speed cap and tick rate change nothing a
decoder does. It is deliberately not `ServerInfo` or `WorldParams`, which
describe a region to an *edge*. The two links share no types even where a field
would look the same.

Which command rides a datagram and which rides a reliable stream is decided by
the message rather than at the call site: a lost move is superseded within a
tick and a lost spawn is not recoverable by anything, so that is a property of
the command and not a choice to push onto a consumer. Nothing asks a consumer to
poll either. A polling API would have made every game define what a timeout
means and what to do about one, which is the library's question, not theirs — a
connection that goes reports itself as `disconnected`.

**Both handles hold weak references.** A server owns its game and the game holds
a handle; an owning handle would close that cycle and neither end would ever be
freed. Upgrading costs an atomic on paths that run per batch rather than per
record, and a call after the server or client is dropped fails cleanly, which is
the truth.

**`EdgeGame` is five callbacks, all defaulted to nothing.** A game whose clients
spawn, move and despawn implements none of them: the library runs that whole
loop. `ClientId` and `EntityKey` surface only once the consumer originates
actions of its own.

**Disconnect is ordered so the obvious mistake is free.** Everything the client
held is despawned, `removed` fires per entity as the regions confirm, and
`disconnected` fires last. A developer cleaning up out of habit finds
`entities_of` empty and `despawn` a no-op.

**A stale key is a race, not a mistake, and is not counted as one.** A removal
arrives unprompted — a region's game can despawn anything — so acting on an
entity that has just gone is dropped silently. `refused` counts commands a
client could not have meant: a malformed message, a spawn reusing a handle it is
already using, a despawn of something it never asked for. It is meant to be zero
in correct operation, because a signal that is never zero is not a signal.

**A client's first move for a handle goes on the reliable stream.** Every move
after it rides a datagram, which is what latest-only wants. The first cannot,
because a spawn travels on the ordered stream and a datagram can pass it: sent
as a datagram, a first move can reach the edge before the spawn that named the
handle and be dropped for naming nothing. One stream message per entity, once,
and the race is gone by construction.

**Measured**, 128 entities with 8 replaced a second, 20 seconds, about 48,000
commands. Before: 127 refused with the region's own game killing entities, 51
with all deaths switched off — so two thirds of them were not deaths at all, but
first moves overtaking their spawns. After both changes: **zero in both, and
zero again with two regions and 176 migrations.**

**Two threads own the region side.** `RegionClient` receives and publishes by
blocking on its runtime, and almost all of an edge runs *inside* that runtime, so
doing either from a task is not allowed. One thread reads what the regions send;
one drains a queue of spawns and despawns and flushes queued positions. That
second thread is also what makes moves cheap: positions are coalesced by entity,
latest wins, and flushed every 5 ms, so one client's move per tick does not become
one broker message per client per tick.

**An edge stops moving an entity the moment it gives it back**, rather than when
the removal is reported. Without that, migrating or churning cost one refused
move each, which is what the smoke test showed before it was fixed.

**Measured, on one M1 over loopback:** one game client with 256 observers at
20 Hz, churning 8 a second, relayed 5,100 packets a second carrying about 526,000
records, with zero undeliverable and zero commands refused by either tier. With a
second region and 64 entities of the edge's own walking between the two, 336
migrations completed in 18 seconds with nothing lost and nothing refused. These
are wiring figures from a laptop; §Whole-pipeline is what to quote for cost.

**A client's moves are batched.** One datagram per entity is one per entity per
tick: 163,840 a second at 8,192 entities and 20 Hz, each carrying sixteen bytes
of payload. Batching saves almost no bytes and about seventy times the packets.

How many fit is the connection's answer rather than a constant's. A sender asks
`max_datagram_size` and takes whichever is smaller, that or the protocol's cap;
sizing to a guess would mean every batch refused on a path with a smaller MTU
than the one it was guessed against. The cap stays fixed because a decoder has
to bound what it allocates for a claimed count.

**Neither end queues state it cannot send.** Both check the connection's
datagram send buffer and drop rather than enqueue, which is the call `Handoff`
already makes on the region side for the same data: a packet waiting behind
staler ones is worth less than the one after it. Without that check, driving the
buffer into its overflow path reaches an assertion inside `quinn-proto`
(`datagrams.outgoing.payload_bytes desynchronized`) that aborts the process. That
is an upstream defect, reproduced at 2,048 entities on one connection and not
diagnosed further; not filling the buffer avoids it and is the right behavior
for latest-only state regardless.

**Measured on one M1, everything on loopback, 20 Hz.** One connection carries
about 2,500 entities: 41,984 packets a second delivered at 2,048 with none
undeliverable, 89% at 2,560, and zero at 3,072. The failure is a cliff rather
than a slope — once the send buffer is saturated it stays saturated. Spread
across connections the edge does better: 8 clients of 512 delivered 84,910 a
second with none undeliverable and the region at 10.6 ms of its 50 ms budget. At
16 clients of 512 half the packets are undeliverable, with the whole stack on one
machine.

**Known and not fixed.** A packet at the region's full payload budget plus the
five-byte datagram header can exceed what the path will carry, and quinn refuses
rather than fragmenting. That shows up as `undeliverable` rather than as a silent
truncation. `herd` sets its own smaller budget for this reason and has a test
pinning it.

**Not in it.** Migration driven by anything but a consumer, input sequence numbers
and client prediction, any movement resolution in the region, edge-side interest
management, and any authorization beyond what QUIC's TLS gives.

### Heartbeats

Both tiers publish one, and the library holds the timer. `docs/adr/0007`
reversed `docs/adr/0002` on that point: cadence is still a deployment choice —
`set_heartbeat_interval`, 30 seconds by default, zero to switch it off — but
every field of a region's load is umwelt's own number, so a consumer assembling
one was maintaining our data on our behalf.

A region needs no new plumbing for it. `RegionServer` already holds
`Arc<Inbound>`, and `Inbound` is already driven by the tick: `apply` is handed a
`Step` and `settle` is handed the simulation, and neither call is optional. So
`Inbound` accumulates the load, `WorldSimulation` accumulates its own tick
timing, `settle` reads it off, and the server's timer publishes.

An edge publishes on `umwelt.control.edge.{edge}.heartbeat`, carrying clients
connected, entities managed and how many observe, which regions hold them, and
per span: packets relayed, packets undeliverable, commands received and commands
it refused. Not commands a *region* refused — a region counts those per edge and
never tells the edge. And no address: an edge's listening address is unlikely to
be usable by whatever reads the control plane, which may be in another VPC, and
a client is told where to connect by the game's matchmaking rather than by
umwelt.

### The smoke test

The smoke test is `herd`, the companion repo, and none of it lives here: herd
depends on umwelt through a path dependency and therefore sees the public API
and nothing else. What herd cannot do through that API is a finding about the
API rather than a reason to reach around it.

It is three peers, matching the three programs a consumer writes.
`herd-sim` owns a `WorldSimulation` and serves a region. `herd-edge` holds an
`EdgeServer`, and given `--to` and `--migrate` also walks a herd of its own
between two regions by the sequence above. `herd-game` is the game client: it
speaks no NATS and knows no region except the ids that come back on its own
entities.

The figures below predate that split and were taken with a program that drove
the region side directly, with no client behind it. They measure the region,
which the split did not touch. Per-edge counters live on `Edges`, since a
region's total says nothing about which edge is carrying what.

Conditions: one region, eight edges, one M1, loopback, 20 Hz, AC power. Mean
and worst tick are taken within one second. These are smoke test figures from a
laptop rather than benchmark figures, and §Whole-pipeline is the measurement to
quote for per-viewer cost.

Both wait modes are reported because they differ by a factor of four at low
load. `Wait::Sleep` sleeps until `SPIN_MARGIN` before the deadline and spins the
rest. `Wait::Hold` holds the core for the whole period.

| observers | entities | sleep mean | hold mean | hold worst | delivered/s | needed/s |
|---|---|---|---|---|---|---|
| 1,024 | 8,192 | 9.35 ms | 2.07 ms | 2.66 ms | 21,513 | 20,480 |
| 2,048 | 8,192 | 11.93 ms | 3.92 ms | 4.57 ms | 43,305 | 40,960 |
| 4,096 | 8,192 | 14.85 ms | 7.40 ms | 9.63 ms | 81,737 | 81,920 |
| 8,192 | 8,192 | 15.65 ms | 13.54 ms | 14.75 ms | 163,839 | 163,840 |
| 12,288 | 12,288 | 22.78 ms | 20.08 ms | 23.26 ms | 255,432 | 245,760 |
| 16,384 | 16,384 | 28.02 ms | 27.02 ms | 30.43 ms | 328,249 | 327,680 |
| 24,576 | 24,576 | 38.50 ms | 38.71 ms | 43.65 ms | 512,627 | 491,520 |

The first four rows hold entities at 8,192 and vary how many of them observe.
The last three make every entity an observer, so both counts move together.

**Per-viewer cost, from the hold column.** Mean tick goes from 2.07 ms at 1,024
observers to 13.54 ms at 8,192 while the snapshot stays at 8,192 entities. The
slope is 1.6 µs per viewer per tick. The intercept is about 0.4 ms, which is the
work done per entity regardless of who observes: the game step, the odometer and
the snapshot rebuild.

The sleep column is not usable for this, because at low observer counts it is
measuring the idle penalty rather than the work.

**The idle penalty, reproduced.** Sleep costs 4.52x at 1,024 observers, 2.01x at
4,096, 1.16x at 8,192 and 0.99x at 24,576. §Idle costs speed predicted 4x on an
idle region and 1.3x on a busy one, from a different measurement. A tick that
occupies 2 ms of a 50 ms period spends 96% of that period asleep, and the core
does not return to full speed immediately.

This also accounts for a set of earlier figures on this page being wrong.
Sweeping observer count under `Wait::Sleep` and reading the low rows as
per-viewer cost measures the sleep, not the viewers.

**Whether to make hold the default: no.** It saves 7.3 ms at 1,024 observers,
where the tick uses 19% of its period and has 41 ms of slack. It saves nothing at
24,576, where the tick uses 77% of its period: 38.50 ms sleeping against 38.71 ms
holding. It costs one core held at 100% for the process lifetime in both cases.
The reason to select it is worst-tick jitter, which is what `Wait::Hold`'s
documentation already says: at 12,288 observers, sleep's worst tick was 40.40 ms
and hold's was 23.26 ms.

Untested, and likely to beat both: `SPIN_MARGIN` is 1 ms of a 50 ms period. If
the penalty is the core returning from a low-power state, spinning for a larger
fraction of the period may recover most of the 4.52x at a fraction of hold's
cost. Nobody has swept it.

#### Delivery had to be batched

The first version of the sink wrote a header, wrote a body, and flushed, for
three syscalls per payload, from the single thread `Handoff` drains with.
Measured on AC power under `Wait::Hold`, with that version against the
current one:

| observers | unbatched/s | kept | batched/s | kept |
|---|---|---|---|---|
| 8,192 | 172,024 | 105% | 163,839 | 100% |
| 12,288 | 246,635 | 100% | 255,432 | 104% |
| 16,384 | 291,772 | 89% | 328,249 | 100% |
| 24,576 | 221,063 | 45% | 512,627 | 104% |

`kept` is payloads delivered per second against payloads produced per second,
which is observers times tick rate. Below 100% a client's update rate falls
without anything reporting an error, because the loss is `Handoff` slots
overwritten before the I/O thread reaches them rather than the queue refusing
work. At 24,576 observers and 45%, a client asking for 20 Hz receives about 9 Hz.

The change: [`PayloadSink`] gained `flush`, with a default that does nothing.
`Handoff` calls it at the end of each drain pass, which is where a burst of sends
ends. Each edge holds a 256 KB buffered writer, so `EdgeSink` writes the viewer
id and the payload directly into it, and one syscall per edge per pass replaces
three per payload. `write_frame` no longer flushes, which changes nothing for the
raw `TcpStream` the handshake writes to, where flush is already a no-op.

An earlier run of this comparison was taken on battery and reported the unbatched
path failing from 12,288 observers upward. On AC power it keeps up to 12,288
and falls behind from 16,384. The battery figures overstated the problem.

**Where the limit sits now.** At 20 Hz with every entity an observer, mean tick
reaches 38.71 ms of the 50 ms period at 24,576 observers, and delivery still
keeps up. That is one M1 on loopback. §Open items still wants this curve on
hardware someone would rent, and still wants a definition of comfortable.

**Payload volume.** 24,576 observers at 98 records each and 20 Hz is 48 million
records per second, or 578 MB/s, across eight edges. That is what the reliable
ordered stream is carrying in place of datagrams.

**Two regions, measured**, before the three-peer split, with one program driving
the region side directly. 512 observers against two regions at 20 Hz, migrating
16 observers a second and churning 8. After
a minute the crowd had settled at 300 entities in one region and 212 in the other:
512 in total, none lost and none duplicated, 960 migrations completed, and no
command refused by either region. Mean tick was 1.22 ms in one and 0.91 ms in the
other, which says the run is about the bookkeeping rather than the load.

Not built: the ack path the ghost mark needs, and the reliable event channel.
The datagram path and the edge server are built; see §The edge server.

### Slot growth under churn

Despawn clears a liveness bit and does not reclaim the slot, so an edge whose
game clients come and go grows the arrays that `CellSnapshot::update` and
`Odometer::accumulate` walk every tick. §Open items predicted that a
long-running region pays tick time proportional to slots ever allocated. It
does.

Ten minutes, one region, three edges, 20 Hz, AC power. Each edge holds 2,048
observers and 512 unattended entities and hands back 32 observers a second,
asking for 32 replacements, for 96 spawns a second across the region. A control
run used the same load with churn switched off, so its slot count never moved.

| segment | churn slots | churn mean | control slots | control mean |
|---|---|---|---|---|
| 0-60 s | 13,248 | 13.05 ms | 7,680 | 14.35 ms |
| 120-180 s | 24,480 | 13.33 ms | 7,680 | 14.67 ms |
| 240-300 s | 35,648 | 13.61 ms | 7,680 | 14.58 ms |
| 360-420 s | 46,816 | 13.64 ms | 7,680 | 14.17 ms |
| 480-540 s | 57,952 | 14.55 ms | 7,680 | 14.10 ms |
| 540-600 s | 63,520 | 15.15 ms | 7,680 | 13.36 ms |

Live entities stayed at 7,680 in both runs, and viewers served stayed at 6,139.
Slots reached 63,712 in the churn run, 8.3 times the live count.

Comparing the first 30 seconds against the last 30 within each run: the churn
run rose 15.2%, from 12.93 ms to 14.89 ms. The control fell 5.6%, from 14.13 ms
to 13.34 ms. The control is what rules out thermal drift over ten minutes of
sustained load, which would otherwise produce the same curve, because slot count
and elapsed time move together in the churn run and cannot be separated from
each other within it.

The two runs differ in absolute level by about 10%, which is inside the
run-to-run variance recorded in §The smoke test. Only the trend within a run is
comparable, not the level between runs.

A least-squares fit over the churn run gives **35 ns per slot per tick**, against
a 12.5 ms intercept. Nothing was dropped or refused in either run.

Taking 35 ns per slot as constant, a 50 ms deadline would be reached at about
1.07 million slots, which at 96 spawns a second is about three hours. That
extrapolation runs 17 times past the measured range and the per-slot cost is
unlikely to hold across it, since the arrays leave successive cache levels on the
way.

#### Skipping dead slots a word at a time

The three walks tested one slot at a time: `CellSnapshot::update` in both of its
passes, and `Odometer::accumulate`. A dead slot cost the same as a live one.

`LiveSet::iter` now walks the liveness bitmap by 64-bit word, yields the set bits
of each, and skips a word that is entirely dead with one comparison. A dead slot
costs a sixty-fourth of what it did.

Re-running the churn configuration unchanged:

| segment | slots | before | after |
|---|---|---|---|
| 0-60 s | 13,248 | 13.05 ms | 12.98 ms |
| 180-240 s | 30,048 | 13.26 ms | 13.07 ms |
| 360-420 s | 46,816 | 13.64 ms | 13.18 ms |
| 480-540 s | 57,952 | 14.55 ms | 13.29 ms |
| 540-600 s | 63,520 | 15.15 ms | 13.06 ms |

First 30 seconds against last 30: the original rose 15.2%, from 12.93 ms to
14.89 ms. The rebuilt walk rose 2.8%, from 12.79 ms to 13.15 ms, which is inside
the run-to-run variance recorded in §The smoke test. The fitted slope went from
34.3 ns per slot to -3.0 ns per slot, so slot count and tick time no longer show
a relationship at this range.

The growth is reduced rather than removed. Words are still scanned, at one test
per 64 dead slots, so the three-hour figure above becomes something on the order
of a week at the same churn rate. That is arithmetic on the factor of 64 and not
a measurement: the effect is too small to see across 63,744 slots, and finding
the new limit needs a longer run or a higher churn rate.

Slot reuse is what removes the growth rather than slowing it, and it is not
built. It needs either a quarantine until every client has acknowledged the
despawn, which waits on the ack path, or compaction during a quiet period.

---

## The priority accumulator

### Origin

Starsiege TRIBES, which shipped December 1998, written up by Frohnmayer and Gift
at GDC 1999. Paper: https://www.gamedevs.org/uploads/tribes-networking-model.pdf

The architecture taken from it: give each client a fixed byte budget, score
every candidate by how stale the client's picture of it has become, and ship only
the top N each tick. Distant entities settle into a low update rate. Per-client
cost stays bounded regardless of crowding, with no separate branch for dense
versus empty areas.

### What the paper actually contains, verified against the text

- **Scoping.** Objects enter and leave scope per connection. Only scoped objects
  are ghosted. Scope is managed by the simulation layer.
- **Per-connection ghost records** carrying a ghost id and a state mask, where
  each mask bit is a class-dependent set of state. TRIBES objects typically had
  upwards of 20.
- **The update list** is every object with a status change or a non-zero state
  mask, ordered first by status change, then by object priority. Traversed in
  order, writing into the packet until it is full.
- **Priority is a static scalar** assigned by the simulation layer as part of
  scoping. Quoting: "an object's priority is a value assigned by the Simulation
  layer as part of the scoping process."
- **Bandwidth control.** Each stream manager has a packet update rate and size,
  set by the receiving host. The server also imposes a maximum per client.
- **Most Recent State.** On packet loss a state bit is only re-marked dirty if no
  later packet already carried that bit. Stale positions are never retransmitted.

### What the paper does not contain

There is no accumulated score, nothing that grows per tick, and nothing that
resets on send. The starvation-avoidance property attributed to the design is not
produced by any mechanism described in the paper.

Recollection, **not verified**: the accumulating behavior, a skip counter fed
into a priority function and cleared on send, belongs to the later Torque/TNL
lineage rather than the 1999 paper. The source has not been read.

### Measured: what static priority does under our load

A scratch simulation of the paper's scheme, with 200 moving entities in a 256 m view
radius, static priority `1/(1+d)`, sorted, filled to 58 records, 400 ticks at
20 Hz. Measured, but the harness has not been reviewed by anyone but its author.

```
never updated:   133 / 200
records sent:    23200  (capacity 23200)
```

Every available byte was spent and two thirds of the population received nothing
in 20 seconds. A static total order under a hard cutoff serves the same head of
the list every tick; the only entities that ever crossed the line were ones that
physically drifted across it.

This does not indict TRIBES. Two things visible in the paper make it a non-issue
there: the state mask means idle objects are not in the update list at all, and a
32-player match does not oversubscribe a packet. Computed at our default config,
a viewer gathers roughly 95 candidates against 98 record slots when idle and 77
under a full event backlog, so the calm uniform case is not oversubscribed at all
and reaches only 1.2x under a full backlog. The budget binds under crowding.
(Two earlier versions of this document were wrong here. One said 185 candidates
and 3.2x; 185 is the number *examined* and about half fail the radius test. The
other said 74 and 58 slots, which are 16-byte-record figures against a measured
12-byte record.)

### Decided: the score is accumulated position error

Built as `select::score_of`. This records the decision; `select.rs` is the
implementation.

Score is `drift x weight(distance band)`, where `drift` is the odometer
difference since this client was last told about the entity and `weight` is a
table indexed by `dist_sq.raw().ilog2()`. One multiply, no divide, no square
root, integer throughout, so replay stays bit-identical. Ties break on candidate
index rather than on `EntityId`, since the distance-ordered walk fixes that
order: a replay ranks identically, and the nearer of two tied entities wins.

The alternative was elapsed time since last send, which needs no per-entity
storage. Rejected because a still entity's score grows without bound under it.
Computed from the assumption that the curve is fair across candidates, and
unverified, idle entities would then take slots in proportion to their share of
the candidate set. Rejected also because the growth curve would be tuned against
a proxy with no error signal feeding back. See §Odometer.

### Decided: weight proportional to 1/d

`Weights::inverse_distance` halves the weight at twice the separation, anchored
at a one-meter separation. How wrong an entity looks falls off with distance,
since a meter of error subtends a smaller angle the further away it is, so a
packet is better spent on what is close.

Measured by the quality harness, swept as `Weights::inverse_power`, which takes
the exponent in halves and builds a table by integer shifts alone. The harness
sweeps that constructor rather than a table of its own, so what is measured is
what ships.

Computed and now pinned by test: a `[u16; BANDS]` table spans 4096 to 1, and the
default 256 m view radius is eight doublings of distance from one meter, so the
steepest curve it can express across the whole view is `d^-1.5`. Anything steeper
reaches the floor inside the radius and is flat from there out, which is why
`d^-4` measures like flat weighting at the far end.

Measured, the top three curves are within 0.3% at the default ghost cap and the
choice is worth 8% against flat; at twice that cap it is worth 35% and `d^-1.5`
takes the three near bands. **The curve matters in proportion to how
oversubscribed the ghost set is.** See §Quality harness, measured.

### Open questions

1. ~~**Where per-(viewer, entity) state lives.**~~ Decided: `GhostTable`, one
   open-addressed table per viewer keyed by entity id, bounded by the ghost cap
   rather than by the candidate set. Measured: 10k viewers times ~95
   candidates is ~950k pairs per tick. Keying off the replication set rather than
   the subscription set bounds it, since an entity only needs a score once it is
   a live candidate. An earlier revision expected the ascending-by-id cell
   ordering to make the reconcile a merge rather than a lookup per entity. The
   distance-ordered walk means candidates no longer arrive id-sorted, which
   §Snapshot predicted when the cap was proposed.
2. ~~**The growth curve.**~~ Decided: `1/d`, and the shape of the answer is
   that steeper is better until the weight table floors, which happens at
   `d^-1.5` across a 256 m radius. Curves between `1/d` and `d^-1.5` are within
   0.3% of each other at the ghost cap that ships and separate by 2 to 4% at
   twice it. What remains open is the population, not the curve: every figure
   comes from an invented mix.
3. ~~**Objective.**~~ Decided: mean angular error over the ghost set, with
   worst-case bounded structurally rather than by the objective. Measured, the
   two are not in tension the way this question assumed. Nothing starves: an
   entity in a viewer's ghost set that is never sent is 0.2% or below at every
   cap, curve, grace and send period swept, because an idle entity scores zero
   and a moving one grows without bound until it wins a slot. And the 99th
   percentile does not answer to the objective at all: it is 2.0 m in every
   configuration at a send period of one, doubling with every doubling of the
   period. It is a cadence and packet-size lever.

### What the accumulator does not fix

It caps what is sent, not what is examined. ~~If 5,000 entities pile into one
cell, every nearby viewer still touches all 5,000 to decide which ~58 fit.
Gather cost remains unbounded under crowding.~~ The walk cap fixes that, and it
is a separate mechanism from the accumulator: the gather stops after `walk_cap`
candidates, checked at cell boundaries, so the overshoot is bounded by the
subdivision threshold rather than by the size of the crowd. **Measured: 8,192
entities in one cell deliver 322.8 candidates to a viewer, not 8,192.** See
§Walk cap and sub-cell subdivision.

What remains unfixed is a different thing, and it is about what a client is
shown rather than what a tick costs. Every entity inside the cap is an
individual record, so a viewer facing a crowd larger than its ghost set is told
about the nearest few hundred and nothing about the rest. The answer to that is
hierarchical, an aggregate representation per cell so a distant crowd costs one
record instead of thousands, and it is not built.

---

## Benchmark results

**Read every tick figure here against the 50 ms a 20 Hz tick has.** Where a
table gives no percentage, divide. Several tables below are over budget on
purpose, because a cap or a crowd size is being pushed until it breaks; a table
that is over budget by accident is a bug in the table.

**Every figure here was taken on a working desktop, not a quiet machine.** The
same laptop is running chat clients, browser windows and terminal sessions while
it measures. Medians and the shape of a curve survive that; a lone outlier
usually belongs to the machine rather than to the code, and no figure here is a
clean-room number. A server-class result needs a server with nothing else on it,
which nothing here has been run on.

**Whole-tick figures taken before payload assembly understate a tick.** A tick
now encodes ~98 records per viewer and hands them to a sink, work that did not
exist when the earliest pipeline tables were taken. Measured against a rerun of
the same scenarios, the gap is 0.39 µs per viewer on the uniform region and 0.27
on the town square. §Thread scaling, §Whole-pipeline benchmark and §Ghost cap
and walk cap have all been rerun and carry current numbers, with the
pre-assembly figures kept beside them. **The scenario groups pin one thread**,
because what they compare is per-viewer cost between populations; §Thread
scaling carries the speedups to divide by.

Core i7-6700 (4 cores / 8 threads, Skylake, 8 MB L3), 64 GB. Single-threaded.
Per-viewer figures are median time divided by N. Columns are cumulative.
Measured.

| viewers | build the box | find cell, then build | ...then walk its cells |
|---|---|---|---|
| 100 | 1.51 ns | 2.65 ns | 28.7 ns |
| 1,000 | 1.62 ns | 2.72 ns | 28.6 ns |
| 10,000 | 1.50 ns | 2.78 ns | 29.1 ns |
| 100,000 | 1.60 ns | 2.65 ns | not run |

Membership test: 0.67 ns per element.

- Flat across three orders of magnitude. No cache cliff at 100k. Computed, 100k
  viewers is 800 KB against 8 MB of L3. The cliff should appear nearer 1M; that
  case has not been run.
- Position load costs ~1.1 ns per viewer (the gap between the first two columns).
  Struct-of-arrays is doing measurable work.
- Computed mean subscription size on a 32x32 grid at radius 2 is 23.16 cells.
  28.7 / 23.16 = 1.24 ns per cell, so the loop is real.
- 10k viewers: 27.8 microseconds against a 50 ms budget, 0.056%. At 100k, 0.53%.
- "Performance has regressed" lines were noise. Every group moved 2-9% in the
  same direction including ones sharing no code.

The gather benchmark, below, is the first measurement that involves entities.

---

## Gather benchmark, measured

Core i7-6700, single-threaded, same machine as the subscription figures.

```
gather/uniform/1000      923 µs     →  923 ns per viewer,  95 candidates
gather/uniform/10000     9.38 ms    →  938 ns per viewer,  95 candidates

gather/hot_cell/512      1.67 µs    →  608 candidates for one viewer
gather/hot_cell/2048     5.39 µs    →  2,127 candidates
gather/hot_cell/8192     19.46 µs   →  8,192 candidates
```

**Uniform cost.** 10k viewers is 9.38 ms
against a 50 ms tick, 18.8% on one core, against subscription's 0.056%. Flat
from 1k to 10k, so linear in viewers with no cache cliff at this size.

**Per-entity cost.** The crowded case is faster than the uniform one. Uniform
walks ~185 entities per viewer in 938 ns, so 5.06 ns each. The 8,192-entity hot
cell is 2.37 ns each.

That is backward from intuition and it is the layout working as designed. A hot
cell is one contiguous run, so the prefetcher gets a clean stream. Uniform is
23 separate runs of ~8 entities, and each run touches four arrays for 32 bytes
apiece, so roughly four cache-line fetches per eight entities, with the prefetcher
never picking up a stride.

The earlier estimate in this document of "near 0.5 ns, gather under 2 ms" was
about 5x optimiztic. The mechanism was right; the assumption that the walk is one
long sequential stream was not. Run length is the variable.

**Hot cell scaling.** Linear per viewer, quadratic per tick. Cost scales
cleanly with crowd size. The problem is that in a real crowd the entities are the
viewers, so the tick cost is per-viewer cost times crowd size:

| crowd in one cell | per viewer | × crowd, 1 core | of a 50 ms tick |
|---|---|---|---|
| 512 | 1.67 µs | 0.85 ms | 1.7% |
| 2,048 | 5.39 µs | 11.0 ms | 22% |
| 8,192 | 19.46 µs | 159 ms | 319% |

Eight cores brings the 8,192 case to roughly 20 ms, which fits the tick and
leaves nothing for simulation, scoring, or packet assembly. The breaking point on
this hardware is between 2,000 and 8,000 entities in a single 128 m cell.

**What the accumulator does not fix.** The budget caps what is sent. Every one of
those 8,192 entities is still examined to decide which 58 win. Closing that gap
needs a hierarchical representation, an aggregate per cell so a distant crowd
costs one record instead of thousands. That is a different mechanism and is not
built.

### Where the time goes, measured

A walk over cells that hold nothing isolates the per-cell cost of the outer loop
with no entity work at all.

```
22.96 cells walked, 0 entities   ->  94.5 ns per viewer  =  4.12 ns per cell
```

That is 95 ns of the uniform case's 906 ns. Loop overhead is 10%; it is not the
cost.

Varying cell size holds the population and the view radius fixed and changes how
many entities sit in one contiguous run. Per-entity figures below have the
4.12 ns per cell subtracted.

| cell size | cell radius | cells walked | entities per cell | examined | kept | ns/viewer | ns per entity |
|---|---|---|---|---|---|---|---|
| 64 m | 4 | 75.16 | 2 | 150.7 | 94.9 | 1,373 | 7.06 |
| 128 m | 2 | 23.08 | 8 | 184.9 | 94.9 | 906 | 4.38 |
| 256 m | 1 | 8.23 | 32 | 263.8 | 94.9 | 925 | 3.38 |
| 512 m | 1 | 7.60 | 128 | 972.3 | 94.9 | 1,795 | 1.81 |

The same entity with the same arithmetic and the same 131 KB working set costs
four times more to examine in a run of 2 than in a run of 128. Run length is the
cost.

`kept` is 94.9 in every row, as it must be: the view radius did not change, so
the same entities are in range regardless of how the grid is drawn.
Only wasted examination moves.

Total per-viewer cost has a minimum between 128 m and 256 m, which land within
2% of each other. Small cells fit the view circle tightly and examine less waste
but scatter the population into short runs; large cells stream well but overshoot
the circle badly. 128 m was chosen from the over-subscription table, not from
throughput, and happens to sit on the measured optimum.

The measured waste ratios also confirm that table:

| cell size | predicted ratio | measured (examined / kept) |
|---|---|---|
| r/4 (64 m) | 1.61x | 1.59x |
| r/2 (128 m) | 1.99x | 1.95x |
| r (256 m) | 2.86x | 2.78x |
| 2r (512 m) | 11.5x | 10.25x |

### The conclusion these lead to

There is no layout win left to find.

The 8,192-entity hot cell measures 2.37 ns per entity, the fastest figure in the
set, because a crowd is one enormous contiguous run. The crowd is already the
best case for memory access, not the worst.

So the town square costs roughly 180 ms per tick on one core, and consumes
essentially the whole 50 ms budget across four, because there are 8,192 entities
and 8,192 viewers looking at them. It is arithmetic, not cache. The next lever cannot
be "examine each entity faster", which is close to exhausted. It has to be
"examine fewer entities," and the priority accumulator does not do that. It caps
what is sent while every one of those 8,192 is still examined to decide which 58
win.

### Parallel scaling, measured

Viewers partitioned across threads, each thread with its own output buffer, all
reading one snapshot immutably. Two independent runs at 50 samples and 10 s
measurement time.

**Report ratios, not absolute times.** Eight of twelve absolute figures had
non-overlapping confidence intervals between the two runs, drifting 1% to 9%,
while each run's internal interval was around 1%. The instability is machine
state between runs, not sampling, and more samples do not remove it. Speedups
are stable because whatever scales a run cancels in the ratio.

| threads | uniform 10k | town square 2,048 | town square 8,192 |
|---|---|---|---|
| 1 | 1.00 | 1.00 | 1.00 |
| 2 | 1.93 | 1.93 | 1.90 |
| 4 | 3.58 | 3.58 | 3.60 |
| 8 | 4.56 | 3.44 | 3.36 |

Absolute times, averaged across the two runs and good to about 10%: uniform
10.25 / 5.31 / 2.87 / 2.25 ms; town square 2,048 at 11.82 / 6.13 / 3.30 /
3.44 ms; town square 8,192 at 180 / 95 / 50 / 54 ms.

**Hyperthreading helps the uniform case and hurts the crowded one.** Eight
threads beats four for uniform (4.56 against 3.58) and loses to four for both
crowd sizes. Six of six across three scenarios and two runs. The cause is not
established; measuring cache misses or pinning threads would settle it. Thread
count should be configurable rather than fixed at logical core count.

**Uniform is settled.** About 2.25 ms at eight threads, roughly 4.5% of a 50 ms
tick, down from 18.8% single-threaded.

**The town square consumes the entire tick.** Four threads gives 48.4 ms and
51.9 ms across the two runs against a 50 ms budget, for the gather alone, with
nothing spent on simulation, scoring, or assembly. Straddling the budget rather
than fitting inside it.

**The 8,192 single-threaded figure is about 180 ms.** Two 50-sample runs gave
178.79 and 182.13. The extrapolation of one probe viewer times the crowd size
gives 159 ms, so extrapolation underestimates by roughly 13%. Two earlier
10-sample runs gave 188.65 and 159.30 and are not trustworthy.

### False sharing, measured

`DiscoveredEntities` carries `#[repr(align(128))]`. Removing it and rerunning:

| | 1 thread | 2 | 4 | 8 |
|---|---|---|---|---|
| uniform | same | 1.7x worse | 2.6x worse | 2.8x worse |
| town square 2,048 | same | 4.9x worse | 11.9x worse | 7.3x worse |
| town square 8,192 | same | 5.0x worse | 6.1x worse | 7.6x worse |

**Single-threaded is unchanged in all three scenarios** (10.04 against 10.35,
11.75 against 11.88, 179.92 against 178.79). With one thread there is no second
thread to contend with. That control rules out a plain layout effect and leaves
inter-thread coherence traffic as the explanation, though cache-line transfers
were not measured directly.

Without the alignment, parallelism is actively harmful: the 2,048 town square
goes from 11.75 ms serial to 39.65 ms at four threads, 3.4x slower for four times
the hardware.

The mechanism: the buffers' heap data is entirely separate, but the 24-byte `Vec`
headers sit adjacent in a `Vec<DiscoveredEntities>`, and `push` writes the length
field. A test asserts adjacent buffers land at least 128 bytes apart; with the
attribute removed it reports 24.

Any per-worker mutable state added later needs the same treatment.

#### Confirmed by `perf c2c`

The mechanism above is no longer inferred. `perf c2c` samples memory accesses
with PEBS and reports HITM events, meaning a load that found its line held in
another core's cache in modified state, forcing a write-back and a transfer.
That transfer is the cost of false sharing.

Both builds profiled on the same workload, the 2,048 town square at four threads.

| | unaligned | aligned |
|---|---|---|
| total local HITM | 6,056 | 120 |
| shared cache lines reported | 22 | 99 |
| worst single line | 99.47%, 6,024 HITM | 7.50%, 9 HITM |
| loads sampled at 30+ cycles latency | 115,262 | 21,806 |
| benchmark time under perf | 32.3 ms | 3.49 ms |

Unaligned, one cache line at `0x5d8e9f2f8480` carried 99.47% of every HITM event
in the program, and 79,823 of 271,016 sampled memory records touched it. Within
that line the stores landed at offsets `0x00`, `0x18`, and `0x30`: a stride of
exactly 24 bytes, which is the size of a `Vec` header and therefore of an
unaligned `DiscoveredEntities`. Three buffers in one line, each written by a
different thread. The offsets between them carried the loads, which is `push`
reading pointer and capacity before writing length. Every contending code address
resolved to `gather_into` with `push` inlined.

Aligned, that line is absent. No remaining line exceeds 9 HITM, 89 of the 99 sit
at one event each, and four of the top five addresses are in kernel space rather
than in this program. That is a noise floor, not contention.

Reading note: HITM is charged to the load that had to fetch the line back, not to
the store that dirtied it. The HITM column names victims. The store column, with
its 24-byte stride, is the evidence.

The 81% drop in loads sampled at 30 or more cycles is a second, independent
signal. The same workload doing the same work has far fewer slow loads once the
lines are separated, because the loads were slow on account of waiting for them.

#### Symbolization caveat

`perf` correlates data addresses to code by recording both the data address and
the instruction pointer in each PEBS sample, then resolving the instruction
pointer against the binary's symbol table. The pairing is recorded by hardware,
not inferred.

Resolution quality is another matter. There is no `[profile.release]` section in
`Cargo.toml`, so release builds carry no debug info, and `perf` falls back to
nearest-preceding-symbol lookup. The `umwelt::gather` attributions are
corroborated by several distinct instruction pointers resolving to the same hot
function, but the report also attributes some samples to
`std::sys::backtrace::__rus...`, which is almost certainly a nearest-symbol
error, and source columns show codegen unit names rather than file and line.
Adding `[profile.release] debug = 1` would give line-level attribution.

The library contained no threading when this was measured, so the fan-out was
the benchmark's own. `CellSnapshot` and `CellOccupants` are `Sync + Send`,
asserted by test, and `gather_into` takes a caller-supplied buffer, which is all
a caller needs to fan out. `WorldSimulation::tick_with` now spends those threads
itself; §Thread scaling, measured is that measurement.

### Walk cap and sub-cell subdivision, measured

A viewer in a crowd examines every entity in the cell to send about 58 records.
The cap bounds that walk. Three pieces:

1. A cell at or above `sub_threshold` is sorted a second time by sub-cell,
   giving `SubCells`. Runs over dense cells only, once per update.
2. `sub_cell_order` returns the sub-cell visit order for a viewer, nearest
   first. Every order for every origin is precomputed at construction, so a
   lookup is an index into a 4 KB table rather than a sort. A viewer outside the
   cell clamps to the nearest edge sub-cell.
3. `gather_into_capped` walks cells outward from the viewer's own cell, walks a
   subdivided cell by sub-cell outward from the viewer's own sub-cell, and stops
   once `out` holds `cap` entries.

Cells had to become distance-ordered as well as sub-cells. A cap on a row-major
walk fills on whatever cell comes first. The viewer always sits at the center of
its own subscription, so one ring order serves every viewer and is built once
from `cell_radius`.

The cap is checked at cell and sub-cell boundaries, not per entity, so `out` can
exceed it by the population of the last one walked. Truncating mid-run would
break the distance property the ordering exists to provide.

Town square, four threads, every entity also a viewer. 50 samples.

| cap | 8,192 crowd | % of tick | vs uncapped | 10,000 crowd | % of tick | vs uncapped |
|---|---|---|---|---|---|---|
| uncapped | 42.01 ms | 84% | 1.0x | 67.91 ms | 136% | 1.0x |
| 2,048 | 11.32 ms | 23% | 3.7x | 14.10 ms | 28% | 4.8x |
| 1,024 | 5.86 ms | 12% | 7.2x | 7.15 ms | 14% | 9.5x |
| 512 | 3.17 ms | 6.3% | 13.3x | 3.69 ms | 7.4% | 18.4x |
| 256 | 1.82 ms | 3.6% | 23.1x | 1.97 ms | 3.9% | 34.5x |

**The cap converts a quadratic into a linear.** Uncapped cost is viewers times
entities examined, and in a town square those are the same number. Going from
8,192 to 10,000 is a crowd factor of 1.22, so quadratic predicts 1.49 and
measurement gives 1.62. Capped, cost is viewers times a constant, so linear
predicts 1.22:

| cap | measured ratio, 8,192 to 10,000 |
|---|---|
| uncapped | 1.62 (quadratic predicts 1.49) |
| 2,048 | 1.25 |
| 1,024 | 1.22 |
| 512 | 1.17 |
| 256 | 1.08 |

At a cap of 512 the crowd is 6.3% of tick against the uniform case's 5.7%, so a
dense crowd stops being a special case. That is what the mechanism does, not
the raw speedup.

The sublinear ratios at small caps are the fixed per-viewer costs becoming
visible: subscription, ring walk, and empty cells do not scale with the crowd.

Single core, same scenario and run:

| cap | 8,192 crowd | % of tick | 10,000 crowd | % of tick |
|---|---|---|---|---|
| uncapped | 152.14 ms | 304% | 229.19 ms | 458% |
| 2,048 | 38.54 ms | 77% | 46.98 ms | 94% |
| 1,024 | 20.00 ms | 40% | 24.36 ms | 49% |
| 512 | 10.89 ms | 22% | 13.78 ms | 28% |
| 256 | 6.18 ms | 12% | 7.29 ms | 15% |

Both crowd sizes fit one core with room to spare at a cap of 512, which was not
true of either before. 8,192 goes from 304% of a single core's tick to 22%, a
14.0x reduction with no threads involved.

The uncapped single-core figure here is 152.14 ms against the 178.79 and
182.13 ms measured for the same scenario in two earlier runs. That is a 16% gap
in the direction of the between-run drift already documented. Uncapped and capped
were measured in the same session here, so the 14.0x ratio is internally
consistent; the absolute figures across sessions are not.

**Cost of the second sort**, worst case with the whole population in one cell:

| | `update` time |
|---|---|
| subdivision off | 75.4 µs |
| 8x8 sub-grid | 139.7 µs |
| 16x16 sub-grid | 170.4 µs |

64 µs added against the tens of milliseconds it saves, paid once per tick rather
than once per viewer. It nearly doubles `update` in this worst case because every
entity in the world is in the dense cell.

**The semantic cost.** A viewer stops seeing everything within its radius and
starts seeing the nearest N. That is the entity cap every MMO ships, and it is a
product decision arriving as an optimization.

~~**Not established.** What N should be.~~ Established: 256, measured against
both ends of the trade. It has to leave the accumulator enough candidates to
choose among, and it has to be small enough that the ones it holds are refreshed
often; below about 160 a client's packet does not even fill. See §Quality
harness, measured. What a viewer notices when entities past the cap stop
existing is still unmeasured, and is a different question from what it notices
about the ones inside it.

Correctness at a ragged population is tested rather than assumed: cell size,
`sub_axis`, and `cell_shift` are all powers of two, so a 10,000 crowd exercises
partial sub-cells, uneven thread chunking, and caps that do not divide evenly.

### Quality harness, measured

Every other measurement here answers how long a tick takes. This one answers
whether what the tick sent was worth sending. `examples/harness.rs` models each
client's belief, comparing the position it was last told against the truth at
the moment the next packet is decided.

60,000 entities, 200 viewers, 400 ticks at 20 Hz with the first 40 discarded.
The population is mixed on purpose: 35% props that never move, 25% idlers, 25%
walkers at 1.5 m/s, 10% sprinters at 6 m/s and 5% vehicles at 30 m/s. **The mix
is chosen, not measured against any real game.** A population moving at one
speed cannot distinguish scoring on displacement from scoring on elapsed time,
which is the reason the odometer exists.

Errors are angular, in milliradians, because a mean over meters cannot compare a
near entity against a far one and is minimized by weighting everything equally.

**Corrected: what the harness counts.** Starvation and coverage are counted over
the ghost set. Error is counted over every ghost a client holds, in the set or
not, since a stale ghost is rendered either way. The first revision counted both
over every candidate the gather returned, which made a cap look worse the more
its walk overshot it: an entity past the ghost set is one selection declines to
score by design, not one being starved. **That accounting is where the 8.5% and
2.9% "never sent" figures this section used to report came from.** Counted
against the set, never-sent is 0.2% or below at every cap, curve, grace and send
period below. Starvation is not a live problem, and the earlier figures were
measuring the walk cap's overshoot.

Every table below is one sweep of `cargo run --release --example harness`, each
row a full 400-tick run, all from one session at the defaults this tuning
settled. Error is mean angular error in milliradians, by separation.

**The curve**, at the default ghost cap of 256:

| curve | 0-32 m | 32-64 m | 64-128 m | 128 m+ |
|---|---|---|---|---|
| flat | 10.20 | 3.11 | 1.44 | **0.69** |
| 1/sqrt(d) | 9.62 | 2.95 | 1.47 | 0.70 |
| **1/d** | 9.42 | 2.91 | 1.40 | 0.75 |
| d^-1.5 | **9.40** | **2.90** | **1.39** | 0.79 |
| 1/d^2 | **9.40** | 2.93 | 1.48 | 0.70 |
| 1/d^4 | 10.03 | 3.11 | 1.44 | 0.69 |

and at a cap of 512, where the set competing for a packet is twice the size:

| curve | 0-32 m | 32-64 m | 64-128 m | 128 m+ |
|---|---|---|---|---|
| flat | 14.78 | 4.70 | 2.21 | **1.04** |
| 1/sqrt(d) | 11.09 | 3.72 | 2.34 | 1.11 |
| **1/d** | 9.61 | 3.28 | 1.87 | 1.30 |
| d^-1.5 | **9.40** | **3.11** | **1.80** | 1.57 |
| 1/d^2 | 9.46 | 3.54 | 2.43 | 1.16 |
| 1/d^4 | 13.72 | 4.75 | 2.24 | 1.06 |

At the default cap the top three curves are within 0.3% of each other and the
choice is worth 8% against flat. At 512 it is worth 35%, and `d^-1.5` wins the
three near bands. **The curve matters in proportion to how oversubscribed the
ghost set is**, which is why a cap sweep and a curve sweep cannot be read apart.

Nothing steeper than `d^-1.5` is expressible: the table spans 4096 to 1, and at
`1/d^2` that floor arrives at 64 m and at `1/d^4` at 8 m, so both are flat across
most of the view and score like flat weighting at the far end. `select.rs` pins
those three floors by test.

`1/d` stays the default. `d^-1.5` is better where it differs, by less than the
run-to-run meaning of these numbers at the cap that ships.

**The ghost cap.** Quality is the harness crowd; cost is the town square at
8,192 viewers from `benches/pipeline.rs`, a different scenario measured the same
session, with the walk cap matched to the ghost cap throughout:

| cap | records used of 98 | 0-32 m | 32-64 m | 64-128 m | 128 m+ | tick, 1 thread | tick, 8 threads |
|---|---|---|---|---|---|---|---|
| 64 | 41.8 | 9.83 | 2.92 | 1.46 | 0.70 | 16.25 ms | 3.91 ms |
| 128 | 83.5 | 9.62 | 2.91 | 1.40 | **0.66** | 29.46 ms | 7.17 ms |
| 160 | 97.5 | 9.55 | 2.90 | 1.38 | 0.67 | | |
| 192 | 98.0 | 9.53 | 2.91 | **1.37** | 0.68 | | |
| 224 | 98.0 | 9.46 | 2.91 | **1.37** | 0.70 | | |
| **256** | 98.0 | **9.42** | **2.90** | 1.40 | 0.75 | 54.26 ms | 13.44 ms |
| 384 | 98.0 | 9.48 | 3.08 | 1.62 | 1.01 | | |
| 512 | 98.0 | 9.61 | 3.28 | 1.87 | 1.30 | 103.75 ms | 24.58 ms |
| 1,024 | 98.0 | 10.00 | 3.82 | 2.45 | 1.82 | 203.19 ms | 48.63 ms |

The cap is bounded from both ends. **Below about 160 a client's packet does not
fill**, because only a ghost that moved consumes a slot: at a cap of 64 a viewer
sends 42 records of the 98 its 1,200-byte payload paid for. Above 256 every
ghost is refreshed less often and every band's error grows, while cost rises by
1.8 to 2.0x per doubling of the cap. Quality is flat from 160 to 384,
which is where the cap is a real dial; 256 is the top of that, keeping packet-fill
margin for a crowd denser or slower than this one.

The cost columns are the same benchmark at two thread counts, since the single
thread figure is what the older tables in this document are comparable to and
the eight-thread figure is what a consumer gets by default. Both were rerun in
this session. **The one-thread column runs 8 to 12% slower than the table in
§Ghost cap and walk cap, measured**, in the direction of the between-session
drift already documented there; the shape is identical.

**Grace.** Viewers drawn from the walkers at 1.5 m/s, and from the vehicles at
30 m/s, which is what a viewer crossing a crowd rather than standing in one
looks like:

| grace | 0-32 m, walking | 0-32 m, driving | first sightings per packet, driving | ghosts held, driving |
|---|---|---|---|---|
| 0 | **9.42** | 9.85 | 2.96 | 254.6 |
| **1** | **9.42** | **9.74** | 2.92 | 258.3 |
| 2 | 9.44 | 9.77 | 2.89 | 261.4 |
| 3 | 9.47 | 9.81 | 2.87 | 264.4 |
| 5 | 9.53 | 9.91 | 2.83 | 269.4 |
| 10 | 9.71 | 10.14 | 2.76 | 283.8 |
| 20 | 9.99 | 10.38 | 2.67 | 311.6 |

Grace buys fewer first sightings and pays in ghosts held past their usefulness,
and every tick of it past the first is a worse trade than the last. One tick is
the least error of any value swept against a moving viewer, ties for the best
against a standing one, and still takes 1.4% off the churn. `DEFAULT_GRACE` was
3 on nothing but taste and is now 1.

A standing viewer barely exercises the parameter at all: 20 ticks of grace costs
it 6% error to save a fifth of an already small churn. The case grace exists for
is the moving viewer, and that is the column that has an interior optimum.

**Send period**, at the default cap and grace:

| period | mean angular | p99 error | 0-32 m | first sightings per packet |
|---|---|---|---|---|
| 1 | 1.63 | 2.0 m | 9.42 | 0.49 |
| 2 | 3.56 | 4.0 m | 19.22 | 0.97 |
| 4 | 7.41 | 8.0 m | 38.74 | 1.82 |
| 8 | 15.05 | 16.0 m | 77.61 | 3.36 |

Error scales with the period exactly, which is the useful result: **the 99th
percentile is a cadence lever, not a curve one.** No weighting in the table
above moves it and every one of these rows doubles it.

This sweep also refutes a claim that was in `ClientLimits::send_period`. A period
above the grace does not make every ghost depart and arrive again each turn: at
a period of 8, a client still holds its 256 ghosts and takes 3.4 first sightings
per packet, because everything still in the ghost set is stamped on every
serve. Measured at a grace of both 1 and 3, which agree. What actually happens is that grace goes inert, since a
ghost is only aged when its viewer is served. Per tick rather than per packet,
the churn at a period of 8 is slightly lower than at 1.

**What none of this fixes.** The 99th-percentile error is 2.0 m in every
configuration at a period of one. That is the vehicles, and no cap, curve or
grace touches it; the levers are a larger packet or a higher send rate.

Caveats. The harness is newer and less trusted than the library: four bugs were
found in it during its first run and a fifth, the accounting corrected above,
during this tuning, each surfacing as an implausible number rather than as a
failing test. One density and one motion mix have been run. The cap sweep was
repeated against three populations, which moved every figure by less than the
gaps being read from it. **Every number here is a measurement of an invented
population, which is the standing reason to hold the defaults loosely.**

### Odometer benchmark, measured

Apple M1, 8 cores, single-threaded. **Not the machine the figures above were
taken on.** Cross-machine comparison with them is not valid.

Every slot live:

| slots | per call | per slot | of a 50 ms tick |
|---|---|---|---|
| 10,000 | 22.87 µs | 2.29 ns | 0.046% |
| 50,000 | 115.03 µs | 2.30 ns | 0.23% |
| 100,000 | 231.10 µs | 2.31 ns | 0.46% |

Flat across an order of magnitude. Computed: the working set is 28 bytes per
slot, so 2.8 MB at 100,000, which still fits the 4 MB this machine's
`hw.l2cachesize` reports. A cliff should appear past roughly 140,000 slots.
Predicted from that figure alone, not run, and the M1 memory hierarchy has a
system-level cache behind L2 that this ignores.

Dead slots, at 50,000 slots:

| live | dead | per call | vs all live |
|---|---|---|---|
| 50,000 | 0 | 115.54 µs | 1.00x |
| 25,000 | 25,000 | 72.91 µs | 0.63x |
| 12,500 | 37,500 | 52.55 µs | 0.45x |

Computed from the first two rows: a live slot costs 2.31 ns and a dead one
0.61 ns, about 26%. The third row checks to within 2%. This is the first number
on the §Open items concern that a long-running region pays for slots ever
allocated rather than entities alive. Computed, at a 20:1 dead-to-live ratio one
live slot at 2.31 ns and twenty dead at 0.61 ns total 14.51 ns against 2.31 ns of
useful work, so the pass costs 6.3 times what it accomplishes.

Positions are held fixed across timed calls. Displacement values do not change
the instruction path, so this measures the pass and not an idle population.

Same machine and session, for the ratio: `gather/uniform/10000` measures
7.139 ms with 95.3 candidates per viewer, against 9.38 ms and 95 on the Core
i7-6700, so the M1 runs the same work about 1.3x faster at 95.3 candidates
against the recorded 95. At the gather benchmark's own 8,192 entities the
odometer is 0.26% of
the gather, 1.88 ns per viewer against 714 ns. Computed: the 8,192 odometer
figure is interpolated from 2.29 ns per slot. The ratio improves as viewers
grow, since only the gather scales with viewer count.

### Whole-pipeline benchmark, measured

Apple M1, 8 cores, single-threaded, against a 50 ms tick. **Not the machine the
subscription and gather figures were taken on.**

The per-tick work every row also pays, with no viewers registered, is 59 µs at
8,192 entities: 0.12% of a tick. Essentially all cost is per-viewer.

**Rerun after payload assembly.** The rows this table used to carry were taken
before a tick encoded anything, and are kept in the last column so the cost of
that work is visible rather than lost.

| scenario | viewers | tick | of a tick | per viewer | before assembly |
|---|---|---|---|---|---|
| uniform | 1,000 | 2.97 ms | 6% | 2.97 µs | 2.45 µs |
| uniform | 8,192 | 24.31 ms | 49% | 2.97 µs | 2.50 µs |
| same world, nothing moving | 8,192 | 13.93 ms | 28% | 1.70 µs | 1.60 µs |
| clustered region | 1,000 | 6.95 ms | 14% | 6.95 µs | 6.75 µs |
| clustered region | 10,000 | 69.69 ms | 139% | 6.97 µs | 6.54 µs |
| town square | 2,048 | 12.60 ms | 25% | 6.15 µs | 5.45 µs |
| town square | 8,192 | 54.22 ms | 108% | 6.62 µs | 6.06 µs |

One thread. Divide by the speedups in §Thread scaling for what these cost on all
cores: the town square's 108% becomes 27% at eight threads, measured, and the
clustered row's 139% becomes about 34%, computed from the town square's 4.08x
rather than measured.

The clustered rows are the region-in-use case §Not yet benchmarked asked for:
50,000 entities, most cells sparse and eight dense ones holding sixty percent,
viewers drawn from the population so they land where the density is. It behaves
like a mild town square rather than like the uniform case.

**What the accumulator saves**, measured: the same world and the same viewers
cost 13.93 ms when nothing moves against 24.44 ms when everything does, and the
still case sends no records at all.

**What assembly costs, from the same pair.** The still row sends nothing, so it
pays a header and a sink dispatch per viewer and no record encoding; the moving
row encodes 91.7 records per viewer. The gap between them was 0.90 µs per viewer
before assembly existed and is 1.28 µs now, and the whole of that 0.39 µs
increase is record encoding: **4.2 ns per record**. The still row's own increase,
just under 0.1 µs per viewer, is the header, the sink call and the timing around
it. Both pairs were measured within one session, so between-session drift
largely cancels out of the difference.

Every earlier figure for this pipeline was assembled by adding separately
measured stages, two of them by subtraction. Those estimates were 55% optimiztic
uniform and, before the fix below, wrong by 4.2x crowded.

### Idle costs speed, measured

Apple M1, macOS. `herd-sim` at 50,000 entities and no viewers, which is the
per-tick floor: the game step, the odometer and the snapshot rebuild, on one
thread, since umwelt spawns no workers when no viewer is registered. 200 ticks
at 20 Hz. "Tick work" is time inside `tick`, excluding whatever the loop does
between ticks.

| how the loop waits | p50 | p99 | worst |
|---|---|---|---|
| no clock, back to back | 0.64 ms | 1.54 ms | 2.79 ms |
| sleeps to the deadline | 2.56 ms | 4.10 ms | 4.71 ms |
| holds the core to the deadline | 0.64 ms | 0.77 ms | 2.71 ms |
| sleeps, then holds it for 3 ms | 2.56 ms | 4.10 ms | 15.49 ms |

**The same work costs four times more when the loop sleeps between ticks.**
Holding the core recovers it exactly. This is a property of the machine, not of
the tick.

It is not the working set going cold. Shrinking the population tenfold, to
5,000 entities and a working set around 140 KB, leaves the ratio at 4.4x. What
does move it is how busy each tick is:

| entities | back to back | sleeping | ratio | share of a 50 ms tick worked |
|---|---|---|---|---|
| 50,000 | 0.64 ms | 2.56 ms | 4.0x | 1.3% |
| 200,000 | 2.56 ms | 8.19 ms | 3.2x | 5% |
| 500,000 | 7.17 ms | 14.34 ms | 2.0x | 14% |
| 1,000,000 | 14.34 ms | 16.38 ms | ~1.1x | 29% |

And a spin before the deadline only helps once it is a large fraction of the
period, which is the signature of sustained utilization rather than of a warm-up
that finishes quickly:

| core held before the deadline | p50 |
|---|---|
| 0 ms | 2.56 ms |
| 3 ms | 2.56 ms |
| 10 ms | 2.05 ms |
| 25 ms | 0.77 ms |
| 45 ms | 0.64 ms |

**Measured: the penalty is a function of duty cycle.** A thread awake one
percent of the time is treated as one that does not need a fast core, and this
machine has four performance cores and four efficiency ones for the scheduler to
choose between. **Unverified: that the mechanism is specifically core placement.**
Cold memory is ruled out by the population control above; frequency ramp and core
choice are not separated, and doing so needs affinity tooling this project does
not have.

**Confirmed under load.** With 8,192 viewers registered, which is 33% duty, the
same three modes measure p50 12.29 ms free-running, 14.34 ms holding the core
and 16.38 ms sleeping. The penalty falls from 4x to 1.3x, which is what the duty
table above predicts, and holding the core recovers only half of it: `Wait::Hold`
holds the thread that runs the loop, and a busy tick's work is done by scoped
workers that are new every tick and start as cold as the schedule left them.

Two things follow. Under the load a region is meant to carry the effect is
mostly gone, so this is a lightly-loaded-server problem rather than a tick
problem. And it
settles how to read every other table here: criterion runs iterations back to
back at full duty, so these figures are the busy case, which is the right case
for capacity planning and the wrong one for guessing what an idle region costs.

Percentiles come from quarter-octave buckets, so each is the upper edge of the
bucket holding it rather than an interpolated value. The 14.34 against 16.38 row
is one bucket apart and may be no difference at all.

### A game on top, measured

`herd`, the companion load generator, is the first consumer to drive umwelt
through its public API, and the first thing outside a unit test to despawn
anything. What it runs is movement toward attractors with a dwell, a gather and
disperse cycle, crowding damage, a lifespan, and a spawner.

50,000 entities, 8,192 clients, paced at 20 Hz by `run`, one minute:

| | |
|---|---|
| tick work | p50 16.38 ms, p99 20.48 ms, worst 39.22 ms of 50 ms |
| ticks over budget | 0 of 1,200 |
| ticks started late | 0 of 1,200 |
| duty | 33% |
| candidates per viewer | 290 |
| records per packet | 97.8 of 98 |
| despawn records per packet | 1.1 to 1.5 |
| subscriptions changed | 0.23% of viewers served, about 18 a tick |
| deaths | 21.5 a second, population held at 50,000 |
| fullest cell | 3,157 at its peak |

The figures a consumer sees agree with the ones the benchmarks report against
hand-built fixtures: 290 candidates against 268 for the clustered fixture and
323 for the town square, and 97.8 records against 98.0.

**The despawn path carries load for the first time.** Between one and one and a
half despawn records ride every packet, from ghosts aging out of a set and from
entities dying.

**Adversarial patterns, measured.** `herd` doubles as the bot harness of build
order item 7: the same binary with its movement replaced by something hostile.
Each pattern targets one mechanism. 8,192 clients, paced, one minute each.

| pattern | targets | what it reached | tick p50 | over budget |
|---|---|---|---|---|
| the plausible world | | 3,157 in the fullest cell | 16.38 ms | 0 of 1,200 |
| flash | the walk cap and the gather | 8,576 in one cell | 16.38 ms | 0 |
| flap | subscription churn | 100% of viewers a tick | 16.38 ms | 0 |
| thrash | `grace` and the departure queue | 1.05% of viewers a tick | 16.38 ms | 0 |
| teleport | the accumulator | 50 jumps a tick | 16.38 ms | 0 |
| cull | the despawn queue | 26,238 deaths | 14.34 ms | 0 |

**Nothing missed a deadline under any of them**, and the two that reached
furthest say why. Flap drives subscription churn from 0.14% of viewers a tick to
100%, seven hundred times, and the tick does not move: a box is four `i32` and
recomputing it is four comparisons. Flash puts 8,576 in a single cell, more than
the town square fixture's 8,192, and candidates per viewer rise only from 290 to
333, which is the walk cap declining to look at a crowd it cannot send.

Cull is the one that found something, in `herd` rather than in umwelt: a quarter
of the population dying at once takes 195 ticks to replace, because the spawner
is capped at 64 a tick, so the region runs 12,000 short for ten seconds.

**Subscription churn is small**: 18 of 8,192 viewers cross a cell boundary in a
tick. Most viewers are residents standing in a crowd, and a cell is 128 m
across, so only the fast classes cross often. Whatever an edge-side or
cross-region subscription protocol has to carry, it is not a flood.

**Slots ever allocated reach 51,290 after one minute** and climb at the death
rate, which at these settings is about 75,000 an hour on top of the 50,000 the
region started with. Every tick walks all of them. That is the open item about
slot reuse, with a number.

### Moving viewers, measured

What a viewer's own motion costs the tick. Every other row in this file stands
its viewers still, oscillating them by a meter, so a ghost set holds and nothing
churns.

50,000 entities spread evenly, **8,192 viewers**, walk cap and ghost cap at the
defaults. A uniform population holds candidate counts constant at ~280 across
every row, so what changes between rows is which entities are in a set rather
than how many. **The `crowd` rows are the control**: they travel the same number
of entities at the same speed, drawn disjoint from the viewers, so the world
carries the same motion while every viewer stands still.

At the default thread count, which is the configuration that has to fit a 50 ms
tick and does:

| row | first sightings per viewer | records per packet | tick | of a tick | per viewer | vs still |
|---|---|---|---|---|---|---|
| still | 0.00 | 98.0 | 14.03 ms | 28% | 1.71 µs | |
| viewers at 2 m/s | 0.18 | 97.9 | 14.26 ms | 29% | 1.74 µs | +1.7% |
| viewers at 6 m/s | 0.57 | 97.9 | 14.20 ms | 28% | 1.73 µs | +1.3% |
| viewers at 30 m/s | 1.92 | 97.5 | 14.34 ms | 29% | 1.75 µs | +2.2% |
| viewers at 60 m/s | 4.25 | 96.9 | 14.42 ms | 29% | 1.76 µs | +2.8% |
| crowd at 30 m/s | 0.45 | 98.0 | 14.15 ms | 28% | 1.73 µs | +0.9% |
| crowd at 60 m/s | 0.86 | 97.9 | 14.28 ms | 29% | 1.74 µs | +1.8% |

Confidence intervals here are 1.2 to 2.1% wide, which is the same size as the
differences between neighboring rows. The same sweep on one thread resolves them
four times more finely, at the cost of being over budget: **8,192 viewers on
this population do not fit a tick on one core, which is what the threads are
for.**

| row | tick | of a tick | per viewer | vs still |
|---|---|---|---|---|
| still | 54.49 ms | 109% | 6.65 µs | |
| viewers at 2 m/s | 55.28 ms | 111% | 6.75 µs | +1.4% |
| viewers at 6 m/s | 55.82 ms | 112% | 6.81 µs | +2.4% |
| viewers at 30 m/s | 56.64 ms | 113% | 6.91 µs | +3.9% |
| viewers at 60 m/s | 57.46 ms | 115% | 7.01 µs | +5.4% |
| crowd at 30 m/s | 55.88 ms | 112% | 6.82 µs | +2.5% |
| crowd at 60 m/s | 55.93 ms | 112% | 6.83 µs | +2.6% |

**A viewer crossing a crowd at 30 m/s costs 2.2% of a tick at the default thread
count, and most of that is not the viewer.** The control puts 0.9 points of it
on the world's extra motion, leaving 1.3 for the viewer's own. One thread agrees
within noise: 3.9% total against a 2.5% control, leaving 1.4. **Two configurations
that differ four-fold in absolute cost agree on the viewer's own share to within
a tenth of a point**, which is the reason to trust it.

Against the churn it buys, on one thread, that share is 63 ns per extra first
sighting and departure at 30 m/s and 55 ns at 60. The marginal cost falls as
churn rises, so part of it is a fixed cost of the subscription box moving rather
than a per-ghost one.

**Churn is not only the viewer's doing.** A viewer standing perfectly still in a
world where one entity in six travels at 30 m/s still takes 0.45 first sightings
per packet. It is entities crossing its ghost set's edge either way, and the
accumulator cannot tell which side moved.

The bandwidth cost is visible in the same table: records per packet fall from
98.0 to 96.9 as departures take despawn bytes out of the payload. Four
departures is 16 bytes, which is 1.3 records at the default codec.

### Measured: choosing the ghost set by staleness churns it

The first whole-pipeline run found the ghost set thrashing wherever the cap
bound. Measured, 8,192 viewers in one cell, 581 candidates each:

| ghost cap | records per viewer | of which first sightings | ghosts departed |
|---|---|---|---|
| 64 | 64.0 | 64.00 | 64.00 |
| 128 | 98.0 | 98.00 | 98.00 |
| 256 | 98.0 | 76.42 | 71.71 |
| 512 | 98.0 | 0.89 | 1.15 |

At a cap of 128 every record in every packet was a first sighting and 98 ghosts
departed per viewer per tick: clients were told about entities, told to forget
them, and told again, with no position update ever getting through. The uniform
case, where the cap does not bind, showed none of it.

Two causes, both in the first version of `select`:

- An entity the client had never seen scored above every refresh. With more
  candidates than ghost slots there is a permanent supply of unseen entities, so
  the top `ghost_cap` were always strangers and every existing ghost fell below
  the line.
- The ghost set was chosen by score, which is staleness. Sending an entity
  resets its drift to zero, so the entity just corrected became the least stale
  and was dropped first.

The fix is §Selection: distance chooses the set, drift chooses what to send, and
an unseen entity scores finitely. Measured afterward, every scenario reports no
arrivals and no departures at steady state, and the crowded cases run 1.7x to
2.5x faster:

| scenario | before | after |
|---|---|---|
| uniform, 8,192 | 20.58 ms | 20.55 ms |
| clustered, 10,000 | 114.10 ms | 65.44 ms |
| town square, 2,048 | 24.26 ms | 11.23 ms |
| town square, 8,192 | 123.84 ms | 49.73 ms |

Uniform is unchanged because the cap never bound there.

Both columns predate payload assembly and neither has been rerun, since the
"before" one would mean putting the bug back. They are a ratio measured within
one session and should not be compared against the refreshed tables above.

**This is the failure §Not yet benchmarked predicted**: "An entity that drops out
and returns looks maximally stale and wins a slot immediately, so the least
useful part of the visible set could generate the most updates." Nothing short
of the whole pipeline could see it. An isolated benchmark of the ghost table
measured a table at rest, and under the bug it was never at rest, so that
benchmark's conclusion that a cap of 256 beat 512 by 2.5x was an artifact of a
state the real pipeline never reached.

### Thread scaling, measured

Viewers partition across workers by contiguous range. The snapshot, the
odometer and the position arrays are read-only during replication and each
viewer owns its own ghosts, so nothing is shared for writing except the cache
lines at chunk boundaries. Per-worker scratch is separated by construction:
`DiscoveredEntities` and `Selection` are each 128-byte aligned.

Worker count defaults to `std::thread::available_parallelism` and is settable,
because the right number is not obvious.

Apple M1, four performance and four efficiency cores, 8,192 viewers:

| threads | uniform | of a tick | speedup | town square | of a tick | speedup |
|---|---|---|---|---|---|---|
| 1 | 25.05 ms | 50% | 1.00x | 54.44 ms | 109% | 1.00x |
| 2 | 13.12 ms | 26% | 1.91x | 28.28 ms | 57% | 1.93x |
| 4 | 7.08 ms | 14% | 3.54x | 15.48 ms | 31% | 3.52x |
| 8 | 6.15 ms | 12% | 4.07x | 13.36 ms | 27% | 4.08x |

The town square goes from 109% of a 50 ms tick to 27%. Four to eight threads
buys 13% to 16%, which is what heterogeneous cores predict and why the count is
configurable.

**Corrected: this table used to read 21.83 and 52.19 ms at one thread and 5.23
and 13.03 at eight.** Rerun, the same scenarios measure **0.39 µs per viewer
more on the uniform region and 0.27 more on the town square**, or 3.22 and
2.25 ms at one thread.

The work that landed between the two runs is payload assembly and the sink call,
which a tick did not do when the old rows were taken. §Whole-pipeline isolates
that against its own still-versus-moving pair: 4.2 ns per record encoded, plus
just under 0.1 µs per viewer for the header and the dispatch, which is the right
size for what is missing here. Corroborating: the figures elsewhere in this
document taken *after* assembly agree with the rows above, 6.02 ms against 6.15
for the uniform eight-thread case; see §Payloads leave through a sink.

**Nothing regressed between those two measurements**, and an A/B rules out the
accumulator tuning as a cause: at a grace of 3 rather than 1 the same rows
measure 25.04, 13.14, 7.09 and 6.08 ms, which is no change at all.

The whole pipeline scales as well as the gather alone did at 3.53x and 4.32x on
this machine, so the earlier practice of extrapolating the gather's figure to
the rest was sound.

Threads are scoped and spawned per tick, so a tick pays one spawn per worker.
The one-thread row takes a serial path with no spawn at all. How much of the
gap between two threads and perfect halving is spawn cost rather than imbalance
or efficiency cores is **not separately measured**. A persistent pool would
remove it and needs either a dependency or channels and parking.

### Ghost cap and walk cap, measured

Both sweeps are the town square at 8,192 viewers, after the fix, with the gather
sized to the ghost cap.

Rerun after payload assembly, one thread, and extended to a cap of 1,024:

| ghost cap | tick | of a tick | per viewer | records sent | before assembly |
|---|---|---|---|---|---|
| 64 | 15.75 ms | 32% | 1.92 µs | 64.0 | 2.16 µs |
| 128 | 29.59 ms | 59% | 3.61 µs | 98.0 | 3.44 µs |
| 256 | 54.31 ms | 109% | 6.63 µs | 98.0 | 5.88 µs |
| 512 | 103.42 ms | 207% | 12.62 µs | 98.0 | 11.75 µs |
| 1,024 | 203.45 ms | 407% | 24.83 µs | 98.0 | |

Monotone and close to linear in the cap, which it was not before the fix. The
cap is a dial between cost and how many entities a client can see. Every row
from 256 up is over a tick on one thread; at eight the default cap is 27%.

The cap of 64 is the one row that got *cheaper* than its pre-assembly figure,
and the records column says why: it fills only 64 of the 98 slots a packet
holds, so it encodes a third fewer records than any row below it. That is the
same finding the quality harness reports as a packet that does not fill.

What the cap should be is no longer open. 256, because below about 160 a
client's packet does not fill and above 256 accuracy falls in every band while
cost keeps rising linearly. See §Quality harness, measured, which pairs each cap
here with what it costs in accuracy.

Selection keeps the nearest `ghost_cap` candidates and discards the rest
unscored, so a walk cap above the ghost cap is waste. At a ghost cap of 256:

| walk cap | candidates gathered | tick | before assembly |
|---|---|---|---|
| 256 | 322.8 | 54.41 ms | 44.54 ms |
| 512 | 581.3 | 58.41 ms | 48.29 ms |
| 1,024 | 1,094.0 | 67.02 ms | 57.22 ms |

6.9% for matching them, and one fewer parameter. Safe because the cap is checked
at cell boundaries, so a walk cap of 256 still delivers ~323 candidates and
fills the set. `DEFAULT_WALK_CAP` is now `DEFAULT_GHOST_CAP`.

The overshoot is bounded by the subdivision threshold, not by a sub-cell: a cell
below the threshold is walked whole. A gather buffer must be sized
`walk_cap + sub_threshold` or it grows inside a tick.

### Still to instrument

`TickStats` now counts viewers served, candidates gathered, records sent, first
sightings and departures per tick, so entities dropped by budget is covered and
arrivals and departures are visible.

Per-client bytes per tick is covered too, now that payloads are assembled.

~~Left: subscription churn rate and p99 tick duration.~~ Both are covered. Tick
duration: `run` times every tick and hands it to the loop's observer in a
`TickReport`, and percentiles are the caller's since holding a histogram is a
presentation decision. Subscription churn: `TickStats::subs_changed` counts the
viewers whose box moved, which is the viewers that crossed a cell boundary,
since a box is defined by the cell its viewer is in.

Nothing on this list is left.

### Not yet benchmarked

**Boundary churn.** ~~Measurable now without the accumulator.~~ Measured, and it
was a bug rather than a cost: see §Measured: choosing the ghost set by staleness
churns it. The prediction that a returning entity "looks maximally stale and
wins a slot immediately" was exactly right, and the pipeline benchmark is the
only thing that could see it.

~~What remains unmeasured is churn under a *moving* viewer.~~ Measured, for
quality: the harness draws its viewers from a chosen motion class, and the grace
sweep runs one at a walker's 1.5 m/s and one at a vehicle's 30 m/s. A viewer
crossing a crowd takes 2.92 first sightings per packet against a walking
viewer's 0.49, six times the churn, and pays 3% more angular error in the near
band. It holds no ghost at all for 1.5% of its ghost-set-tick pairs, against
0.2% for a walking viewer.
That is the case `grace` exists for, and it is the one with an interior optimum.

~~What remains unmeasured is the *cost* of that churn.~~ Measured, and small:
2.2% of a tick at 30 m/s and 8,192 viewers, of which 0.9 points is the world's
extra motion rather than the viewer's. See §Moving viewers, measured. The other pipeline rows still
oscillate entities in place and stand their viewers still, which is what makes
them stationary enough to compare across sessions.

**A whole tick over a populated region.** ~~Every benchmark so far measures one
population shape in isolation.~~ Built and measured: see the clustered rows of
§Whole-pipeline benchmark. What follows in this section is the reasoning that
motivated it, kept because the question it asks about cache behavior is still
open.

The world is sharded, one sim process per region, so this is a smaller question
than it first appears. A snapshot is bounded by entities per region, not per
world. At 16 bytes each, L2 on the benchmark machine holds about 16,000 entities
and L3 about 520,000, so a region has to be busy before its snapshot stops
fitting cache.

The question that remains: does a dense cell hold its measured numbers when the
rest of its own region is also populated? The town square benchmark ran against a
131 KB snapshot that was entirely the crowd, so the walk had the cache to itself.
A region carrying 50,000 entities is an 800 KB snapshot, past L2, and the dense
cell then competes with the rest of the region rather than owning it.

What to build: a region with realistic clustering, most cells sparse and a few
dense, viewers distributed with the density rather than uniformly, measuring the
full tick of `update` plus every viewer's gather. Then compare the dense cell's
per-viewer cost against its measurement in isolation.

Secondary things it would exercise that current benchmarks do not: subdivision
cost with many dense cells rather than one, and `update` scaling with slot count
and cell count together rather than one at a time.

---

## Build order

1. ~~Core library: cells, subscription~~ (done)
2. ~~Snapshot, cell ordering, gather pass~~ (done)
3. ~~Gather benchmark: uniform and single hot cell~~ (done)
4. ~~Sub-cell subdivision, distance-ordered walk, walk cap~~ (done)
5. ~~Priority accumulator and budget selection~~ (done)
6. ~~`WorldSimulation` and the game hook trait; a minimal `herd` game step~~
   (done). `herd` is now three binaries: `herd-sim` serves a region and runs
   its own game, `herd-edge` holds an `EdgeServer`, and `herd-game` is a
   client
7. Bot harness with adversarial movement patterns
8. `SimulatorEdge` and the sim-to-edge protocol. The link, its handshake and its
   session are built (§The region link, §The session): edges populate a region,
   move what they own, and are sent the replication back. Not built: the
   datagram path, and the edge server that holds game client sockets
9. Checkpoint path (full-fidelity, distinct from the wire payload)
10. Cross-region: boundary replication, authority epochs, handoff
11. Control plane
12. Second library consumer, to prove the API is not shaped around `herd`

Notes on order:

- Phase 3-4 is the whole bet. If the budget system does not degrade gracefully,
  no amount of tiers or regions helps.
- Checkpoint before multi-region. Handoff bugs are miserable to debug without
  reliable restore.
- Two regions on one machine before two machines.
- The second consumer should be sketched early and kept compiling, not written at
  the end.

---

## Payload formats: two, not one

**Client payload.** Replicated fields only, quantized, ~16 B/record, budgeted to
an MTU-sized packet. Produced by the simulation, relayed by edges. **Cannot
restore a simulation**. It omits AI aggro tables, pathfinding progress,
cooldowns, RNG state, spawn timers.

**Checkpoint.** Everything, full precision, larger record, low rate. Consumed by
disk and standby. This is the payload that has a wire format problem, since it
does cross a process boundary.

A standby simulation cannot be fed by the client stream. Two designs:

- **Deterministic replica.** Checkpoint plus the live input stream, running in
  lockstep. Inputs are tiny. Failover is instant. Requires bit-exact determinism,
  which is why `Fixed` exists.
- **State-shipping standby.** Full state every tick. No determinism required.
  Computed: 10k entities x ~200 B x 20 Hz = ~40 MB/s point to point, roughly 50x
  the deterministic option.

---

## Rust implementation notes

- `WorldSimulation`'s tick loop runs on a dedicated pinned OS thread, not Tokio.
  Tokio belongs in `SimulatorEdge`, where the work is connection-shaped.
- Per-client work parallelizes across threads reading the published snapshot.
  Built, with `std::thread::scope` and no dependency. Measured at 3.6x on 4
  physical cores for the gather alone and 3.5x to 3.6x for the whole pipeline.
  Hyperthreading past that helps the
  uniform case and hurts the crowded one, so thread count should be
  configurable.
- Any per-worker mutable state must be separated onto its own cache line.
  `DiscoveredEntities` handles its own; anything added beside it must too.
- Any per-worker mutable state must be separated onto its own cache line.
  `DiscoveredEntities` handles its own; anything added beside it must too.
- `ArcSwap` or `triple_buffer` for the tick-thread-to-worker-threads handoff.
- Struct-of-arrays. `Pos3` is a **value type**, what functions take and return.
  Never `Vec<Pos3>`.
- `bytes::Bytes` for refcounted zero-copy slices. `sendmmsg` via `socket2` or
  `nix` for batched sends. `quinn` if QUIC's unreliable datagrams are wanted.
- Avoid: `Arc<Mutex<World>>` in the tick path, a task per entity, channels in the
  fan-out path, any allocation inside a tick.
- `io_uring` is a later step, after `sendmmsg` is measured as the bottleneck.
- **`#[inline(always)]` was on 170 items and three of them earn it.** Measured
  by downgrading and re-running the benchmarks. The 167 others cost nothing
  measurable at plain `#[inline]`, which is the attribute a library wants there
  anyway, since without it a downstream crate cannot inline a non-generic
  function at all. Three private helpers do earn it, and each carries a comment
  saying so: `gather::take` is 9% of `gather/uniform`, and `ghost::home` with
  `ghost::find` are 9% of `ghost/seen/hot` together.

  **Measure the restore as well as the removal.** A fourth,
  `config::axis_to_cell`, first measured at 2% and was kept on that basis. Run
  again against a saved baseline it read +0.76% downgraded and +0.86% restored,
  the same figure in both states, against a +0.15% floor from re-running
  unchanged code. The machine was drifting upward across the runs and the
  attribute did nothing. A one-directional measurement cannot tell those apart.
- Debug builds do not inline the `Fixed` operators. `Cargo.toml` carries
  `[profile.dev] opt-level = 1` and `[profile.dev.package."*"] opt-level = 3`.
  The magnitude of the debug slowdown is unmeasured.

---

## Prior art

**Exists at single-node scale, worth reading:**

- `lightyear`: rooms-based interest management plus bandwidth-capped priority
  replication. Closest existing implementation.
- `bevy_replicon`: replication core, no built-in I/O, `no_std` capable.
  lightyear rebased onto it and lost distributed authority in the process.
- `renet`: transport and channels only, no ECS coupling.
- `naia`: revocable per-entity authority delegation, conceptually adjacent to
  region handoff.

All assume simulation and replication share a process. Most are Bevy-ECS coupled.

**Does not exist:** any open-source distributed simulation framework. SpatialOS
and Hadean both built one commercially; retrospectives cite server overhead,
network fragility, and integration cost, and both pivoted toward defense
simulation. The Eve Aether Wars demonstration of 14,000 concurrent players in one
environment (2019) reportedly still stands as the high-water mark.

**Generalization limit:** rings 1-3 (tick loop, spatial and entity storage,
movement) generalize. Ring 4 (combat, abilities, AI, loot) does not, because it
needs to reach into rings 2-3 constantly. The achievable target is an engine games are
written *in*, not a platform games are plugged *into*.

**WASM was considered and rejected** for the simulation core. It is a slowdown
against native Rust, and a component boundary would force marshaling on exactly
the query-shaped operations gameplay code performs constantly. It remains a
reasonable fit for the bot harness and for a later gameplay scripting tier.

---

## Open items

- **`Step::positions_mut` borrows the whole `Step`**, so nothing else on it is
  reachable while the position slices are held. A game that reads liveness while
  moving entities has to clone the `LiveSet` first, which `herd` does every
  tick. Handing back the liveness reference alongside the slices would fix it.
- **`WorldSimulation` reports `entity_count`, which is live entities, and has no
  accessor for slots ever allocated.** That is the number the slot reuse item
  above is about, and a consumer currently measures it from the length of its
  own parallel arrays.
- **`TickStats::merge` is private**, so a consumer accumulating across ticks
  writes the summation again. It should be `AddAssign` and `Sum`: summing worker
  stats within a tick and summing tick stats across a run are the same
  operation. `sink_nanos` is not an obstacle, since it is already a sum across
  workers and its doc comment already says to divide by the worker count.
- **A comfortable concurrent-player count per cell has to be published, and is
  not.** A consumer sizing a region, a cell, or a shard has no number from this
  library to plan against, and today the only way to get one is to read the
  benchmark tables and do the arithmetic. That is the wrong thing to ask of
  anyone.

  It is not one number. What a cell can comfortably hold depends on how many
  viewers are within view of it, the ghost cap, the packet budget, the tick rate
  and the worker count, so the useful form is a small table or a formula over
  those rather than a scalar. The pieces exist: per-viewer cost is roughly linear
  in candidates gathered, the walk cap bounds candidates whatever the crowd does,
  and §Whole-pipeline and §Ghost cap between them give the slope.

  What is measured today is a data point, not guidance: the town square carries
  8,192 entities in one cell with every one of them a viewer, at 27% of a 50 ms
  tick on eight cores of an M1. **Guidance needs the curve either side of that,
  on hardware a consumer might actually rent**, and it needs to say what
  "comfortable" means, which is a headroom decision rather than a measurement.
  Until then any number quoted from here is a benchmark result being mistaken
  for a capacity limit.

- **Decided: liveness mask.** `LiveSet` is a bitset parallel to the position
  arrays, one bit per slot. `CellSnapshot::update` consults it in both passes, so
  a despawned entity is absent from the snapshot without moving any other id.
  Ids stay valid for the lifetime of the entity they name.

  Two things it does not solve. A freed slot is still unsafe to reuse, because a client
  holding a ghost of the previous occupant would alias the new one, so reuse
  needs either compaction during a quiet period or a quarantine until every
  client has acknowledged the despawn. And `update` still walks every slot,
  including dead ones, so a long-running region with heavy churn pays tick time
  proportional to slots ever allocated rather than entities currently alive.
  ~~Not measured.~~ Measured: see §Slot growth under churn. Word-level skipping
  in `LiveSet` would cut the walk to one test per 64 dead slots. Not built.
- ~~**Delivery saturates at about 170,000 payloads per second, and the
  simulation does not.**~~ Fixed by batching: see §Delivery used to stop long
  before the tick did. It was three syscalls per payload from one thread; it is
  now one syscall per edge per drain pass, and delivery keeps up to at least
  24,576 observers at 20 Hz. What remains unproven is the next ceiling, since
  nothing has been pushed hard enough to find it, and `Handoff` still drains
  from a single thread.
- **`run`'s observer takes `&mut WorldSimulation`.** It took `&WorldSimulation`
  until the session work, which meant a consumer using `run` could not register
  a viewer at all: between ticks is the only safe point to add or drop one, and
  the observer is the only thing that runs there. A connection-driven server
  does that every time a client arrives, so the hole was not hypothetical.
- **Entity to viewer is still missing from the library, and `Inbound` now keeps
  its own copy.** Tearing an entity down means dropping the viewer watching it,
  and nothing in `WorldSimulation` answers that. The map below is what would
  replace it.
- `CellSnapshot` has no id-to-slot lookup, so nothing can answer "where is entity
  N." Still needed to locate a client's own avatar and to route an event to a
  specific entity. The replication path no longer needs it: `DiscoveredEntity`
  carries the snapshot index and `CellSnapshot::pos_at` resolves it. The cheap
  fix for the remaining cases is one array of length n written during the
  scatter, where the writes are sequential.
- ~~Per-client state (subscription, accumulator scores, send cadence) belongs to
  `WorldSimulation`. Client registration is the API that does not exist yet.~~
  Built. `register_viewer` takes an avatar entity and what the connection
  declared, `unregister_viewer` drops the client, and the per-client state lives
  on `Viewer`. Viewer ids are dense and reusable, since a recycled viewer has an
  empty ghost set. Nothing here names a socket: mapping a viewer to a connection
  is the edge's. What is still missing is the reverse lookup, entity to viewer,
  which is what `EventTarget::Entity` needs; see §Events.
- **A ghost's mark advances on send rather than on acknowledgment**, so a lost
  packet leaves a client's copy of a since-idle entity permanently wrong. There
  is no protocol to acknowledge against yet. Fixing it needs a pending mark
  beside the acknowledged one, growing a ghost record from 12 bytes to 16.
- Viewers are not padded to a cache line, so two workers share the line at each
  chunk boundary. A viewer is written twice per tick, so the contention is
  2(N-1) lines against ~10,000 writes. Not measured.
- ~~`WorldSimulation` has no `run`, so a consumer writes the loop. Building it
  waits on the edge work only because the thread that owns the clock is the one
  the edge will want.~~ Built, in `sim::clock`. `run` and `run_with` pace the
  loop against absolute deadlines as §Pacing the loop settled, `Wait` carries the
  sleep-then-hold choice §Idle costs speed measured, and `Overrun` chooses what a
  late tick does instead of running extra ticks to catch up. It did not have to
  wait on the edge after all.
- **`grace` is in ticks but is only evaluated when a viewer is served**, since
  that is when its ghosts are stamped and aged. A grace below a client's send
  period therefore behaves as zero. Measured as harmless: per tick, the churn at
  a send period of 8 is slightly lower than at 1. Expressing it in serves would
  cost a field per viewer to hold the serve count, and nothing yet needs it.
- `Mul`/`Div` rounding disagree for negative values.
- `protocol_hash` could send raw field values instead of a digest, which is certain rather
  than near-certain, and names the offending field.
- `CellList::push` panics on overflow. Unreachable through the public path, but it
  is a panic in a library.
- `MAX_CELL_RADIUS` of 4 sizes every `CellList` inline at 81 cells.
- Region-local coordinates only. A global coordinate space would need `i64`.
- Stale `CellSet` in a panic message at `subscription.rs:136`.
- Axis convention: `pos.rs` uses z-up. An earlier Elixir design used Y-up to match
  Unity and Unreal. Never resolved deliberately.
- `let cell = snapshot.entities_for_cell(..)` is a misleading variable name at
  `gather.rs:121` and in `snapshot.rs` tests.
- `crates.io` name should be claimed with an empty 0.0.0 publish.

---

## Unverified claims

Do not cite these as fact.

- Whether the run-length penalty is cache-line amortization or prefetcher
  engagement. The cell-size sweep proves run length is the variable and rules out
  per-cell loop setup, but cannot separate those two mechanisms. The lever is the
  same either way.

- Debug build slowdown magnitude for non-inlined `Fixed` operators.
- WASM's slowdown factor versus native.
- Cloud egress cost figures. The 810 TB/month arithmetic from 2.5 Gbit/s
  sustained checks out; the pricing does not.
- Checkpoint amplification ratios are arithmetic from assumed inputs, not
  measurements.
- That the accumulator mechanism belongs to the Torque/TNL lineage. Recollection
  only; that source has not been read.

- What other engines do about tick pacing, in §Pacing the loop: the clamped
  accumulator, Eve Online dilating simulated time under load, servers dropping
  ticks and saying so. Recollection, not read back against a source. The
  mechanisms are sound whether or not the attributions are exact, but do not
  cite the attributions.
- Whether rebuild or incremental maintenance of the cell ordering is faster under
  this load.
