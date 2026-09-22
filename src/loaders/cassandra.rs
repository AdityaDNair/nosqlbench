//! Cassandra loader: drop and recreate the keyspace, then concurrent prepared
//! inserts (256 in flight). Timelines get only the newest `timeline_len` rows.

use std::future::Future;

use anyhow::{bail, Result};
use futures::stream::{self, StreamExt};
use scylla::client::session::Session;
use scylla::errors::ExecutionError;
use scylla::response::query_result::QueryResult;
use scylla::value::CqlTimestamp;

use super::Phase;
use crate::adapters::cassandra::{connect, KEYSPACE};
use crate::config::Config;
use crate::dataset::{Dataset, DAY_MS, EPOCH_MS};

const CONCURRENCY: usize = 256;

const SCHEMA: &[&str] = &[
    "CREATE KEYSPACE social_bench WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}",
    "CREATE TABLE social_bench.users (user_id bigint PRIMARY KEY, username text, display_name text, bio text, created_at timestamp)",
    "CREATE TABLE social_bench.posts (post_id bigint PRIMARY KEY, author_id bigint, body text, hashtags list<text>, created_at timestamp)",
    "CREATE TABLE social_bench.posts_by_author (author_id bigint, created_at timestamp, post_id bigint, body text, PRIMARY KEY (author_id, created_at, post_id)) WITH CLUSTERING ORDER BY (created_at DESC, post_id DESC)",
    "CREATE TABLE social_bench.user_timeline (user_id bigint, created_at timestamp, post_id bigint, author_id bigint, body text, PRIMARY KEY (user_id, created_at, post_id)) WITH CLUSTERING ORDER BY (created_at DESC, post_id DESC)",
    "CREATE TABLE social_bench.posts_by_hashtag (hashtag text, created_at timestamp, post_id bigint, author_id bigint, body text, PRIMARY KEY (hashtag, created_at, post_id)) WITH CLUSTERING ORDER BY (created_at DESC, post_id DESC)",
    "CREATE TABLE social_bench.following_by_user (follower_id bigint, followee_id bigint, followed_at timestamp, PRIMARY KEY (follower_id, followee_id))",
    "CREATE TABLE social_bench.followers_by_user (followee_id bigint, follower_id bigint, followed_at timestamp, PRIMARY KEY (followee_id, follower_id))",
    "CREATE TABLE social_bench.post_likes (post_id bigint, user_id bigint, PRIMARY KEY (post_id, user_id))",
    "CREATE TABLE social_bench.post_like_counts (post_id bigint PRIMARY KEY, like_count counter)",
    // Benchmark bookkeeping: which scenario's dataset is loaded.
    "CREATE TABLE social_bench.bench_meta (k text PRIMARY KEY, v text)",
];

/// Run the futures with at most `CONCURRENCY` in flight; fail on the first error.
async fn run_bounded<F>(futs: impl Iterator<Item = F>) -> Result<u64>
where
    F: Future<Output = Result<QueryResult, ExecutionError>>,
{
    let mut s = stream::iter(futs).buffer_unordered(CONCURRENCY);
    let mut n = 0u64;
    while let Some(r) = s.next().await {
        r?;
        n += 1;
    }
    Ok(n)
}

async fn count(session: &Session, q: &str) -> Result<i64> {
    let res = session.query_unpaged(q.to_string(), &[]).await?;
    Ok(res.into_rows_result()?.first_row::<(i64,)>()?.0)
}

