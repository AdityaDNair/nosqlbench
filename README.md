# nosqlbench

A cross-database benchmark of one social-feed workload on **Redis 7.4,
MongoDB 8.0, Cassandra 5.0 and Neo4j 5.26 Community**, driven by a Rust load
generator designed not to be the bottleneck. Each database uses its idiomatic
data model: Redis and Cassandra fan timelines out on write, MongoDB and Neo4j
fan out on read.

Everything runs on one Windows 11 laptop: the load generator natively, the
databases in Docker Desktop (Linux containers, WSL2 backend), one database at a
time.

## Workload

A deterministic social graph (seed 42): 10,000 users, ~200,000 follows,
100,000 posts, 1,000 hashtags. Follow targets are Zipf over *popularity*; post
authors and acting users are Zipf over a separate *activity* ranking.

| Operation | Share | What it does |
|---|---|---|
| Timeline read | 40% | 20 newest posts by accounts the user follows, with body and author |
| Profile read | 25% | fetch a user profile |
| Post like | 15% | record a like, increment the post's like count |
| Post create | 10% | store a post, index its hashtags, make it visible to followers |
| Hashtag search | 5% | 20 newest posts for a tag |
| User follow | 5% | record a follow (idempotent, no timeline backfill) |

Two scenarios:

- **normal** (the standard sweep): activity is a seeded random permutation of
  users, independent of popularity. A post fans out to ~17 followers.
- **celebrity**: the most-followed users are also the most active (user 0 has
  8,573 followers and writes ~10% of posts), so a post fans out to ~1,880
  followers on average.

Fan-out-on-write timelines keep each user's newest 800 posts, so a timeline
read returns the same 20 posts as fan-out on read.

## Architecture

```
            Windows host (i7-13620H: 6 P-cores + 4 E-cores)                  Docker VM (WSL2)
 ┌──────────────────────────────────────────────────────────────┐        ┌──────────────────┐
 │ bench.exe                                                    │        │ one database      │
 │  ┌─worker 0 (CPU 2)──┐ ┌─worker 1 (CPU 4)──┐ … (CPUs 6, 8)    │  TCP   │ container         │
 │  │ current_thread     │ │ current_thread     │                │ ─────► │ (4 GB, 6 CPUs,    │
 │  │ Tokio runtime      │ │ Tokio runtime      │                │ 127.0. │  ports on         │
 │  │ C request loops    │ │ C request loops    │                │ 0.1    │  127.0.0.1 only)  │
 │  │ own driver client  │ │ own driver client  │                │        └──────────────────┘
 │  └──────┬─────────────┘ └──────┬─────────────┘                │
 │         │ u64 records, SPSC ring (never blocks; drops counted) │
 │  ┌──────▼──────────────────────▼──────┐                        │
 │  │ aggregator (E-core, CPU 12):       │ → summary.csv, timeseries.csv,
 │  │ HDR histograms, per-second series  │   <op>.hgrm, meta.json
 │  └────────────────────────────────────┘                        │
 └──────────────────────────────────────────────────────────────┘
```

- **Workers:** one OS thread per physical P-core (the P-core holding CPU 0 is
  skipped, as Windows routes most interrupts there), pinned, raised to
  `THREAD_PRIORITY_HIGHEST`, each with its own `current_thread` Tokio runtime
  and its own database client. Request loops are created once; nothing is
  spawned or allocated per request. A test with a counting allocator shows
  zero heap allocations in the request loops in steady state.
- **Closed mode:** C loops per worker, each sends, waits, repeats. Latency =
  t1 − t0 around the driver call.
- **Open mode (coordinated-omission safe):** requests are scheduled at a fixed
  rate; latency is measured from the *intended* send time, so client lateness
  (send lag, also recorded) is charged to latency rather than hidden.
- **Telemetry:** one `u64` per event through a per-worker SPSC ring to a pinned
  aggregator; HdrHistogram (1 ns–60 s, 3 significant figures). Warm-up (15 s) is
  discarded; 60 s are measured. A single dropped record invalidates a run.
- **Calibration:** the noop adapter measures engine overhead (~53M ops/s
  closed with a 19 ns p50); `redis-ping` (one PING per op) measures the
  Windows → Docker VM → container transport floor.

Code: `src/` (engine, adapters, loaders), `scripts/` (PowerShell), `plots/plot.py`,
`docker-compose.yml`, `config/default.toml`. Full design and every decision:
[`CLAUDE.md`](CLAUDE.md).

## Reproducing

### Setup (once)

1. Windows 11, Rust stable (MSVC toolchain), Docker Desktop with the WSL2
   backend, Python 3 with `polars` and `matplotlib` (a `.venv` with both is
   expected at the repo root: `python -m venv .venv` then
   `.venv\Scripts\pip install polars matplotlib`).
2. Docker Desktop: turn **Resource Saver off**. In `%USERPROFILE%\.wslconfig`
   set `vmIdleTimeout=-1` under `[wsl2]`. Both stop the VM from restarting
   between databases.
3. Free the benchmark ports: 6379, 27018, 9042, 7687, 7474. (MongoDB is published
   on 27018 because a native MongoDB service commonly owns 27017.)
4. Plug the laptop in and stop it from sleeping during the run; close heavy
   applications. The scripts abort on battery and warn about busy background
   processes.

