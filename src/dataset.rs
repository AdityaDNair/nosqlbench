//! Deterministic dataset generation. Every loader and the op generator derive
//! from `ChaCha8Rng::seed_from_u64(seed)`, so they all see the same world.

use rand::seq::SliceRandom;
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rand_distr::{Distribution, Zipf};

use crate::config::{Config, Scenario};

/// 2026-01-01T00:00:00Z in Unix milliseconds.
pub const EPOCH_MS: i64 = 1_767_225_600_000;
pub const DAY_MS: i64 = 86_400_000;

const WORDS: &[&str] = &[
    "the",
    "quick",
    "brown",
    "fox",
    "jumps",
    "over",
    "lazy",
    "dog",
    "coffee",
    "morning",
    "rust",
    "database",
    "latency",
    "throughput",
    "cluster",
    "shard",
    "index",
    "query",
    "cache",
    "graph",
    "node",
    "edge",
    "timeline",
    "follow",
    "post",
    "like",
    "share",
    "today",
    "great",
    "news",
    "weekend",
    "music",
    "travel",
    "city",
    "river",
    "mountain",
    "sunset",
    "code",
    "build",
    "ship",
    "deploy",
    "test",
    "benchmark",
    "fast",
    "slow",
    "happy",
    "tired",
    "learning",
    "reading",
    "book",
    "game",
    "team",
    "win",
    "launch",
    "update",
    "release",
    "idea",
    "project",
    "friend",
    "family",
    "weather",
    "rain",
    "sun",
    "night",
];

/// Salt for the activity-ranking RNG, so the permutation does not disturb the
/// main dataset stream or collide with per-worker op seeds (`seed ^ worker_id`).
const ACTIVITY_SALT: u64 = 0xAC71_7171_0000_0000;

/// Activity ranking: `ranking[r]` is the user at Zipf rank `r + 1` for post
/// authorship and acting users. Identity in the celebrity scenario; a seeded
/// random permutation in the normal scenario.
pub fn activity_ranking(cfg: &Config) -> Vec<u64> {
    let mut v: Vec<u64> = (0..cfg.dataset.users).collect();
    if cfg.workload.scenario == Scenario::Normal {
        let mut rng = ChaCha8Rng::seed_from_u64(cfg.seed ^ ACTIVITY_SALT);
        v.shuffle(&mut rng);
    }
    v
}

/// Contiguous storage for many short ASCII strings.
#[derive(Debug, Default, Clone)]
pub struct BodyArena {
    bytes: Vec<u8>,
    /// `offsets[i]..offsets[i+1]` is body `i`. Length = count + 1.
    offsets: Vec<u32>,
}

impl BodyArena {
    pub fn with_capacity(count: usize, bytes: usize) -> BodyArena {
        let mut offsets = Vec::with_capacity(count + 1);
        offsets.push(0);
        BodyArena {
            bytes: Vec::with_capacity(bytes),
            offsets,
        }
    }

    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// Body `i` as `&str`. Arena contents are ASCII by construction.
    #[inline]
    pub fn get(&self, i: u32) -> &str {
        let a = self.offsets[i as usize] as usize;
        let b = self.offsets[i as usize + 1] as usize;
        std::str::from_utf8(&self.bytes[a..b]).expect("arena is ASCII")
    }

    /// Append a random body of 40..=280 chars built from the fixed word list.
    pub fn push_random(&mut self, rng: &mut ChaCha8Rng) {
        let target = rng.random_range(40..=280usize);
        let start = self.bytes.len();
        while self.bytes.len() - start < target {
            if self.bytes.len() > start {
                self.bytes.push(b' ');
            }
            let w = WORDS[rng.random_range(0..WORDS.len())];
            self.bytes.extend_from_slice(w.as_bytes());
        }
        self.bytes.truncate(start + target);
        let end = u32::try_from(self.bytes.len()).expect("arena exceeds 4 GiB");
        self.offsets.push(end);
    }

    fn hash_into(&self, h: &mut Fnv) {
        h.bytes(&self.bytes);
        for o in &self.offsets {
            h.u64(*o as u64);
        }
    }
}

/// Zipf sampler over `0..n` (rank 1 -> id 0, the most popular).
#[derive(Debug, Clone, Copy)]
pub struct ZipfIds {
    z: Zipf<f64>,
    n: u64,
}

impl ZipfIds {
    pub fn new(n: u64, s: f64) -> ZipfIds {
        ZipfIds {
            z: Zipf::new(n as f64, s).expect("valid zipf params"),
            n,
        }
    }

