# 0010 — World coordinates at the edge, and seamless crossing

Status: Proposed, 2026-09-07.
Refines `docs/adr/0003` (the seamless case it deferred), `docs/adr/0005` (the
edge holds the map, the region holds its box) and `docs/adr/0008` (the teleport
sequence).

## Context

Everything built is region-local. A `Pos3` is an `i32` with 10 fractional
bits; the rest is a position inside a region whose size the world config sets,
22 bits at the default of 4096 m. A region refuses a spawn or a move outside
its box. Entity ids are unique within a region. An observer is served by the
region holding it and by no other. Two regions on one broker are two islands,
and a consumer that runs two today runs a client for each.

`docs/adr/0003` listed three things a seamless crossing needs and deferred all
of them: a view that does not pop at the border, an identity that survives the
crossing, and a ghost set that moves with the viewer so the destination does
not re-send everything. `docs/adr/0008` built teleport on spawn and despawn. A
teleported entity gets a new id, and every bystander sees a despawn and an
arrival. That is acceptable for a portal. `0003` said a field boundary
cannot have that flicker.

Each tier has one consumer implementation: `Game` in the region, `EdgeGame` at
the edge, `ClientGame` at the client. `docs/adr/0005` gives each tier the
facts it is the authority on: the region its box, the edge its clients. A map
of which region sits where is neither. It is a fact about the deployment, and
it changes when the deployment does. A region started to hold a lobby, an
arena, a dungeon or a raid runs for as long as that lasts and is on no map at
all. A region that carried its own placement would hold a fact it is not the
authority on, and a pocket universe would have to carry a false one.

`docs/adr/0005` already places "the map of regions, and each region's origin
in the game's world frame" at the edge, and says the map is the game's. Both
stand. This record adds the map's form, the key it is served from, and which
library half reads it.

A client sends intent, the region's game resolves every position, and no
message a client can send moves an entity further than the game allows. A
client that asserts positions through the edge is a shortcut for a load
generator, and nothing here is built for it.

A client today has to name the region it spawns into, because nothing else
can choose one for it.

### Measurements

Measured, in the whole-pipeline benchmark in `DESIGN.md`: 8,192 entities in
one cell, every one of them a viewer, cost 27% of a 50 ms tick on eight cores
of an M1. A second region exists because of that ceiling, so a seam must not
add much to the cost of the regions on either side of it.

Computed, from the default config: the band within one view radius (256 m) of
a seam is 23% of a 4096 m region's area, and the corner bands, within a view
radius of two seams at once, are 1.6%. Those are the shares of a uniformly
spread population that a seam touches.

## Decision

**The deployment owns the map and serves it on the bus. The edge reads it,
owns the world coordinate space, and performs every crossing. A region knows
nothing of either. A client speaks world coordinates and never chooses a
region, except to enter one the map does not join.**

Within the edge, the library's `EdgeServer` does all of it. `EdgeGame` gains
nothing and keeps the veto it already has for teleport. A bare edge with no
consumer code carries players across seams, which the README promises of an
unmodified edge.

![Two placed regions on a dotted placement grid, an entity in A whose view
radius crosses the seam, a shadow viewer on the seam in B, the edge below
holding the map and receiving state and collision events, and the game client
below that drawing one world](../../assets/diagram/seam.svg)

### Roles

| | region | edge | client | deployment |
|---|---|---|---|---|
| library half | `RegionServer`, `WorldSimulation` | `EdgeServer` | `EdgeClient` | `WorldMap` |
| consumer half | `Game` | `EdgeGame` | `ClientGame` | whatever decides where regions go, writing one key |
| coordinates it sees | local `Pos3` only | both, and translates inbound | `WorldPos` only | placements, never a position |
| knows of the map | nothing | all of it, from the bus | nothing | authors it |
| resolves movement | its `Game`, where the world is | never | never | never |
| decides a crossing | never; reports a boundary collision | always, in response to a boundary collision | never | never |
| what a seam costs it | serving shadows | one event per cell a seam-side viewer crosses | one add per record | nothing per tick |

