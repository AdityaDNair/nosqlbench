//! Dataset loaders, one per database. Each regenerates the deterministic
//! dataset from the config seed, so every database gets the same world.

pub mod cassandra;
pub mod mongo;
pub mod redis;

use std::time::Instant;

/// Prints progress for a load phase.
pub struct Phase {
    name: &'static str,
    start: Instant,
}

impl Phase {
    pub fn start(name: &'static str) -> Phase {
        eprintln!("  {name} ...");
        Phase {
            name,
            start: Instant::now(),
        }
    }

    pub fn done(self, items: u64) {
        let s = self.start.elapsed().as_secs_f64();
        eprintln!(
            "  {} done: {items} in {s:.1} s ({:.0}/s)",
            self.name,
            items as f64 / s.max(1e-9)
        );
    }
}
