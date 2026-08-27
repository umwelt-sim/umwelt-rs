# 0004 — Region events, and entity ids on the wire

Status: Accepted, 2026-08-27. Supersedes the subject scheme in
`docs/adr/0001` and the reasoning about replies in `docs/adr/0003`.

## Context

`0001` gave an edge a `.reply` subject, on which a region answered a
`SpawnEntities` command with the ids it had allocated. Three things are wrong
with that.

**It reimplements request and reply.** `RegionClient::info` already uses
`client.request` and lets NATS carry the inbox and the correlation. The spawn
answer was a second mechanism for the same job, with its own subject and its own
matching problem, and nothing distinguished the two cases.

**A reply can only answer a question the edge asked.** A region despawns
entities for reasons of its own: the consumer's game kills one, or an edge is
expired and everything it managed is orphaned. `settle` drops the viewer and
tells nobody. The edge goes on sending moves for an entity that is gone, `apply`
refuses every one because ownership no longer matches, and that continues for as
long as the edge runs. Nothing reports it and nothing recovers.

**Spawning and despawning are things a region does, not things an edge is told.**
Addressing them to one edge says the region cares who was listening. It does
not.

Separately, a payload is addressed by `ViewerId`, so an edge keeps a viewer map
beside the entity map it already has for the same clients. `EdgeSink::send`
resolves viewer to avatar before it can route at all, so the entity id is in
hand at the moment of publishing.

## Decision

### Presence

`umwelt.{region}.edge.{edge}.presence` carries entities added and entities
removed. An add carries the entity id and a correlation token; a remove carries
the entity id. That covers both what an edge asked for and what it did not,
because a removal is a removal whatever caused it.

`{edge}` is the edge that **owns** the entity, which the region knows from
`Edges`. It is not who happens to be subscribed. Putting it in the subject is
what keeps the traffic from multiplying: an edge subscribes to
`umwelt.*.edge.{edge}.presence` once and receives only its own entities, from
every region, including regions that did not exist when it subscribed.

A region-wide subject would have cost every edge every other edge's events. At
100 regions and the 96 spawns a second the smoke test runs, that is 9,600 events
published a second; with 50 edges each taking all of them the broker would
deliver 480,000 a second, against a measured ceiling near 1.07 million for the
whole data plane. Half the broker spent delivering what almost every recipient
discards, growing as regions times edges.

Presence is on its own subject rather than sharing the state one so a subscriber
can take population changes without the state stream. An operator takes
`umwelt.*.edge.*.presence` and sees every change across every region without a
single position record. There is one such subscriber rather than one per edge,
so nothing multiplies.

It is not called `events`. §Events go through umwelt, not around it reserves
that word for death, chat, loot and damage, which are reliable and ordered and
go to a game client. Presence is neither of those things and travels a different
link.

### The correlation token

An edge chooses a token per spawn and the region echoes it in the add event,
without ever looking inside it. Opaque bytes, the way a credential was and the
way an event payload is under §Events go through umwelt, not around it.

It exists because the subject no longer says who asked. An edge that spawned
three avatars in one batch has no other way to tell which added entity belongs
to which of its game clients. In practice the token is the handle the edge
already holds for that client, so its own map is token to socket, and the event
fills in the entity id.

### Payloads carry an entity id

A payload names the avatar rather than the viewer. Four bytes either way, and
the edge then holds one map instead of two. `ViewerId` becomes region internal
and leaves the protocol entirely.

The two stay separate inside a region, and should. A `Viewer` is a subscription,
a ghost table, a packet budget and a despawn queue, so indexing viewers by
entity would make that array as long as the entity array — 63,712 entries
against 8,192 live entities in the ten-minute churn run of §Slot growth under
churn. They also have opposite reuse rules: an entity slot is never reclaimed
because a stale ghost would alias the next occupant, while a viewer id is reused
safely because a recycled viewer starts empty. Merging them would force the
entity rule on both.

### Edge names are per incarnation

An edge names itself afresh each time it starts — a shortcode, a hash, anything
short and unique. Edges are cattle. A crash boots every game client attached to
one, so the entities that incarnation managed should go, and the next
incarnation will have a different population anyway.

Nothing scopes permissions on the name, so it carries no structure. `docs/adr/0002`
grants by role rather than by instance, for the same reason the name is
disposable.

