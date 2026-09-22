# CLAUDE.md — nosqlbench

Cross-database benchmark: one social-feed workload run against Redis, MongoDB,
Cassandra and Neo4j by a Rust load generator that must not be the bottleneck.

**Platform:** Windows 11. The engine runs natively on Windows. The databases run
in Docker Desktop (Linux containers). All scripts are PowerShell.

Deadline is 3–4 days. Build milestone by milestone (see bottom). Do not start a
milestone until the previous one passes its acceptance check. Prefer boring,
working code over clever code.

---

## 0. Non-negotiable rules

1. **Never change measurement semantics without asking.** Where T0/T1 are taken,
   what counts as latency, warm-up handling and histogram settings are fixed by
   this file.
2. **Never claim a performance property you haven't measured.** If you say
   "overhead is X", show the command and output.
3. **No allocation in engine code on the request path.** Payloads are
   pre-generated. Request loops are long-lived tasks created once. Driver
   internals may allocate; that cost is part of client cost and is visible in
   the no-op calibration.
4. **No locks, channels or syscalls on the request path** except the driver
   call itself. Telemetry goes through a per-worker SPSC ring.
5. **Never block when the ring is full.** Increment a dropped counter and move on.
   Any non-zero dropped count invalidates the run.
6. **Only one database container runs at a time.**
7. **Windows only.** No Linux-only APIs, no bash, no `/sys`, no `cfg(unix)` code
   paths that the Windows build depends on. Use `PathBuf`, never hard-coded `/`.
8. After every change: `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test`.
   Commit after each working step with a clear message.

---

## 1. Tech stack

- Rust stable (MSVC toolchain), edition 2021, single binary crate `bench`
- `tokio` (current_thread runtime **per worker**; features: rt, time, net, macros)
- `core_affinity` — pin threads
- `windows` — P/E-core detection (`GetLogicalProcessorInformationEx`) and
  `timeBeginPeriod(1)`
- `quanta` — high-resolution clock
- `rtrb` — SPSC ring buffers
- `hdrhistogram` — percentiles
- `rand`, `rand_chacha`, `rand_distr` — seeded RNG, Zipf
- `serde`, `toml`, `serde_json`, `csv`, `clap` (derive), `anyhow`, `thiserror`, `futures`
- Drivers: `redis` (tokio-comp), `mongodb`, `scylla`, `neo4rs`
- Databases: Docker Desktop + `docker compose`
- Scripts: PowerShell 7 (`pwsh`), falling back to Windows PowerShell 5.1 syntax
- Plots: Python (`polars`, `matplotlib`)

---

## 2. Layout

```
nosqlbench/
  Cargo.toml
  CLAUDE.md
  config/default.toml
  docker-compose.yml
  src/
    main.rs            # clap: `bench load`, `bench run`, `bench calibrate`
    config.rs
    cpu.rs             # P-core detection (Windows API), pinning plan, timer resolution
    dataset.rs         # deterministic dataset generation
    workload.rs        # Op type, pre-generated op streams, body arena
    worker.rs          # per-core runtime, request loops, closed/open scheduling
    telemetry.rs       # record packing, rings, aggregator thread
    output.rs          # summary.csv, timeseries.csv, meta.json
    adapters/
      mod.rs           # Adapter trait + factory
      noop.rs
      redis_ping.rs    # transport baseline
      redis.rs mongo.rs cassandra.rs neo4j.rs
    loaders/
      redis.rs mongo.rs cassandra.rs neo4j.rs
  scripts/
    sysinfo.ps1        # CPU, OS build, power source, power plan, Docker version
    run_db.ps1         # up → wait healthy → load → sweep → down -v
    run_all.ps1
  plots/plot.py
  results/
```

---

## 3. Config (`config/default.toml`)

