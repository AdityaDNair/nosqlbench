//! Redis adapter. Fan-out on write: POST_CREATE pushes the post into each
//! follower's timeline ZSET and trims it to `timeline_len`.
//!
//! One multiplexed connection per worker; the worker's request loops share it
//! (requests are pipelined on the wire by the driver).

use std::time::Duration;

use redis::aio::MultiplexedConnection;
use redis::{AsyncConnectionConfig, RedisError, Value};

use super::{Adapter, AdapterError};
use crate::config::Config;
use crate::dataset::{tag_name, BodyArena};
use crate::workload::{Op, OpKind};

/// Posts returned by TIMELINE_READ and HASHTAG_SEARCH.
const PAGE: isize = 20;

pub async fn connect(url: &str) -> anyhow::Result<MultiplexedConnection> {
    let client = redis::Client::open(url)?;
    // No driver-side response timeout: the engine's request_timeout_ms is the
    // single timeout for every database.
    let cfg = AsyncConnectionConfig::new()
        .set_connection_timeout(Some(Duration::from_secs(10)))
        .set_response_timeout(None);
    Ok(client
        .get_multiplexed_async_connection_with_config(&cfg)
        .await?)
}

pub fn map_err(e: RedisError) -> AdapterError {
    if e.is_timeout() {
        AdapterError::Timeout
    } else {
        AdapterError::Db(e.to_string())
    }
}

pub struct RedisAdapter {
    con: MultiplexedConnection,
    /// `tag:tagNNNN` keys and `tagNNNN` names, precomputed.
    tag_keys: Vec<String>,
    tag_names: Vec<String>,
    fanout_cap: usize,
    /// ZREMRANGEBYRANK stop index that keeps the newest `timeline_len`.
    trim_stop: isize,
}

impl RedisAdapter {
    /// ZREVRANGE `key` 0 19, then HGETALL each post in one pipeline.
    async fn read_page(
        &self,
        con: &mut MultiplexedConnection,
        key: &str,
    ) -> Result<(), AdapterError> {
        let ids: Vec<u64> = redis::cmd("ZREVRANGE")
            .arg(key)
            .arg(0)
            .arg(PAGE - 1)
            .query_async(con)
            .await
            .map_err(map_err)?;
        if ids.is_empty() {
            return Ok(());
        }
        let mut p = redis::pipe();
        for id in ids {
            p.cmd("HGETALL").arg(format!("post:{id}"));
        }
        let _: Value = p.query_async(con).await.map_err(map_err)?;
        Ok(())
    }
}

impl Adapter for RedisAdapter {
    async fn connect(cfg: &Config, _worker_id: u32) -> anyhow::Result<Self> {
        let tag_names: Vec<String> = (0..cfg.dataset.hashtags)
            .map(|i| tag_name(i as u16))
            .collect();
        let mut con = connect(&cfg.endpoints.redis).await?;
        let want = cfg.workload.scenario.as_str();
        let loaded: Option<String> = redis::cmd("GET")
            .arg(crate::loaders::redis::SCENARIO_KEY)
            .query_async(&mut con)
            .await?;
        if loaded.as_deref() != Some(want) {
            anyhow::bail!(
                "Redis holds the {} dataset but this run uses scenario {want}; \
                 run `bench load --db redis` with the same scenario",
                loaded.as_deref().unwrap_or("(no)")
            );
        }
        Ok(RedisAdapter {
            con,
            tag_keys: tag_names.iter().map(|t| format!("tag:{t}")).collect(),
            tag_names,
            fanout_cap: cfg.dataset.fanout_cap as usize,
            trim_stop: -(cfg.dataset.timeline_len as isize) - 1,
        })
    }

    async fn execute(&self, op: &Op, bodies: &BodyArena) -> Result<(), AdapterError> {
        let mut con = self.con.clone();
        match op.kind {
            OpKind::UserProfileRead => {
                let _: Value = redis::cmd("HGETALL")
                    .arg(format!("user:{}", op.user))
                    .query_async(&mut con)
                    .await
                    .map_err(map_err)?;
            }
            OpKind::TimelineRead => {
                self.read_page(&mut con, &format!("timeline:{}", op.user))
                    .await?;
            }
            OpKind::HashtagSearch => {
                self.read_page(&mut con, &self.tag_keys[op.tags[0] as usize])
                    .await?;
            }
            OpKind::PostLike => {
                let _: () = redis::pipe()
                    .cmd("SADD")
                    .arg(format!("likers:{}", op.target))
                    .arg(op.user)
                    .cmd("INCR")
                    .arg(format!("likes:{}", op.target))
                    .query_async(&mut con)
                    .await
                    .map_err(map_err)?;
            }
            OpKind::UserFollow => {
                let _: () = redis::pipe()
                    .cmd("SADD")
                    .arg(format!("following:{}", op.user))
                    .arg(op.target)
                    .cmd("SADD")
                    .arg(format!("followers:{}", op.target))
                    .arg(op.user)
                    .query_async(&mut con)
                    .await
                    .map_err(map_err)?;
            }
            OpKind::PostCreate => {
                let pid = op.new_post_id;
                let tags = &op.tags[..op.ntags as usize];
                let mut hashtags = String::new();
                for (i, t) in tags.iter().enumerate() {
                    if i > 0 {
                        hashtags.push(',');
                    }
                    hashtags.push_str(&self.tag_names[*t as usize]);
                }
                let mut p = redis::pipe();
                p.cmd("HSET")
                    .arg(format!("post:{pid}"))
                    .arg("author_id")
                    .arg(op.user)
                    .arg("body")
                    .arg(bodies.get(op.body))
                    .arg("created_at")
                    .arg(op.ts_ms)
                    .arg("hashtags")
                    .arg(hashtags);
                for t in tags {
                    p.cmd("ZADD")
                        .arg(&self.tag_keys[*t as usize])
                        .arg(op.ts_ms)
                        .arg(pid);
                }
                let _: () = p.query_async(&mut con).await.map_err(map_err)?;

                let mut followers: Vec<u64> = redis::cmd("SMEMBERS")
                    .arg(format!("followers:{}", op.user))
                    .query_async(&mut con)
                    .await
                    .map_err(map_err)?;
                followers.truncate(self.fanout_cap);
                if followers.is_empty() {
                    return Ok(());
                }
                let mut p = redis::pipe();
                for f in followers {
                    let key = format!("timeline:{f}");
                    p.cmd("ZADD").arg(&key).arg(op.ts_ms).arg(pid);
                    p.cmd("ZREMRANGEBYRANK").arg(key).arg(0).arg(self.trim_stop);
                }
                let _: () = p.query_async(&mut con).await.map_err(map_err)?;
            }
        }
        Ok(())
    }
}