**Region.** Local positions in, local positions out. No accessor on
`RegionServer`, `WorldSimulation`, `Step` or `Game` returns a world coordinate
or a placement. A region cannot tell whether it is on the map; the same binary
serves a field in the overworld and a raid nobody can walk to. Movement is
resolved in the region, because the world and the tick are there: a `Game`
walks its entities, collides them with terrain and with each other, and stops
them at cliffs, as it does today. The region gains a way to report that an
entity or a viewer's view radius reached its boundary, without knowing what
is beyond it.

A `Step::move_to` whose target lies outside the box clamps the entity to the
box and reports a boundary collision to the owning edge, carrying the target.
The target is the argument: the position the game handed to `move_to`, or the
current position plus the delta handed to `translate`, in region coordinates,
negative or past the box. A game that clamps before calling
`move_to` today, as the debug assertion there requires, hands over the raw
target instead. The entity has not left. It stands at the box until an edge
moves it, or for as long as the game keeps walking it into a wall.

A `Game` whose own messages carry positions makes them relative to an entity
it holds; an action at the sender's own position needs no coordinate. The
library does not translate the consumer's bytes (`docs/adr/0006`).

**Deployment.** Whatever starts regions and decides where they go writes the
`WorldMap` to one key in a JetStream key-value bucket on the same broker.
Placing a region and starting it are one decision, so the map's owner is
whoever makes it: in a game that starts raids on demand, the game's backend;
in a static world, an operator with a file and the `nats` command line. The
library supplies the map's form, its encoding, a writer, and the edge's
reader. It supplies no process. Nothing in a region reads or writes the key.
No tier owns the map. `docs/adr/0002` found no need for a control plane; the
map is the one fact that would have belonged to one, and it is a key rather
than a service.

**Edge.** `EdgeServer` reads the map once at startup, keeps it for its
lifetime, and derives adjacency from it. It translates every inbound world
position to a region and a local position. A position no placed region covers
is dropped and counted, as a bad move is today. It spawns, moves and despawns
shadows in response to view collisions, and on a boundary collision it
teleports the entity across the seam with the teleport from `docs/adr/0008`.
All of that is inside the library. `EdgeGame` gains no method and no
obligation: `teleporting` fires for a crossing as for any teleport, with the
veto and the `Carry` it already has, and `teleport_arrived` fires when the
entity is across; both are defaulted, and an `EdgeGame` that implements
neither still crosses seams. Both report `at` as a `WorldPos`. The edge is the
tier that sees both frames; its consumer sees the world frame.

**Client.** `ClientHandle::spawn`, `move_entity` and `teleport` take a
`WorldPos` and no `RegionId`. `TickObservation::updates` yields a `WorldPos`,
translated by `EdgeClient` from the frame the edge sends the client for each
region it is registered in, and names the entity by the 64-bit name the edge
assigned and the region wrote into the record, never by a region's id. No
region-local position and no region-local id reaches a `ClientGame`, its own
entity's included. `observed` still says which region served the packet,
because a game holding one entity sees two packets a tick near a seam, one
from its region and one from its shadow's. It keys what it draws by the name,
and an entity it holds from both regions is one entity. A crossing reaches it
as `spawned` with the new region, then `teleported`, as a requested teleport
does. `ClientGame` gains nothing.

### The map

A `WorldMap` is a set of placements, each a `RegionId` and two integers naming
a square on a grid of region-sized squares. A square holds at most one region
and a region appears at most once. In this record "cell" means only the
subscription grid inside a region. The map does not carry the region size. That
is world config, every region announces it, and the edge already refuses a
region whose `protocol_hash` differs from its neighbors'.

A region fills its whole square, so placements do not overlap and no side is
split. Each of a region's four sides faces exactly one square, so it faces one
neighbor or none, and every boundary coordinate along that side is adjacent to
the same region. A boundary collision or a view collision identifies its
neighbor by the side it happened on, and the edge never has to decide which of
two regions a position on a side belongs to. Crossing a side changes one axis
by exactly the region size and leaves the other unchanged. A corner is the
only place two sides meet, and it touches three squares: the two across the
sides and one across the diagonal.

