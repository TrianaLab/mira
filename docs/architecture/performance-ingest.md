# Performance — the ingest plateau

## The plateau is one mutex held across a `write(2)`

The sweep above cannot see the cause from outside: a closed-loop generator
reports `connections × batch / ack latency`, so every hypothesis predicts the
same curve, and CPU is flat at 2.23 cores at both ends of it. `mira_core::diag`
answers it from inside — two `Instant::now()` pairs and five relaxed atomics per
export, under 150 ns against a critical section measured in milliseconds — and
prints only when its target is enabled:

```sh
RUST_LOG=mira=info,mira_core=info,mira::probe=debug mira --data-dir ./data
```

Three runs, `loadgen --for 20s --batch 8192`, fresh store each, means over the
whole run (`scripts/measure/ingest-probe.sh`).

| | 4 conns | 32 conns | 96 conns |
| --- | --- | --- | --- |
| records/s | 1,297,149 | 1,917,983 | 1,814,829 |
| ack p50 | 8.3 ms | 64.3 ms | 199.9 ms |
| `submit.total` | 7.571 ms | 22.117 ms | 26.177 ms |
| `submit.admit` | 0.000 ms | 0.002 ms | 0.001 ms |
| `wal.encode` | 0.733 ms | 1.545 ms | 1.400 ms |
| `wal.lock_wait` | 3.920 ms | 18.152 ms | 22.067 ms |
| `wal.held` | 2.915 ms | 2.272 ms | 2.611 ms |
| of which `wal.write` | 2.849 ms | 2.042 ms | 2.422 ms |
| `runtime.lag`, ticks | 9.7 ms, 418 | 33.9 ms, 238 | 80.2 ms, 192 |
| `wal.inflight_max` | 4 | 12 | 12 |

Read down a column rather than across, for the reason below. **`wal.lock_wait`
is 52% of `submit.total` at four connections and 82% and 84% at thirty-two and
ninety-six.** One `Wal` sits behind all three signals and all `ingest.shards`
shards; `wal.held` times the append count is 15.2 s, 14.7 s and 18.6 s of a
twenty-second run, so **the mutex is occupied 74% to 93% of the wall clock**,
and 90–98% of that is the three `write_all`s. At 2.2–2.9 ms an append the log
serialises at most 345 to 440 appends per second, and at ~5,100 records an
append that is a hard **1.7 to 2.6 M records/s whatever the connection count**.
Connections past the plateau add waiters, not appends.

## A parked worker is not replaced

`std::sync::Mutex` on aarch64-apple-darwin is the pthread backend, so a
contended `lock()` parks the OS thread in the kernel, and a parked tokio worker
runs no other task. `wal.inflight_max` of exactly 12 on a 12-worker runtime says
every worker was inside `append_then` at once. `runtime.lag` borrows nothing
from the client: a task that asks to sleep 50 ms and does no work wakes 9.7,
33.9 and 80.2 ms late, a cadence of **16.8, 11.9 and 7.7 wake-ups per second**
against the 20 it asked for. The tick counts beside it are **not** out of a
fixed denominator: `RUNTIME_LAG` is never reset. Ack latency cannot separate
"working hard" from "cannot schedule anything".

## Each obvious suspect is ruled out by a number

`submit.admit` — `reserve()` plus the `ADMIT_WAIT` park — is a mean of 0.000 to
0.002 ms with a maximum of 6.5 ms at every shape, and `reserve()` is a first-fit
`try_reserve` across shards touching atomics only, so shard dispatch goes with
admission. Park/wake under saturation is real, but it is *inside*
`wal.lock_wait`.

More workers is not the fix: 96 connections, same binary, same box, back to
back, `TOKIO_WORKER_THREADS` 12 against 48. **2,229,315 records/s against
1,675,695, a 25% loss, `wal.lock_wait` up 6.4x from 16.3 ms to 104.5 ms and
`wal.inflight_max` from 12 to 47**. Held time barely moved, 14.98 s against
14.50 s. Four times the workers bought four times the queue and the same
serialised section.

## One log per signal was built, and its rate is not quotable