Under a stable name a restart inside the edge timeout is a leak: the region
keeps hearing commands under that name and never expires it, so the previous
incarnation's entities are never orphaned, are moved by nobody, and go on
costing snapshot and gather forever. The new incarnation cannot enumerate them,
because those additions happened before it subscribed and presence does not
replay. A fresh suffix makes the old name go silent, be expired, and have its
entities despawned by the mechanism that already exists.

**A reconnect is a new incarnation.** `async-nats` retries forever by default,
so a process partitioned from the broker comes back silently and carries on with
the entity ids it held. Those may be gone: if the partition outlasted the edge
timeout, the region expired that name and despawned everything under it, and the
edge would spend the rest of its life sending moves that are refused while its
still-connected clients receive nothing.

An edge must not lean on that. On losing the connection it takes a new name and
re-spawns its entities at their last known positions, which it has because it is
the side that supplies positions. It does not drop its game clients: a partition
between an edge and a broker is not their problem and forwarding it to them
would be. The clients see a gap and then continue.

The correlation token is what makes that work. The edge re-spawns with the same
tokens it used the first time, presence returns fresh entity ids carrying them,
and each rebinds to the right client with nothing to guess at.

If the partition was shorter than the edge timeout the old name has not been
expired, so its entities are still there while the new ones exist. Duplicates
for the rest of that timeout, and the edge cannot clear them: under its new name
it does not own them, and the ownership check will refuse. That is the same
window a crashing edge already leaves, reached another way.

This is also why there is no roster subject. An edge that has seen every
presence message can accumulate what it owns, and the case that accumulation
could not cover was a restart under a name that already owned things. That case
is now designed away rather than repaired.

### The subject scheme, revised

Every subject below the region takes the same shape, so a wildcard reads the
same way whichever leaf it ends in.

| subject | direction | carries |
|---|---|---|
| `umwelt.{region}.info` | request and reply | region id, versions, world parameters |
| `umwelt.{region}.edge.{edge}.state` | region to edge | one observer's packet, addressed by entity |
| `umwelt.{region}.edge.{edge}.presence` | region to anyone | entities added and removed |
| `umwelt.{region}.edge.{edge}.command` | edge to region | spawn, move, despawn |

What each side subscribes to:

| subscriber | subscription | receives |
|---|---|---|
| an edge | `umwelt.*.edge.{me}.state` | its observers' packets, from any region |
| an edge | `umwelt.*.edge.{me}.presence` | changes to its own entities, from any region |
| a region | `umwelt.{region}.edge.*.command` | commands from any of its edges |
| an operator | `umwelt.*.edge.*.presence` | every change everywhere, and no state |

An edge takes its two subscriptions at construction and never takes another,
whatever regions its clients later move to.

`umwelt.{region}.edge.{edge}.reply` is removed.

**A despawn record on the state subject is not a presence removal.** A despawn
record comes from `GhostTable::evict` and means one client should forget an
entity that left *its* interest set: the entity is alive, and a viewer crossing a
crowd generates dozens a second. It is per viewer, high frequency, and budgeted
to half a payload so a turnover cannot fill a packet. A presence removal means
the entity ceased to exist, and goes only to the edge that owned it. An entity
that genuinely despawns produces both, to different audiences.

## Consequences

**An edge learns about removals it did not cause.** That is new, and it is the
gap that motivated this. Whether an edge does anything useful with it is the
edge server's business.

**A presence message reaches one edge and any operator, and nothing else.** The
owner is in the subject, so the broker filters and a region publishes once
however many edges exist.

**A lost presence message leaks one entity until that edge cycles.** Core NATS
drops on slow consumers, and unlike a state packet there is no next one to
correct it: the region owns the entity, the edge does not know, and nobody moves
it. It is despawned when that incarnation goes away, which for cattle is soon.
Nothing repairs it before then, and nothing is built to.

**One inbox still carries state and presence.** They arrive on two subscriptions
and are handed to the edge through one channel, because an edge's loop wants
both. What is gone is the reason that was a problem: nothing in the
library waits inside that stream any more.

**`spawn` stays fire and forget.** It does not block for an answer, because
there is no answer — the ids arrive as events like any other population change.

## Open questions

**Whether removals should say why.** Despawned by the game, given back by the
edge, or orphaned when an edge was expired are different things, and an edge
might act differently on each. Not decided, because nothing consumes it yet.
