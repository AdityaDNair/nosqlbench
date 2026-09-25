# Database dumps

Portable dumps of the benchmark dataset (scenario `normal`, seed 42,
`config\default.toml`) as loaded by `bench load`, before any benchmark run has
written to it. One file per database. `manifest.json` records each file's size,
SHA-256, the Docker image it came from, the git commit, and how long the load
and export took.

Produced by:

```powershell
scripts\make_dumps.ps1            # all four; -Db redis,mongo to pick; -Force to overwrite
```

For each database in turn, the script runs `docker compose --profile <db> up -d`,
waits until the container is healthy, runs `bench load --db <db>` with
`workload.scenario="normal"`, exports, then runs `docker compose --profile <db> down -v`.
Only one database container runs at a time.

| File | Size | Tool | Contents |
|---|---|---|---|
| `redis.rdb` | 44.3 MB | `redis-cli --rdb` | RDB snapshot, pulled over the replication protocol (the server runs with persistence off) |
| `mongo.archive.gz` | 8.4 MB | `mongodump --db social_bench --archive --gzip` | 4 collections and their indexes |
| `cassandra_cql.tar.gz` | 94.7 MB | `cqlsh DESCRIBE KEYSPACE` + `COPY TO` | `schema.cql` plus one CSV with a header row per table |
| `neo4j.dump` | 42.3 MB | `neo4j-admin database dump neo4j` | Full store of the `neo4j` database, including constraints |

What's in each dump (the same dataset in each database's schema, see CLAUDE.md §10):

- 10,000 users, 100,000 posts, 1,000 hashtags, 199,589 follows, no likes.
- Redis: 140,783 keys, including 1,511,681 timeline entries (newest 800 per user).
- Cassandra: `user_timeline` has 1,511,681 rows (as a CSV, 312 MB uncompressed).
- Each dump contains the scenario marker (`bench:scenario` in Redis, a
  `bench_meta` collection in MongoDB, a `bench_meta` table in Cassandra,
  a `BenchMeta` node in Neo4j). The adapters check it, so `bench run` works
  directly against a restored database.

**The SHA-256s in `manifest.json` identify these exact files, but they are
not reproducible.** Re-running the script loads the same data but produces
different bytes each time:

- `redis.rdb`: the header holds `ctime`, `used-mem` and a random `repl-id`,
  and keys and hash fields are written in dict order, which depends on
  Redis's per-process random hash seed.
- `mongo.archive.gz`: mongodump dumps collections in parallel, so the order
  of collections and blocks in the archive varies.
- `cassandra_cql.tar.gz`: `COPY TO` splits the token ring across worker
  processes and writes rows in arrival order; tar headers hold file times.
- `neo4j.dump`: the store's bytes differ between loads (not traced further).

To check that two dumps hold the same data, compare the content instead:
`redis-cli DEBUG DIGEST` (server started with `--enable-debug-command yes`),
`db.runCommand({dbHash: 1})` in MongoDB, sorted CSVs for Cassandra, and a
sorted Cypher export of all node and relationship properties for Neo4j. Two
consecutive runs on 2026-09-25 matched on all four checks.

## Restoring

Run these from the repo root. They use the compose services, so the restored
database is where `bench run` expects it. Each procedure below was run against
the dumps in this directory, followed by
`bench run --db <db> --mode closed --concurrency 8`, which completed with
0 errors on all four databases.

### Redis (`redis:7.4`)

Put the file where Redis looks for it at startup (`/data/dump.rdb`) before the
server starts. It is loaded even with `--save ""`.

```powershell
docker compose --profile redis create
docker cp dumps\redis.rdb nosqlbench-redis:/data/dump.rdb
docker compose --profile redis start
docker exec nosqlbench-redis redis-cli dbsize          # 140783
```

RDB format version 12, readable by Redis 7.4 or newer.

### MongoDB (`mongo:8.0`)

```powershell
docker compose --profile mongo up -d                   # wait until healthy
docker cp dumps\mongo.archive.gz nosqlbench-mongo:/tmp/mongo.archive.gz
docker exec nosqlbench-mongo mongorestore --archive=/tmp/mongo.archive.gz --gzip --drop
```

Indexes are restored from the archive metadata. Outside Docker:
`mongorestore --uri <uri> --archive=mongo.archive.gz --gzip --drop`.

### Cassandra (`cassandra:5.0`)

```powershell
docker compose --profile cassandra up -d               # wait until healthy (~2 min)
docker cp dumps\cassandra_cql.tar.gz nosqlbench-cassandra:/tmp/cassandra_cql.tar.gz
docker exec nosqlbench-cassandra sh -c 'mkdir -p /tmp/cassdump && tar -xzf /tmp/cassandra_cql.tar.gz -C /tmp/cassdump'
docker exec nosqlbench-cassandra cqlsh -f /tmp/cassdump/schema.cql
foreach ($t in 'users','posts','posts_by_author','posts_by_hashtag','user_timeline',
               'following_by_user','followers_by_user','post_likes','post_like_counts','bench_meta') {
    docker exec nosqlbench-cassandra cqlsh --request-timeout=600 -e "COPY social_bench.$t FROM '/tmp/cassdump/$t.csv' WITH HEADER = true"
}
```

The CSVs load into any Cassandra or ScyllaDB version. `schema.cql` comes from
Cassandra 5.0's `DESCRIBE`, so older versions may reject some table options;
the plain definitions are in CLAUDE.md §10. `post_likes` and `post_like_counts`
are empty because no likes are pre-loaded.

This is a CQL export rather than `nodetool snapshot`, because SSTable snapshots
depend on the Cassandra version and on table IDs, so they are not portable.

### Neo4j (`neo4j:5.26-community`)

`neo4j-admin database load` needs the database offline, so load into the
compose volume from a one-off container before Neo4j starts:

```powershell
docker compose --profile neo4j create
docker create --name nosqlbench-neo4j-load -v nosqlbench_neo4j-data:/data neo4j:5.26-community `
    neo4j-admin database load neo4j --from-path=/tmp --overwrite-destination=true
docker cp dumps\neo4j.dump nosqlbench-neo4j-load:/tmp/neo4j.dump
docker start -a nosqlbench-neo4j-load
docker rm -v nosqlbench-neo4j-load
docker compose --profile neo4j start
```

Tested with 5.26; `neo4j-admin` loads dumps from the same or an older
version, so newer Neo4j releases should also accept it. The `user_id`, `post_id` and `tag`
uniqueness constraints are part of the dump.

Tear down with `docker compose --profile <db> down -v` as usual.
