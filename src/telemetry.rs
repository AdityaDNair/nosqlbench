//! Telemetry: one `u64` record per event, per-worker SPSC rings, and an
//! aggregator thread that drains them into histograms.
//!
//! Record layout:
//! ```text
//! bits 63..16  latency_ns (48 bits, saturating)
//! bits 15..8   op kind
//! bits  7..4   status: 0 ok, 1 error, 2 timeout
//! bit   3      warmup (set for every record built outside the MEASURE phase)
//! bit   2      is_send_lag record
//! bits  1..0   reserved
//! ```

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use rtrb::Consumer;

use crate::workload::OpKind;

pub const STATUS_OK: u8 = 0;
pub const STATUS_ERROR: u8 = 1;
pub const STATUS_TIMEOUT: u8 = 2;

const LAT_MAX: u64 = (1 << 48) - 1;
const WARMUP_BIT: u64 = 1 << 3;
const SEND_LAG_BIT: u64 = 1 << 2;

/// Histogram range: 1 ns .. 60 s, 3 significant figures.
pub const HIST_LOW: u64 = 1;
pub const HIST_HIGH: u64 = 60_000_000_000;
pub const HIST_SIGFIG: u8 = 3;

pub fn new_histogram() -> Histogram<u64> {
    Histogram::new_with_bounds(HIST_LOW, HIST_HIGH, HIST_SIGFIG).expect("valid histogram bounds")
}

