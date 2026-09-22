//! Cassandra adapter (scylla driver). Fan-out on write: POST_CREATE inserts the
//! post into each follower's `user_timeline` partition (concurrent single-
//! partition inserts, no multi-partition batches).
//!
//! One `Session` per worker, `drivers.cassandra_connections` connections to
//! the node (CQL multiplexes many requests per connection). Consistency ONE.
//! All statements are prepared in `connect`.

use std::num::NonZeroUsize;

use futures::future::join_all;
use scylla::client::execution_profile::ExecutionProfile;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::client::PoolSize;
use scylla::errors::ExecutionError;
use scylla::statement::prepared::PreparedStatement;
use scylla::statement::Consistency;
use scylla::value::CqlTimestamp;

use super::{Adapter, AdapterError};
use crate::config::Config;
use crate::dataset::{tag_name, BodyArena};
use crate::workload::{Op, OpKind};

pub const KEYSPACE: &str = "social_bench";

pub async fn connect(cfg: &Config) -> anyhow::Result<Session> {
    // No driver request timeout: the engine's request_timeout_ms is the single
    // timeout for every database.
    let profile = ExecutionProfile::builder()
        .consistency(Consistency::One)
        .request_timeout(None)
        .build();
    let conns = NonZeroUsize::new(cfg.drivers.cassandra_connections as usize)
        .ok_or_else(|| anyhow::anyhow!("drivers.cassandra_connections must be >= 1"))?;
    Ok(SessionBuilder::new()
        .known_node(&cfg.endpoints.cassandra)
        .pool_size(PoolSize::PerHost(conns))
        .default_execution_profile_handle(profile.into_handle())
        .build()
        .await?)
}

fn map_err(e: ExecutionError) -> AdapterError {
    match e {
        ExecutionError::RequestTimeout(_) => AdapterError::Timeout,
        e => AdapterError::Db(e.to_string()),
    }
}

fn db_err(e: impl std::fmt::Display) -> AdapterError {
    AdapterError::Db(e.to_string())
}

/// Deserialize every row of a result as `$row` (fully consume the response).
/// A macro because rows borrow from the response frame (`&str` columns).
macro_rules! drain {
    ($res:expr, $row:ty) => {{
        let rows = $res.into_rows_result().map_err(db_err)?;
        for r in rows.rows::<$row>().map_err(db_err)? {
            std::hint::black_box(r.map_err(db_err)?);
        }
    }};
}

pub async fn loaded_scenario(session: &Session) -> anyhow::Result<Option<String>> {
    let res = session
        .query_unpaged(
            format!("SELECT v FROM {KEYSPACE}.bench_meta WHERE k = 'scenario'"),
            &[],
        )
        .await?;
    let rows = res.into_rows_result()?;
    Ok(rows.maybe_first_row::<(String,)>()?.map(|(v,)| v))
}

pub struct CassandraAdapter {
    session: Session,
    sel_user: PreparedStatement,
    sel_timeline: PreparedStatement,
    sel_tag: PreparedStatement,
    ins_like: PreparedStatement,
    upd_like_count: PreparedStatement,
    ins_following: PreparedStatement,
    ins_follower: PreparedStatement,
    ins_post: PreparedStatement,
    ins_post_by_author: PreparedStatement,
    ins_post_by_tag: PreparedStatement,
    sel_followers: PreparedStatement,
    ins_timeline: PreparedStatement,
    tag_names: Vec<String>,
    fanout_cap: i32,
}