    #[inline]
    pub fn sample(&self, rng: &mut ChaCha8Rng) -> u64 {
        let r = self.z.sample(rng) as u64;
        r.clamp(1, self.n) - 1
    }
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: u64,
    pub username: String,
    pub display_name: String,
    pub bio: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct Post {
    pub id: u64,
    pub author: u64,
    pub created_at_ms: i64,
    pub tags: [u16; 3],
    pub ntags: u8,
}

impl Post {
    pub fn tags(&self) -> &[u16] {
        &self.tags[..self.ntags as usize]
    }
}

#[derive(Debug)]
pub struct Dataset {
    pub users: Vec<User>,
    /// CSR adjacency: user `u` follows `follow_targets[follow_offsets[u]..follow_offsets[u+1]]`.
    pub follow_offsets: Vec<u32>,
    pub follow_targets: Vec<u64>,
    pub posts: Vec<Post>,
    /// Body of post `i` is `bodies.get(i)`.
    pub bodies: BodyArena,
    pub tag_names: Vec<String>,
    pub fanout_cap: u32,
    pub timeline_len: u32,
    /// See `activity_ranking`.
    pub activity: Vec<u64>,
}

pub fn tag_name(i: u16) -> String {
    format!("tag{i:04}")
}

impl Dataset {
    pub fn generate(cfg: &Config) -> Dataset {
        let d = &cfg.dataset;
        let mut rng = ChaCha8Rng::seed_from_u64(cfg.seed);
        // Follow targets: Zipf over popularity (user id order).
        // Post authors: Zipf over the activity ranking.
        let user_z = ZipfIds::new(d.users, d.zipf_s);
        let tag_z = ZipfIds::new(d.hashtags as u64, d.zipf_s);
        let activity = activity_ranking(cfg);

        // Users. Joined some time in the year before the post window.
        let mut users = Vec::with_capacity(d.users as usize);
        let mut bio = BodyArena::with_capacity(1, 300);
        for id in 0..d.users {
            bio.push_random(&mut rng);
            let created_at_ms = EPOCH_MS - 30 * DAY_MS - rng.random_range(0..365 * DAY_MS);
            users.push(User {
                id,
                username: format!("user{id}"),
                display_name: format!("User {id}"),
                bio: bio.get(bio.len() as u32 - 1).to_string(),
                created_at_ms,
            });
        }

        // Follows: avg ±50% distinct Zipf targets, no self-follows.
        let lo = (d.avg_follows as f64 * 0.5).ceil() as u64;
        let hi = (d.avg_follows as f64 * 1.5).floor() as u64;
        let mut follow_offsets = Vec::with_capacity(d.users as usize + 1);
        let mut follow_targets = Vec::with_capacity((d.users * d.avg_follows as u64) as usize);
        follow_offsets.push(0u32);
        let mut picked: Vec<u64> = Vec::new();
        for u in 0..d.users {
            let want = rng.random_range(lo..=hi.max(lo)).min(d.users - 1) as usize;
            picked.clear();
            while picked.len() < want {
                let t = user_z.sample(&mut rng);
                if t != u && !picked.contains(&t) {
                    picked.push(t);
                }
            }
            follow_targets.extend_from_slice(&picked);
            follow_offsets.push(u32::try_from(follow_targets.len()).expect("too many follows"));
        }

        // Posts.
        let window_start = EPOCH_MS - 30 * DAY_MS;
        let mut posts = Vec::with_capacity(d.posts as usize);
        let mut bodies = BodyArena::with_capacity(d.posts as usize, d.posts as usize * 160);
        for id in 0..d.posts {
            let author = activity[user_z.sample(&mut rng) as usize];
            let created_at_ms = rng.random_range(window_start..EPOCH_MS);
            let (tags, ntags) = draw_tags(&mut rng, &tag_z, d.max_tags_per_post);
            bodies.push_random(&mut rng);
            posts.push(Post {
                id,
                author,
                created_at_ms,
                tags,
                ntags,
            });
        }

        let tag_names = (0..d.hashtags).map(|i| tag_name(i as u16)).collect();
        Dataset {
            users,
            follow_offsets,
            follow_targets,
            posts,
            bodies,
            tag_names,
            fanout_cap: d.fanout_cap,
            timeline_len: d.timeline_len,
            activity,
        }
    }

    pub fn following(&self, u: u64) -> &[u64] {
        let a = self.follow_offsets[u as usize] as usize;
        let b = self.follow_offsets[u as usize + 1] as usize;
        &self.follow_targets[a..b]
    }

