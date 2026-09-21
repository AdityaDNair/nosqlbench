//! Workers: one pinned OS thread per worker, each running a `current_thread`
//! Tokio runtime with C long-lived request loops (`LocalSet` + `spawn_local`,
//! spawned once). The engine drives phases and collects telemetry.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use rtrb::{Producer, RingBuffer};
use tokio::task::LocalSet;

use crate::adapters::{Adapter, AdapterError};
use crate::config::{Config, Mode};
use crate::cpu::{self, PinPlan};
use crate::telemetry::{
    self, pack, Aggregate, Shared, PHASE_ABORT, PHASE_INIT, PHASE_MEASURE, PHASE_STOP,
    PHASE_WARMUP, STATUS_ERROR, STATUS_OK, STATUS_TIMEOUT,
};
use crate::workload::{Op, OpStream};

/// Open mode: sleep until this long before the intended time, then spin.
const SPIN_NS: u64 = 2_000_000;

/// What one worker runs.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkerSpec {
    pub id: u32,
    pub cpu: usize,
    /// Long-lived request loops in this worker.
    pub loops: u32,
    /// Open mode: this worker's target ops/s. Zero in closed mode.
    pub rate: f64,
}

#[derive(Debug, Default)]
pub struct WorkerReport {
    pub id: u32,
    pub dropped: u64,
    pub first_error: Option<String>,
}

/// Per-worker state shared by its request loops. Single-threaded: `Cell` and
/// `RefCell` only, never borrowed across an `.await`.
struct Ctx<A> {
    adapter: A,
    stream: OpStream,
    cursor: Cell<usize>,
    ring: RefCell<Producer<u64>>,
    dropped: Cell<u64>,
    first_error: RefCell<Option<String>>,
    shared: Arc<Shared>,
    clock: quanta::Clock,
    timeout: Duration,
    // Open mode pacing.
    ticket: Cell<u64>,
    interval_ns: f64,
    start_raw: u64,
    tokio_start: tokio::time::Instant,
}

impl<A: Adapter> Ctx<A> {
    #[inline(always)]
    fn next_op(&self) -> &Op {
        let i = self.cursor.get();
        let next = i + 1;
        self.cursor.set(if next == self.stream.ops.len() {
            0
        } else {
            next
        });
        &self.stream.ops[i]
    }

    #[inline(always)]
    fn phase(&self) -> u8 {
        self.shared.phase.load(Relaxed)
    }

    /// Nanoseconds since this worker's start (open mode time base).
    #[inline(always)]
    fn since_start_ns(&self, raw: u64) -> u64 {
        self.clock.delta_as_nanos(self.start_raw, raw)
    }

    #[inline(always)]
    fn status(&self, res: Result<Result<(), AdapterError>, tokio::time::error::Elapsed>) -> u8 {
        match res {
            Ok(Ok(())) => STATUS_OK,
            Ok(Err(AdapterError::Timeout)) | Err(_) => STATUS_TIMEOUT,
            Ok(Err(AdapterError::Db(msg))) => {
                // Error path only: keep the first message for the report.
                let mut first = self.first_error.borrow_mut();
                if first.is_none() {
                    *first = Some(msg);
                }
                STATUS_ERROR
            }
        }
    }

    /// Never blocks: a full ring increments `dropped` and moves on.
    #[inline(always)]
    fn push(&self, rec: u64) {
        if self.ring.borrow_mut().push(rec).is_err() {
            self.dropped.set(self.dropped.get() + 1);
        }
    }
}

async fn closed_loop<A: Adapter>(ctx: Rc<Ctx<A>>) {
    while ctx.phase() != PHASE_STOP {
        let op = ctx.next_op();
        // Building the future does no work; the request starts at `.await`.
        let fut = tokio::time::timeout(ctx.timeout, ctx.adapter.execute(op, &ctx.stream.bodies));
        let t0 = ctx.clock.raw(); // T0: immediately before execute
        let res = fut.await;
        let t1 = ctx.clock.raw(); // T1: immediately after the response is consumed
        let status = ctx.status(res);
        let warmup = ctx.phase() != PHASE_MEASURE;
        let latency = ctx.clock.delta_as_nanos(t0, t1);
        ctx.push(pack(latency, op.kind, status, warmup, false));
        // Yields only when this task's coop budget is spent, so loops whose
        // adapter never pends (noop) still take turns.
        tokio::task::consume_budget().await;
    }
}