impl Adapter for CassandraAdapter {
    async fn connect(cfg: &Config, _worker_id: u32) -> anyhow::Result<Self> {
        let session = connect(cfg).await?;
        let want = cfg.workload.scenario.as_str();
        let loaded = loaded_scenario(&session).await?;
        if loaded.as_deref() != Some(want) {
            anyhow::bail!(
                "Cassandra holds the {} dataset but this run uses scenario {want}; \
                 run `bench load --db cassandra` with the same scenario",
                loaded.as_deref().unwrap_or("(no)")
            );
        }
        session.use_keyspace(KEYSPACE, false).await?;
        let p = |q: &'static str| session.prepare(q);
        Ok(CassandraAdapter {
            sel_user: p("SELECT user_id, username, display_name, bio, created_at FROM users WHERE user_id = ?").await?,
            sel_timeline: p("SELECT post_id, author_id, body, created_at FROM user_timeline WHERE user_id = ? LIMIT 20").await?,
            sel_tag: p("SELECT post_id, author_id, body, created_at FROM posts_by_hashtag WHERE hashtag = ? LIMIT 20").await?,
            ins_like: p("INSERT INTO post_likes (post_id, user_id) VALUES (?, ?)").await?,
            upd_like_count: p("UPDATE post_like_counts SET like_count = like_count + 1 WHERE post_id = ?").await?,
            ins_following: p("INSERT INTO following_by_user (follower_id, followee_id, followed_at) VALUES (?, ?, ?)").await?,
            ins_follower: p("INSERT INTO followers_by_user (followee_id, follower_id, followed_at) VALUES (?, ?, ?)").await?,
            ins_post: p("INSERT INTO posts (post_id, author_id, body, hashtags, created_at) VALUES (?, ?, ?, ?, ?)").await?,
            ins_post_by_author: p("INSERT INTO posts_by_author (author_id, created_at, post_id, body) VALUES (?, ?, ?, ?)").await?,
            ins_post_by_tag: p("INSERT INTO posts_by_hashtag (hashtag, created_at, post_id, author_id, body) VALUES (?, ?, ?, ?, ?)").await?,
            sel_followers: p("SELECT follower_id FROM followers_by_user WHERE followee_id = ? LIMIT ?").await?,
            ins_timeline: p("INSERT INTO user_timeline (user_id, created_at, post_id, author_id, body) VALUES (?, ?, ?, ?, ?)").await?,
            session,
            tag_names: (0..cfg.dataset.hashtags)
                .map(|i| tag_name(i as u16))
                .collect(),
            fanout_cap: i32::try_from(cfg.dataset.fanout_cap).unwrap_or(i32::MAX),
        })
    }

    async fn execute(&self, op: &Op, bodies: &BodyArena) -> Result<(), AdapterError> {
        let s = &self.session;
        let uid = op.user as i64;
        match op.kind {
            OpKind::UserProfileRead => {
                let res = s
                    .execute_unpaged(&self.sel_user, (uid,))
                    .await
                    .map_err(map_err)?;
                drain!(res, (i64, &str, &str, &str, CqlTimestamp));
            }
            OpKind::TimelineRead => {
                let res = s
                    .execute_unpaged(&self.sel_timeline, (uid,))
                    .await
                    .map_err(map_err)?;
                drain!(res, (i64, i64, &str, CqlTimestamp));
            }
            OpKind::HashtagSearch => {
                let tag = self.tag_names[op.tags[0] as usize].as_str();
                let res = s
                    .execute_unpaged(&self.sel_tag, (tag,))
                    .await
                    .map_err(map_err)?;
                drain!(res, (i64, i64, &str, CqlTimestamp));
            }
            OpKind::PostLike => {
                let pid = op.target as i64;
                let (a, b) = futures::join!(
                    s.execute_unpaged(&self.ins_like, (pid, uid)),
                    s.execute_unpaged(&self.upd_like_count, (pid,)),
                );
                a.map_err(map_err)?;
                b.map_err(map_err)?;
            }
            OpKind::UserFollow => {
                let t = op.target as i64;
                let ts = CqlTimestamp(op.ts_ms);
                let (a, b) = futures::join!(
                    s.execute_unpaged(&self.ins_following, (uid, t, ts)),
                    s.execute_unpaged(&self.ins_follower, (t, uid, ts)),
                );
                a.map_err(map_err)?;
                b.map_err(map_err)?;
            }
            OpKind::PostCreate => {
                let pid = op.new_post_id as i64;
                let ts = CqlTimestamp(op.ts_ms);
                let body = bodies.get(op.body);
                let tags: Vec<&str> = op.tags[..op.ntags as usize]
                    .iter()
                    .map(|t| self.tag_names[*t as usize].as_str())
                    .collect();
                let (a, b, c) = futures::join!(
                    s.execute_unpaged(&self.ins_post, (pid, uid, body, &tags, ts)),
                    s.execute_unpaged(&self.ins_post_by_author, (uid, ts, pid, body)),
                    join_all(tags.iter().map(|&tag| {
                        s.execute_unpaged(&self.ins_post_by_tag, (tag, ts, pid, uid, body))
                    })),
                );
                a.map_err(map_err)?;
                b.map_err(map_err)?;
                for r in c {
                    r.map_err(map_err)?;
                }

                let res = s
                    .execute_unpaged(&self.sel_followers, (uid, self.fanout_cap))
                    .await
                    .map_err(map_err)?;
                let rows = res.into_rows_result().map_err(db_err)?;
                let mut followers: Vec<i64> = Vec::with_capacity(rows.rows_num());
                for r in rows.rows::<(i64,)>().map_err(db_err)? {
                    followers.push(r.map_err(db_err)?.0);
                }
                let results = join_all(
                    followers
                        .iter()
                        .map(|&f| s.execute_unpaged(&self.ins_timeline, (f, ts, pid, uid, body))),
                )
                .await;
                for r in results {
                    r.map_err(map_err)?;
                }
            }
        }
        Ok(())
    }
}
