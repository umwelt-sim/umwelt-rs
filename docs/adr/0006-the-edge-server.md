# 0006 — The edge server

Status: Accepted, 2026-08-27. Not built.
The heartbeat this tier publishes is `docs/adr/0007`.

## Context

`net::region` is built and carries a region's traffic to whatever relays for it.
Nothing relays. `RegionClient` is an edge's side of that link and knows nothing
about game clients, sockets or fan-out, and the tier that holds those does not
exist. `examples/herd-edge.rs` drives the region side of an edge with no clients
behind it, which is what has been standing in.

Seamless transition across a region boundary is deferred. It needs boundary
replication, an identity that survives a crossing, and a partition somebody
owns — see `docs/adr/0003`. Ad hoc migration is built and measured, so nothing
about the edge server waits on that work.

`docs/adr/0005` decided where state lives. This is the first thing built under
that rule, and most of the API follows from it: a fact about a connection stays
at the edge, and a fact about the world stays in the region.

## Decision

### Placement

`net::edge`, a peer of `net::region`, holding `EdgeServer`, `EdgeHandle` and the
client-facing protocol.

`Game` and `EdgeGame` move to the crate root, `umwelt::Game` and
`umwelt::EdgeGame`. They are the consumer's two extension points and belong
together, and neither belongs inside a networking module. There is no
`sim::Game` re-export; the crate is 0.0.0 and nothing outside it depends on the
old path.

### The client link is QUIC

Reliable ordered stream for spawn, despawn and the consumer's own messages.
Datagrams for the state stream down and moves up. Both of those are latest-only,
so a lost one is superseded within a tick, and a lost spawn is not recoverable by
anything.

Not TCP plus a second UDP socket: two handshakes, NAT traversal on the second,
and two congestion controllers over one path. QUIC carries both on one
connection.

This ends the deviation §The session records. State is latest-only, lossy and
unordered, and it now reaches the client that way. That the region-to-edge hop in
the middle is reliable is a property of a datacenter-internal link and costs
nothing measured.

**The region ships final packets and the edge relays them without decoding.**
Authoritative state does not lose authority by passing through a relay. The edge
strips the four-byte avatar it routed on and prepends the four-byte region, which
is not a size change and not a decode. Where the edge has something of its own to
say, it has its own channel to say it on.

### Three identifier spaces

| id | width | minted by | unique within | reused |
|---|---|---|---|---|
| client handle | u32 | the game client | one connection | the client's business |
| `EntityKey` | u64 | the edge | one edge | never |
| `EntityId` | u32 | the region | one region | never |

A client names entities by a handle it chose, so it can move one before the
region has confirmed it. The edge maps that to an `EntityKey`, which doubles as
the correlation token on `SpawnEntities` — there is no fourth space.

`ClientId` names one live connection and nothing else: not an account, not a
player, not a session surviving a reconnect. **It must not alias after reuse.**
A recycled `EdgeId` is safe because nothing outside a region holds one across the
gap; a `ClientId` is held freely by the consumer, in timers and its own tables,
and a recycled one would send one player's packets to another. Either never
reused, or index plus generation so a stale one fails to resolve.

### The client-facing protocol

Up: `Spawn { handle, position, kind }`, `Move { handle, position }`,
`Despawn { handle }`, `Message { bytes }`.

Down: `Spawned { handle, region, entity }`, `Removed { handle }`,
`State { region, packet }`, `Message { bytes }`.

umwelt owns the movement and lifetime vocabulary because that is what it
replicates. Everything else a game says to its clients is opaque bytes it hands
over and gets back, and umwelt does not read them.

A client keys its world model by `EntityId`, since the packet is relayed
untouched, and learns its own from `Spawned`.

### The API

`EdgeServer::new(nats, runtime, quic, game)`. The caller supplies a connected
NATS client, a Tokio handle and a bound QUIC endpoint, so credentials,
certificates and binding stay with the deployment. `game` is a closure taking an
`EdgeHandle`, which breaks the cycle between a server that owns the game and a
game that needs to send.

**The edge names itself.** `docs/adr/0004` makes a fresh name per incarnation a
correctness requirement — reuse it and the edge inherits entities a region still
thinks it owns and cannot enumerate. A correctness requirement is not the
consumer's to remember. There is no prefix parameter: if the library generates
the name, it generates the name.