`scripts/measure/wal-split-ab.sh`, nine paired passes at 4/32/96 connections
across three sittings minutes apart. **The records/s signs split at every
shape** — medians 0.948, 1.039 and 0.909 with 4, 6 and 2 of 9 passes
favourable — so no throughput figure from it is quotable.
The mechanism is unambiguous where the rate is not: `wal.lock_wait` does fall,
0.63x at four connections and 0.795x at thirty-two, and `wal.write` takes all of
it back at 1.94x and 2.39x, **all nine passes agreeing at both shapes**. The
reason is the device, measurable with no Mira code in the loop: two concurrent
appenders at the measured 790 KiB frame return 0.98x the aggregate bandwidth of
one and three return 0.86x, both signs split. Three mutexes are free; a second
appender is not. The diff is on the `wal-per-signal` branch, not deleted.

## Group commit is priced by a RAM disk, and rejected

`scripts/measure/wal-volume.sh` symlinks `<data-dir>/.wal` at a RAM disk and
changes nothing else, which deletes the serialised section rather than
shortening it: `wal.write` −89%, `wal.held` −83%, `wal.lock_wait` −93%,
`wal.inflight_max` off its pin at 10 of 12 and `runtime.lag` from 33.9 ms to
2.967 ms. Throughput moves **1.096x at thirty-two connections on 3 of 3 passes,
and 1.005x at ninety-six with signs split**. A *perfect* log fix is worth ten
percent at one shape and nothing at the other; group commit writes the same
bytes down the same fd, so it cannot be worth more and is not worth
building. The sweep period is shortened so the log truncates inside the run
(`ticks % 240` → `ticks % 4`), same binary in both arms, both asserting
`0 shed`.

## The constraint behind the log is admission, which is the flusher

The same RAM-disk dump says where the queue re-forms once the log is free.
`submit.admit`, 0.000–0.002 ms in every disk-backed dump above and dismissed on
that reading, is **44.054 ms of a 47.910 ms `submit.total`, 92%**, while the
whole of the log comes to under 4 ms. Admission blocks when no `Config::queue`
slot frees on any shard, so what bounds ingest is the rate at which blocks seal
and publish. The log was the louder constraint, not the binding one.

Why the flusher is slow is not claimed here. Two candidates are open: its own
CPU — Arrow encode plus zstd, against 2.23 of twelve cores busy — or the volume
it shares with the log, already at its limit with one writer by the
concurrent-appender control. **What is settled is the envelope: anything spent on
the log's mutex is spent inside 10%**, so the next measurement belongs on the
flusher.

## What did land on the log, which is little

`Wal::sync()` takes its `F_FULLFSYNC` outside the lock rather than inside it:
structurally right given the 4,230 µs section 10 already publishes for that
call, and **not measured to move any number in the table above**: at 4,230 µs on
a 250 ms period it is a ~2% duty cycle, landing on whichever exports are
unlucky.

## The box, and a number withdrawn

This machine is not quiet: two runs of the *identical* 96-connection
configuration minutes apart returned 1,814,829 and 2,229,315 records/s, a 23%
spread. **The 26% fall from 32 to 96 connections that the ingest row publishes
did not reproduce on the day the diagnosis was measured — the fall was 5%.** The
published rates were taken on a quieter day and are left as they were; the
diagnosis rests on ratios taken inside one process during one run.

## `--offload` costs the retention sweep and nothing else

`scripts/measure/offload-cycle.sh`, one box, back to back: 137 blocks,
3.352 GiB, three signals. The same sweep is **0.868 s and 0.518 s** as the
unlink it always was and **14.999 s and 14.844 s** with `--offload file://`, so
against the medians the copy is 14.229 s — **241.2 MiB/s**, which is `read` +
`write` + `fsync` per file on this volume. `mira offload restore` brings it all
back in a median of 16.642 s = **206.3 MiB/s**, spread 176.1 to 268.5 MiB/s
across three runs; the two directions "agreeing within 6%" was a coincidence of
two samples and is withdrawn. `mira offload list` over all 137 blocks is
**0.050 s**, one `readdir` per partition, and a second `restore` copies **0**
blocks. Both costs land on the retention `spawn_blocking` thread, so the ingest
rows above are unchanged.

Two checks, because a query comparison alone would not catch a silently
re-encoded block. The same two queries — page one of the newest logs, and a
predicate that prunes nothing so every block is opened — return
**byte-identical** responses over original and restored. `diff -r` over
all 137 restored block directories reports **no difference at all**.
`elapsed_us` is normalised first; its being the only unstable field is what
makes the comparison worth anything.