async fn open_loop<A: Adapter>(ctx: Rc<Ctx<A>>) {
    while ctx.phase() != PHASE_STOP {
        let k = ctx.ticket.get() + 1;
        ctx.ticket.set(k);
        // Intended send time, ns since start. Never re-based: late tickets are
        // charged to latency.
        let intended = (k as f64 * ctx.interval_ns) as u64;

        let now = ctx.since_start_ns(ctx.clock.raw());
        if intended > now + SPIN_NS {
            let wake = ctx.tokio_start + Duration::from_nanos(intended - SPIN_NS);
            tokio::time::sleep_until(wake).await;
        }
        while ctx.since_start_ns(ctx.clock.raw()) < intended {
            tokio::task::yield_now().await;
        }
        if ctx.phase() == PHASE_STOP {
            break;
        }

        let op = ctx.next_op();
        let fut = tokio::time::timeout(ctx.timeout, ctx.adapter.execute(op, &ctx.stream.bodies));
        let t0 = ctx.clock.raw(); // T0: immediately before execute
        let res = fut.await;
        let t1 = ctx.clock.raw(); // T1: immediately after the response is consumed
        let status = ctx.status(res);
        let warmup = ctx.phase() != PHASE_MEASURE;
        let t0_ns = ctx.since_start_ns(t0);
        let t1_ns = ctx.since_start_ns(t1);
        // Response time is measured from the intended send time.
        ctx.push(pack(
            t1_ns.saturating_sub(intended),
            op.kind,
            status,
            warmup,
            false,
        ));
        ctx.push(pack(
            t0_ns.saturating_sub(intended),
            op.kind,
            status,
            warmup,
            true,
        ));
        tokio::task::consume_budget().await;
    }
}

fn spawn_worker<A: Adapter>(
    spec: WorkerSpec,
    cfg: Arc<Config>,
    shared: Arc<Shared>,
    ready: Arc<AtomicU32>,
    ring: Producer<u64>,
    clock: quanta::Clock,
) -> Result<JoinHandle<Result<WorkerReport>>> {
    std::thread::Builder::new()
        .name(format!("worker-{}", spec.id))
        .spawn(move || {
            cpu::pin_current(spec.cpu)?;
            cpu::set_thread_priority(cfg.cpu.worker_priority.win32())?;
            let stream = OpStream::generate(&cfg, spec.id);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("building worker runtime")?;
            let local = LocalSet::new();
            local.block_on(
                &rt,
                worker_main::<A>(spec, cfg, shared, ready, ring, clock, stream),
            )
        })
        .context("spawning worker thread")
}

async fn worker_main<A: Adapter>(
    spec: WorkerSpec,
    cfg: Arc<Config>,
    shared: Arc<Shared>,
    ready: Arc<AtomicU32>,
    ring: Producer<u64>,
    clock: quanta::Clock,
    stream: OpStream,
) -> Result<WorkerReport> {
    let adapter = A::connect(&cfg, spec.id)
        .await
        .with_context(|| format!("worker {} connect", spec.id))?;
    ready.fetch_add(1, Relaxed);
    loop {
        match shared.phase.load(Relaxed) {
            PHASE_INIT => tokio::time::sleep(Duration::from_millis(1)).await,
            PHASE_ABORT => {
                return Ok(WorkerReport {
                    id: spec.id,
                    ..Default::default()
                })
            }
            _ => break,
        }
    }

    let ctx = Rc::new(Ctx {
        adapter,
        stream,
        cursor: Cell::new(0),
        ring: RefCell::new(ring),
        dropped: Cell::new(0),
        first_error: RefCell::new(None),
        shared,
        start_raw: clock.raw(),
        tokio_start: tokio::time::Instant::now(),
        clock,
        timeout: Duration::from_millis(cfg.run.request_timeout_ms),
        ticket: Cell::new(0),
        interval_ns: if spec.rate > 0.0 {
            1e9 / spec.rate
        } else {
            0.0
        },
    });

    // All loops are created once, here. Nothing is spawned per request.
    let mut handles = Vec::with_capacity(spec.loops as usize);
    for _ in 0..spec.loops {
        let c = ctx.clone();
        handles.push(match cfg.run.mode {
            Mode::Closed => tokio::task::spawn_local(closed_loop(c)),
            Mode::Open => tokio::task::spawn_local(open_loop(c)),
        });
    }
    for h in handles {
        h.await.map_err(|e| anyhow!("request loop panicked: {e}"))?;
    }
    let first_error = ctx.first_error.borrow_mut().take();
    Ok(WorkerReport {
        id: spec.id,
        dropped: ctx.dropped.get(),
        first_error,
    })
}