It is a list of placements, not a list of which regions touch. Adjacency
follows from placements, and the edge needs more than adjacency: it needs a
frame, so that a world position translates into the neighbor's local one and a
shadow lands where its owner stands. An adjacency list with sides is a
placement list in another form, and one that can contradict itself.

A seam is the whole side two placed regions share. The map has no gates. Where
a player may cross, such as a road through a cliff wall, is terrain, and
the library has none. That restriction lives where the game resolves movement,
in the region's `Game`: an entity stopped by a cliff never reaches the box, and
one on the road walks out of it. There is no second path; a position asserted
through `ClientHandle::move_entity` reaches the same `move_to` and the same
terrain.

The map lives at the key `map` in a JetStream key-value bucket named `umwelt`,
on the broker every tier already connects to. An edge reads the key when it
starts and keeps what it read, so every edge that starts sees the same map
whenever it starts, and the bucket's revision is the map's version. Nothing
watches the key at runtime. Regions started on demand are not on the map and
never change it, so the map changes only when the world's layout does, and a
changed map reaches the edges by restarting them, which `docs/adr/0006` made
cheap by making an edge disposable. Whoever places a region writes the whole
map.

The value at the key is a plain array of bytes, encoded the way every other
wire struct in this crate is: fixed-width fields, little-endian, no JSON and
no serde. Four bytes of format version, a `u32` holding 1, then one 12-byte
record per placement: the `RegionId` as a `u32`, then the square's column and
row as two `i32`. The length is therefore 4 plus 12 times the number of
placements, and nothing else is in the value: no count, no name, no envelope.
`WorldMap::encode` and `WorldMap::decode` in the crate are the only encoder
and decoder, and the deployment's writer and the edge's reader both go
through them. A value of the wrong length, an unknown version, or a region or
square that appears twice is rejected whole and counted, and the edge starts
with an empty map. Computed, a map of 1,000 regions is 12,004 bytes, and the
1 MB default limit on a JetStream value holds about 87,000 placements.

`docs/adr/0001` kept state payloads off JetStream because persistence would be
paid for a guarantee the design does not want. The map wants that guarantee,
and it is the first thing on the bus that does. State stays on core NATS. A
deployment enables JetStream on the broker it already runs, `nats-server -js`,
and runs no new process. The client for it ships in the `async-nats` version
this crate already depends on, so no dependency is added.

An edge that finds no bucket or no key starts with an empty map and treats
every region as an island until it is restarted. An edge that loses its
broker's JetStream after starting is unaffected, since it has already read
the map.

A map naming a region no heartbeat has come from is kept; that region is a
wall until it is heard from, the same as a region that is down. A map that
moves a placed region is a deployment mistake, because every world position
inside that region moves with it, and the edges that read it will disagree
with the edges that read the old one until all have been restarted.

### World coordinates

`WorldPos` is three `i64` with the same 10 fractional bits as `Fixed`. `x` and
`y` are the placement times the region size plus the local position; `z`
passes through, since placed regions tile a plane and do not stack. Computed:
at the default region size of 4096 m, 512 regions along one axis exhaust an
`i32`. A region size that is a power of two, which the builder already
enforces, makes the translation a shift and a subtract.

The wire changes in one place. The entity field of a record and of a despawn
is the entity's 64-bit name rather than the region's 32-bit id, so a record is
18 bytes at the default config instead of 14, and a default packet holds 65
records instead of 84, computed. Positions stay local on the wire and are
rebuilt into world coordinates where the record is read. This closes the
`DESIGN.md` open item that a global space would need `i64`: it does, but only
off the wire.

### One name per entity

The mapping between a region's ids and the name a client sees is done by the
edge and by nothing else, and its purpose is to hide the regions from the
client. The edge already assigns that name: it is the `EntityKey` the edge
mints at spawn, sends to the region as the token, and keeps across a teleport
(`docs/adr/0008`), and the edge already holds the only table from names to
region ids. What changes is that the name reaches the client's wire. The
region keeps the token per entity, eight bytes it does not read, and writes
it into each record and each despawn in place of the region's id. The region
maps nothing; it echoes what the edge told it.