    /// Reverse adjacency (followers of each user) in CSR form, follower ids ascending.
    pub fn followers(&self) -> (Vec<u32>, Vec<u64>) {
        let n = self.users.len();
        let mut counts = vec![0u32; n + 1];
        for &t in &self.follow_targets {
            counts[t as usize + 1] += 1;
        }
        for i in 0..n {
            counts[i + 1] += counts[i];
        }
        let offsets = counts.clone();
        let mut fill = counts;
        let mut out = vec![0u64; self.follow_targets.len()];
        for u in 0..n as u64 {
            for &t in self.following(u) {
                out[fill[t as usize] as usize] = u;
                fill[t as usize] += 1;
            }
        }
        (offsets, out)
    }

    /// Fan-out-on-write timelines: for each user, the newest `timeline_len`
    /// posts by accounts they follow, newest first by `(created_at, post_id)`.
    /// A post reaches a follower only if that follower is among the first
    /// `fanout_cap` followers (by id) of its author.
    /// Returns CSR `(offsets, post_ids)`.
    pub fn timelines(&self) -> (Vec<u32>, Vec<u64>) {
        let n = self.users.len();
        let cap = self.fanout_cap as usize;
        let len = self.timeline_len as usize;
        let (fol_off, fol) = self.followers();

        // Posts per author, as (created_at, post_id).
        let mut by_author: Vec<Vec<(i64, u64)>> = vec![Vec::new(); n];
        for p in &self.posts {
            by_author[p.author as usize].push((p.created_at_ms, p.id));
        }

        let mut offsets = Vec::with_capacity(n + 1);
        let mut out = Vec::with_capacity(n * len);
        offsets.push(0u32);
        let mut buf: Vec<(i64, u64)> = Vec::new();
        for u in 0..n as u64 {
            buf.clear();
            for &a in self.following(u) {
                let followers =
                    &fol[fol_off[a as usize] as usize..fol_off[a as usize + 1] as usize];
                // Followers are sorted ascending; honour fanout_cap.
                if followers.len() > cap && followers.partition_point(|&f| f < u) >= cap {
                    continue;
                }
                buf.extend_from_slice(&by_author[a as usize]);
            }
            let newest_first = |x: &(i64, u64), y: &(i64, u64)| y.cmp(x);
            if buf.len() > len {
                buf.select_nth_unstable_by(len, newest_first);
                buf.truncate(len);
            }
            buf.sort_unstable_by(newest_first);
            out.extend(buf.iter().map(|&(_, id)| id));
            offsets.push(u32::try_from(out.len()).expect("too many timeline entries"));
        }
        (offsets, out)
    }

