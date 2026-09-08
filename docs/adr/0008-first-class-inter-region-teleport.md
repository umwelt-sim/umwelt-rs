# 0008 — First-class inter-region teleport

Status: Accepted, 2026-09-08. Proposed 2026-08-31.
Refines `docs/adr/0003`.

## Context

ADR 0003 decided that ad hoc migration is three steps the edge performs:
spawn in the destination, wait for the id, despawn from the origin. It
provided no library helper because the edge server didn't exist yet and
the wait would have meant consuming messages from someone else's event loop.

ADR 0006 built the edge server. The edge now owns the client connection, the
handle-to-id mapping, and the event loop. The wait is no longer a problem.

Every consumer reimplements the same three steps. The steps are always
identical — only the destination, position, and game state vary. The id swap
is error-prone: the consumer has to rekey its own state tables and the game
client has to update every reference. Both are avoidable because the
`EntityHandle` and `EntityKey` already survive migration. Only `EntityId`
changes, and that remap is the edge's job.

ADR 0005 left an open question about whether the library should carry game
state during migration. This answers it for the ad hoc case.

## Decision

The library provides a teleport operation. The client asks, the edge executes,
the region doesn't know it happened.

### Client

`ClientHandle` gains `teleport(handle, region, position)`. It sends a message
to the edge and returns immediately. The handle stays valid — moves sent
during the transition are held at the edge and forwarded when the destination
confirms, same as moves on an unconfirmed spawn (ADR 0006).

`ClientGame` gains two callbacks:

- `teleported(handle, region)` — the entity arrived. Same handle the client
  has always held.
- `teleport_failed(handle, region)` — destination unreachable, spawn refused,
  or the edge's game code denied the request. The entity stays in the origin.

The client learns the new `EntityId` through the existing `spawned` callback,
which fires before `teleported`.

### Edge

The edge receives the teleport request and runs the ADR 0003 sequence:

1. Call `EdgeGame::teleporting`. The consumer can attach game state (opaque
   bytes) or deny the request.
2. Spawn in the destination, carrying the game state as the token.
3. Wait for the destination's add event.
4. Remap the `EntityKey` to the new `(RegionId, EntityId)`.
5. Despawn from the origin.
6. Send `Teleported` to the client.

Spawn first, despawn second. Same crash semantics as ADR 0003: if the edge
dies between steps 2 and 5, the origin's copy survives until the edge is
expired for silence.

`EdgeGame::teleporting` defaults to allowing with no state. A consumer that
needs to transfer inventory or health serializes it here. The destination's
`EdgeGame::spawned` receives the same bytes as the echoed token.

The normal `removed` callback does not fire for the departing entity — it
wasn't removed, it moved. `spawned` fires on the destination side as usual.
`teleported` follows it so the client can tell this apart from a fresh spawn.

### Region

No change. A region sees a spawn and a despawn. It doesn't know a teleport
happened. State transfer rides the spawn token. The region holds bytes it
doesn't interpret, same rule as ADR 0005.

### Protocol additions

Client to edge: `Teleport { handle, region, position }`.
Edge to client: `Teleported { handle, region }`, `TeleportFailed { handle, region }`.

No new region-to-edge messages. The edge uses existing `SpawnEntities` and
`DespawnEntities`.

## Consequences

`ClientHandle::migrate` is removed. It exists because the edge couldn't
perform the wait, and that is no longer true. It returns a new handle,
forcing the client to manage the swap — the opposite of what `teleport`
provides. `teleport` supersedes it.

The simplest teleport needs no consumer code beyond calling `teleport` and
implementing the callback. The three-step sequence, the id swap, and the wait
are the library's problem.

A consumer that carries game state implements `teleporting` to serialize and
handles `spawned` on the destination edge to deserialize. One callback each
direction.

Moves during teleport are held, not dropped. Since `docs/adr/0010` the hold
is in world coordinates, latest move only, with entity messages queued in
order beside it; the remap forwards both to the destination, and a held move
that does not fit the destination is dropped rather than clamped to its box.

The manual sequence still works. A game that needs a staged handoff or a
round trip to a database uses the spawn/despawn primitives directly. The
teleport operation covers the common case.

ADR 0003 stays as written. ADR 0005's open question about carrying game state
is answered for the ad hoc case. Seamless migration may need a different
answer.

## Open questions

**Timeout.** Decided 2026-09-08, with `docs/adr/0010`, because a crossing
holds everything sent to an entity in transit and so needs a bound. The edge
gives a transition up after `TELEPORT_TIMEOUT`, two seconds: the destination
copy is forgotten, what was held goes to the region the entity is still in,
and `teleport_failed` fires. `docs/adr/0003` measured the sequence at 10.5 to
22.9 ms with both regions on one machine, so only a destination that is down
or unreachable reaches the bound. Two seconds is a guess, exported as a public
constant and not yet a setting; the right duration still depends on the
deployment, and a setting can carry it when one asks.

**Server-initiated teleport.** A region's game logic might decide to teleport
an entity (a trap, a script, an admin action). That needs a new region-to-edge
message. Separate record.

**Whether `spawned` fires before `teleported`.** Decided with
`docs/adr/0010`: `spawned` fires first, naming the destination region and the
name the entity has held all along, then `teleported`. There is no new id to
carry, because the client never sees a region's id.

**Batch teleport.** Whether `teleport_many` is worth providing. Deferred
until a use case measures it.
