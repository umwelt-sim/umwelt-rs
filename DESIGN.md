# umwelt design

Interest management for real-time simulation servers. Rust. Solo project.

Companion project: `herd`, a minimal game used only to generate load. Repo name
only, never published to crates.io.

This document supersedes `chat_pre_context.md`, which contained three errors
serious enough to mislead: it placed per-client work on the edge tier, it
proposed starting as a single process, and it described a TRIBES priority
mechanism that is not in the paper. All three are corrected here.

---

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

### What umwelt stores

**Rule: umwelt stores only what umwelt's own code reads.**

Position is stored because the gather reads it. Liveness is stored because the
sort needs it. Accumulated displacement is stored for priority scoring, which is
not built, so it is the one field currently admitted on intent rather than on an
existing caller; see §Odometer. Health, inventory, and AI state never enter,
because nothing in the replication pipeline reads them.

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

- Priority scoring is the only thing that will read it, which is the test in
  §What umwelt stores. Scoring is not built, so this field currently passes that
  test on intent rather than on a caller.
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
set, which is the work umwelt exists to do and has been optimized to do quickly.
`Entity` and `Near` a consumer could plausibly build; `Observers` they could not.

Delivery machinery is umwelt's and is not small: sequence numbers, a sliding
window per connection, retransmit on loss, and ordered delivery. The TRIBES event
manager is a worked design for exactly this and the paper describes it.

**Ordering constraint.** An event naming an entity is meaningless to a client
that has not been told the entity exists. Spawn notifications and events on the
same entity must be ordered relative to each other. TRIBES gave ghost creation
the same guaranteed delivery path as events.

**The event reserve is currently a fixed cut and should be a floor.**
`state_budget_bytes` is computed once as `payload - header - reserve`, so 256 of
1,200 bytes are unavailable to state even when no events are pending. That is
over 20% of every packet held for something usually absent. It should be
reserved only against an actual backlog, with state taking the remainder
otherwise.

**Blocked on client registration.** `Entity(EntityId)` means "the client
controlling this entity" and nothing maps a connection to an avatar yet. That is
the same missing API the accumulator's per-client state needs, not a new one.

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

The packet figures below use constants that no longer live anywhere, since they
were on `ViewConfig`. They belong to packet assembly, which is not built. They
are kept here as the arithmetic, not as an API reference.

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
`odometer`, `ghost`, `select`, `budget`, `sim`. 170 library tests pass.

A tick runs end to end: the game moves entities, the odometer observes how far
they went, the snapshot is rebuilt in cell order, and every due viewer is
subscribed, gathered, scored and selected against it. It stops at a `Selection`
per viewer. Packet assembly, events and transport are not built.

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
  Unverified: no measurement of churn exists.

Displacement is `|dx| + |dy| + |dz|`. Computed: that over-estimates the
Euclidean step by up to 1.73x, direction-dependent, and needs no square root.
Under movement that changes direction it is noise rather than bias against any
one entity; a permanent diagonal is the case that would show it. Unmeasured.

The total wraps and the per-call step saturates. Opposite on purpose: a
saturating total would pin at `u32::MAX` and freeze that entity's score forever,
which is the starvation this exists to avoid, while a wrapping step would turn
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

**The mark advances on send, not on acknowledgement.** A lost packet therefore
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
synchronous callback, so a sink that blocks cannot be cancelled. What is done
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

Not built: a watchdog that fails the process loudly when a sink hangs rather
than silently missing every subsequent tick. That is a process-level policy and
belongs with `run`, which is also not built. Nothing currently defends against a
sink that blocks forever.

Not built: the shipped implementation that speaks the sim-to-edge protocol,
which is the one that would hand off to an I/O thread and drop rather than
queue.

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

Not built: `run` and its clock, threading, packet assembly, events.

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

Not built. This records the decision and the shape intended, not an
implementation.

Score is `drift x weight(distance band)`, where `drift` is the odometer
difference since this client was last told about the entity and `weight` is a
table indexed by `dist_sq.raw().ilog2()`. One multiply, no divide, no square
root, integer throughout, so replay stays bit-identical. Ties break on
`EntityId`.

The alternative was elapsed time since last send, which needs no per-entity
storage. Rejected because a still entity's score grows without bound under it.
Computed from the assumption that the curve is fair across candidates, and
unverified, idle entities would then take slots in proportion to their share of
the candidate set. Rejected also because the growth curve would be tuned against
a proxy with no error signal feeding back. See §Odometer.

### Decided provisionally: weight proportional to 1/d

`Weights::inverse_distance` halves the weight at twice the separation, anchored
at a one-meter separation. How wrong an entity looks falls off with distance,
since a meter of error subtends a smaller angle the further away it is, so a
packet is better spent on what is close.

