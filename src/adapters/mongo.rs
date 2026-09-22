//! MongoDB adapter. Fan-out on read: TIMELINE_READ looks up the followees and
//! queries their newest posts; POST_CREATE only stores the post.
//!
//! One `Client` per worker, pool size `drivers.mongo_max_pool_size` (one
//! request per connection at a time). Write concern w:1 (server default).

use futures::TryStreamExt;
use mongodb::bson::{doc, DateTime, Document};
use mongodb::error::{Error, ErrorKind, WriteFailure};
use mongodb::options::ClientOptions;
use mongodb::{Client, Collection, Database};

use super::{Adapter, AdapterError};
use crate::config::Config;
use crate::dataset::{tag_name, BodyArena};
use crate::workload::{Op, OpKind};

pub const DB: &str = "social_bench";
/// Collection holding `{_id: "scenario", value}`, written by the loader.
pub const META: &str = "bench_meta";
/// Posts returned by TIMELINE_READ and HASHTAG_SEARCH.
const PAGE: i64 = 20;

pub async fn connect(cfg: &Config) -> anyhow::Result<Client> {
    let mut o = ClientOptions::parse(&cfg.endpoints.mongo).await?;
    o.max_pool_size = Some(cfg.drivers.mongo_max_pool_size);
    o.app_name = Some("nosqlbench".into());
    Ok(Client::with_options(o)?)
}

/// Duplicate key (E11000): expected for idempotent re-inserts.
pub fn is_duplicate(e: &Error) -> bool {
    matches!(&*e.kind, ErrorKind::Write(WriteFailure::WriteError(w)) if w.code == 11000)
}

fn map_err(e: Error) -> AdapterError {
    match &*e.kind {
        ErrorKind::Io(io) if io.kind() == std::io::ErrorKind::TimedOut => AdapterError::Timeout,
        _ => AdapterError::Db(e.to_string()),
    }
}

/// Ignore duplicate-key errors, map everything else.
fn ok_if_duplicate<T>(r: Result<T, Error>) -> Result<(), AdapterError> {
    match r {
        Ok(_) => Ok(()),
        Err(e) if is_duplicate(&e) => Ok(()),
        Err(e) => Err(map_err(e)),
    }
}

pub struct MongoAdapter {
    users: Collection<Document>,
    posts: Collection<Document>,
    follows: Collection<Document>,
    likes: Collection<Document>,
    tag_names: Vec<String>,
}

impl MongoAdapter {
    /// Read every document of a cursor (fully consume the response).
    async fn drain(cursor: Result<mongodb::Cursor<Document>, Error>) -> Result<(), AdapterError> {
        let mut cursor = cursor.map_err(map_err)?;
        while let Some(d) = cursor.try_next().await.map_err(map_err)? {
            std::hint::black_box(&d);
        }
        Ok(())
    }
}

pub async fn loaded_scenario(db: &Database) -> anyhow::Result<Option<String>> {
    let d = db
        .collection::<Document>(META)
        .find_one(doc! { "_id": "scenario" })
        .await?;
    Ok(d.and_then(|d| d.get_str("value").ok().map(str::to_string)))
}

impl Adapter for MongoAdapter {
    async fn connect(cfg: &Config, _worker_id: u32) -> anyhow::Result<Self> {
        let db = connect(cfg).await?.database(DB);
        let want = cfg.workload.scenario.as_str();
        let loaded = loaded_scenario(&db).await?;
        if loaded.as_deref() != Some(want) {
            anyhow::bail!(
                "MongoDB holds the {} dataset but this run uses scenario {want}; \
                 run `bench load --db mongo` with the same scenario",
                loaded.as_deref().unwrap_or("(no)")
            );
        }
        Ok(MongoAdapter {
            users: db.collection("users"),
            posts: db.collection("posts"),
            follows: db.collection("follows"),
            likes: db.collection("likes"),
            tag_names: (0..cfg.dataset.hashtags)
                .map(|i| tag_name(i as u16))
                .collect(),
        })
    }

    async fn execute(&self, op: &Op, bodies: &BodyArena) -> Result<(), AdapterError> {
        let uid = op.user as i64;
        match op.kind {
            OpKind::UserProfileRead => {
                let d = self
                    .users
                    .find_one(doc! { "_id": uid })
                    .await
                    .map_err(map_err)?;
                std::hint::black_box(&d);
            }
            OpKind::TimelineRead => {
                let mut cursor = self
                    .follows
                    .find(doc! { "follower_id": uid })
                    .projection(doc! { "followee_id": 1, "_id": 0 })
                    .await
                    .map_err(map_err)?;
                let mut followees: Vec<i64> = Vec::with_capacity(32);
                while let Some(d) = cursor.try_next().await.map_err(map_err)? {
                    if let Ok(f) = d.get_i64("followee_id") {
                        followees.push(f);
                    }
                }
                Self::drain(
                    self.posts
                        .find(doc! { "author_id": { "$in": followees } })
                        .sort(doc! { "created_at": -1 })
                        .limit(PAGE)
                        .await,
                )
                .await?;
            }
            OpKind::HashtagSearch => {
                let tag = &self.tag_names[op.tags[0] as usize];
                Self::drain(
                    self.posts
                        .find(doc! { "hashtags": tag })
                        .sort(doc! { "created_at": -1 })
                        .limit(PAGE)
                        .await,
                )
                .await?;
            }
            OpKind::PostLike => {
                let pid = op.target as i64;
                ok_if_duplicate(
                    self.likes
                        .insert_one(doc! { "_id": { "u": uid, "p": pid } })
                        .await,
                )?;
                self.posts
                    .update_one(doc! { "_id": pid }, doc! { "$inc": { "like_count": 1 } })
                    .await
                    .map_err(map_err)?;
            }
            OpKind::UserFollow => {
                let t = op.target as i64;
                ok_if_duplicate(
                    self.follows
                        .insert_one(doc! {
                            "_id": { "f": uid, "t": t },
                            "follower_id": uid,
                            "followee_id": t,
                            "followed_at": DateTime::from_millis(op.ts_ms),
                        })
                        .await,
                )?;
            }
            OpKind::PostCreate => {
                let tags: Vec<&str> = op.tags[..op.ntags as usize]
                    .iter()
                    .map(|t| self.tag_names[*t as usize].as_str())
                    .collect();
                ok_if_duplicate(
                    self.posts
                        .insert_one(doc! {
                            "_id": op.new_post_id as i64,
                            "author_id": uid,
                            "body": bodies.get(op.body),
                            "hashtags": tags,
                            "like_count": 0i64,
                            "created_at": DateTime::from_millis(op.ts_ms),
                        })
                        .await,
                )?;
            }
        }
        Ok(())
    }
}