An entity the region's game spawned has no edge and no token. The region
names it by its own `RegionId` and the entity's id composed into one `u64`
with the top bit clear. It never leaves its region, so that name is stable
for its whole life. An edge's keys carry the top bit set, so the two kinds of
name cannot collide. A region's id has to fit 31 bits for that: `RegionId` is a
`u32`, and its top bit is reserved. The key is minted from a per-edge counter
today; its high half now carries 31 bits of the edge's random name under the
top bit, and two edges mint disjoint keys.

A client holds no table. `EdgeClient` keeps only which regions are currently
sending a name, so that a despawn from one region for a name another region
is still sending, which is what a crossing looks like from beside the seam,
is not reported as the entity going away. A pocket universe needs nothing
more: its entities carry the same kinds of name, and nothing crosses out of
it.

The translation is done in the client's library half, not at the edge.
`docs/adr/0006` decided the edge relays a packet without decoding it, and it
could not rewrite one anyway: a record's position field is sized for the
region, and a world coordinate does not fit. One add per record at every
client is also the cheapest place to do it. It is not done in the region
because that would put world coordinates on the wire and widen every record.

### Regions off the map

A lobby, an arena, a dungeon, a raid: a pocket universe, started for a purpose
and stopped after, with no neighbors. Such a region is never placed. Nothing
marks it except its absence from the map. Every region is off the map until a
map says otherwise.

An unplaced region is its own frame. A `WorldPos` in it is its local position;
the client still speaks world coordinates and the translation is the identity.
It has no seams: every side of its box is a wall, no shadow is spawned in it or
from it, and no move crosses out of it. Its one door is
`ClientHandle::teleport_into(handle, region, at)`, which is `docs/adr/0008`'s
teleport naming a region, with `at` in the destination's frame: the map's if
the region is placed, its own if not. `teleport(handle, at)` without a region
resolves through the map and is refused for a position the map does not cover.
How a client learns which instance to enter is the game's, in its own
messages, as the map of regions was the game's before this record.

### Seeing across a seam: shadows

An observer within one horizontal view radius of a seam has a **shadow** in
the region across it: a second viewer, so the client is served the neighbor's
border strip as well as its own region. The edge spawns, moves and despawns it
with the calls it already has. It is an ordinary entity in every respect but
one: its role keeps it out of the snapshot, so nothing gathers it and nobody is
told about it. `EntityKind` holds its role in a byte with two values today.
The shadow is a third value of that byte and one check in
`CellSnapshot::update`, next to the check that skips a dead slot.

A plain observer with a tag the game agrees not to draw would need no library
change, but it would be in the snapshot. On a seam with a crowd on both sides,
every viewer near it would hold each neighbor twice, once as the entity and
once as its phantom, and spend half its ghost set and half its packet on
records it will not draw. The role avoids that.

The edge places a shadow inside the neighbor's box, at the point of the box
nearest the owner's world position, and moves it along the seam as the owner
moves, so the two travel together with one in each region. The shadow is a
viewer with the full view radius, so nothing the owner should see is lost as
it approaches. At distance d from the seam the owner's own circle reaches a
view radius less d into the neighbor; the shadow reaches a full view radius in
from the seam, which contains that at every d and equals it at the crossing.
The cost is over-delivery: the shadow serves entities the owner cannot see
yet, up to a view radius deep when the owner is still a view radius away, and
it ranks the neighbor's entities by distance from the seam rather than from
the owner, so depth is under-penalized by a term that grows with d. Both are
bounded, both stay in the shadow's own packet and ghost set rather than
competing with the owner's, and neither removes anything. No position outside
a box exists anywhere; the bounds check and the subscription do not change.

