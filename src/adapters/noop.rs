//! No-op adapter: measures engine overhead and ceiling throughput.

use super::{Adapter, AdapterError};
use crate::config::Config;
use crate::dataset::BodyArena;
use crate::workload::Op;

pub struct Noop;

impl Adapter for Noop {
    async fn connect(_cfg: &Config, _worker_id: u32) -> anyhow::Result<Self> {
        Ok(Noop)
    }

    async fn execute(&self, op: &Op, _bodies: &BodyArena) -> Result<(), AdapterError> {
        std::hint::black_box(op);
        Ok(())
    }
}
