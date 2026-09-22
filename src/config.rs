//! Configuration: `config/default.toml` plus CLI overrides.
//!
//! Overrides are applied to the raw TOML table before deserialisation, so any
//! field can be set with `--set section.field=value`. Named CLI flags are
//! sugar that expand to the same `--set` entries.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub seed: u64,
    pub dataset: DatasetConfig,
    pub workload: WorkloadWeights,
    pub run: RunConfig,
    pub cpu: CpuConfig,
    pub drivers: DriverConfig,
    pub endpoints: Endpoints,
}

/// Per-worker driver connection settings. Every worker builds its own client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverConfig {
    /// Multiplexed Redis connections per worker; loops pick one round-robin.
    pub redis_connections: u32,
    /// MongoDB connection pool size per worker (one request per connection).
    pub mongo_max_pool_size: u32,
    /// CQL connections per worker (each multiplexes many requests).
    pub cassandra_connections: u32,
    /// Neo4j (Bolt) connection pool size per worker (one query per connection).
    pub neo4j_max_connections: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetConfig {
    pub users: u64,
    pub posts: u64,
    pub avg_follows: u32,
    pub hashtags: u32,
    pub max_tags_per_post: u8,
    pub zipf_s: f64,
    pub fanout_cap: u32,
    /// Redis/Cassandra timelines keep only the newest N posts per user.
    pub timeline_len: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadWeights {
    /// Not a weight: how activity relates to popularity (see `Scenario`).
    pub scenario: Scenario,
    pub timeline_read: f64,
    pub user_profile_read: f64,
    pub post_like: f64,
    pub post_create: f64,
    pub hashtag_search: f64,
    pub user_follow: f64,
}

/// How user activity relates to popularity.
///
/// Popularity (who gets followed) is Zipf over user ids: user 0 is the most
/// followed. Activity (who authors posts and who acts in ops) is Zipf over an
/// activity ranking:
/// - `Normal`: the ranking is a seeded random permutation of user ids, so
///   activity is independent of popularity.
/// - `Celebrity`: the ranking is the identity, so the most-followed users are
///   also the most active (the M1/M2 behaviour).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scenario {
    Normal,
    Celebrity,
}

