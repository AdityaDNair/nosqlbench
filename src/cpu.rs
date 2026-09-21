//! CPU topology (P/E cores), pinning plan and Windows timer resolution.
//!
//! This is the only module with `unsafe`: two Win32 calls, each commented.

use anyhow::{bail, Result};
use serde::Serialize;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, RelationProcessorCore,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};

use crate::config::Config;

/// One physical core as reported by Windows.
#[derive(Debug, Clone, Serialize)]
pub struct PhysicalCore {
    pub efficiency_class: u8,
    /// Logical CPU indices (group 0) belonging to this core, ascending.
    pub logical: Vec<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Topology {
    pub cores: Vec<PhysicalCore>,
    /// True when every core has the same EfficiencyClass (no hybrid design).
    pub uniform: bool,
}

impl Topology {
    fn max_class(&self) -> u8 {
        self.cores
            .iter()
            .map(|c| c.efficiency_class)
            .max()
            .unwrap_or(0)
    }

    /// P-cores: highest EfficiencyClass. On a uniform CPU every core is P.
    pub fn p_cores(&self) -> Vec<&PhysicalCore> {
        let m = self.max_class();
        self.cores
            .iter()
            .filter(|c| self.uniform || c.efficiency_class == m)
            .collect()
    }

    pub fn e_cores(&self) -> Vec<&PhysicalCore> {
        let m = self.max_class();
        self.cores
            .iter()
            .filter(|c| !self.uniform && c.efficiency_class != m)
            .collect()
    }
}

/// Query Windows for physical cores via `GetLogicalProcessorInformationEx`.
pub fn detect_topology() -> Result<Topology> {
    let mut len: u32 = 0;
    // SAFETY: first call with a null buffer only writes the required length
    // into `len`; it is expected to fail with ERROR_INSUFFICIENT_BUFFER.
    let _ = unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut len) };
    if len == 0 {
        bail!("GetLogicalProcessorInformationEx returned no data");
    }
    // u64 storage gives the 8-byte alignment the structs need.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `len` bytes, is 8-byte aligned, and Windows
    // writes at most `len` bytes of variable-size records into it.
    unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            Some(buf.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
            &mut len,
        )?;
    }

    let base = buf.as_ptr() as *const u8;
    let mut off = 0usize;
    let mut cores = Vec::new();
    while off < len as usize {
        // SAFETY: `off` always lands on a record boundary inside the `len`
        // bytes Windows filled; each record starts with its own `Size`. For
        // RelationProcessorCore records the union member is `Processor`.
        let (size, class, group_count, group, mask) = unsafe {
            let rec = &*(base.add(off) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
            let p = &rec.Anonymous.Processor;
            (
                rec.Size as usize,
                p.EfficiencyClass,
                p.GroupCount,
                p.GroupMask[0].Group,
                p.GroupMask[0].Mask,
            )
        };
        if size == 0 {
            bail!("malformed processor record (size 0)");
        }
        if group_count != 1 || group != 0 {
            bail!(
                "multiple processor groups detected (core in group {group}, group_count \
                 {group_count}); this benchmark supports a single processor group only"
            );
        }
        let logical: Vec<usize> = (0..usize::BITS as usize)
            .filter(|b| mask & (1usize << b) != 0)
            .collect();
        cores.push(PhysicalCore {
            efficiency_class: class,
            logical,
        });
        off += size;
    }
    cores.sort_by_key(|c| c.logical[0]);
    let first = cores.first().map(|c| c.efficiency_class);
    let uniform = cores.iter().all(|c| Some(c.efficiency_class) == first);
    Ok(Topology { cores, uniform })
}

/// Why the auto plan leaves the P-core holding logical CPU 0 alone.
pub const SKIP_CPU0_REASON: &str = "P-core 0 skipped for workers: Windows routes most device interrupts and DPCs to logical CPU 0, which would add jitter to a worker pinned there";

#[derive(Debug, Clone, Serialize)]
pub struct PinPlan {
    /// Logical CPU for each worker thread, index = worker id.
    pub worker_cpus: Vec<usize>,
    pub aggregator_cpu: usize,
    pub p_core_cpus: Vec<Vec<usize>>,
    pub e_core_cpus: Vec<Vec<usize>>,
    pub uniform_cores: bool,
    pub workers_requested: u32,
    /// P-cores excluded from the auto worker pool, and why.
    pub skipped_p_cores: Vec<Vec<usize>>,
    pub skip_reason: Option<String>,
    pub note: String,
}

