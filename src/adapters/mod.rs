//! Database adapters. Each worker builds its own instance inside its own
//! runtime; workers are generic over `A: Adapter` (static dispatch).

pub mod cassandra;
pub mod mongo;
pub mod noop;
pub mod redis;
pub mod redis_ping;

use crate::config::Config;
use crate::dataset::BodyArena;
use crate::workload::Op;

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    /// The driver reported a timeout.
    #[error("timeout")]
    Timeout,
    /// Any other database or driver error. Built only on the error path.
    #[error("{0}")]
    Db(String),
}

// Only used inside this binary with `current_thread` runtimes, so the
// futures never need to be `Send`.
#[allow(async_fn_in_trait)]
pub trait Adapter: Sized + 'static {
    /// Connect and prepare. Called once per worker, inside that worker's runtime.
    async fn connect(cfg: &Config, worker_id: u32) -> anyhow::Result<Self>;

    /// Execute one op and fully consume the response.
    async fn execute(&self, op: &Op, bodies: &BodyArena) -> Result<(), AdapterError>;
}