impl Scenario {
    pub fn as_str(self) -> &'static str {
        match self {
            Scenario::Normal => "normal",
            Scenario::Celebrity => "celebrity",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Closed,
    Open,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Closed => "closed",
            Mode::Open => "open",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    pub mode: Mode,
    pub concurrency: u32,
    pub rate: u64,
    pub max_inflight: u32,
    pub warmup_secs: u64,
    pub measure_secs: u64,
    pub request_timeout_ms: u64,
    pub workers: u32,
    pub ring_slots: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuConfig {
    pub worker_cpus: Vec<usize>,
    pub aggregator_cpu: i64,
    /// Windows thread priority for worker threads. Raised by default so other
    /// normal-priority threads cannot take a worker's CPU for a full quantum.
    pub worker_priority: ThreadPriority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadPriority {
    Normal,
    AboveNormal,
    Highest,
    TimeCritical,
}

impl ThreadPriority {
    /// Win32 THREAD_PRIORITY_* value.
    pub fn win32(self) -> i32 {
        match self {
            ThreadPriority::Normal => 0,
            ThreadPriority::AboveNormal => 1,
            ThreadPriority::Highest => 2,
            ThreadPriority::TimeCritical => 15,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoints {
    pub redis: String,
    pub mongo: String,
    pub cassandra: String,
    pub neo4j: String,
    pub neo4j_user: String,
    pub neo4j_pass: String,
}

impl Config {
    /// Load `path`, apply `key.path=value` overrides, deserialise and validate.
    pub fn load(path: &Path, overrides: &[String]) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut table: toml::Table =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        for ov in overrides {
            apply_override(&mut table, ov)?;
        }
        let cfg: Config = toml::Value::Table(table)
            .try_into()
            .context("config does not match schema")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        let d = &self.dataset;
        if d.users < 2 {
            bail!("dataset.users must be >= 2");
        }
        if d.users > u32::MAX as u64 || d.posts > u32::MAX as u64 {
            bail!("dataset.users and dataset.posts must fit in u32");
        }
        if d.posts < 1 {
            bail!("dataset.posts must be >= 1");
        }
        if d.hashtags < 1 || d.hashtags > 10_000 {
            bail!("dataset.hashtags must be in 1..=10000 (names are tag0000..tag9999)");
        }
        if d.timeline_len < 20 {
            bail!("dataset.timeline_len must be >= 20 (TIMELINE_READ returns 20 posts)");
        }
        if d.max_tags_per_post > 3 {
            bail!("dataset.max_tags_per_post must be <= 3");
        }
        if !(d.zipf_s >= 0.0 && d.zipf_s.is_finite()) {
            bail!("dataset.zipf_s must be a finite value >= 0");
        }
        let w = &self.workload;
        let ws = [
            w.timeline_read,
            w.user_profile_read,
            w.post_like,
            w.post_create,
            w.hashtag_search,
            w.user_follow,
        ];
        if ws.iter().any(|x| !(*x >= 0.0 && x.is_finite())) || ws.iter().sum::<f64>() <= 0.0 {
            bail!("workload weights must be finite, >= 0 and sum to > 0");
        }
        let r = &self.run;
        if r.workers < 1 {
            bail!("run.workers must be >= 1");
        }
        if r.mode == Mode::Closed && r.concurrency < 1 {
            bail!("run.concurrency must be >= 1 in closed mode");
        }
        if r.mode == Mode::Open && (r.rate == 0 || r.max_inflight < 1) {
            bail!("open mode needs run.rate > 0 and run.max_inflight >= 1");
        }
        if r.measure_secs < 1 {
            bail!("run.measure_secs must be >= 1");
        }
        if !r.ring_slots.is_power_of_two() {
            bail!("run.ring_slots must be a power of two");
        }
        let dr = &self.drivers;
        if dr.redis_connections < 1
            || dr.mongo_max_pool_size < 1
            || dr.cassandra_connections < 1
            || dr.neo4j_max_connections < 1
        {
            bail!("drivers.* connection counts must be >= 1");
        }
        if self.cpu.aggregator_cpu < -1 {
            bail!("cpu.aggregator_cpu must be -1 (auto) or a logical CPU index");
        }
        Ok(())
    }

    /// Normalised weights in `OpKind` order.
    pub fn normalised_weights(&self) -> [f64; 6] {
        let w = &self.workload;
        let raw = [
            w.timeline_read,
            w.user_profile_read,
            w.post_like,
            w.post_create,
            w.hashtag_search,
            w.user_follow,
        ];
        let sum: f64 = raw.iter().sum();
        raw.map(|x| x / sum)
    }
}

/// Apply one `a.b.c=value` override. The key must already exist, so typos fail
/// loudly. The value is parsed as a TOML literal, falling back to a string.
fn apply_override(table: &mut toml::Table, ov: &str) -> Result<()> {
    let (key, raw) = ov
        .split_once('=')
        .with_context(|| format!("override `{ov}` is not key=value"))?;
    let key = key.trim();
    let raw = raw.trim();
    let value: toml::Value = match toml::from_str::<toml::Table>(&format!("v = {raw}")) {
        Ok(mut t) => t.remove("v").expect("parsed key"),
        Err(_) => toml::Value::String(raw.to_string()),
    };

    let parts: Vec<&str> = key.split('.').collect();
    let (last, path) = parts.split_last().expect("split yields at least one part");
    let mut cur = table;
    for p in path {
        cur = cur
            .get_mut(*p)
            .and_then(|v| v.as_table_mut())
            .with_context(|| format!("override `{key}`: no section `{p}`"))?;
    }
    let slot = cur
        .get_mut(*last)
        .with_context(|| format!("override `{key}`: unknown field"))?;
    // Let integer-typed fields accept integer literals and float fields accept
    // integers (e.g. `--set workload.post_like=20`).
    *slot = match (&*slot, value) {
        (toml::Value::Float(_), toml::Value::Integer(i)) => toml::Value::Float(i as f64),
        (_, v) => v,
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_path() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("config")
            .join("default.toml")
    }

    #[test]
    fn default_config_loads() {
        let c = Config::load(&default_path(), &[]).unwrap();
        assert_eq!(c.seed, 42);
        assert_eq!(c.dataset.users, 10_000);
        let w = c.normalised_weights();
        assert!((w.iter().sum::<f64>() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn overrides_apply_and_typos_fail() {
        let c = Config::load(
            &default_path(),
            &[
                "run.concurrency=128".into(),
                "run.mode=open".into(),
                "run.rate=1000".into(),
                "cpu.worker_cpus=[0,2]".into(),
            ],
        )
        .unwrap();
        assert_eq!(c.run.concurrency, 128);
        assert_eq!(c.run.mode, Mode::Open);
        assert_eq!(c.cpu.worker_cpus, vec![0, 2]);
        assert!(Config::load(&default_path(), &["run.concurency=1".into()]).is_err());
    }
}