```toml
seed = 42

[dataset]
users = 10_000
posts = 100_000
avg_follows = 20
hashtags = 1_000
max_tags_per_post = 3
zipf_s = 0.99
fanout_cap = 10_000
timeline_len = 800    # Redis/Cassandra timelines keep the newest N posts per user

[workload]            # weights, normalised at load time
timeline_read = 40
user_profile_read = 25
post_like = 15
post_create = 10
hashtag_search = 5
user_follow = 5
scenario = "normal"   # "normal" | "celebrity" (see section 4)

[run]
mode = "closed"       # "closed" | "open"
concurrency = 32      # closed: total in-flight across all workers
rate = 0              # open: total target ops/s
max_inflight = 1024   # open: in-flight cap across all workers
warmup_secs = 15
measure_secs = 60
request_timeout_ms = 5000
workers = 4           # capped at available physical P-cores
ring_slots = 1_048_576

[cpu]
worker_cpus = []      # empty = auto: one logical CPU per physical P-core
aggregator_cpu = -1   # -1 = auto: first E-core, else last P-core
worker_priority = "highest"  # "normal" | "above_normal" | "highest" | "time_critical"

[drivers]
redis_connections = 1 # multiplexed Redis connections per worker (round-robin)

[endpoints]
redis = "redis://127.0.0.1:6379"
mongo = "mongodb://127.0.0.1:27018/?directConnection=true"   # 27017 is taken by a native MongoDB service
cassandra = "127.0.0.1:9042"
neo4j = "127.0.0.1:7687"
neo4j_user = "neo4j"
neo4j_pass = "benchpass"
```

Every field can be overridden from the CLI.

---

## 4. Dataset (deterministic)

All generation uses `ChaCha8Rng::seed_from_u64(seed)` so every loader and the op
generator see the same world.

- **IDs are `u64` everywhere** (users `0..users`, posts `0..posts`). Stored as
  `bigint` / integer, never strings.
- **Two rankings** (*Decision (M3)*): *popularity* is user id order (user 0 is
  the most followed); *activity* is a separate ranking used for post authors
  and acting users. `workload.scenario` picks the activity ranking:
  - `normal` (default, the standard sweep): a seeded random permutation of user
    ids (`ChaCha8Rng::seed_from_u64(seed ^ 0xAC71_7171_0000_0000)`), so
    activity is independent of popularity.
  - `celebrity`: the identity, so the most-followed users are also the most
    active. This reproduces the original M1 dataset exactly (fingerprint
    `0x3e9c076ea0bd0bfe`, pinned by a unit test).
  Reason: with one shared ranking, user 0 has 8,573 followers *and* writes ~10%
  of posts, so each POST_CREATE fans out to ~1,880 timelines on average and
  fan-out-on-write databases spend ~95% of their time on it. With `normal`,
  the expected fan-out per POST_CREATE is ~17.
- **Follows:** each user follows `avg_follows` distinct targets (±50% uniform),
  targets drawn Zipf over the popularity ranking. No self-follows.
- **Posts:** author drawn Zipf over the activity ranking; `created_at` spread
  uniformly over the 30 days before `2026-01-01T00:00:00Z`;
  0..=max_tags_per_post hashtags drawn Zipf.
- **Bodies:** 40–280 ASCII chars from a fixed word list, stored in one `Vec<u8>`
  arena with an offsets `Vec<u32>`; adapters receive `&str` slices.
- **Hashtags:** `tag0000 .. tag0999`.
- No likes are pre-loaded.
- **Timelines** (Redis, Cassandra only): each post goes to the timeline of each
  follower of its author, capped at `fanout_cap`, and each timeline keeps only
  its newest `timeline_len` (800) posts, ordered by `(created_at, post_id)` desc.
  *Decision (M3):* in the celebrity scenario untrimmed fan-out is 188M entries
  (median timeline 19,680 posts), which does not fit Redis in 4 GB; trimmed it
  is 8.0M. In the normal scenario it is 1.78M untrimmed, 1.51M trimmed. The
  trim leaves the 20 newest (all TIMELINE_READ returns) identical to fan-out on
  read.

---

## 5. Workload