    /// Stable 64-bit fingerprint of the full dataset (FNV-1a).
    pub fn fingerprint(&self) -> u64 {
        let mut h = Fnv::new();
        for u in &self.users {
            h.u64(u.id);
            h.bytes(u.username.as_bytes());
            h.bytes(u.display_name.as_bytes());
            h.bytes(u.bio.as_bytes());
            h.u64(u.created_at_ms as u64);
        }
        for o in &self.follow_offsets {
            h.u64(*o as u64);
        }
        for t in &self.follow_targets {
            h.u64(*t);
        }
        for p in &self.posts {
            h.u64(p.id);
            h.u64(p.author);
            h.u64(p.created_at_ms as u64);
            for t in p.tags() {
                h.u64(*t as u64);
            }
            h.u64(p.ntags as u64);
        }
        self.bodies.hash_into(&mut h);
        h.finish()
    }
}

/// 0..=max distinct hashtags, Zipf-distributed.
pub fn draw_tags(rng: &mut ChaCha8Rng, tag_z: &ZipfIds, max: u8) -> ([u16; 3], u8) {
    let n = rng.random_range(0..=max).min(tag_z.n as u8);
    let mut tags = [0u16; 3];
    let mut k = 0usize;
    while k < n as usize {
        let t = tag_z.sample(rng) as u16;
        if !tags[..k].contains(&t) {
            tags[k] = t;
            k += 1;
        }
    }
    (tags, n)
}

/// FNV-1a, 64-bit. Used for fingerprints because it is stable across Rust versions.
pub struct Fnv(u64);

impl Fnv {
    pub fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    pub fn bytes(&mut self, b: &[u8]) {
        for x in b {
            self.0 ^= *x as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // Length delimiter so ("ab","c") != ("a","bc").
        self.u64(b.len() as u64);
    }
    pub fn u64(&mut self, v: u64) {
        for x in v.to_le_bytes() {
            self.0 ^= x as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    pub fn finish(&self) -> u64 {
        self.0
    }
}

impl Default for Fnv {
    fn default() -> Self {
        Fnv::new()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::Path;

    pub(crate) fn small_cfg(seed: u64) -> Config {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("config")
            .join("default.toml");
        let mut c = Config::load(&path, &[]).unwrap();
        c.seed = seed;
        c.dataset.users = 500;
        c.dataset.posts = 2_000;
        c.dataset.hashtags = 50;
        c
    }

    #[test]
    fn same_seed_same_hash() {
        for scenario in [Scenario::Normal, Scenario::Celebrity] {
            let cfg = |seed| {
                let mut c = small_cfg(seed);
                c.workload.scenario = scenario;
                c
            };
            let a = Dataset::generate(&cfg(42)).fingerprint();
            let b = Dataset::generate(&cfg(42)).fingerprint();
            let c = Dataset::generate(&cfg(43)).fingerprint();
            assert_eq!(a, b, "{scenario:?}");
            assert_ne!(a, c, "{scenario:?}");
        }
        let mut n = small_cfg(42);
        n.workload.scenario = Scenario::Normal;
        let mut c = small_cfg(42);
        c.workload.scenario = Scenario::Celebrity;
        assert_ne!(
            Dataset::generate(&n).fingerprint(),
            Dataset::generate(&c).fingerprint()
        );
    }

    /// The celebrity scenario is exactly the M1/M2 dataset (default config).
    #[test]
    fn celebrity_reproduces_original_dataset() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("config")
            .join("default.toml");
        let c = Config::load(&path, &["workload.scenario=\"celebrity\"".into()]).unwrap();
        assert_eq!(Dataset::generate(&c).fingerprint(), 0x3e9c_076e_a0bd_0bfe);
    }

    #[test]
    fn normal_scenario_decouples_activity_from_popularity() {
        let mut cfg = small_cfg(42);
        cfg.workload.scenario = Scenario::Normal;
        let d = Dataset::generate(&cfg);
        let r = activity_ranking(&cfg);
        let mut sorted = r.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            (0..cfg.dataset.users).collect::<Vec<_>>(),
            "permutation"
        );
        // Most prolific author is the top of the activity ranking, and it is
        // not the most-followed user (user 0).
        let mut posts = vec![0u32; cfg.dataset.users as usize];
        for p in &d.posts {
            posts[p.author as usize] += 1;
        }
        let top = (0..posts.len()).max_by_key(|&u| posts[u]).unwrap() as u64;
        assert_eq!(top, r[0]);
        assert_ne!(top, 0);
    }

    #[test]
    fn follows_are_valid() {
        let cfg = small_cfg(7);
        let d = Dataset::generate(&cfg);
        for u in 0..cfg.dataset.users {
            let f = d.following(u);
            assert!(
                f.len() >= 10 && f.len() <= 30,
                "user {u} follows {}",
                f.len()
            );
            assert!(!f.contains(&u), "self-follow");
            let mut s = f.to_vec();
            s.sort();
            s.dedup();
            assert_eq!(s.len(), f.len(), "duplicate follow");
        }
        let (off, fol) = d.followers();
        assert_eq!(fol.len(), d.follow_targets.len());
        assert_eq!(*off.last().unwrap() as usize, fol.len());
    }

    #[test]
    fn posts_and_bodies_are_valid() {
        let cfg = small_cfg(9);
        let d = Dataset::generate(&cfg);
        assert_eq!(d.bodies.len(), d.posts.len());
        for p in &d.posts {
            assert!(p.created_at_ms >= EPOCH_MS - 30 * DAY_MS && p.created_at_ms < EPOCH_MS);
            assert!(p.ntags <= 3);
            assert!(p.tags().iter().all(|t| (*t as u32) < cfg.dataset.hashtags));
            let b = d.bodies.get(p.id as u32);
            assert!((40..=280).contains(&b.len()) && b.is_ascii());
        }
        assert_eq!(d.tag_names[7], "tag0007");
    }

    #[test]
    fn timelines_are_newest_followee_posts() {
        let mut cfg = small_cfg(5);
        cfg.dataset.timeline_len = 30;
        let d = Dataset::generate(&cfg);
        let (off, tl) = d.timelines();
        for u in [0u64, 1, 17, 499] {
            let got = &tl[off[u as usize] as usize..off[u as usize + 1] as usize];
            // Brute force: all followee posts, newest first, top 30.
            let f = d.following(u);
            let mut all: Vec<(i64, u64)> = d
                .posts
                .iter()
                .filter(|p| f.contains(&p.author))
                .map(|p| (p.created_at_ms, p.id))
                .collect();
            all.sort_by(|a, b| b.cmp(a));
            let want: Vec<u64> = all.iter().take(30).map(|x| x.1).collect();
            assert_eq!(got, &want[..], "user {u}");
        }
    }
}
