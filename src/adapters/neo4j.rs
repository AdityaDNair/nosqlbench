//! Neo4j adapter. Fan-out on read: TIMELINE_READ traverses
//! `(:User)-[:FOLLOWS]->(:User)-[:POSTED]->(:Post)`; POST_CREATE only stores
//! the post and its relationships.
//!
//! One `Graph` per worker, pool size `drivers.neo4j_max_connections` (Bolt:
//! one query per connection at a time). All queries are parameterised; the
//! Cypher text is fixed in `connect`.

use neo4rs::{query, ConfigBuilder, Graph};

use super::{Adapter, AdapterError};
use crate::config::Config;
use crate::dataset::{tag_name, BodyArena};
use crate::workload::{Op, OpKind};

const Q_PROFILE: &str = "MATCH (u:User {user_id: $uid}) RETURN u";
const Q_TIMELINE: &str = "MATCH (:User {user_id: $uid})-[:FOLLOWS]->(a:User)-[:POSTED]->(p:Post) \
     RETURN p.post_id, p.body, p.created_at, a.user_id ORDER BY p.created_at DESC LIMIT 20";
const Q_HASHTAG: &str = "MATCH (:Hashtag {tag: $t})<-[:TAGGED_WITH]-(p:Post) \
     RETURN p ORDER BY p.created_at DESC LIMIT 20";
const Q_POST_CREATE: &str = "MATCH (u:User {user_id: $uid}) \
     MERGE (p:Post {post_id: $pid}) ON CREATE SET p.body = $body, p.created_at = $ts \
     MERGE (u)-[:POSTED]->(p) \
     WITH p UNWIND $tags AS t MERGE (h:Hashtag {tag: t}) MERGE (p)-[:TAGGED_WITH]->(h)";
const Q_LIKE: &str =
    "MATCH (u:User {user_id: $uid}), (p:Post {post_id: $pid}) MERGE (u)-[:LIKED]->(p)";
const Q_FOLLOW: &str = "MATCH (a:User {user_id: $uid}), (b:User {user_id: $t}) \
     MERGE (a)-[f:FOLLOWS]->(b) ON CREATE SET f.since = $ts";

pub async fn connect(cfg: &Config) -> anyhow::Result<Graph> {
    let config = ConfigBuilder::default()
        .uri(&cfg.endpoints.neo4j)
        .user(&cfg.endpoints.neo4j_user)
        .password(&cfg.endpoints.neo4j_pass)
        .max_connections(cfg.drivers.neo4j_max_connections as usize)
        .build()?;
    Ok(Graph::connect(config).await?)
}

fn map_err(e: neo4rs::Error) -> AdapterError {
    match &e {
        neo4rs::Error::IOError { detail } if detail.kind() == std::io::ErrorKind::TimedOut => {
            AdapterError::Timeout
        }
        _ => AdapterError::Db(e.to_string()),
    }
}

/// Scenario recorded by the loader in a `(:BenchMeta {k: 'scenario'})` node.
pub async fn loaded_scenario(graph: &Graph) -> anyhow::Result<Option<String>> {
    let mut rows = graph
        .execute(query("MATCH (m:BenchMeta {k: 'scenario'}) RETURN m.v AS v"))
        .await?;
    let mut v = None;
    while let Some(row) = rows.next().await? {
        v = Some(row.get::<String>("v")?);
    }
    Ok(v)
}

pub struct Neo4jAdapter {
    graph: Graph,
    tag_names: Vec<String>,
}

impl Neo4jAdapter {
    /// Run a query and read every row (fully consume the response).
    async fn drain(&self, q: neo4rs::Query) -> Result<(), AdapterError> {
        let mut rows = self.graph.execute(q).await.map_err(map_err)?;
        while let Some(row) = rows.next().await.map_err(map_err)? {
            std::hint::black_box(&row);
        }
        Ok(())
    }
}

impl Adapter for Neo4jAdapter {
    async fn connect(cfg: &Config, _worker_id: u32) -> anyhow::Result<Self> {
        let graph = connect(cfg).await?;
        let want = cfg.workload.scenario.as_str();
        let loaded = loaded_scenario(&graph).await?;
        if loaded.as_deref() != Some(want) {
            anyhow::bail!(
                "Neo4j holds the {} dataset but this run uses scenario {want}; \
                 run `bench load --db neo4j` with the same scenario",
                loaded.as_deref().unwrap_or("(no)")
            );
        }
        Ok(Neo4jAdapter {
            graph,
            tag_names: (0..cfg.dataset.hashtags)
                .map(|i| tag_name(i as u16))
                .collect(),
        })
    }

    async fn execute(&self, op: &Op, bodies: &BodyArena) -> Result<(), AdapterError> {
        let uid = op.user as i64;
        match op.kind {
            OpKind::UserProfileRead => self.drain(query(Q_PROFILE).param("uid", uid)).await,
            OpKind::TimelineRead => self.drain(query(Q_TIMELINE).param("uid", uid)).await,
            OpKind::HashtagSearch => {
                let tag = self.tag_names[op.tags[0] as usize].as_str();
                self.drain(query(Q_HASHTAG).param("t", tag)).await
            }
            OpKind::PostLike => {
                self.drain(
                    query(Q_LIKE)
                        .param("uid", uid)
                        .param("pid", op.target as i64),
                )
                .await
            }
            OpKind::UserFollow => {
                self.drain(
                    query(Q_FOLLOW)
                        .param("uid", uid)
                        .param("t", op.target as i64)
                        .param("ts", op.ts_ms),
                )
                .await
            }
            OpKind::PostCreate => {
                let tags: Vec<&str> = op.tags[..op.ntags as usize]
                    .iter()
                    .map(|t| self.tag_names[*t as usize].as_str())
                    .collect();
                self.drain(
                    query(Q_POST_CREATE)
                        .param("uid", uid)
                        .param("pid", op.new_post_id as i64)
                        .param("body", bodies.get(op.body))
                        .param("ts", op.ts_ms)
                        .param("tags", tags),
                )
                .await
            }
        }
    }
}