```rust
#[derive(Clone, Copy)]
pub enum OpKind { TimelineRead, UserProfileRead, PostLike, PostCreate, HashtagSearch, UserFollow }

#[derive(Clone, Copy)]
pub struct Op {
    pub kind: OpKind,
    pub user: u64,      // acting user, Zipf over the activity ranking
    pub target: u64,    // followee (UserFollow) or post id (PostLike)
    pub body: u32,      // index into body arena (PostCreate)
    pub tags: [u16; 3],
    pub ntags: u8,
    pub new_post_id: u64,
    pub ts_ms: i64,
}
```

- Each worker gets its own pre-generated `Vec<Op>` of 1,048,576 ops (seed =
  `seed ^ worker_id`), cycled with a wrapping cursor.
- Acting users are Zipf over the activity ranking; USER_FOLLOW targets are Zipf
  over the popularity ranking; POST_LIKE targets are Zipf over preloaded posts.
- `new_post_id = (1 << 40) | (worker_id << 32) | seq`. Writes must be idempotent
  so the second pass through the stream doesn't error.
- New post timestamps: epoch + 1 day + seq ms.
- TimelineRead and HashtagSearch return **20** posts, newest first.

| Op | Must do |
|---|---|
| POST_CREATE | Store post; index under each hashtag; make it visible to followers' timelines (fan-out on write for Redis/Cassandra, nothing extra for Mongo/Neo4j) |
| USER_FOLLOW | Record `user → target`. Idempotent. No timeline backfill |
| TIMELINE_READ | 20 newest posts by accounts `user` follows, with body and author |
| POST_LIKE | Record `(user, post)` like; increment post's like count |
| USER_PROFILE_READ | Fetch full profile by id |
| HASHTAG_SEARCH | 20 newest posts for tag, with body |

Redis and Cassandra fan out on write; MongoDB and Neo4j fan out on read. This is
deliberate: each database is benchmarked with its idiomatic design.

---

## 6. Adapter trait

```rust
pub trait Adapter {
    async fn execute(&self, op: &Op, bodies: &BodyArena) -> Result<(), AdapterError>;
}
```

- Each worker constructs **its own** adapter instance inside its own runtime.
- Static dispatch: worker is generic over `A: Adapter`; `main` matches on db name once.
- Prepare statements (Cassandra) and parameterise queries (Neo4j) in the constructor.
- Adapters must fully consume responses.
- Errors map to status codes; never panic on a DB error.

`noop.rs`: returns `Ok(())` after `std::hint::black_box(op)`. Measures engine
overhead and ceiling throughput.