### One command

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\run_all.ps1
```

It checks that Docker is running and the ports are free, builds the release
binary, then runs: noop calibration ×3 → redis-ping transport sweep → Redis →
MongoDB → Cassandra → Neo4j (each: start container, wait healthy, load, closed
c = 1/8/32/128 × 3 runs, open at 50/80/95% of the best closed throughput × 3
runs, `down -v`) → a short celebrity run per database (c = 8 and 32, 1 run).
Results go to `results\final\` (about 2.5 hours, estimated from the
validation-run timings). Then:

```powershell
.venv\Scripts\python.exe plots\plot.py results\final
```

writes `results\final\figures\*.png|svg` and `results\final\results_summary.md`.
A single database: `scripts\run_db.ps1 -Db cassandra` (see its help).

## Key findings (validation runs, `results\`)

These come from the per-milestone validation sweeps (104 runs, 0 errors, 0
timeouts, 0 dropped records). The final numbers come from one `run_all.ps1`
session in `results\final\`; see `results_summary.md` there for the tables.

1. **Fan-out on write wins the normal workload.** Closed-mode throughput at
   c=128: Redis 30,057 ops/s, Cassandra 25,288, MongoDB 5,275, Neo4j 4,644.
   MongoDB and Neo4j saturate from c≈32. Their timeline read (40% of the mix)
   gathers followees' posts at read time and is their slowest operation (p99 at
   c=32: 33 ms MongoDB, 28 ms Neo4j vs 8.3 ms Redis and 6.7 ms Cassandra).
2. **…and loses badly when popular users are also active.** In the celebrity
   scenario throughput drops 87–92% for Redis and 91–96% for Cassandra, but only
   40–66% for MongoDB and 66–72% for Neo4j. MongoDB becomes the fastest (2,352
   ops/s at c=8).
3. **Latency under load (open mode at 50% of each database's best closed
   throughput):** p99 11.4 ms Redis, 15.1 ms Neo4j, 15.3 ms MongoDB, 31.3 ms
   Cassandra. Near saturation (95%) tails reach hundreds of ms for all but
   Redis.
4. **The transport floor is ~0.34–0.44 ms** (redis-ping p50), so single-request
   latencies below ~1 ms are mostly the Windows ↔ Docker VM path, not the
   database.
5. **Driver pool sizes were checked, not assumed.** For each database a larger
   pool / more connections per worker was tested at c=128 on the same boot; none
   clearly helped, so every database uses its driver default (Redis 1
   multiplexed connection, MongoDB 10, Cassandra 1, Neo4j 16).

## Limitations

- **Laptop scale.** 10k users / 100k posts on a 16-thread laptop, not a
  production-sized dataset or server hardware. Absolute numbers do not
  transfer; relative behaviour is the point.
- **Single node.** Every database runs as one node (Cassandra RF=1, no Mongo
  replica set, Neo4j Community). No replication, failover or network-partition
  behaviour is measured.
- **Windows + Docker VM transport floor.** Every request crosses Windows →
  WSL2 VM → container and back. redis-ping measures that floor at 0.34 ms p50
  (c=1) to 0.44 ms (c=128), and it is included in every latency here.
- **Host CPU contention (send lag).** The load generator and the Docker VM share
  host CPUs. In open mode at 50% load, send-lag p99 was 0.9 ms for Redis but
  4.1 ms for MongoDB and 5.8 ms for Cassandra. Diagnostic: the noop adapter at
  12,644 ops/s had send-lag p99 **26 µs with the VM idle and 5,718 µs while
  `cassandra-stress` kept the container at ~590% CPU**, so the lateness is the
  VM's vCPUs competing with the workers, not driver work. It scales with the
  database's CPU use and is charged to latency, so open-mode tails of the
  CPU-heavy databases include a few ms of client-side delay. Send-lag p99 is
  also unstable near the 1% line (Neo4j: 2.7 ms at 50%, 0.09 ms at 80%, with
  p99.9 5–8 ms in both), so p99.9 is reported alongside it.
- **Differing durability settings.** Redis runs with persistence off (no RDB,
  no AOF); MongoDB uses w:1 with default journaling; Cassandra uses consistency
  ONE with its commit log; Neo4j uses its default transaction log. Write
  latencies are not comparable as durability guarantees.
- **Boot-to-boot and in-session drift.** Numbers move between boots
  (redis-ping c=32: ~82k vs ~74k ops/s) and within a session when Windows
  background jobs run (Redis c=128: 30.1k in the sweep vs 20.8k two hours later
  with a Windows download and Disk Cleanup active). Only same-session
  comparisons are meaningful, hence the single-session final run.
- **VM CPU affinity was tested and rejected.** Pinning the Docker VM away from
  the worker cores made results worse. In a same-boot A/B (redis-ping c=32,
  3 interleaved rounds): no affinity 73.1k–73.9k ops/s, p99 718–759 µs; mask
  0xFC03 (includes the E-cores) 63.4k–68.3k ops/s, p99 788–895 µs; mask 0x0C03
  70.8k–72.8k ops/s, p99 816–902 µs. So the VM runs unpinned, and the
  contention above is the cost.
- **Timelines are capped at 800 posts** in Redis and Cassandra (reads are
  unaffected; the untrimmed celebrity fan-out would be 188M entries), and
  Cassandra does not trim on write during runs.