The edge learns when to spawn, move and despawn a shadow from the region,
which reports a viewer's view reaching its boundary the way it reports an
entity reaching it. A viewer's subscription is rebuilt only when the viewer
crosses a cell, and the box it builds is already clamped to the region. When
the box it would have built reaches past the region, the view is at the
boundary. The region reports `ViewCollision { entity, position }` then, again
on each later cell change while that holds, and `ViewCleared { entity }` when
it stops. The edge spawns the shadow on the first, moves it on the rest, and
despawns it on the last. That is one compare on a path the region already
runs and already counts as `subs_changed`: nothing per tick, and nothing at
all for a viewer away from a seam. The shadow exists a full view radius before
the entity reaches the seam, so the client sees across the seam while walking
up to it, and the crossing finds the shadow already there.

Near a corner an observer has three shadows: two across the edges and one
across the diagonal. A shadow's packets reach the client as
`observed(handle, neighbor, ..)` under the owner's handle, so the client sees
one entity with two views and merges them. A shadow is registered with its
owner's `ClientLimits`. It is counted in `RegionLoad::viewers` and not in
`entities`.

### Crossing

A crossing is the teleport from `docs/adr/0008`, performed by the edge on its
own initiative between two placed regions that share a seam. The sequence is
unchanged: spawn in the destination, wait for the add, remap the key, despawn
from the origin. This record adds the trigger and the shadows on either side.

The edge is the only thing that decides a crossing, and it decides only in
response to a boundary collision the region reported, which is the entity
itself reaching the box. A view collision, which is the view radius reaching
the box, produces a shadow and never a crossing; a boundary collision produces
a crossing and never a shadow. A position a client
asserts through `move_entity` is translated by the edge into the frame of the
region the entity is in, inside the box or not, and forwarded. If it lies
outside, the region's own move path clamps and reports it; the region does not
know or care who asked.

**The region reports a boundary collision and decides nothing.** It does not
know
whether the far side of its box is a neighbor or the end of the world. It
clamps the entity and reports once, and reports again only after the entity
has been back inside the box, so an entity held against a wall by a game that
keeps walking it costs one event, not one per tick. The event carries the
entity and the target.

On a boundary collision the edge looks the target up in the map. If no placed
region is there, because that side is the end of the world or because the
region is a pocket universe, the event is dropped and counted, and the entity
stands where the box stopped it. Otherwise the edge teleports the entity to the
translated target. Two things differ from a requested teleport:

- The shadow the entity had in the destination is despawned once the entity
  is there, and a shadow is spawned for it in the origin, since it now stands
  in the origin's band from the other side.
- Entity messages sent during the transition are held with the moves
  `docs/adr/0008` already holds, in arrival order, and forwarded after the
  remap. Anything sent before the spawn travels ahead of it on the edge's
  command subject and reaches the origin's game, so nothing is lost or
  reordered. The hold lasts one transition and is bounded by the connection's
  flow-control window, the bound the stream already has.

Spawn first, despawn second, as `docs/adr/0003` decided: at every step the
entity exists somewhere, and an edge dying between the two leaves it in both
regions until each expires the edge.

**The ghost set does not migrate.** `docs/adr/0003`
listed migrating it so that the destination does not re-send everything. The
shadow already covers that: the client has held the destination's border strip
under the destination's own keys since it entered the band. The fresh viewer's
ghost table starts empty and re-sends that strip, computed at up to 256
records, three packets over three ticks, each a key the client already holds.
It costs bandwidth for three ticks and changes nothing the client sees.
Handing a ghost table from one viewer to another would save only that
bandwidth.

**The walk resumes on the far side because the edge carries the intent.**
Nothing of the region's crosses. `docs/adr/0005` puts input state at the edge,
and a heading is input state. A game whose region resolves movement from
intent may send that intent to the edge's game rather than to the entity; the
library does not require it, and this is the game's own state handled with
the callbacks it already has:
`EdgeGame::message_received` forwards it to the region with `send_to_region`
and remembers the last one per entity, a byte for a heading. On a crossing
`teleporting` returns it in `Carry`, and `teleport_arrived` sends it to the
destination, which walks the entity on its next tick. The region still takes
every step. A game that keeps nothing at the edge re-sends its intent from the
client on `teleported` and pays one more round trip of standing still.