*Connections per worker (M3/M4).* Each worker's client uses its driver's
default pool unless noted: redis-rs 1 multiplexed connection
(`drivers.redis_connections`, also used by redis-ping); mongodb 3.9 up to 10
connections, one request each; scylla 1.9 against Cassandra 1 connection with
up to 32,768 multiplexed streams; neo4rs 0.8 up to 16 connections, one query
each. Same-boot closed c=128 checks (`results\fairness\`), keep the setting
unless the alternative clearly helps:
- *Redis (M3):* 1 connection 20,799 ops/s, p50 4.69 ms, p99 25.4 ms vs 4
  connections 16,110 ops/s, p50 6.23 ms, p99 29.4 ms → keep 1.
- *MongoDB (M4):* pool 10 (driver default) 5,997 ops/s, p50 18.4 ms, p99
  63.1 ms vs pool 32 (one per loop) 5,955 ops/s, p50 17.1 ms, p99 84.6 ms →
  keep 10 (`drivers.mongo_max_pool_size`).
- *Cassandra (M4):* 1 connection 26,319 ops/s, p50 3.39 ms, p99 21.2 ms vs 4
  connections 22,139 ops/s, p50 3.96 ms, p99 26.3 ms → keep 1
  (`drivers.cassandra_connections`).
- *Neo4j (M5):* pool 16 (driver default) 4,867 ops/s, p50 22.8 ms, p99 90.4 ms,
  0 timeouts vs pool 32 4,703 ops/s, p50 10.9 ms, p99 442 ms, 16 timeouts
  (the queue moves from the client pool to server-side write locks) → keep 16
  (`drivers.neo4j_max_connections`).

`redis_ping.rs`: sends `PING` for every op regardless of kind. Measures the
round trip Windows → Docker VM → container → back, i.e. the transport floor
every database pays. Reported as its own result, never mixed into Redis numbers.

---

## 7. Worker and scheduling

### CPU and timers (`cpu.rs`)
- Call `timeBeginPeriod(1)` at startup and `timeEndPeriod(1)` on exit. Windows'
  default timer resolution is ~15.6 ms, which would wreck open-loop pacing.
- Detect cores with `GetLogicalProcessorInformationEx(RelationProcessorCore)`.
  Each core entry has `EfficiencyClass` and a group mask. The highest
  `EfficiencyClass` = P-cores. Pick the lowest logical CPU of each P-core
  (one thread per physical core).
- *Decision (M2):* the auto plan skips the P-core holding logical CPU 0,
  because Windows routes most device interrupts/DPCs there (workers on this
  laptop: CPUs 2, 4, 6, 8). The reason is written to `meta.json`
  (`pinning.skip_reason`). Not skipped if it is the only P-core, or when
  `worker_cpus` is set explicitly.
- If every core has the same class, treat all as P.
- Assume a single processor group (true on these laptops); error clearly otherwise.
- Print the pinning plan at startup and write it to `meta.json`.
- One OS thread per worker, pinned with `core_affinity`, running a
  `current_thread` Tokio runtime. One aggregator thread, pinned.
- *Decision (M2):* worker threads run at `cpu.worker_priority = "highest"`
  (`SetThreadPriority`). Measured on noop open mode at 5M ops/s: send_lag p99
  2.1 ms at normal priority (other threads took a worker's CPU for ~30 ms
  quanta), 0.53 ms at highest. `time_critical` was lower still but risks
  starving system threads on the worker cores, so it is not the default.

### Request loops
Inside a worker, create C long-lived async loops once (`join_all` over a pre-built
Vec, or `LocalSet` + `spawn_local`). Shared per-worker state is `Rc<Cell<..>>` /
`Rc<RefCell<Producer>>`. No `tokio::spawn` per request.

- **Closed mode:** C = `concurrency / workers` (workers = `min(workers, concurrency)`).
  ```
  loop {
      op   = next op from cursor
      t0   = clock.now()             // immediately before execute
      res  = adapter.execute(op).await
      t1   = clock.now()
      push record(latency = t1 - t0)
  }
  ```

- **Open mode (coordinated-omission safe):** per-worker rate R = `rate / workers`,
  interval I = 1e9 / R ns, C = `max_inflight / workers`. Shared ticket counter `k`.
  ```
  loop {
      k += 1; intended = start + k * I
      wait until intended:
          if intended - now > 2ms: tokio::time::sleep_until(intended - 2ms)
          while now < intended: tokio::task::yield_now().await
      t0  = clock.now()
      res = adapter.execute(op).await
      t1  = clock.now()
      push record(latency = t1 - intended)   // response time
      push send_lag record (t0 - intended)   // engine health
  }
  ```
  Late tickets are charged to latency. This is intended. Never re-base `start`.

### Phases
Global `AtomicU8`: WARMUP → MEASURE → STOP, read with `Relaxed` when building a
record. STOP: loops exit after their in-flight request.

---

## 8. Telemetry

Record = one `u64`:

```
bits 63..16  latency_ns (48 bits)
bits 15..8   op kind
bits  7..4   status: 0 ok, 1 error, 2 timeout
bit   3      warmup
bit   2      is_send_lag record
bits  1..0   reserved
```

- One `rtrb` ring per worker. Aggregator drains all rings, sleeping 1 ms when empty.
- Histograms: one per op kind (ok only) + one send-lag histogram;
  `Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3)`. Errors/timeouts counted.
- Warm-up records discarded.
- Per-second timeseries of ok/errors per op kind, bucketed by drain time.
- Throughput = measured ok ops / measure_secs.

---

## 9. Output

`results\<scenario>\<db>\<mode>_<level>_run<n>\` (open-mode level = `<pct>pct`).

- `summary.csv`
  `db,mode,level,run,op,count,errors,timeouts,throughput_ops_s,p50_us,p90_us,p95_us,p99_us,p999_us,max_us,send_lag_p99_us,dropped_records`
  plus one `op=ALL` row.
- `timeseries.csv` — `second,op,ok,errors`
- `meta.json` — scenario, resolved config, git commit, pinning plan, CPU model,
  Windows build, power source (AC/battery), active power scheme, Docker version
  and image tags, Docker VM affinity, start time.
- `<op>.hgrm` — HdrHistogram percentile export.

Print a short table to stdout at the end of each run.

*Reporting (M3 decision):* closed mode is the primary comparison; open mode at
50% is the headline latency-under-load figure. Wherever open-mode latency
tails are shown (summary tables, plots), show send-lag p99 next to them:
send lag rises at 80–95% load (Redis: p99 0.9 / 3.7 / 6.5 ms at 50 / 80 / 95%)
and is charged to latency, so part of those tails is client-side.

---

## 10. Schemas

### Redis
Persistence off (`--save "" --appendonly no`). Durability difference noted in results.

```
user:{id}              HASH  username, display_name, bio, created_at
post:{id}              HASH  author_id, body, created_at, hashtags("t1,t2")
following:{id}         SET   followee ids
followers:{id}         SET   follower ids
timeline:{id}          ZSET  post_id  score=created_at_ms
tag:{tag}              ZSET  post_id  score=created_at_ms
likes:{post_id}        STRING counter (INCR)
likers:{post_id}       SET   user ids
```
- POST_CREATE: pipeline HSET post + ZADD tag:*; SMEMBERS followers (truncate to
  cap); pipeline ZADD timeline:{f} + ZREMRANGEBYRANK timeline:{f} 0 -(timeline_len+1).
- TIMELINE_READ: ZREVRANGE timeline 0 19 → pipeline HGETALL post:* (20).
- HASHTAG_SEARCH: ZREVRANGE tag 0 19 → pipeline HGETALL.
- POST_LIKE: pipeline SADD likers + INCR likes.
- USER_FOLLOW: pipeline SADD following + SADD followers.
- USER_PROFILE_READ: HGETALL user.
- Loader: pipelines of 1000 commands.

### MongoDB
Write concern `w:1`, default journaling.

```js
users   { _id: NumberLong, username, display_name, bio, created_at }
posts   { _id: NumberLong, author_id, body, hashtags: [..], like_count, created_at }
follows { _id: { f: follower, t: followee }, follower_id, followee_id, followed_at }
likes   { _id: { u: user, p: post } }

