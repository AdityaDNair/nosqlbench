//! Op type and pre-generated per-worker op streams.

use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;

use crate::config::Config;
use crate::dataset::{draw_tags, BodyArena, Fnv, ZipfIds, DAY_MS, EPOCH_MS};

/// Ops per worker stream (cycled with a wrapping cursor).
pub const OPS_PER_WORKER: usize = 1 << 20;
/// Distinct new-post bodies per worker.
pub const BODIES_PER_WORKER: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum OpKind {
    TimelineRead = 0,
    UserProfileRead = 1,
    PostLike = 2,
    PostCreate = 3,
    HashtagSearch = 4,
    UserFollow = 5,
}

impl OpKind {
    pub const ALL: [OpKind; 6] = [
        OpKind::TimelineRead,
        OpKind::UserProfileRead,
        OpKind::PostLike,
        OpKind::PostCreate,
        OpKind::HashtagSearch,
        OpKind::UserFollow,
    ];
    pub const COUNT: usize = 6;

    pub fn name(self) -> &'static str {
        match self {
            OpKind::TimelineRead => "TIMELINE_READ",
            OpKind::UserProfileRead => "USER_PROFILE_READ",
            OpKind::PostLike => "POST_LIKE",
            OpKind::PostCreate => "POST_CREATE",
            OpKind::HashtagSearch => "HASHTAG_SEARCH",
            OpKind::UserFollow => "USER_FOLLOW",
        }
    }

    pub fn from_u8(v: u8) -> Option<OpKind> {
        OpKind::ALL.get(v as usize).copied()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Op {
    pub kind: OpKind,
    pub user: u64,
    pub target: u64,
    pub body: u32,
    pub tags: [u16; 3],
    pub ntags: u8,
    pub new_post_id: u64,
    pub ts_ms: i64,
}

/// A worker's pre-generated ops plus the bodies its PostCreate ops refer to.
pub struct OpStream {
    pub ops: Vec<Op>,
    pub bodies: BodyArena,
}

pub fn new_post_id(worker_id: u32, seq: u32) -> u64 {
    (1u64 << 40) | ((worker_id as u64) << 32) | seq as u64
}

impl OpStream {
    pub fn generate(cfg: &Config, worker_id: u32) -> OpStream {
        Self::generate_n(cfg, worker_id, OPS_PER_WORKER, BODIES_PER_WORKER)
    }

    pub fn generate_n(cfg: &Config, worker_id: u32, n_ops: usize, n_bodies: usize) -> OpStream {
        assert!(
            worker_id < 256,
            "worker_id must fit in 8 bits of new_post_id"
        );
        let d = &cfg.dataset;
        let mut rng = ChaCha8Rng::seed_from_u64(cfg.seed ^ worker_id as u64);
        let user_z = ZipfIds::new(d.users, d.zipf_s);
        let post_z = ZipfIds::new(d.posts, d.zipf_s);
        let tag_z = ZipfIds::new(d.hashtags as u64, d.zipf_s);

        let mut bodies = BodyArena::with_capacity(n_bodies, n_bodies * 160);
        for _ in 0..n_bodies {
            bodies.push_random(&mut rng);
        }

        // Cumulative weights for kind selection.
        let w = cfg.normalised_weights();
        let mut cum = [0f64; OpKind::COUNT];
        let mut acc = 0.0;
        for (i, x) in w.iter().enumerate() {
            acc += x;
            cum[i] = acc;
        }

        let base_ts = EPOCH_MS + DAY_MS;
        let mut ops = Vec::with_capacity(n_ops);
        let mut seq: u32 = 0;
        for i in 0..n_ops {
            let r: f64 = rng.random();
            let k = cum.iter().position(|c| r < *c).unwrap_or(OpKind::COUNT - 1);
            let kind = OpKind::ALL[k];
            let user = user_z.sample(&mut rng);
            let mut op = Op {
                kind,
                user,
                target: 0,
                body: 0,
                tags: [0; 3],
                ntags: 0,
                new_post_id: 0,
                ts_ms: base_ts + i as i64,
            };
            match kind {
                OpKind::UserFollow => {
                    let mut t = user_z.sample(&mut rng);
                    while t == user {
                        t = user_z.sample(&mut rng);
                    }
                    op.target = t;
                }
                OpKind::PostLike => op.target = post_z.sample(&mut rng),
                OpKind::PostCreate => {
                    op.body = rng.random_range(0..n_bodies as u32);
                    let (tags, n) = draw_tags(&mut rng, &tag_z, d.max_tags_per_post);
                    op.tags = tags;
                    op.ntags = n;
                    op.new_post_id = new_post_id(worker_id, seq);
                    op.ts_ms = base_ts + seq as i64;
                    seq += 1;
                }
                OpKind::HashtagSearch => {
                    op.tags[0] = tag_z.sample(&mut rng) as u16;
                    op.ntags = 1;
                }
                OpKind::TimelineRead | OpKind::UserProfileRead => {}
            }
            ops.push(op);
        }
        OpStream { ops, bodies }
    }

    pub fn fingerprint(&self) -> u64 {
        let mut h = Fnv::new();
        for o in &self.ops {
            h.u64(o.kind as u64);
            h.u64(o.user);
            h.u64(o.target);
            h.u64(o.body as u64);
            for t in o.tags {
                h.u64(t as u64);
            }
            h.u64(o.ntags as u64);
            h.u64(o.new_post_id);
            h.u64(o.ts_ms as u64);
        }
        h.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::tests::small_cfg;

    #[test]
    fn streams_are_deterministic_and_differ_per_worker() {
        let cfg = small_cfg(42);
        let a = OpStream::generate_n(&cfg, 0, 20_000, 64).fingerprint();
        let b = OpStream::generate_n(&cfg, 0, 20_000, 64).fingerprint();
        let c = OpStream::generate_n(&cfg, 1, 20_000, 64).fingerprint();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn op_fields_are_valid_and_mix_matches_weights() {
        let cfg = small_cfg(1);
        let n = 100_000;
        let s = OpStream::generate_n(&cfg, 3, n, 64);
        let mut counts = [0usize; OpKind::COUNT];
        let mut seq = 0u32;
        for o in &s.ops {
            counts[o.kind as usize] += 1;
            assert!(o.user < cfg.dataset.users);
            match o.kind {
                OpKind::UserFollow => assert!(o.target != o.user && o.target < cfg.dataset.users),
                OpKind::PostLike => assert!(o.target < cfg.dataset.posts),
                OpKind::PostCreate => {
                    assert_eq!(o.new_post_id, (1 << 40) | (3 << 32) | seq as u64);
                    assert_eq!(o.ts_ms, EPOCH_MS + DAY_MS + seq as i64);
                    assert!((o.body as usize) < s.bodies.len());
                    seq += 1;
                }
                _ => {}
            }
        }
        let w = cfg.normalised_weights();
        for k in 0..OpKind::COUNT {
            let got = counts[k] as f64 / n as f64;
            assert!((got - w[k]).abs() < 0.01, "kind {k}: {got} vs {}", w[k]);
        }
    }
}
