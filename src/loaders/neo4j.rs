//! Neo4j loader: clear the graph, create the constraints, then `UNWIND $rows`
//! batches of 5,000 per transaction: users -> hashtags -> posts ->
//! POSTED / TAGGED_WITH -> FOLLOWS.

use std::collections::HashMap;

use anyhow::{bail, Result};
use neo4rs::{query, BoltType, Graph};

use super::Phase;
use crate::adapters::neo4j::connect;
use crate::config::Config;
use crate::dataset::{Dataset, DAY_MS, EPOCH_MS};

const BATCH: usize = 5_000;

const CONSTRAINTS: &[&str] = &[
    "CREATE CONSTRAINT user_id IF NOT EXISTS FOR (u:User) REQUIRE u.user_id IS UNIQUE",
    "CREATE CONSTRAINT post_id IF NOT EXISTS FOR (p:Post) REQUIRE p.post_id IS UNIQUE",
    "CREATE CONSTRAINT tag IF NOT EXISTS FOR (h:Hashtag) REQUIRE h.tag IS UNIQUE",
];

type Row = HashMap<&'static str, BoltType>;

/// Run `cypher` (which must `UNWIND $rows AS r`) over `rows` in transactions
/// of `BATCH` rows each.
async fn unwind(graph: &Graph, cypher: &str, rows: impl Iterator<Item = Row>) -> Result<u64> {
    let mut batch: Vec<Row> = Vec::with_capacity(BATCH);
    let mut n = 0u64;
    let flush = |batch: Vec<Row>| {
        let q = query(cypher).param("rows", batch);
        async move {
            let mut txn = graph.start_txn().await?;
            txn.run(q).await?;
            txn.commit().await?;
            anyhow::Ok(())
        }
    };
    for r in rows {
        batch.push(r);
        if batch.len() == BATCH {
            n += batch.len() as u64;
            flush(std::mem::replace(&mut batch, Vec::with_capacity(BATCH))).await?;
        }
    }
    if !batch.is_empty() {
        n += batch.len() as u64;
        flush(batch).await?;
    }
    Ok(n)
}

async fn count(graph: &Graph, cypher: &str) -> Result<i64> {
    let mut rows = graph.execute(query(cypher)).await?;
    let mut c = 0;
    while let Some(row) = rows.next().await? {
        c = row.get::<i64>("c")?;
    }
    Ok(c)
}

fn row<const N: usize>(pairs: [(&'static str, BoltType); N]) -> Row {
    HashMap::from(pairs)
}

pub async fn load(cfg: &Config, ds: &Dataset) -> Result<()> {
    let g = connect(cfg).await?;

    // Clear any previous data in batches (a fresh container is empty).
    loop {
        let deleted = count(
            &g,
            "MATCH (n) WITH n LIMIT 20000 DETACH DELETE n RETURN count(*) AS c",
        )
        .await?;
        if deleted == 0 {
            break;
        }
    }
    for c in CONSTRAINTS {
        g.run(query(c)).await?;
    }
    eprintln!(
        "  graph cleared, {} constraints in place",
        CONSTRAINTS.len()
    );

    let ph = Phase::start("users");
    let n = unwind(
        &g,
        "UNWIND $rows AS r CREATE (:User {user_id: r.id, username: r.username, \
         display_name: r.dn, bio: r.bio, created_at: r.ts})",
        ds.users.iter().map(|u| {
            row([
                ("id", (u.id as i64).into()),
                ("username", u.username.as_str().into()),
                ("dn", u.display_name.as_str().into()),
                ("bio", u.bio.as_str().into()),
                ("ts", u.created_at_ms.into()),
            ])
        }),
    )
    .await?;
    ph.done(n);

    let ph = Phase::start("hashtags");
    let n = unwind(
        &g,
        "UNWIND $rows AS r CREATE (:Hashtag {tag: r.tag})",
        ds.tag_names
            .iter()
            .map(|t| row([("tag", t.as_str().into())])),
    )
    .await?;
    ph.done(n);

    let ph = Phase::start("posts");
    let n = unwind(
        &g,
        "UNWIND $rows AS r CREATE (:Post {post_id: r.id, body: r.body, created_at: r.ts})",
        ds.posts.iter().map(|p| {
            row([
                ("id", (p.id as i64).into()),
                ("body", ds.bodies.get(p.id as u32).into()),
                ("ts", p.created_at_ms.into()),
            ])
        }),
    )
    .await?;
    ph.done(n);

    let ph = Phase::start("POSTED, TAGGED_WITH");
    let mut n = unwind(
        &g,
        "UNWIND $rows AS r MATCH (u:User {user_id: r.a}), (p:Post {post_id: r.p}) \
         CREATE (u)-[:POSTED]->(p)",
        ds.posts
            .iter()
            .map(|p| row([("a", (p.author as i64).into()), ("p", (p.id as i64).into())])),
    )
    .await?;
    n += unwind(
        &g,
        "UNWIND $rows AS r MATCH (p:Post {post_id: r.p}), (h:Hashtag {tag: r.t}) \
         CREATE (p)-[:TAGGED_WITH]->(h)",
        ds.posts.iter().flat_map(|p| {
            p.tags().iter().map(move |t| {
                row([
                    ("p", (p.id as i64).into()),
                    ("t", ds.tag_names[*t as usize].as_str().into()),
                ])
            })
        }),
    )
    .await?;
    ph.done(n);

    let ph = Phase::start("FOLLOWS");
    let since = EPOCH_MS - 30 * DAY_MS;
    let n = unwind(
        &g,
        "UNWIND $rows AS r MATCH (a:User {user_id: r.f}), (b:User {user_id: r.t}) \
         CREATE (a)-[:FOLLOWS {since: r.ts}]->(b)",
        (0..ds.users.len() as u64).flat_map(|u| {
            ds.following(u).iter().map(move |&t| {
                row([
                    ("f", (u as i64).into()),
                    ("t", (t as i64).into()),
                    ("ts", since.into()),
                ])
            })
        }),
    )
    .await?;
    ph.done(n);

    // Sanity check against the generated dataset.
    let users = count(&g, "MATCH (u:User) RETURN count(u) AS c").await?;
    let posts = count(&g, "MATCH (p:Post) RETURN count(p) AS c").await?;
    let follows = count(&g, "MATCH ()-[f:FOLLOWS]->() RETURN count(f) AS c").await?;
    let posted = count(&g, "MATCH ()-[r:POSTED]->() RETURN count(r) AS c").await?;
    if users != ds.users.len() as i64
        || posts != ds.posts.len() as i64
        || follows != ds.follow_targets.len() as i64
        || posted != ds.posts.len() as i64
    {
        bail!("loaded users={users} posts={posts} follows={follows} posted={posted} do not match the dataset");
    }
    g.run(
        query("MERGE (m:BenchMeta {k: 'scenario'}) SET m.v = $v")
            .param("v", cfg.workload.scenario.as_str()),
    )
    .await?;
    eprintln!("  loaded: {users} users, {posts} posts, {follows} FOLLOWS, {posted} POSTED");
    Ok(())
}