posts.createIndex({ author_id: 1, created_at: -1 })
posts.createIndex({ hashtags: 1, created_at: -1 })
follows.createIndex({ follower_id: 1 })
follows.createIndex({ followee_id: 1 })
```
- TIMELINE_READ: `follows.find({follower_id}, {followee_id:1})` →
  `posts.find({author_id:{$in}}).sort({created_at:-1}).limit(20)`.
- POST_LIKE: insert likes (ignore duplicate key) then `$inc like_count`.
- USER_FOLLOW: insert follows (ignore duplicate key).
- Loader: `insert_many` batches of 10,000, `ordered: false`; indexes after load.

### Cassandra (single node)
```sql
CREATE KEYSPACE social_bench
  WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1};

CREATE TABLE users (user_id bigint PRIMARY KEY, username text, display_name text,
  bio text, created_at timestamp);

CREATE TABLE posts (post_id bigint PRIMARY KEY, author_id bigint, body text,
  hashtags list<text>, created_at timestamp);

CREATE TABLE posts_by_author (author_id bigint, created_at timestamp, post_id bigint,
  body text, PRIMARY KEY (author_id, created_at, post_id))
  WITH CLUSTERING ORDER BY (created_at DESC, post_id DESC);

CREATE TABLE user_timeline (user_id bigint, created_at timestamp, post_id bigint,
  author_id bigint, body text, PRIMARY KEY (user_id, created_at, post_id))
  WITH CLUSTERING ORDER BY (created_at DESC, post_id DESC);