#[inline(always)]
pub fn pack(latency_ns: u64, kind: OpKind, status: u8, warmup: bool, send_lag: bool) -> u64 {
    (latency_ns.min(LAT_MAX) << 16)
        | ((kind as u64) << 8)
        | (((status & 0xF) as u64) << 4)
        | if warmup { WARMUP_BIT } else { 0 }
        | if send_lag { SEND_LAG_BIT } else { 0 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub latency_ns: u64,
    pub kind: u8,
    pub status: u8,
    pub warmup: bool,
    pub send_lag: bool,
}

#[inline(always)]
pub fn unpack(r: u64) -> Record {
    Record {
        latency_ns: r >> 16,
        kind: ((r >> 8) & 0xFF) as u8,
        status: ((r >> 4) & 0xF) as u8,
        warmup: r & WARMUP_BIT != 0,
        send_lag: r & SEND_LAG_BIT != 0,
    }
}

/// Run phases, shared by main, workers and aggregator.
pub const PHASE_INIT: u8 = 0;
pub const PHASE_WARMUP: u8 = 1;
pub const PHASE_MEASURE: u8 = 2;
pub const PHASE_STOP: u8 = 3;
pub const PHASE_ABORT: u8 = 4;

pub struct Shared {
    pub phase: AtomicU8,
    /// Set by main once every worker has exited; aggregator does a final drain.
    pub workers_done: AtomicBool,
}

impl Shared {
    pub fn new() -> Arc<Shared> {
        Arc::new(Shared {
            phase: AtomicU8::new(PHASE_INIT),
            workers_done: AtomicBool::new(false),
        })
    }
}

/// Aggregated results for one run.
pub struct Aggregate {
    /// Ok latencies per op kind (ns), measured phase only.
    pub ok: Vec<Histogram<u64>>,
    pub errors: [u64; OpKind::COUNT],
    pub timeouts: [u64; OpKind::COUNT],
    /// Open-loop send lag (t0 - intended, ns), measured phase only.
    pub send_lag: Histogram<u64>,
    /// Per-second (by drain time since MEASURE began) ok/error counts per op.
    pub timeseries: Vec<[[u64; 2]; OpKind::COUNT]>,
    pub records_drained: u64,
    pub warmup_discarded: u64,
}

impl Aggregate {
    fn new() -> Aggregate {
        Aggregate {
            ok: (0..OpKind::COUNT).map(|_| new_histogram()).collect(),
            errors: [0; OpKind::COUNT],
            timeouts: [0; OpKind::COUNT],
            send_lag: new_histogram(),
            timeseries: Vec::new(),
            records_drained: 0,
            warmup_discarded: 0,
        }
    }

    fn ingest(&mut self, raw: u64, second: usize) {
        self.records_drained += 1;
        let r = unpack(raw);
        if r.warmup {
            self.warmup_discarded += 1;
            return;
        }
        let k = r.kind as usize;
        if k >= OpKind::COUNT {
            return;
        }
        let v = r.latency_ns.clamp(HIST_LOW, HIST_HIGH);
        if r.send_lag {
            self.send_lag.saturating_record(v);
            return;
        }
        match r.status {
            STATUS_OK => self.ok[k].saturating_record(v),
            STATUS_TIMEOUT => self.timeouts[k] += 1,
            _ => self.errors[k] += 1,
        }
        if self.timeseries.len() <= second {
            self.timeseries.resize(second + 1, [[0; 2]; OpKind::COUNT]);
        }
        let slot = if r.status == STATUS_OK { 0 } else { 1 };
        self.timeseries[second][k][slot] += 1;
    }
}

/// Aggregator thread body. Drains every ring; sleeps 1 ms when all are empty.
pub fn aggregate(mut rings: Vec<Consumer<u64>>, shared: Arc<Shared>) -> Aggregate {
    let mut agg = Aggregate::new();
    let mut measure_start: Option<Instant> = None;
    loop {
        // Read the flag before draining so a final pass always follows it.
        let done = shared.workers_done.load(Ordering::Acquire);
        if measure_start.is_none() && shared.phase.load(Ordering::Relaxed) >= PHASE_MEASURE {
            measure_start = Some(Instant::now());
        }
        // Measured records seen before the aggregator notices MEASURE (<= 1 ms)
        // land in second 0.
        let second = measure_start.map_or(0, |t| t.elapsed().as_secs() as usize);
        let mut drained = 0usize;
        for ring in rings.iter_mut() {
            let n = ring.slots();
            if n == 0 {
                continue;
            }
            let chunk = ring.read_chunk(n).expect("slots() were available");
            for raw in chunk {
                agg.ingest(raw, second);
            }
            drained += n;
        }
        if done && drained == 0 {
            return agg;
        }
        if drained == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_roundtrip() {
        let r = pack(
            123_456_789,
            OpKind::HashtagSearch,
            STATUS_TIMEOUT,
            true,
            false,
        );
        assert_eq!(
            unpack(r),
            Record {
                latency_ns: 123_456_789,
                kind: OpKind::HashtagSearch as u8,
                status: STATUS_TIMEOUT,
                warmup: true,
                send_lag: false
            }
        );
        let r = pack(u64::MAX, OpKind::UserFollow, STATUS_OK, false, true);
        let u = unpack(r);
        assert_eq!(u.latency_ns, LAT_MAX);
        assert!(u.send_lag && !u.warmup);
        assert_eq!(r & 0b11, 0, "reserved bits stay zero");
    }

    #[test]
    fn aggregator_discards_warmup_and_counts_status() {
        let (mut p, c) = rtrb::RingBuffer::new(16);
        let shared = Shared::new();
        p.push(pack(10, OpKind::PostLike, STATUS_OK, true, false))
            .unwrap();
        p.push(pack(20, OpKind::PostLike, STATUS_OK, false, false))
            .unwrap();
        p.push(pack(30, OpKind::PostLike, STATUS_ERROR, false, false))
            .unwrap();
        p.push(pack(40, OpKind::PostLike, STATUS_TIMEOUT, false, false))
            .unwrap();
        p.push(pack(50, OpKind::PostLike, STATUS_OK, false, true))
            .unwrap();
        shared.workers_done.store(true, Ordering::Release);
        let a = aggregate(vec![c], shared);
        let k = OpKind::PostLike as usize;
        assert_eq!(a.ok[k].len(), 1);
        assert_eq!(a.errors[k], 1);
        assert_eq!(a.timeouts[k], 1);
        assert_eq!(a.send_lag.len(), 1);
        assert_eq!(a.warmup_discarded, 1);
    }
}
