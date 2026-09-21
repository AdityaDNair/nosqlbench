//! Redis loader: pipelines of 1000 commands over one connection.

use anyhow::{bail, Result};
use redis::aio::MultiplexedConnection;
use redis::Pipeline;

use super::Phase;
use crate::adapters::redis::connect;
use crate::config::Config;
use crate::dataset::Dataset;

const PIPELINE: usize = 1000;
/// Members per timeline ZADD command.
const ZADD_CHUNK: usize = 100;

/// Buffers commands and flushes every `PIPELINE` commands.
struct Batch<'a> {
    con: &'a mut MultiplexedConnection,
    pipe: Pipeline,
    n: usize,
    total: u64,
}

impl<'a> Batch<'a> {
    fn new(con: &'a mut MultiplexedConnection) -> Batch<'a> {
        Batch {
            con,
            pipe: redis::pipe(),
            n: 0,
            total: 0,
        }
    }

    async fn add(&mut self, cmd: redis::Cmd) -> Result<()> {
        self.pipe.add_command(cmd).ignore();
        self.n += 1;
        if self.n == PIPELINE {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        if self.n > 0 {
            let _: () = self.pipe.query_async(self.con).await?;
            self.pipe.clear();
            self.total += self.n as u64;
            self.n = 0;
        }
        Ok(())
    }

    async fn finish(mut self) -> Result<u64> {
        self.flush().await?;
        Ok(self.total)
    }
}

pub async fn load(cfg: &Config, ds: &Dataset) -> Result<()> {
    let mut con = connect(&cfg.endpoints.redis).await?;
    let _: () = redis::cmd("FLUSHALL").query_async(&mut con).await?;
    eprintln!("  FLUSHALL done");

    let ph = Phase::start("users");
    let mut b = Batch::new(&mut con);
    for u in &ds.users {
        let mut c = redis::cmd("HSET");
        c.arg(format!("user:{}", u.id))
            .arg("username")
            .arg(&u.username)
            .arg("display_name")
            .arg(&u.display_name)
            .arg("bio")
            .arg(&u.bio)
            .arg("created_at")
            .arg(u.created_at_ms);
        b.add(c).await?;
    }
    ph.done(b.finish().await?);

    let ph = Phase::start("posts + hashtag index");
    let mut b = Batch::new(&mut con);
    for p in &ds.posts {
        let hashtags: Vec<&str> = p
            .tags()
            .iter()
            .map(|t| ds.tag_names[*t as usize].as_str())
            .collect();
        let mut c = redis::cmd("HSET");
        c.arg(format!("post:{}", p.id))
            .arg("author_id")
            .arg(p.author)
            .arg("body")
            .arg(ds.bodies.get(p.id as u32))
            .arg("created_at")
            .arg(p.created_at_ms)
            .arg("hashtags")
            .arg(hashtags.join(","));
        b.add(c).await?;
        for t in p.tags() {
            let mut c = redis::cmd("ZADD");
            c.arg(format!("tag:{}", ds.tag_names[*t as usize]))
                .arg(p.created_at_ms)
                .arg(p.id);
            b.add(c).await?;
        }
    }
    ph.done(b.finish().await?);

    let ph = Phase::start("follows");
    let mut b = Batch::new(&mut con);
    for u in 0..ds.users.len() as u64 {
        for &t in ds.following(u) {
            let mut c = redis::cmd("SADD");
            c.arg(format!("following:{u}")).arg(t);
            b.add(c).await?;
            let mut c = redis::cmd("SADD");
            c.arg(format!("followers:{t}")).arg(u);
            b.add(c).await?;
        }
    }
    ph.done(b.finish().await?);

    let ph = Phase::start("timelines (fan-out, trimmed)");
    let (off, tl) = ds.timelines();
    let created: Vec<i64> = ds.posts.iter().map(|p| p.created_at_ms).collect();
    let mut b = Batch::new(&mut con);
    for u in 0..ds.users.len() {
        let posts = &tl[off[u] as usize..off[u + 1] as usize];
        for chunk in posts.chunks(ZADD_CHUNK) {
            let mut c = redis::cmd("ZADD");
            c.arg(format!("timeline:{u}"));
            for &pid in chunk {
                c.arg(created[pid as usize]).arg(pid);
            }
            b.add(c).await?;
        }
    }
    b.finish().await?;
    ph.done(tl.len() as u64);

    // Sanity check against the generated dataset.
    let keys: u64 = redis::cmd("DBSIZE").query_async(&mut con).await?;
    let tl0: u64 = redis::cmd("ZCARD")
        .arg("timeline:0")
        .query_async(&mut con)
        .await?;
    let want0 = (off[1] - off[0]) as u64;
    if tl0 != want0 {
        bail!("timeline:0 has {tl0} entries, expected {want0}");
    }
    let mem: String = redis::cmd("INFO")
        .arg("memory")
        .query_async(&mut con)
        .await?;
    let used = mem
        .lines()
        .find_map(|l| l.strip_prefix("used_memory_human:"))
        .unwrap_or("?");
    eprintln!(
        "  loaded: {keys} keys, {} timeline entries, used_memory {used}",
        tl.len()
    );
    Ok(())
}