Measured by the quality harness. **The population it ran against is invented
rather than taken from a real game**, so this settles open question 2 only
provisionally.

Computed: a `[u16; BANDS]` table spans 4096 to 1, and the default 256 m view
radius is eight doublings of distance from one meter, so the steepest curve it
can express across the whole view is about `d^-1.5`. Anything steeper flattens
partway out, which is why `d^-4` measured identically to flat weighting.

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
2. **The growth curve.** What function makes a distant entity update at a rate
   that looks right to a human rather than merely being cheap. Tuning against
   perception, not a derivation.
3. **Objective.** Minimizing mean client-side position error and bounding
   worst-case error for any single entity are different schemes.

### What the accumulator does not fix

It caps what is sent, not what is examined. If 5,000 entities pile into one cell,
every nearby viewer still touches all 5,000 to decide which ~58 fit. Gather cost
remains unbounded under crowding. If the benchmark shows that is fatal, the
answer is hierarchical: an aggregate representation per cell so a distant crowd
costs one record instead of thousands.

---

## Benchmark results

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

That is backwards from intuition and it is the layout working as designed. A hot
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

The library itself contains no threading. `CellSnapshot` and `CellOccupants` are
`Sync + Send`, asserted by test, and `gather_into` takes a caller-supplied
buffer, which is all a caller needs to fan out. The replication phase that spends
those threads belongs to `WorldSimulation` and is not built.

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
dense crowd stops being a special case. That is the point of the mechanism, not
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

**Not established.** What N should be. It has to leave the accumulator enough
candidates to choose among, and records per packet is itself arithmetic from
three unverified inputs. The 512 default is a placeholder. Nothing has measured
what a viewer notices when entities beyond the cap stop existing.

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

Mean angular error within 32 m, at a ghost cap of 512:

| curve | walkers only | with 5% vehicles |
|---|---|---|
| flat | 4.31 | 14.78 |
| 1/sqrt(d) | 2.81 | 10.35 |
| **1/d** | **2.51** | **9.71** |
| 1/d^2 | 2.44 | 9.46 |
| 1/d^4 | 3.93 | 13.72 |

One entity in twenty at vehicle speed roughly triples the error. Below that the
curve is cosmetic: every option leaves error small enough that no one would see
it. Above it the curve is worth 34%.

`1/d^2` is marginally better within 32 m and worse from 64 to 128 m. `1/d^4`
matches flat because the table cannot express it.

**Starvation is solved.** Entities that were candidates for a viewer and never
once sent: 2.9% at a ghost cap of 512 and 8.5% at 256, against the 66.5% the
static-priority scratch simulation starved. Candidate-ticks where the client
held no ghost at all: 3.5% and 11.1%.

**What the curve does not fix.** The 99th-percentile error is 2.0 m in every
configuration measured. That is the vehicles, and no weighting touches it. The
levers there are a larger packet or a higher send rate, not the curve.

Caveats. The harness is newer and less trusted than the library: four bugs were
found in it during its first run, each surfacing as an implausible number rather
than a failing test. Only one density and one motion mix have been run, and
viewers are all drawn from the walkers, so a viewer traveling at vehicle speed
is unmeasured and the grace period is barely exercised.

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

The per-tick work every row also pays, with no viewers registered, is 60 µs at
8,192 entities: 0.12% of a tick. Essentially all cost is per-viewer.

| scenario | viewers | tick | of a tick | per viewer |
|---|---|---|---|---|
| uniform | 1,000 | 2.51 ms | 5% | 2.45 µs |
| uniform | 8,192 | 20.55 ms | 41% | 2.50 µs |
| same world, nothing moving | 8,192 | 13.20 ms | 26% | 1.60 µs |
| clustered region | 1,000 | 6.81 ms | 14% | 6.75 µs |
| clustered region | 10,000 | 65.44 ms | 131% | 6.54 µs |
| town square | 2,048 | 11.23 ms | 22% | 5.45 µs |
| town square | 8,192 | 49.73 ms | 99% | 6.06 µs |

The clustered rows are the region-in-use case §Not yet benchmarked asked for:
50,000 entities, most cells sparse and eight dense ones holding sixty percent,
viewers drawn from the population so they land where the density is. It behaves
like a mild town square rather than like the uniform case.

**What the accumulator saves**, measured: the same world and the same viewers
cost 13.20 ms when nothing moves against 20.55 ms when everything does, and the
still case sends no records at all.

Every earlier figure for this pipeline was assembled by adding separately
measured stages, two of them by subtraction. Those estimates were 55% optimiztic
uniform and, before the fix below, wrong by 4.2x crowded.

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
an unseen entity scores finitely. Measured afterwards, every scenario reports no
arrivals and no departures at steady state, and the crowded cases run 1.7x to
2.5x faster:

