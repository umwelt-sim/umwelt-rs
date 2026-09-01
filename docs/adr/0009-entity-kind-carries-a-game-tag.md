# 0009 — EntityKind carries a game-defined tag

Status: Accepted, 2026-08-31.

## Context

A simulation creates entities through `Step::spawn`. An observer near one of
those entities receives it in `TickObservation::updates` as an `(EntityId, Pos3)`
pair — an id and a position, nothing else. The game client has no built-in way
to know what the entity is: a rock, a stone brick, an NPC, a projectile.

`EntityKind` exists but carries only the observer/unattended distinction, which
gates viewer registration. It does not reach the observation path at all. It
travels from the client through the edge to the region, decides whether a viewer
is registered, and is then discarded by the simulation. The simulation does not
store it per entity. The observation record does not carry it. The client never
sees it again.

Without a type tag in the observation record, a game developer is left with two
unsatisfying choices: build a separate entity-metadata channel outside umwelt, or
put game logic on the edge so the edge can send game messages alongside
observations. The first is a significant amount of plumbing for a fundamental
need. The second inverts the architecture: the simulation is where game logic
belongs, the edge is a relay.

The tag must travel in every observation record, not as a one-time first-sight
message. Observations ride datagrams, which are unreliable. A tag sent once and
then dropped leaves the client with an entity it cannot render.

## Decision

**`EntityKind` widens to carry a game-defined `u16` tag alongside its
observer/unattended role.**

The role stays a single byte (0 = unattended, 1 = observer). The tag is a `u16`
the game defines and umwelt does not interpret. Together they make `EntityKind` a
three-byte value on the wire.

**The tag is stored per entity in the simulation's SoA arrays**, beside the
position arrays. `Step::spawn` takes a tag. `Step::set_tag` changes it after
the fact, for entities whose visual state changes over time (a depleting
resource node, an NPC switching posture). `Step::tag` reads it back.

**The observation record carries the tag.** Every record in every packet includes
the tag alongside the entity id and position. At the default world config the
record grows from 12 to 14 bytes — two bytes per entity per packet. The tag
rides the same codec, the same snapshot, the same budget, and the same packet
format that positions already use.

**`TickObservation::updates` yields `(EntityId, Pos3, u16)`.** The client maps
the tag to an asset, a model, a sprite index — whatever its rendering needs.

**The edge does not decode observations.** It relays the bytes. The wider record
changes nothing on the edge side.

**For edge-originated spawns, the tag comes from `EntityKind`.** A client that
calls `ClientHandle::spawn` with `EntityKind::observer(42)` sends the tag
through the edge to the region, where it enters the sim's tag array. Other
observers then see the entity with tag 42 in their observations.

## Consequences

**`EntityKind` is no longer a bare enum.** Construction changes from
`EntityKind::Observer` to `EntityKind::observer(tag)` or
`EntityKind::unattended(tag)`. The `observes` method still works. A new `tag`
method returns the `u16`. This is a breaking change at every call site that
constructs an `EntityKind`.

**`Step::spawn` takes a tag parameter.** This is a breaking change for every
`Game::step` implementation that spawns entities.

**`TickObservation::updates` yields a wider tuple.** This is a breaking change
for every `ClientGame::observed` implementation that reads updates.

**Fewer entities fit per packet.** The record grows by two bytes, so at
1200-byte MTU the capacity drops from roughly 98 to 84 records. Whether this is
measurable at scale is determined by benchmarking before and after.

**The game module documentation must explain the boundary.** The simulation
creates entities and sets their tags. The edge relays. The client reads tags
from observations and renders. A developer should not have to research where
entity metadata belongs.

## Open questions

**Whether two bytes is enough.** A `u16` covers 65,536 entity types, which is
more than any shipping game has shipped. A game that needs structured metadata
(health bars, team colors, animation state) will still need its own channel for
that; the tag covers type identity, not full state.
