//! Transport baseline: one `PING` per op, whatever the op kind. Measures the
//! Windows -> Docker VM -> container round trip every database pays.

use super::redis::{map_err, Connections};
use super::{Adapter, AdapterError};
use crate::config::Config;
use crate::dataset::BodyArena;
use crate::workload::Op;

pub struct RedisPing {
    cons: Connections,
}

impl Adapter for RedisPing {
    async fn connect(cfg: &Config, _worker_id: u32) -> anyhow::Result<Self> {
        Ok(RedisPing {
            cons: Connections::open(&cfg.endpoints.redis, cfg.drivers.redis_connections).await?,
        })
    }

    async fn execute(&self, _op: &Op, _bodies: &BodyArena) -> Result<(), AdapterError> {
        let mut con = self.cons.pick();
        let _: redis::Value = redis::cmd("PING")
            .query_async(&mut con)
            .await
            .map_err(map_err)?;
        Ok(())
    }
}
