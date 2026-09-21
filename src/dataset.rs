//! Deterministic dataset generation. Every loader and the op generator derive
//! from `ChaCha8Rng::seed_from_u64(seed)`, so they all see the same world.

use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rand_distr::{Distribution, Zipf};

use crate::config::Config;

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
    #[cfg_attr(not(test), expect(dead_code, reason = "used by the loaders (M3+)"))]
    pub tag_names: Vec<String>,
    #[expect(dead_code, reason = "used by the loaders (M3+)")]
    pub fanout_cap: u32,
}

pub fn tag_name(i: u16) -> String {
    format!("tag{i:04}")
}

impl Dataset {
    pub fn generate(cfg: &Config) -> Dataset {
        let d = &cfg.dataset;
        let mut rng = ChaCha8Rng::seed_from_u64(cfg.seed);
        let user_z = ZipfIds::new(d.users, d.zipf_s);
        let tag_z = ZipfIds::new(d.hashtags as u64, d.zipf_s);

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
            let author = user_z.sample(&mut rng);
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
        }
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "used by the loaders (M3+)"))]
    pub fn following(&self, u: u64) -> &[u64] {
        let a = self.follow_offsets[u as usize] as usize;
        let b = self.follow_offsets[u as usize + 1] as usize;
        &self.follow_targets[a..b]
    }

    /// Reverse adjacency (followers of each user) in CSR form, follower ids ascending.
    #[cfg_attr(not(test), expect(dead_code, reason = "used by the loaders (M3+)"))]
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
        let a = Dataset::generate(&small_cfg(42)).fingerprint();
        let b = Dataset::generate(&small_cfg(42)).fingerprint();
        let c = Dataset::generate(&small_cfg(43)).fingerprint();
        assert_eq!(a, b);
        assert_ne!(a, c);
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
}