pub async fn load(cfg: &Config, ds: &Dataset) -> Result<()> {
    let session = connect(cfg).await?;
    session
        .query_unpaged(format!("DROP KEYSPACE IF EXISTS {KEYSPACE}"), &[])
        .await?;
    for ddl in SCHEMA {
        session.query_unpaged(*ddl, &[]).await?;
    }
    session.await_schema_agreement().await?;
    session.use_keyspace(KEYSPACE, false).await?;
    eprintln!(
        "  keyspace {KEYSPACE} recreated ({} statements)",
        SCHEMA.len()
    );

    let s = &session;
    let ts = |ms: i64| CqlTimestamp(ms);

    let ph = Phase::start("users");
    let st = s.prepare("INSERT INTO users (user_id, username, display_name, bio, created_at) VALUES (?, ?, ?, ?, ?)").await?;
    let n = run_bounded(ds.users.iter().map(|u| {
        s.execute_unpaged(
            &st,
            (
                u.id as i64,
                u.username.as_str(),
                u.display_name.as_str(),
                u.bio.as_str(),
                ts(u.created_at_ms),
            ),
        )
    }))
    .await?;
    ph.done(n);

    let tags_of = |p: &crate::dataset::Post| -> Vec<&str> {
        p.tags()
            .iter()
            .map(|t| ds.tag_names[*t as usize].as_str())
            .collect()
    };

    let ph = Phase::start("posts, posts_by_author, posts_by_hashtag");
    let st_post = s.prepare("INSERT INTO posts (post_id, author_id, body, hashtags, created_at) VALUES (?, ?, ?, ?, ?)").await?;
    let st_author = s.prepare("INSERT INTO posts_by_author (author_id, created_at, post_id, body) VALUES (?, ?, ?, ?)").await?;
    let st_tag = s.prepare("INSERT INTO posts_by_hashtag (hashtag, created_at, post_id, author_id, body) VALUES (?, ?, ?, ?, ?)").await?;
    let mut n = run_bounded(ds.posts.iter().map(|p| {
        let tags = tags_of(p);
        let st_post = &st_post;
        async move {
            s.execute_unpaged(
                st_post,
                (
                    p.id as i64,
                    p.author as i64,
                    ds.bodies.get(p.id as u32),
                    tags,
                    ts(p.created_at_ms),
                ),
            )
            .await
        }
    }))
    .await?;
    n += run_bounded(ds.posts.iter().map(|p| {
        s.execute_unpaged(
            &st_author,
            (
                p.author as i64,
                ts(p.created_at_ms),
                p.id as i64,
                ds.bodies.get(p.id as u32),
            ),
        )
    }))
    .await?;
    n += run_bounded(ds.posts.iter().flat_map(|p| {
        let st_tag = &st_tag;
        p.tags().iter().map(move |t| {
            s.execute_unpaged(
                st_tag,
                (
                    ds.tag_names[*t as usize].as_str(),
                    ts(p.created_at_ms),
                    p.id as i64,
                    p.author as i64,
                    ds.bodies.get(p.id as u32),
                ),
            )
        })
    }))
    .await?;
    ph.done(n);

    let ph = Phase::start("following_by_user, followers_by_user");
    let followed_at = ts(EPOCH_MS - 30 * DAY_MS);
    let st_following = s.prepare("INSERT INTO following_by_user (follower_id, followee_id, followed_at) VALUES (?, ?, ?)").await?;
    let st_follower = s.prepare("INSERT INTO followers_by_user (followee_id, follower_id, followed_at) VALUES (?, ?, ?)").await?;
    let pairs = || {
        (0..ds.users.len() as u64)
            .flat_map(|u| ds.following(u).iter().map(move |&t| (u as i64, t as i64)))
    };
    let mut n =
        run_bounded(pairs().map(|(u, t)| s.execute_unpaged(&st_following, (u, t, followed_at))))
            .await?;
    n += run_bounded(pairs().map(|(u, t)| s.execute_unpaged(&st_follower, (t, u, followed_at))))
        .await?;
    ph.done(n);

    let ph = Phase::start("user_timeline (fan-out, trimmed)");
    let (off, tl) = ds.timelines();
    let st_tl = s.prepare("INSERT INTO user_timeline (user_id, created_at, post_id, author_id, body) VALUES (?, ?, ?, ?, ?)").await?;
    let n = run_bounded((0..ds.users.len()).flat_map(|u| {
        let st_tl = &st_tl;
        tl[off[u] as usize..off[u + 1] as usize]
            .iter()
            .map(move |&pid| {
                let p = &ds.posts[pid as usize];
                s.execute_unpaged(
                    st_tl,
                    (
                        u as i64,
                        ts(p.created_at_ms),
                        p.id as i64,
                        p.author as i64,
                        ds.bodies.get(p.id as u32),
                    ),
                )
            })
    }))
    .await?;
    ph.done(n);

    // Sanity checks on single partitions (full-table COUNT is too slow).
    let tl0 = count(s, "SELECT COUNT(*) FROM user_timeline WHERE user_id = 0").await?;
    let want_tl0 = (off[1] - off[0]) as i64;
    let (fo, _) = ds.followers();
    let fol0 = count(
        s,
        "SELECT COUNT(*) FROM followers_by_user WHERE followee_id = 0",
    )
    .await?;
    let want_fol0 = (fo[1] - fo[0]) as i64;
    if tl0 != want_tl0 || fol0 != want_fol0 {
        bail!("user 0: timeline {tl0} (want {want_tl0}), followers {fol0} (want {want_fol0})");
    }
    s.query_unpaged(
        "INSERT INTO bench_meta (k, v) VALUES ('scenario', ?)",
        (cfg.workload.scenario.as_str(),),
    )
    .await?;
    eprintln!(
        "  loaded: {} users, {} posts, {} follows, {} timeline rows; user 0 timeline {tl0}, followers {fol0}",
        ds.users.len(),
        ds.posts.len(),
        ds.follow_targets.len(),
        tl.len()
    );
    Ok(())
}