CREATE TABLE posts_by_hashtag (hashtag text, created_at timestamp, post_id bigint,
  author_id bigint, body text, PRIMARY KEY (hashtag, created_at, post_id))
  WITH CLUSTERING ORDER BY (created_at DESC, post_id DESC);

CREATE TABLE following_by_user (follower_id bigint, followee_id bigint,
  followed_at timestamp, PRIMARY KEY (follower_id, followee_id));

CREATE TABLE followers_by_user (followee_id bigint, follower_id bigint,
  followed_at timestamp, PRIMARY KEY (followee_id, follower_id));

CREATE TABLE post_likes (post_id bigint, user_id bigint, PRIMARY KEY (post_id, user_id));

CREATE TABLE post_like_counts (post_id bigint PRIMARY KEY, like_count counter);
```
- Consistency `ONE`. All statements prepared.
- POST_CREATE: insert posts, posts_by_author, posts_by_hashtag per tag; select
  followers_by_user (limit cap); concurrent inserts into user_timeline
  (`join_all`, no multi-partition batches). Loader writes only the newest
  `timeline_len` rows per timeline; writes during runs are not trimmed.
- POST_LIKE: insert post_likes + counter update.
- USER_FOLLOW: insert both follow tables.
- Loader: concurrent prepared inserts, concurrency ~256.

### Neo4j (Community 5)
```cypher
CREATE CONSTRAINT user_id IF NOT EXISTS FOR (u:User) REQUIRE u.user_id IS UNIQUE;
CREATE CONSTRAINT post_id IF NOT EXISTS FOR (p:Post) REQUIRE p.post_id IS UNIQUE;
CREATE CONSTRAINT tag IF NOT EXISTS FOR (h:Hashtag) REQUIRE h.tag IS UNIQUE;
```
Nodes: `User{user_id, username, display_name, bio, created_at}`,
`Post{post_id, body, created_at}`, `Hashtag{tag}`.
Rels: `FOLLOWS{since}`, `POSTED`, `LIKED`, `TAGGED_WITH`. Likes exist only as
`LIKED` relationships.

- TIMELINE_READ:
  `MATCH (:User {user_id:$uid})-[:FOLLOWS]->(a:User)-[:POSTED]->(p:Post)
   RETURN p.post_id, p.body, p.created_at, a.user_id ORDER BY p.created_at DESC LIMIT 20`
- HASHTAG_SEARCH:
  `MATCH (:Hashtag {tag:$t})<-[:TAGGED_WITH]-(p:Post) RETURN p ORDER BY p.created_at DESC LIMIT 20`
- POST_CREATE: `MATCH (u:User {user_id:$uid}) MERGE (p:Post {post_id:$pid})
  ON CREATE SET p.body=$body, p.created_at=$ts MERGE (u)-[:POSTED]->(p)
  WITH p UNWIND $tags AS t MERGE (h:Hashtag {tag:t}) MERGE (p)-[:TAGGED_WITH]->(h)`
- POST_LIKE / USER_FOLLOW: `MATCH` both ends, `MERGE` the relationship.
- Loader: constraints first, then `UNWIND $rows` batches of 5,000 per transaction:
  users → hashtags → posts → POSTED/TAGGED_WITH → FOLLOWS.

---

## 11. Docker Compose (Docker Desktop)

- Pin images: `redis:7.4`, `mongo:8.0`, `cassandra:5.0`, `neo4j:5.26-community`.
- One profile per database: `docker compose --profile cassandra up -d`.
- Publish ports on `127.0.0.1` only. Mongo is published on host port 27018
  (*M4:* a native Windows MongoDB service owns 27017; the first Mongo load went
  into it because `compose up` failed to bind). `run_db.ps1` checks that the
  profile's host ports are free before `compose up` and names the owner if not.
- Every service: `mem_limit: 4g`, `cpus: 6`, healthcheck. No `cpuset` (VM CPU
  numbering doesn't map to host cores).
- Cassandra env: `MAX_HEAP_SIZE=2G`, `HEAP_NEWSIZE=400M`,
  `CASSANDRA_BROADCAST_RPC_ADDRESS=127.0.0.1` (*M4:* the driver reconnects to
  the advertised RPC address, which is otherwise the unreachable container IP). Health:
  `cqlsh -e 'describe keyspaces'`. `start_period: 120s`.
- Neo4j env: `NEO4J_AUTH=neo4j/benchpass`, `NEO4J_server_memory_heap_max__size=2G`,
  `NEO4J_server_memory_heap_initial__size=2G`, `NEO4J_server_memory_pagecache_size=1G`.
- Named volumes, removed with `down -v` after each database's sweep.
- *Decision (M3): no Docker VM CPU affinity.* Pinning the VM process
  (`vmmem` / `vmmemWSL`) away from the worker cores was tested and rejected.
  `scripts\affinity_ab.ps1` (elevated) ran redis-ping, closed, c=32, 60 s,
  3 rounds interleaved (none, 0xFC03, 0x0C03 per round), same boot:

  | VM mask | CPUs | ops/s | p50 µs | p99 µs | E-core load |
  |---|---|---|---|---|---|
  | none (all) | 0-15 | 73.1k-73.9k | 414-418 | 718-759 | 37-38% |
  | 0xFC03 | 0,1,10,11,12-15 | 63.4k-68.3k | 451-485 | 788-895 | 43-50% |
  | 0x0C03 | 0,1,10,11 | 70.8k-72.8k | 417-425 | 816-902 | 36-40% |

  No affinity wins on throughput, p50, p99 and run-to-run variance; 0xFC03
  pushes VM work onto the E-cores. Results: `results\vm_affinity\ab\`.
  (Numbers differ between boots: redis-ping c=32 "none" was ~82k ops/s on the
  previous boot, so only same-boot comparisons are meaningful.)
  `run_db.ps1` / `run_all.ps1` neither apply nor require an affinity.
  `set_vm_affinity.ps1`, `affinity_ab.ps1` and `bench --require-vm-affinity`
  are kept to reproduce the experiment. Every run records the VM's actual mask
  in `meta.json` (`system.vm_affinity.actual_masks`, via
  `GetProcessAffinityMask`); Windows denies that read unless `bench` runs
  elevated, in which case the access error is recorded instead of a mask.
- Docker Desktop here uses the WSL2 backend (containers run in `vmmemWSL`,
  recorded as `system.docker_kernel`). `.wslconfig` sets `vmIdleTimeout=-1`
  and Resource Saver is off, so the VM is not restarted between databases.

---

## 12. Sweep protocol (`scripts\run_db.ps1 -Db <db>`)

1. `sysinfo.ps1` → abort if on battery (`Win32_Battery.BatteryStatus -ne 2`);
   record power scheme (`powercfg /getactivescheme`).
2. `docker compose --profile <db> up -d`, poll `docker inspect` health until healthy.
3. `bench load --db <db>`.
4. Closed mode at concurrency 1, 8, 32, 128 × 3 runs.
5. Best closed-mode `ALL` throughput → open mode at 50%, 80%, 95% of it × 3 runs.
6. `docker compose --profile <db> down -v`.

`run_all.ps1`: noop calibration → redis-ping baseline (inside the redis profile)
→ redis → mongo → cassandra → neo4j, all with scenario `normal`; then a short
`celebrity` run per database (`run_db.ps1 -Scenario celebrity -Levels 8,32
-Runs 1 -NoOpen`, 60 s measure), results under `results\celebrity\`. Each
scenario loads its own dataset; loaders record the loaded scenario and adapters
refuse to run against a different one.

Writes during runs mutate state; data is reloaded per database, not per run.

*Validation vs final (M3 decision):* the per-milestone sweeps (M3–M5, under
`results\normal\`, `results\celebrity\`) are validation runs. Numbers drift
between boots (redis-ping c=32: ~82k vs ~74k ops/s) and even within a session
when Windows background jobs run (Redis c=128: 30.1k in the M3 sweep vs 20.8k
two hours later with BackgroundDownload and Disk Cleanup active). The final
numbers come from one `run_all.ps1` in a single session, written to
`results\final\` (noop calibration and redis-ping first). `sysinfo.ps1` warns
about busy background processes before each database.

Scripts must use `$ErrorActionPreference = 'Stop'` and check `$LASTEXITCODE`
after every native command.

---

## 13. Milestones and acceptance checks

**M1 — Skeleton.** Config, CLI, dataset + workload generation, `cpu.rs`.
✅ `bench run --db noop --dry-run` prints resolved config and the pinning plan
with correct P/E classification; same seed → same dataset hash (unit test).

**M2 — Engine + no-op.** Workers, closed & open loops, rings, aggregator, output,
`timeBeginPeriod`.
✅ `bench calibrate` (noop, closed, concurrency 128) reports throughput and
p50/p99; dropped_records = 0; open mode at 50% of that rate reports send_lag p99
under 1 ms.
✅ Show me the exact lines where t0/t1/intended are taken and explain why nothing
in the loop allocates.

**M3 — Redis + transport baseline.** Redis adapter, `redis_ping`, loader,
compose, `run_db.ps1`.
✅ redis-ping at concurrency 1, 8, 32, 128 produces results (this is the
transport floor). Full Redis sweep produces all CSVs. `docker exec` + `redis-cli`
shows a new post in a follower's timeline.

**M4 — MongoDB + Cassandra.**
✅ Each: loader completes; one smoke run per op kind with 0 errors; full sweep runs.

**M5 — Neo4j.**
✅ Same as M4. `EXPLAIN` of the timeline query uses the user_id constraint index.
  (Verified: `NodeUniqueIndexSeek` on `User(user_id)`, the index owned by the
  `user_id` constraint; plans saved in `results\neo4j_explain\`.)

**M6 — Plots + README.**
✅ `python plots\plot.py results` produces per-op p50/p99 vs concurrency,
throughput vs concurrency, and open-mode latency CDFs with all four DBs on shared
axes, with the redis-ping floor drawn as a reference line on latency plots.
README: one-command reproduction (`scripts\run_all.ps1`) and limitations
(laptop scale, single node, Windows host + Docker VM transport overhead as
measured by redis-ping, differing durability settings, and the host CPU
contention below).

*Limitation (M4 finding): the load generator and the Docker VM share host
CPU.* Open-mode send lag at 50% load was 0.9 ms p99 for Redis but 4.1 ms
(MongoDB) and 5.8 ms (Cassandra). Diagnostic, same session: the noop adapter
in open mode at 12,644 ops/s (Cassandra's 50% rate) had send-lag p99 26 µs
with the VM idle and 5,718 µs while `cassandra-stress` kept the container at
~590% CPU. No driver was involved, so the lateness comes from the VM's vCPUs
competing with the worker threads for physical cores, not from driver work.
It scales with how much CPU the database uses (Redis ~1 core, Cassandra and
MongoDB up to 6). Send lag is charged to latency, so open-mode tails of the
CPU-heavy databases include a few ms of client-side delay; closed-mode
latency is unaffected by send lag but shares the same CPU contention.
Pinning the VM away from the workers was tested and made results worse
(section 11). Engine settings were not changed.

---

## 14. Don'ts

- No io_uring, huge pages, custom allocators, unsafe code (except the `windows`
  crate calls in `cpu.rs`, kept minimal and commented).
  *Exception (M2):* a test-only counting allocator in `worker.rs`
  (`#[cfg(test)]`, wraps `System`) verifies that request loops make zero heap
  allocations in steady state. It is never compiled into the benchmark binary.
- Don't share a client/connection between workers.
- Don't `tokio::spawn` per request.
- Don't format strings or generate random numbers on the request path outside
  adapter calls.
- Don't silently swallow errors: count them.
- Don't "fix" slow numbers by changing the workload. Ask.