**What a crossing costs the crosser** is the teleport's own latency, which
`docs/adr/0003` measured end to end at 10.5 to 22.9 ms with both regions on
one machine, plus the hop that carries the intent. Computed at the quality
harness's motion classes, that is under 4 cm of standing at the wall for a
walker at 1.5 m/s and under 70 cm for a vehicle at 30 m/s, inside the tick or
two a client draws behind. A client that reckons its own entity's motion
carries on through it as it does through a late packet.

**A bystander sees nothing.** The name it holds for the crosser is the same
in both regions, so the destination's first record for that name updates an
entity the bystander already draws, and the origin's despawn for a name the
destination is still sending is not reported. The edge assigned the name and
the region wrote the same one on both sides; nothing at the client maps
anything.

An unattended entity has no shadow and crosses by the same teleport under the
same key; a bystander sees nothing of that either.

### Protocol additions

On the bus: the key `map` in the key-value bucket `umwelt`, read by every
edge at startup. Region to edge: `Presence::BoundaryCollision { entity,
target }` when a move would leave the box, `Presence::ViewCollision { entity,
position }` when a viewer's view reaches the box and on each cell change while
it does, and `Presence::ViewCleared { entity }` when it no longer does.
`Presence` widens from 13 to 17 bytes, the width `BoundaryCollision` needs for
its `Pos3`, and every variant is written at that width. Edge to region:
nothing. In a packet: records and despawns carry the entity's
64-bit name in place of the region's id, and nothing else changes. Nothing in
a region's info reply or heartbeat changes, and nothing is added for the
teleport itself.

Edge to client: `Frame { region, placement }`, with no placement for a region
off the map, sent when the client is first registered in a region. Client to
edge: `Spawn`, `Move` and `Teleport` carry a `WorldPos`; `Teleport` names a
region only for `teleport_into`. A state packet from a region whose frame has
not yet reached the client is dropped; the next tick supersedes it.

### Not in this record

Region-owned entities crossing. An entity the `Game` spawned itself has no
owning edge to report a boundary collision to, so it stops at its box; an NPC
that walks the world is owned by a client or is a detached spawn at an edge,
because only an edge can address two regions. Server-initiated teleport, other
than the crossing a `Game`'s own walk into a boundary leads to. Migrating
anything a `Game` holds about an entity. An edge handing its clients to
another edge. Starting and stopping regions, which is the deployment's before
and after this record.

## Consequences

**The map has one owner, and it is neither tier.** The deployment writes it,
in a form the library defines, and the edge reads it. A deployment with seams
runs no new process; it enables JetStream on the broker it has and writes one
key. A deployment of islands and dungeons changes nothing.

**A region cannot tell whether it is placed.** Nothing in the region tier
gained a field, a message or a flag for it. The overworld and an instance are
the same binary with the same arguments; only the map differs. The rule in
`docs/adr/0003` and `docs/adr/0005` that the map is the game's stands. The
library now says what shape it has and where it is served.

**Crossing is internal to the edge's library half.** Reading the map,
translating coordinates, spawning, moving and despawning shadows, answering
view and boundary collisions, running the teleport, holding a client's
messages during it, and swapping the shadows afterward all happen inside
`EdgeServer`. A game developer building an edge is given no new callback, no
new type to hold, and nothing to call. The two callbacks that fire during a
crossing, `teleporting` and `teleport_arrived`, exist already, default to
doing nothing, and are the same hooks a requested teleport uses. The README's
promise that an unmodified edge relays and manages transitions now holds for
the seamless case as well as the teleport.

**`RegionId` leaves the client's inputs, except at a door.** A client spawns at
a world position and is told which region answered. It names a region only in
`teleport_into`, to enter a place no walk reaches. The README's line that the
game owns the real map of the universe was true of two islands. The map is the
deployment's, the edge applies it, and the client keeps track of which of its
handles is where.

**A seam costs both sides, and the cost is bounded by geometry.** A client in a
band receives two packets a tick, and one in a corner receives four. Computed
from the default config, a region serves about a quarter more viewers for the
bands of its neighbors that face it, if the population is uniform. A crowd on
a seam costs more, and a crowd away from one costs nothing. None of this is
measured. A load generator that walks its bots across a seam is what measures
it.

