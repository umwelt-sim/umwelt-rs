# Changelog

## 0.1.0

The client API changed shape: a client works in world coordinates and sees
one name per entity.

- World coordinates. `WorldPos` (three `i64` axes, 10 fractional bits) at the
  client and the edge; a region never sees one. `ClientHandle::spawn`, `move_entity`,
  `move_entities` and `teleport` take a `WorldPos`; `spawn_into` and
  `teleport_into` name a region for a place no walk reaches. `EdgeInfo` carries
  the region's placement. Design: `docs/adr/0010`.
- The world map. `WorldMap` and `Placement`, read once by every edge at startup
  from the JetStream key-value bucket `umwelt`, key `map`, in the byte layout
  the record defines. A broker without JetStream or without the key is a
  deployment of islands. Every integration test now needs `nats-server -js`.
- The region reports its box. `Presence::BoundaryCollision`,
  `Presence::ViewCollision` and `Presence::ViewCleared`; a move out of the box
  is clamped and reported once per push rather than refused. `Presence` is
  17 bytes.
- Shadows. An observer whose view reaches a seam is served the neighbor's
  border strip by a second viewer the edge keeps there, under the observer's
  own handle. `EntityKind` has a third, library-only role.
- One name per entity. Records are 18 bytes and despawns 8: the 64-bit
  `EntityKey` the edge assigned, written by the region. A default packet holds
  65 records where it held 84. `ClientGame::spawned` reports the name;
  `TickObservation::updates` yields `(EntityKey, WorldPos, u16)` and
  `despawns` yields `EntityKey`. A despawn for a name another region still
  sends is not reported.
- Seamless crossing. The edge answers a boundary collision with the teleport
  of `docs/adr/0008`, holds moves and entity messages during it, and swaps
  the shadows. `TELEPORT_TIMEOUT` (two seconds) bounds a transition;
  `EdgeStats` gains `placed_regions`, `off_map`, `shadows`, `crossings`,
  `walls`, `transitions` and `teleport_timeouts`.
- `EdgeGame::teleporting` and `teleport_arrived` take `Option<ClientId>`, as
  `spawned` and `removed` do, so an entity with no client behind it can cross.
- Also since 0.0.1: first-class teleport (`docs/adr/0008`), `EntityKind`
  carries a game tag (`docs/adr/0009`), and an edge keeps idle regions alive.

## 0.0.1

Initial release. Interest management for real-time simulation:

- Spatial subscription over a cell grid with configurable region and view radius
- Priority accumulation so nearby entities update more often than distant ones
- Per-viewer bandwidth budgeting to an MTU-sized packet
- Region simulation server over NATS, edge relay over QUIC
- Fixed-point position arithmetic (1024 units per meter)
