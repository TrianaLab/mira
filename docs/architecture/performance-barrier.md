# Performance — what the publish barrier is worth

[The ingest plateau](performance-ingest.md#the-constraint-behind-the-log-is-admission-which-is-the-flusher)
ends on the flusher with two candidates open for why it is slow: its own CPU,
or the volume it shares with the log. The volume is the one the engine can stop
paying for. Publishing one block costs **eleven** `F_FULLFSYNC` — five tables,
three sidecars, three directories — and on macOS `File::sync_all` is not a
page-cache flush but a device-wide cache barrier, the 4,230 µs section 10
publishes. Eleven of those per block, against a flusher that has to seal fast
enough to free an admission slot, is the obvious reading of that page. It is
wrong.

## The arm is a build, not a switch

`scripts/measure/barrier-ab.sh` builds the tree twice. The second arm is
`--features weak-sync-ab`, which replaces every barrier with a plain `fsync(2)`
and changes nothing else. It runs eight ABBA-interleaved pairs at thirty-two
connections and 12M records, both arms in every round, zero shed — strong first
on even rounds and weak first on odd, so this box's 23% day-to-day drift
cancels within a round rather than loading onto whichever arm ran second.

| | barrier-free / shipped | passes favourable |
| --- | --- | --- |
| records/s | 1.12x | **6 of 8** |
| ack p50 | 1.65x better | 8 of 8 |
| ack p99 | — | 3 of 8 |
| records/s per busy core | 0.976x | 3 of 8 |
| cores busy | 2.6 → 3.0 | 7 of 8 |

The feature is compile-time and not a config key because it trades the
power-loss guarantee `sync_all` documents. A released binary has no code path
that reaches it, and `--features` is visible in the build line of whoever
measured.

## The throughput signs split, and the series says why

By [section 3 of the measurement contract](../internals/measurement.md) — the
rule that made one-log-per-signal unquotable — **1.12x is not a number this
engine claims**. What is worth more than the verdict is the per-round series:
1.640, 2.544, 1.435, 0.912, 1.185, 1.056, 1.019, 0.838. The effect decays, both
arms converging on the same 1.25–1.5 M records/s from opposite directions.

Two earlier runs of this identical intervention sampled different ends of that
curve and returned 1.40x over five passes and 0.991x over thirteen. Neither was
a disagreement about the mechanism, and it is why eight interleaved pairs is
the floor for an ingest A/B here rather than the three the log fixes were
priced with.

## What survives is latency, and not the tail

Ack p50 improves on **8 of 8** passes while the per-core rate does not move at
all — 463,025 against 467,615, signs split — and cores busy rises. **The
barrier gates concurrency; it is not CPU the engine spends.** Removing it
recruits about half a core, and that half core does the same work per second as
the ones already running.

So it is an ack-latency change and should be proposed as one. A 10x argument
cannot rest on it, and the p50 win does not even reach the tail: ack p99 comes
back 3 of 8, split, with the barrier-free arm's worst rounds worse than
anything the shipped binary produced.

## What did land on the log, which is little

`Wal::sync()` takes its own `F_FULLFSYNC` outside the lock rather than inside
it: structurally right at 4,230 µs a call, and **not measured to move any
number in [the plateau table](performance-ingest.md#the-plateau-is-one-mutex-held-across-a-write2)**.
On a 250 ms period that is a ~2% duty cycle, landing on whichever exports are
unlucky.

## What this leaves standing

The flusher's cost is not its barriers, so the volume candidate is answered in
the only form that was actionable, and the open one is the flusher's own CPU.
Against that sits a fact neither page explains: at the plateau this engine uses
**2.23 of twelve cores**, and removing the largest single device stall it pays
recruits half of one more. Nothing measured here says what stops it using the
rest, and no ingest work is worth costing until something does.

One hazard guards the negative result rather than restating it. `wal_sweep`
(`pipeline.rs`) calls `wal.sync()` *before* it reads `block::wal_watermarks`,
so a block renamed between the two is truncated over. That is safe today only
because `publish` barriers the block durable before the rename — anyone who
lands the collapse has to reorder those two first.