**A move out of the box is no longer refused.** It is applied as far as the box
allows, and the collision is reported. `docs/adr/0006` said a region applies a
move or refuses it; it now also clamps, in the one case where a refusal would
hide a boundary collision. Whether the move came from the game or through the
edge makes no difference to the region.

**Nothing new runs in the tick.** The region pays for seams with one compare
per move, which the debug build already makes; one compare when a viewer's
subscription is rebuilt, which happens only on a cell change and is already
counted; an event per collision of either kind, queued off the hot path like a
despawn; eight bytes per entity for a name it already receives or composes;
and four more bytes per record on the wire. Serving shadows is the seam's
cost, and it goes through the machinery every viewer already uses, bounded by
the geometry above. No per-viewer per-tick work is added. No tier does another
tier's job: the region reports its box and the world in it, the edge reads the
map and assigns names, and the client adds an offset.

**Nothing of the region's crosses but the entity.** `docs/adr/0005` asked
whether umwelt should carry the consumer's per-entity game state, and
`docs/adr/0008` answered with `Carry`, bytes the edge's game writes. The
seamless case needs no other answer, because the state that has to survive a
crossing is input state, which `docs/adr/0005` already puts at the edge. A
game that holds something in the region it wants on the other side has put
per-client state in the world tier, and this record does not carry it.

**Three roles in `EntityKind`.** Shadow is the library's: not constructible by
a consumer, never reported to one through `spawned` or `removed`, and skipped
by the snapshot as a dead slot is.

**`teleport` is the crossing.** One sequence serves the portal and the seam;
the seam adds a trigger and a shadow on each side. `teleport` through the map
by position, `teleport_into` by name into a region the map does not join, and
the edge's own teleport on a boundary collision all run it.

**One name per entity, everywhere.** A portal, a teleport into a raid, a
crossing and a plain spawn all show one name to everyone who can see the
entity. The edge assigns it and is the only tier that can map it to a
region's id, which is the table it already keeps. The price is four bytes per
record: 65 records in a default packet instead of 84, computed, so a full
ghost set refreshes over four packets instead of three. The alternative, a
mapping sent to the client once per first sighting, costs less bandwidth and
puts a table of region ids in the client, which is what this record refuses.

**What a consumer changes.** It starts its broker with JetStream and writes
its map to the bucket. It stops clamping at the box, since the library clamps
and reports instead. If its region resolves movement from intent, it sends
that intent to the edge's game, which forwards it and carries it across a
seam. It keys its world model by the key the observation now carries, and
anything it derives from that key survives a crossing. It makes any position
in its own messages relative to an entity. Its load generator walks bots
across a seam, so the seam has a measured cost.

## Open questions

**Whether the ghost set should migrate after all.** The re-send above is
computed, not measured. If a crowd on a seam makes it cost more than three
packets, a viewer that inherits another's ghost table is the fix, and it is a
library change this record chose not to make.

**Whether a game should learn that a boundary collision was ignored.** The
entity stands clamped at the box, which is right for an entity walking into a
wall. A game that resolves its own collision has hit a wall it did not know
about. Whether the region should be told, so the game can turn the entity
around, is not decided. Telling it is a fact about the map reaching the region,
which this record keeps out.

**A map change with clients connected.** A changed map reaches an edge by
restarting it, and `docs/adr/0005` already records that an edge's death takes
its clients' continuity with it. Whether a deployment can roll its edges to a
new map without dropping clients is `docs/adr/0005`'s open question about
handing clients between edges, and this record does not answer it.

**What a client running many observers does about shadows.** Each observer in
a band gets its own, so a client walking 256 observers along a seam costs the
neighbor 256 viewers. Nothing caps that beyond `ClientLimits`, and whether it
should is undecided.

**Whether an instance is named by `RegionId` or by something the game
chooses.** `teleport_into` takes a `RegionId` because that is what a heartbeat
carries and what an edge routes on. A game that starts a hundred raids and
hands each party a door may want to name them, and nothing here says how the
id reaches the client except the game's own messages.