| scenario | before | after |
|---|---|---|
| uniform, 8,192 | 20.58 ms | 20.55 ms |
| clustered, 10,000 | 114.10 ms | 65.44 ms |
| town square, 2,048 | 24.26 ms | 11.23 ms |
| town square, 8,192 | 123.84 ms | 49.73 ms |

Uniform is unchanged because the cap never bound there.

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

| threads | uniform | speedup | town square | speedup |
|---|---|---|---|---|
| 1 | 21.83 ms | 1.00x | 52.19 ms | 1.00x |
| 2 | 11.43 ms | 1.91x | 28.71 ms | 1.82x |
| 4 | 6.31 ms | 3.46x | 14.42 ms | 3.62x |
| 8 | 5.23 ms | 4.17x | 13.03 ms | 4.01x |

The town square goes from 104% of a 50 ms tick to 26%. Four to eight threads
buys 17% to 21%, which is what heterogeneous cores predict and why the count is
configurable.

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

| ghost cap | tick | per viewer |
|---|---|---|
| 64 | 17.74 ms | 2.16 µs |
| 128 | 28.27 ms | 3.44 µs |
| 256 | 48.21 ms | 5.88 µs |
| 512 | 96.26 ms | 11.75 µs |

Monotone and close to linear in the cap, which it was not before the fix. The
cap is a dial between cost and how many entities a client can see.

Selection keeps the nearest `ghost_cap` candidates and discards the rest
unscored, so a walk cap above the ghost cap is waste. At a ghost cap of 256:

| walk cap | candidates gathered | tick |
|---|---|---|
| 256 | 322.8 | 44.54 ms |
| 512 | 581.3 | 48.29 ms |
| 1,024 | 1,094.0 | 57.22 ms |

7.8% for matching them, and one fewer parameter. Safe because the cap is checked
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

Left: subscription churn rate and p99 tick duration.

### Not yet benchmarked

**Boundary churn.** ~~Measurable now without the accumulator.~~ Measured, and it
was a bug rather than a cost: see §Measured: choosing the ghost set by staleness
churns it. The prediction that a returning entity "looks maximally stale and
wins a slot immediately" was exactly right, and the pipeline benchmark is the
only thing that could see it.

What remains unmeasured is churn under a *moving* viewer. The pipeline benchmark
oscillates entities in place so a population shape survives thousands of
iterations, which holds distances nearly constant and understates how often
entities cross a view radius. A viewer walking through a crowd is the case that
would exercise `grace`, and nothing does yet.

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
6. `WorldSimulation` and the game hook trait (done); a minimal `herd` game step
   (movement, health; nothing else) is next
7. Bot harness with adversarial movement patterns
8. `SimulatorEdge` and the sim-to-edge protocol
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
  Word-level skipping in `LiveSet` would cut that to one test per 64 dead slots;
  not built, not measured.
- `CellSnapshot` has no id-to-slot lookup, so nothing can answer "where is entity
  N." Still needed to locate a client's own avatar and to route an event to a
  specific entity. The replication path no longer needs it: `DiscoveredEntity`
  carries the snapshot index and `CellSnapshot::pos_at` resolves it. The cheap
  fix for the remaining cases is one array of length n written during the
  scatter, where the writes are sequential.
- Per-client state (subscription, accumulator scores, send cadence) belongs to
  `WorldSimulation`. Client registration is the API that does not exist yet:
  something has to say a connection exists, name its avatar entity, and attach a
  the edge. Deferred deliberately; it belongs with the accumulator, since
  that is what gives per-client state its shape. Event delivery is blocked on the
  same API.
- **A ghost's mark advances on send rather than on acknowledgement**, so a lost
  packet leaves a client's copy of a since-idle entity permanently wrong. There
  is no protocol to acknowledge against yet. Fixing it needs a pending mark
  beside the acknowledged one, growing a ghost record from 12 bytes to 16.
- Viewers are not padded to a cache line, so two workers share the line at each
  chunk boundary. A viewer is written twice per tick, so the contention is
  2(N-1) lines against ~10,000 writes. Not measured.
- `WorldSimulation` has no `run`. The clock and the pinned thread belong with
  the edge work.
- `Mul`/`Div` rounding disagree for negative values.
- `protocol_hash` could send raw field values instead of a digest, which is certain rather
  than near-certain, and names the offending field.
- `CellList::push` panics on overflow. Unreachable through the public path, but it
  is a panic in a library.
- `MAX_CELL_RADIUS` of 4 sizes every `CellList` inline at 81 cells.
- Region-local coordinates only. A global coordinate space would need `i64`.
- British spellings in `config.rs` method names, 15 occurrences of
  `quantise`/`dequantise`.
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
- Whether rebuild or incremental maintenance of the cell ordering is faster under
  this load.
