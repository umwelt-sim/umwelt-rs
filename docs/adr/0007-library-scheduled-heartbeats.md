# 0007 — Library-scheduled heartbeats, on both tiers

Status: Accepted, 2026-08-27. Not built.
Supersedes part of `docs/adr/0002`: the library no longer publishes only when
asked, and it no longer holds no timer. Cadence remains a deployment choice.
`docs/adr/0002`'s open question about whether edges heartbeat is closed here:
they do.

## Context

`docs/adr/0002` gave a region a `heartbeat(RegionLoad)` and no timer, reasoning
that how much resolution an operator wants and how much traffic that is worth are
deployment judgments rather than protocol ones. The reasoning about cadence holds.
What it produced does not.

Every field of `RegionLoad` is umwelt's own number: tick count, entities, slots,
viewers, mean and worst tick, ticks late, ticks dropped. A consumer publishing a
heartbeat is therefore a consumer maintaining umwelt's numbers on umwelt's behalf.
`examples/herd-sim.rs` shows the cost — a `Beat` accumulator, a `--heartbeat`
argument, a branch in the tick callback, and an `AtomicUsize` carrying a slot
count only because `WorldSimulation` exposes no accessor for it.

The edge tier is about to exist and nothing observes it at all. Edges are cattle,
but a herd is still worth watching.

## Decision

**The library holds the timer. The deployment chooses the interval.**
`set_heartbeat_interval(Duration)` on both servers, defaulting to 30 seconds,
with zero switching heartbeats off. Both servers already take a Tokio handle and
already run tasks, so this imposes no thread a consumer did not ask for.

**A region needs no new plumbing.** `RegionServer` already holds `Arc<Inbound>`,
and `Inbound` is already driven by the tick: `apply` is handed a `Step`, `settle`
is handed `&mut WorldSimulation`, and neither call is optional. So `Inbound`
accumulates the load, `WorldSimulation::run_with` accumulates its own tick
timing, `settle` reads that off the simulation it is already given, and the
server's timer publishes. The consumer wires nothing and passes nothing.

`RegionServer::heartbeat` and `RegionLoad` as a consumer-supplied input both go.
`RegionLoad` survives as the shape of what a heartbeat carries.

**An edge publishes on `umwelt.control.edge.{edge}.heartbeat`**, with
`umwelt.control.edge.*.heartbeat` for the tier, mirroring the region subject so a
watcher can take one or take all of them.

It carries the edge name, protocol version and crate version, clients connected,
entities managed and how many of those observe, which regions hold them, and over
the span since the last beat: packets relayed, packets undeliverable, commands
received, and commands refused by this edge.

**It does not carry commands a region refused.** A region counts those per edge
and never tells the edge, so an edge cannot report a number it does not have.

**It does not carry the QUIC endpoint address.** An edge's listening address is
unlikely to be usable by whatever reads the control plane, which may be on
another host, another VM, or in another VPC, and a game client is told where to
connect by the game's matchmaking rather than by umwelt. `docs/adr/0002` gave
regions no address because nothing needs to reach a region; an edge is reachable
and still carries none, because the reader is the wrong audience for it.

**umwelt still does not consume heartbeats.** It publishes them. It does not
subscribe, decide anything from them, or rebalance, drain or place anything.
`docs/adr/0002` drew that line and this record does not move it.

## Consequences

**`herd-sim` gets smaller.** Its `Beat` struct, its heartbeat branch, its
`--heartbeat` argument and its hand-maintained slot count all go.

**A heartbeat is a snapshot taken off the tick, not during it.** The timer runs
on the runtime rather than the tick thread, so a published beat may be a tick or
two behind. At any interval an operator would choose, that is invisible.

**A consumer driving `tick` rather than `run` reports zero for `late` and
`dropped`.** Those counts exist only in the pacing loop. The other fields still
fill in, so the beat is incomplete rather than wrong.

**Switching heartbeats off is now an explicit act.** Under `docs/adr/0002` a
consumer that never called `heartbeat` simply had none. Now silence means a
process that has stopped, which is the point, and a deployment that wants no
heartbeats sets the interval to zero.

**An operator still cannot place a name.** Neither tier's heartbeat says where it
runs, so mapping a region id or an edge name to a host is the deployment's own
logging or orchestrator.

## Open questions

**What interval an operator actually wants.** Thirty seconds is carried over from
what `herd-sim` defaulted to, and nothing has measured what a control plane needs
or what the traffic costs at tier scale.

**Whether a consumer should be able to add its own fields.** A game may want to
report something umwelt does not know about. Nothing here allows it, and nothing
yet asks for it.
