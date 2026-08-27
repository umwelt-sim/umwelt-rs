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

What remains is operational rather than routing.

**An operator has no way to see the region tier.** Which regions are running,
what each is carrying, whether any is missing its deadline, and whether one has
stopped.

**A region id is no longer mutually exclusive.** This is new, and it is a
consequence of 0001 rather than something that was true before. Two processes
could not previously serve region 7, because the second one failed to bind the
port. Now both can subscribe to `umwelt.7.edge.*.command` and both can answer
`umwelt.7.info`, and nothing objects. Edges would send commands that reach two
simulations, each holding half the world and each believing it holds all of it.
Removing the connection removed an accident that was doing real work.

**Nothing authenticates anything yet.** 0001 deleted the bearer secret and said
NATS accounts replace it. That has to be specified before it is true.

The map of regions — where each sits, which is next to which — is the game's,
maintained out of band. It is not here and this record does not change that.

## Decision

### What a region publishes

Every region publishes a heartbeat once a second on
`umwelt.control.region.{region}.heartbeat`. One subject per region, so a
subscriber can watch one or watch `umwelt.control.region.*.heartbeat` and see
the tier.

The body carries:

| field | why |
|---|---|
| region id | which region this is |
| instance id | which process, so two claiming one id are distinguishable |
| protocol version, crate version | version skew across the tier, visible |
| `protocol_hash` | config skew across the tier, visible |
| uptime, tick count | how long it has been up and whether it is ticking |
| entities, slots, viewers, edges | what it is carrying |
| mean and worst tick over the last second | whether it is meeting its deadline |
| ticks late, ticks dropped | whether it has been missing it |

No address, no port, no neighbors. A heartbeat says what a region is and how it
is doing, and nothing about how to reach it, because nothing needs to reach it.

A region is considered gone after three missed heartbeats, three seconds. The
same shape as `EDGE_TIMEOUT` in 0001 and for the same reason: with nothing to
close, silence is the signal.

### Region ownership

**A region checks its id is free before it serves anything.** At startup it
requests `umwelt.{region}.info` with a short timeout. An answer means another
process is already serving that id, and this one refuses to start rather than
joining it. That uses what 0001 already built and needs no new infrastructure.

**A region that hears its own id from another instance stops.** The instance id
in the heartbeat is what makes this detectable. Two processes that started
simultaneously can both pass the startup check; the heartbeat catches them
within a second, and the one that notices exits rather than continuing to serve
half a world.

Neither is consensus and neither is airtight. They catch a misconfiguration, a
stale process that outlived its supervisor, and a deployment that started a
replacement before the original stopped. A guarantee needs a lease from
something that implements consensus, which §Architecture says should be etcd or
similar and should not be written here.

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

### What umwelt does and does not do

umwelt publishes heartbeats and refuses to serve a taken id. It does not
subscribe to heartbeats, does not decide anything from them, and does not
rebalance, drain or place regions. Those run at human timescales and belong to
the control plane tier, which is a separate program and is not built.

## Consequences

**The region config gains no `advertise` address.** It was going to exist so a
heartbeat could carry a reachable endpoint. Nothing is reachable and nothing is
carried.

**A region gains a startup failure mode it did not have.** It can now refuse to
start because its id is taken. That is intended, and it means a deployment that
restarts a region before the old one has gone now fails rather than silently
producing two. An operator has to be able to tell those apart.

**Heartbeats cost almost nothing.** One message a second per region against the
983,040 a second the data plane carries.

**An operator tool becomes possible without any more library work.** Everything
needed to watch the tier is on `umwelt.control.region.*.heartbeat`, which the
`nats` CLI can already subscribe to.

## Open questions

**Whether the startup check should be a lease instead.** The check and the
heartbeat catch the common cases and guarantee nothing. A lease from etcd would
guarantee it. Doing that means the control plane tier exists first, and it means
a region cannot start when etcd is unreachable, which is a different failure
mode and possibly a worse one.

**What a region does when it loses its own race.** Exiting is the safe answer
and it is what this record says. Draining first would be gentler on the clients
already attached, and it is more machinery than the situation is worth until
somebody has seen it happen.

**Whether edges heartbeat too.** They already announce themselves by sending
commands, and a region expires them by silence, so a region needs nothing more.
An operator watching the edge tier would want something. Not decided, because
nothing consumes it yet.