/// Build the pinning plan.
///
/// Auto mode: one worker per P-core (its lowest logical CPU), skipping the
/// P-core that holds logical CPU 0 when at least one other P-core exists.
/// Worker count = min(run.workers, available CPUs), further capped at
/// concurrency in closed mode.
pub fn plan(cfg: &Config, topo: &Topology) -> Result<PinPlan> {
    let p = topo.p_cores();
    let e = topo.e_cores();
    if p.is_empty() {
        bail!("no cores detected");
    }
    let n_logical: usize = topo.cores.iter().map(|c| c.logical.len()).sum();
    let mut notes = Vec::new();
    let mut skipped_p_cores = Vec::new();
    let mut skip_reason = None;

    let pool: Vec<usize> = if cfg.cpu.worker_cpus.is_empty() {
        let (with0, rest): (Vec<&&PhysicalCore>, Vec<&&PhysicalCore>) =
            p.iter().partition(|c| c.logical.contains(&0));
        if !with0.is_empty() && !rest.is_empty() {
            skipped_p_cores = with0.iter().map(|c| c.logical.clone()).collect();
            skip_reason = Some(SKIP_CPU0_REASON.to_string());
            rest.iter().map(|c| c.logical[0]).collect()
        } else {
            p.iter().map(|c| c.logical[0]).collect()
        }
    } else {
        for &c in &cfg.cpu.worker_cpus {
            if c >= n_logical {
                bail!("cpu.worker_cpus contains {c}, but only {n_logical} logical CPUs exist");
            }
        }
        notes.push("worker CPUs set explicitly".to_string());
        cfg.cpu.worker_cpus.clone()
    };

    let mut n = (cfg.run.workers as usize).min(pool.len());
    if cfg.run.mode == crate::config::Mode::Closed {
        n = n.min(cfg.run.concurrency as usize);
    }
    let worker_cpus: Vec<usize> = pool.into_iter().take(n).collect();
    if (worker_cpus.len() as u32) < cfg.run.workers {
        notes.push(format!(
            "workers capped {} -> {}",
            cfg.run.workers,
            worker_cpus.len()
        ));
    }

    let aggregator_cpu = if cfg.cpu.aggregator_cpu >= 0 {
        let c = cfg.cpu.aggregator_cpu as usize;
        if c >= n_logical {
            bail!("cpu.aggregator_cpu {c} out of range (0..{n_logical})");
        }
        c
    } else if let Some(ec) = e.first() {
        ec.logical[0]
    } else {
        // No E-cores: last P-core. Prefer its highest logical CPU (the SMT
        // sibling) so the aggregator never shares a logical CPU with a worker.
        *p.last()
            .expect("non-empty")
            .logical
            .last()
            .expect("core has a logical CPU")
    };
    if worker_cpus.contains(&aggregator_cpu) {
        notes.push(format!(
            "aggregator shares logical CPU {aggregator_cpu} with a worker"
        ));
    }

    Ok(PinPlan {
        worker_cpus,
        aggregator_cpu,
        p_core_cpus: p.iter().map(|c| c.logical.clone()).collect(),
        e_core_cpus: e.iter().map(|c| c.logical.clone()).collect(),
        uniform_cores: topo.uniform,
        workers_requested: cfg.run.workers,
        skipped_p_cores,
        skip_reason,
        note: notes.join("; "),
    })
}

impl PinPlan {
    pub fn print(&self) {
        println!("CPU topology:");
        println!(
            "  P-cores ({}): {}",
            self.p_core_cpus.len(),
            fmt_cores(&self.p_core_cpus)
        );
        println!(
            "  E-cores ({}): {}",
            self.e_core_cpus.len(),
            fmt_cores(&self.e_core_cpus)
        );
        if self.uniform_cores {
            println!("  (all cores report the same EfficiencyClass: treated as P-cores)");
        }
        println!("Pinning plan:");
        for (i, c) in self.worker_cpus.iter().enumerate() {
            println!("  worker {i} -> logical CPU {c}");
        }
        println!("  aggregator -> logical CPU {}", self.aggregator_cpu);
        if let Some(r) = &self.skip_reason {
            println!("  skipped {}: {r}", fmt_cores(&self.skipped_p_cores));
        }
        if !self.note.is_empty() {
            println!("  note: {}", self.note);
        }
    }
}