`EdgeHandle` is cheap to clone and callable from anywhere, including timers and
tasks. It is deliberately not a capability object handed to a callback. `Step`
earns that shape because spawning is only valid inside `Game::step`; nothing on
the edge is moment-scoped, and an edge that can only send from inside a callback
forces a consumer to queue work until some unrelated event fires.

It offers `spawn`, `spawn_many`, `spawn_detached`, `move_entity`,
`move_entities`, `despawn`, `despawn_many`, `send`, `send_datagram`,
`send_to_entity`, `disconnect`, and the mapping — `client_of`, `entities_of`,
`key_of`, `entity_id`. `move_entities` groups by region and chunks to the message
cap here rather than at the call site.

`EdgeGame` is five callbacks, every one defaulted to nothing: `connected`,
`disconnected`, `spawned`, `removed`, `message`. They are called on the I/O path,
serialized, and must not block, on the same terms as `PayloadSink`.

**No associated per-client type.** It was considered so that the library could
drop a consumer's state and remove the disconnect callback entirely. Associated
type defaults are unstable, so every consumer would write one whether or not it
had per-client state, and `EdgeServer` would become generic in every signature
that names it. What it protects against is already inert; see below.

### Rules

**Spawn before move.** A move naming a handle this connection did not spawn is
dropped and counted. That is per-connection and is a different question from the
region's per-edge ownership check. Both are consistency checks, not
authorization.

**A move on an unconfirmed key holds the latest position** and sends it when the
region reports the id. At most one is held per key, because it is latest-only.

**A stale key is not an error.** Removal arrives unprompted — a region's game can
despawn anything — so acting on an entity that has just gone is a race rather
than a mistake. It is dropped and counted, the way a region already treats a
stale id. The only `Err` is a transport failure.

**Disconnect order.** Everything the client owned is despawned, `removed` fires
per entity as regions confirm, and `disconnected` fires last. When it fires, that
`ClientId` owns nothing and nothing further about it will be delivered. A
developer cleaning up out of habit finds `entities_of` empty and `despawn` a
no-op, which is why the callback needs no guard beyond this ordering.

Entities from `spawn_detached` have no client and are not swept. Cleaning them up
is the consumer's, which is what calling the detached version asked for.

**The client-to-entity mapping is maintained by the library**, from the
connection a command arrived on. That is what makes `send_to_entity` and
`client_of` possible without the consumer keeping a map.

### Not in this record

Migration, whether ad hoc or automatic. Input sequence numbers and client
prediction. Any movement resolution in the region — `MoveEntities` remains an
assertion the region applies or refuses. Edge-side interest management. Any
authorization beyond what QUIC's TLS gives, on the same terms as
`docs/adr/0002`. What the edge publishes about itself, which is
`docs/adr/0007`.

## Consequences

**A new dependency.** `quinn`, and the certificate handling that comes with it.
The crate's first dependency that is not a consequence of `docs/adr/0001`.

**An opaque edge name cannot be placed from inside umwelt.** A region's edge list
becomes names with no host or binary in them, and `docs/adr/0007` keeps the
address out of the heartbeat as well. Correlating a name to a process is the
operator's own logging or orchestrator, and nothing here helps.

**The consumer's simplest edge does nothing.** A game whose clients spawn, move
and despawn needs no `EdgeGame` method at all: the library runs the whole loop.
`ClientId` and `EntityKey` surface only when the consumer originates actions.

**The edge is where the client protocol's cost lands.** Serializing callbacks on
the I/O path is the simple choice and it is a per-connection bottleneck if a
consumer does real work in `message`. Nothing here measures that.

## Open questions

**How the client-facing protocol is versioned.** The region link checks
`PROTOCOL_VERSION` for exact equality because region and edge deploy together and
a skew is a deployment mistake. Game clients run on someone else's machine on
someone else's schedule, so exact match is the wrong rule. Deferred to its own
record rather than guessed at here.

**What a client is told when its spawn is refused.** A region refuses a spawn
outside its bounds, and today nothing reports that to anyone. The edge would have
to synthesize a failure for the handle, and the shape of that is not decided.

**Whether serialized callbacks hold.** One connection doing slow work in
`message` stalls the others. A per-connection task would fix it and would make
`&mut self` on `EdgeGame` impossible. Not decided, because nothing has measured
it.
