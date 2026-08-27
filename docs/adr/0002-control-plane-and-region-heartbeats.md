# 0002 — Control plane and region heartbeats

Status: Accepted, 2026-08-27. Not built.

## Context

`docs/adr/0001` moved the region-to-edge transport to NATS, and in doing so
removed most of what a control plane was going to be for. An edge no longer
needs a region's address, because it never connects to a region. It no longer
needs to know which regions exist, because one wildcard subscription taken at
startup receives from regions that did not exist when it subscribed. The
`advertise` address that was going to sit in the region config has nothing to
advertise.

Two things remain.

**An operator has no way to see the region tier.** Which regions are running,
what each is carrying, whether any is missing its deadline, and whether one has
stopped.

**Nothing authenticates anything.** 0001 deleted the bearer secret and said NATS
accounts replace it. That has to be specified before it is true.

One process serves one region, and one region is served by one process. The map
of regions — where each sits, which is next to which — is the game's, maintained
out of band. Neither is changed by this record.

## Decision

### What a region publishes

Every region publishes a heartbeat once a second on
`umwelt.control.region.{region}.heartbeat`. One subject per region, so a
subscriber can watch one or watch `umwelt.control.region.*.heartbeat` and see
the tier.

The body carries:

| field | why |
|---|---|
| region id | which region this is, when a heartbeat is read away from its subject |
| protocol version, crate version | version skew across the tier, visible |
| `protocol_hash` | config skew across the tier, visible |
| tick count | whether it is ticking, and how far along it is |
| entities, slots, viewers, edges | what it is carrying |
| mean and worst tick over the last second | whether it is meeting its deadline |
| ticks late, ticks dropped | whether it has been missing it |

No address, no port, no neighbors. A heartbeat says what a region is and how it
is doing, and nothing about how to reach it, because nothing needs to reach it.

A region is considered gone after three missed heartbeats, three seconds. The
same shape as `EDGE_TIMEOUT` in 0001 and for the same reason: with nothing to
close, silence is the signal.

`protocol_hash` earns its place. Two regions built from configs that differ in
any field affecting wire layout will decode each other's packets into nonsense,
and nothing else on this page would show it.

### Authentication

NATS accounts, with user JWTs and `.creds` files. Subject permissions do the
work the bearer secret did not:

| principal | may publish | may subscribe |
|---|---|---|
| region `{r}` | `umwelt.{r}.>`, `umwelt.control.region.{r}.heartbeat` | `umwelt.{r}.info`, `umwelt.{r}.edge.*.command` |
| edge `{e}` | `umwelt.*.edge.{e}.command`, `umwelt.*.info` | `umwelt.*.edge.{e}.payload`, `umwelt.*.edge.{e}.reply` |
| an operator | nothing | `umwelt.control.>` |

This is stronger than what it replaces in a way worth naming. `RegionServer`
reads the sending edge out of the subject a command arrived on, and treats that
as the sender's identity. With these permissions the broker enforces it: an edge
cannot publish on another edge's command subject, so it cannot move another
edge's entities even by trying. Under the bearer secret, any edge holding the
secret could have published anything.

The library accepts credentials and does nothing else about them. Issuing them,
and the account layout the table implies, are deployment work.

### What umwelt does and does not do

umwelt publishes heartbeats. It does not subscribe to them, does not decide
anything from them, and does not rebalance, drain or place regions. Those run at
human timescales and belong to the control plane tier, which is a separate
program and is not built.

## Consequences

**The region config gains no `advertise` address.** It was going to exist so a
heartbeat could carry a reachable endpoint. Nothing is reachable and nothing is
carried.

**Heartbeats cost almost nothing.** One message a second per region against the
983,040 a second the data plane carries.

**An operator tool needs a decoder.** The payload is bytes, so `nats sub` shows
nothing readable on its own. Something has to decode it, and that something is
small.

## Open questions

**Whether edges heartbeat too.** They already announce themselves by sending
commands, and a region expires them by silence, so a region needs nothing more.
An operator watching the edge tier would want something. Not decided, because
nothing consumes it yet.