fn fmt_cores(cores: &[Vec<usize>]) -> String {
    cores
        .iter()
        .map(|l| {
            let s: Vec<String> = l.iter().map(|x| x.to_string()).collect();
            format!("[{}]", s.join(","))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Pin the calling thread to one logical CPU.
pub fn pin_current(cpu: usize) -> Result<()> {
    if core_affinity::set_for_current(core_affinity::CoreId { id: cpu }) {
        Ok(())
    } else {
        bail!("failed to pin thread to logical CPU {cpu}")
    }
}

/// Raise the calling thread's scheduling priority (0 = normal, 2 = highest,
/// 15 = time critical).
pub fn set_thread_priority(level: i32) -> Result<()> {
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY};
    // SAFETY: GetCurrentThread returns a pseudo-handle for this thread that
    // needs no closing; SetThreadPriority takes it plus an integer level.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY(level))? };
    Ok(())
}

/// RAII guard: 1 ms system timer resolution for the lifetime of the value.
pub struct TimerResolution {
    active: bool,
}

impl TimerResolution {
    pub fn one_ms() -> Result<TimerResolution> {
        // SAFETY: plain Win32 call with no pointers; paired with
        // timeEndPeriod(1) in Drop. Returns 0 (TIMERR_NOERROR) on success.
        let rc = unsafe { timeBeginPeriod(1) };
        if rc != 0 {
            bail!("timeBeginPeriod(1) failed with code {rc}");
        }
        Ok(TimerResolution { active: true })
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: matches the successful timeBeginPeriod(1) above.
            unsafe {
                timeEndPeriod(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topo(classes: &[(u8, &[usize])]) -> Topology {
        let cores: Vec<PhysicalCore> = classes
            .iter()
            .map(|(c, l)| PhysicalCore {
                efficiency_class: *c,
                logical: l.to_vec(),
            })
            .collect();
        let uniform = cores
            .iter()
            .all(|c| c.efficiency_class == cores[0].efficiency_class);
        Topology { cores, uniform }
    }

    fn cfg(workers: u32) -> Config {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("config")
            .join("default.toml");
        let mut c = Config::load(&path, &[]).unwrap();
        c.run.workers = workers;
        c
    }

    #[test]
    fn hybrid_plan_uses_p_cores_and_first_e_core() {
        let t = topo(&[
            (1, &[0, 1]),
            (1, &[2, 3]),
            (1, &[4, 5]),
            (0, &[6]),
            (0, &[7]),
        ]);
        let p = plan(&cfg(4), &t).unwrap();
        assert_eq!(p.worker_cpus, vec![2, 4]);
        assert_eq!(p.aggregator_cpu, 6);
        assert_eq!(p.skipped_p_cores, vec![vec![0, 1]]);
        assert!(p.skip_reason.is_some());
    }

    #[test]
    fn uniform_plan_uses_sibling_of_last_core_for_aggregator() {
        let t = topo(&[(0, &[0, 1]), (0, &[2, 3])]);
        let p = plan(&cfg(4), &t).unwrap();
        assert_eq!(p.worker_cpus, vec![2]);
        assert_eq!(p.aggregator_cpu, 3);
        assert!(p.uniform_cores);
    }

    #[test]
    fn single_p_core_is_not_skipped() {
        let t = topo(&[(1, &[0, 1]), (0, &[2])]);
        let p = plan(&cfg(4), &t).unwrap();
        assert_eq!(p.worker_cpus, vec![0]);
        assert!(p.skip_reason.is_none());
    }

    #[test]
    fn explicit_worker_cpus_are_used_as_given() {
        let t = topo(&[(1, &[0, 1]), (1, &[2, 3]), (0, &[4])]);
        let mut c = cfg(4);
        c.cpu.worker_cpus = vec![0, 1, 2];
        let p = plan(&c, &t).unwrap();
        assert_eq!(p.worker_cpus, vec![0, 1, 2]);
        assert!(p.skip_reason.is_none());
    }

    #[test]
    fn real_topology_is_detectable() {
        let t = detect_topology().unwrap();
        assert!(!t.cores.is_empty());
    }
}
