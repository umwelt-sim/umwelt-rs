# Architecture decision records

One file per decision that would otherwise be re-argued. `DESIGN.md` holds the
design and its measurements; these hold the decisions and what they cost.

A record is written before the work it describes, and it is not edited once the
work lands. If a decision is reversed, a new record supersedes it and says so.

| # | Decision | Status |
|---|---|---|
| [0001](0001-nats-for-the-region-edge-transport.md) | NATS for the region-to-edge transport | Accepted |
| [0002](0002-control-plane-and-region-heartbeats.md) | Control plane and region heartbeats | Accepted, not built |
| [0003](0003-ad-hoc-entity-migration.md) | Ad hoc entity migration between regions | Accepted, nothing to build |
| [0004](0004-region-events-and-entity-addressing.md) | Region events, and entity ids on the wire | Accepted, not built |

## Format

Status, Context, Measurements where a number decided it, Decision,
Consequences, and Open questions. Plain statements, no argument by emphasis.
