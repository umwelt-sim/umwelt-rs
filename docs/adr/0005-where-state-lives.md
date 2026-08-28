# 0005 — Per-client state at the edge, world state in the region

Status: Accepted, 2026-08-27. Nothing to build in the library.
The rule is already followed by everything that is built. It is written down
because the edge server and automatic migration are where it would be broken,
and both are ahead.

## Context

Two tiers hold state, and they have different lifetimes.

A region owns a box of space. An entity's stay in one is temporary, and once
migration is automatic it is routine rather than exceptional: `docs/adr/0003`
measured the ad hoc case at 10.5 to 22.9 ms end to end, so nothing about a
crossing is expensive enough to be rare.

An edge owns a game client's connection. That connection outlives every region
the client's avatar passes through, by design — the client never learns which
region it is in, and `docs/adr/0001` made reaching a new region cost the edge
neither a connect nor a subscribe.

Nothing has forced the question yet. A `RegionClient` talks to one region at a
time in practice, and nothing migrates without an edge deciding to. Both change,
and the cost of having guessed wrong is asymmetric: state in the wrong tier has
to be migrated, and migration is where correctness goes.

Several decisions already taken are instances of a rule nobody has stated.
`ViewerId` never leaves the region. The correlation token on a `Spawn` is the
edge's own handle and the region echoes it without looking inside. An edge keys
by `(RegionId, EntityId)` because ids are unique only within a region. The map of
regions is the game's, held wherever the game holds it, and umwelt does not know
which regions exist.

**This is not in tension with §Why per-client work stays in the simulation.**
That section is about work, and it is about the world: subscribing, gathering,
scoring, selecting and assembling a packet all need the world's state, so they
belong where the world is. This record is about facts that describe a client
rather than the world. Neither needs the other, and putting each where its
inputs are is the same argument twice.

## Decision

**Per-client continuity state belongs to the edge. Per-entity world state
belongs to the region.**

The test, for a fact that is not obviously one or the other: if keeping it
correct across a crossing would mean migrating it, and it describes the client
rather than the world, it belongs at the edge, where nothing migrates.

The region holds the world:

- position, liveness and the slot, which are the same thing — an `EntityId` is
  an array index
- per-viewer replication state: the subscription box, the ghost table, the
  accumulator's scores, the packet budget
- which edge manages which entity
- the world config

The edge holds the client:

- the connection
- its own handle for that client, which is the token a `Spawn` carries
- `(RegionId, local id)` for each entity it manages, rekeyed when one crosses
- the map of regions, and each region's origin in the game's world frame
- input sequence numbers and anything else the client-facing protocol needs in
  order to stay continuous

**Where a region must hold something on a client's behalf, it holds bytes it
does not interpret.** The spawn token already works this way. If movement
resolution ever moves into the region, so that the edge can no longer know which
input an adjusted position reflects, the region carries an opaque `u32` and
copies it into the packet header. It stores and echoes; it does not read. That
keeps a region from acquiring knowledge of a protocol it is not party to.

**Per-viewer replication state is the deliberate exception, and it is not one.**
A ghost table describes what a client has been told about the world, not the
client. It is world state, it lives in the region, and it therefore has to
migrate when the viewer does. `docs/adr/0003` already names that as something the
seamless case needs.

## Consequences

**The edge server is constrained before it is written.** Input buffers,
prediction state, sequence numbers and client-facing identifiers stay at the
edge, however convenient it would be to push one of them down into a region that
is already tracking the entity.

**A region stays substitutable.** Everything it holds about an entity either
derives from the world or travels with the entity. Nothing about a connection
crosses, so a migration payload is position, ownership and whatever per-entity
game state the consumer keeps.

**An edge's death takes per-client continuity with it.** There is no other copy,
because that is the point. This is not new — an edge's death already despawns
what it managed — but it does mean a client reconnecting through a different edge
starts fresh rather than resuming. Nothing decides that this is acceptable; it
is recorded so nobody discovers it late.

**The rule says nothing about how much state either tier holds.** It says where
a given fact goes. A tier holding too much is a different problem.

## Open questions

**Whether umwelt should carry the consumer's per-entity game state at all.**
`DESIGN.md` says state that is not position is the consumer's, keyed by
`EntityId`, and `docs/adr/0003` says the consumer rekeys it when the id changes.
Whether the library should offer to move it is undecided, and this record does
not decide it.

**Whether an edge can hand its clients to another edge.** If per-client
continuity exists only at one edge, failover means losing it. A protocol for
handing clients over would change that, and none exists.

**Whether the region ever resolves movement.** Today `MoveEntities` is an
assertion: a region applies the position or refuses it, and never adjusts. If
that changes, the opaque echo above becomes necessary rather than hypothetical,
and its shape is not decided.