/// Split `total` into `n` near-equal parts (earlier workers get the remainder).
fn split(total: u32, n: usize) -> Vec<u32> {
    let base = total / n as u32;
    let rem = total % n as u32;
    (0..n as u32).map(|i| base + u32::from(i < rem)).collect()
}

pub fn worker_specs(cfg: &Config, plan: &PinPlan) -> Vec<WorkerSpec> {
    let n = plan.worker_cpus.len();
    let loops = match cfg.run.mode {
        Mode::Closed => split(cfg.run.concurrency, n),
        Mode::Open => split(cfg.run.max_inflight, n),
    };
    let rate = match cfg.run.mode {
        Mode::Closed => 0.0,
        Mode::Open => cfg.run.rate as f64 / n as f64,
    };
    plan.worker_cpus
        .iter()
        .enumerate()
        .map(|(i, &cpu)| WorkerSpec {
            id: i as u32,
            cpu,
            loops: loops[i],
            rate,
        })
        .collect()
}

pub struct EngineResult {
    pub agg: Aggregate,
    pub workers: Vec<WorkerReport>,
    pub specs: Vec<WorkerSpec>,
    pub dropped: u64,
    /// Wall-clock length of the MEASURE phase as actually slept.
    pub measure_elapsed: Duration,
}

/// Run one benchmark: start aggregator and workers, drive the phases, join.
pub fn run_engine<A: Adapter>(cfg: &Config, plan: &PinPlan) -> Result<EngineResult> {
    let cfg = Arc::new(cfg.clone());
    let specs = worker_specs(&cfg, plan);
    let shared = Shared::new();
    let ready = Arc::new(AtomicU32::new(0));
    let clock = quanta::Clock::new();

    let mut producers = Vec::new();
    let mut consumers = Vec::new();
    for _ in &specs {
        let (p, c) = RingBuffer::new(cfg.run.ring_slots);
        producers.push(p);
        consumers.push(c);
    }

    let agg_shared = shared.clone();
    let agg_cpu = plan.aggregator_cpu;
    let aggregator = std::thread::Builder::new()
        .name("aggregator".into())
        .spawn(move || -> Result<Aggregate> {
            cpu::pin_current(agg_cpu)?;
            Ok(telemetry::aggregate(consumers, agg_shared))
        })?;

    let mut handles = Vec::new();
    for (spec, ring) in specs.iter().cloned().zip(producers) {
        handles.push(spawn_worker::<A>(
            spec,
            cfg.clone(),
            shared.clone(),
            ready.clone(),
            ring,
            clock.clone(),
        )?);
    }

    // Wait for every worker to connect; abort if any exits early.
    let n = specs.len() as u32;
    let mut early_exit = false;
    while ready.load(Relaxed) < n {
        if handles.iter().any(|h| h.is_finished()) {
            early_exit = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut measure_elapsed = Duration::ZERO;
    if early_exit {
        shared.phase.store(PHASE_ABORT, Relaxed);
    } else {
        eprintln!("warm-up {} s ...", cfg.run.warmup_secs);
        shared.phase.store(PHASE_WARMUP, Relaxed);
        std::thread::sleep(Duration::from_secs(cfg.run.warmup_secs));
        eprintln!("measure {} s ...", cfg.run.measure_secs);
        let t = Instant::now();
        shared.phase.store(PHASE_MEASURE, Relaxed);
        std::thread::sleep(Duration::from_secs(cfg.run.measure_secs));
        shared.phase.store(PHASE_STOP, Relaxed);
        measure_elapsed = t.elapsed();
    }

    let mut workers = Vec::new();
    let mut first_err = None;
    for h in handles {
        match h.join() {
            Ok(Ok(r)) => workers.push(r),
            Ok(Err(e)) => {
                first_err.get_or_insert(e);
            }
            Err(_) => {
                first_err.get_or_insert(anyhow!("worker thread panicked"));
            }
        }
    }
    shared
        .workers_done
        .store(true, std::sync::atomic::Ordering::Release);
    let agg = aggregator
        .join()
        .map_err(|_| anyhow!("aggregator panicked"))??;
    if let Some(e) = first_err {
        return Err(e);
    }
    let dropped = workers.iter().map(|w| w.dropped).sum();
    Ok(EngineResult {
        agg,
        workers,
        specs,
        dropped,
        measure_elapsed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_distributes_remainder() {
        assert_eq!(split(32, 4), vec![8, 8, 8, 8]);
        assert_eq!(split(10, 4), vec![3, 3, 2, 2]);
        assert_eq!(split(1, 1), vec![1]);
    }
}
