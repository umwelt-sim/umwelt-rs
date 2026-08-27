# 0003 — Ad hoc entity migration between regions

Status: Accepted, 2026-08-27. Nothing to build in the library.
How the edge learns the new ids changed with `docs/adr/0004`: they arrive as
region events rather than as a reply, which does not change the sequence or its
ordering.

## Context

An entity has to be able to move from one region to another. Two cases, and
they want different things.

**Ad hoc.** A player walks through a door into a dungeon instance, takes a
portal, or is teleported by a script. The destination is wherever the game says,
and it is usually nowhere near the origin. The player expects a transition and
would not notice a short gap if there were one. This record covers that case.

**Seamless.** A player walks across the boundary between two adjacent regions
and must not be able to tell. That needs boundary replication so the view does
not pop at the border, a stable identity so nobody's ghosts churn, and
migration of the client's ghost set so the destination does not re-send
everything. It is a separate record and is not this one.

The two are separable because a dungeon has no border to see across. Building
the ad hoc case first covers instances, and it turns out to cost nothing.

`docs/adr/0001` matters here in a way that was not obvious before it landed. An
edge holds one connection and reaches every region through wildcard
subscriptions taken at startup, so the destination region is already reachable
before anyone decides to use it. Over the transport this replaced, the same move
would have meant a connect, a handshake and a wait for viewer registration.

The map of regions is the game's, maintained out of band. umwelt does not know
which regions exist, where they are, or which is next to which, and this record
does not change that.

## Decision

**Ad hoc migration is a sequence an edge performs. It needs no new messages,
and none are added.**

The edge already holds everything required:

1. `spawn` the entity in the destination region, at the position the game chose.
2. Wait for the destination's add event, which carries the new entity id and
   echoes the token the spawn was sent with.
3. `despawn` the entity in the origin region.

**Spawn first, despawn second.** Ordered that way, a failure at any point leaves
the entity somewhere. The reverse order has a window where it exists nowhere,
and there is no reason to accept it.

**The entity gets a new id, and that is correct here.** Ids are unique within a
region, so the destination allocates its own. Other players in the origin see
the entity despawn, which is what happened. Other players in the destination see
it appear, which is also what happened. The migrating player is transitioning and
is not watching for continuity.

This is precisely where the ad hoc case diverges from the seamless one. Crossing
a field boundary with a despawn and a spawn would make every bystander drop the
entity and re-add it, which is a visible flicker for people who did not move. A
stable identity is what fixes that, and it belongs to the record that needs it.

**The consumer decides everything about where and whether.** Which region, what
position, and when. It also carries the entity's own state across, since
`DESIGN.md` says game state that is not position is the consumer's, keyed by
`EntityId` — and the id changes, so the consumer rekeys it using what
`Incoming::Spawned` returned.

**The edge drives it, not a region.** The edge holds the client's connection and
can already reach both regions. A region driving it would need a region-to-region
protocol, which does not exist and which this case does not justify.

## Consequences

**The gap is about one tick.** The destination registers viewers between ticks,
so the first payload arrives within a tick of the spawn reply, plus a round trip
through the broker. Tens of milliseconds, against the seconds a connect and
handshake would have cost.

**A crash between the two steps leaves the entity in both regions.** The edge
spawns in the destination, dies, and never despawns in the origin. The origin's
copy survives until that edge is expired for silence, at which point everything
it managed is despawned. The window is bounded by the timeout the region was
built with, and it needs no other machinery.

**Nothing in the library changes.** No message, no method, no field. An ADR that
adds nothing is worth writing down, because the alternative is somebody deciding
later that migration obviously needs a `MigrateEntity` command and building one.

**No helper is provided either.** A `migrate` call would have to wait for the
destination's add event, and events reach the edge through the same channel as
payloads, so waiting inside the library would mean consuming messages meant for
the edge's own loop. The edge already tracks which of its clients are
mid-transition, and the wait belongs with that.

## Open questions

**Whether the seamless case reuses any of this.** It probably does not: the
sequence here works because a new identity is acceptable, and that is the one
thing seamless crossing cannot have.

**What an edge does if the destination never answers.** Today it would wait
forever, holding a client mid-transition. A timeout belongs in the edge server,
which is not built, and the right length depends on the deployment.
