mod adapters;
mod config;
mod cpu;
mod dataset;
mod loaders;
mod output;
mod telemetry;
mod worker;
mod workload;

use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};

use config::Config;

#[derive(Parser)]
#[command(name = "bench", about = "Cross-database social-feed benchmark")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Load the deterministic dataset into a database.
    Load(LoadArgs),
    /// Run the workload against a database.
    Run(RunArgs),
    /// No-op calibration (closed, concurrency 128 unless overridden):
    /// engine overhead and ceiling throughput.
    Calibrate(CalibrateArgs),
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Db {
    Noop,
    RedisPing,
    Redis,
    Mongo,
    Cassandra,
    Neo4j,
}

impl Db {
    fn name(self) -> &'static str {
        match self {
            Db::Noop => "noop",
            Db::RedisPing => "redis-ping",
            Db::Redis => "redis",
            Db::Mongo => "mongo",
            Db::Cassandra => "cassandra",
            Db::Neo4j => "neo4j",
        }
    }
}

#[derive(Args, Clone)]
struct CommonArgs {
    /// Config file.
    #[arg(long, default_value = "config/default.toml")]
    config: PathBuf,
    /// Override any config field: `--set run.concurrency=128` (repeatable).
    #[arg(long = "set", value_name = "KEY=VALUE")]
    set: Vec<String>,
    #[arg(long)]
    seed: Option<u64>,
    #[arg(long, value_parser = ["closed", "open"])]
    mode: Option<String>,
    #[arg(long)]
    concurrency: Option<u32>,
    #[arg(long)]
    rate: Option<u64>,
    #[arg(long)]
    max_inflight: Option<u32>,
    #[arg(long)]
    workers: Option<u32>,
    #[arg(long)]
    warmup_secs: Option<u64>,
    #[arg(long)]
    measure_secs: Option<u64>,
}

impl CommonArgs {
    fn load_config(&self) -> Result<Config> {
        let mut ov = Vec::new();
        let mut push = |k: &str, v: Option<String>| {
            if let Some(v) = v {
                ov.push(format!("{k}={v}"));
            }
        };
        push("seed", self.seed.map(|v| v.to_string()));
        push("run.mode", self.mode.as_ref().map(|m| format!("\"{m}\"")));
        push("run.concurrency", self.concurrency.map(|v| v.to_string()));
        push("run.rate", self.rate.map(|v| v.to_string()));
        push("run.max_inflight", self.max_inflight.map(|v| v.to_string()));
        push("run.workers", self.workers.map(|v| v.to_string()));
        push("run.warmup_secs", self.warmup_secs.map(|v| v.to_string()));
        push("run.measure_secs", self.measure_secs.map(|v| v.to_string()));
        // Explicit --set entries win over named flags.
        ov.extend(self.set.iter().cloned());
        Config::load(&self.config, &ov)
    }
}

#[derive(Args)]
struct LoadArgs {
    #[arg(long, value_enum)]
    db: Db,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Args, Clone)]
struct OutputArgs {
    /// Results root directory.
    #[arg(long, default_value = "results")]
    out: PathBuf,
    /// Level label for the output directory (default: concurrency in closed
    /// mode, rate in open mode).
    #[arg(long)]
    level: Option<String>,
    /// Run number for the output directory.
    #[arg(long, default_value_t = 1)]
    run: u32,
    /// Overwrite an existing result directory.
    #[arg(long)]
    force: bool,
    /// Refuse to start unless the Docker VM CPU affinity covers every running
    /// VM process, and fail the run if a VM process changed during it.
    #[arg(long)]
    require_vm_affinity: bool,
}

#[derive(Args)]
struct RunArgs {
    #[arg(long, value_enum)]
    db: Db,
    /// Print the resolved config and pinning plan, then exit.
    #[arg(long)]
    dry_run: bool,
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    output: OutputArgs,
}

#[derive(Args)]
struct CalibrateArgs {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    output: OutputArgs,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Load(a) => {
            let cfg = a.common.load_config()?;
            load(a.db, &cfg)
        }
        Cmd::Run(a) => {
            let cfg = a.common.load_config()?;
            if a.dry_run {
                return dry_run(a.db, &cfg);
            }
            run(a.db, &cfg, &a.output)
        }
        Cmd::Calibrate(mut a) => {
            // Calibration defaults, applied before user overrides so those win.
            let mut set = vec![
                "run.mode=\"closed\"".to_string(),
                "run.concurrency=128".into(),
            ];
            set.append(&mut a.common.set);
            a.common.set = set;
            if a.common.mode.is_some() || a.common.concurrency.is_some() {
                // Named flags are applied before --set; move them after.
                if let Some(m) = a.common.mode.take() {
                    a.common.set.push(format!("run.mode=\"{m}\""));
                }
                if let Some(c) = a.common.concurrency.take() {
                    a.common.set.push(format!("run.concurrency={c}"));
                }
            }
            let cfg = a.common.load_config()?;
            run(Db::Noop, &cfg, &a.output)
        }
    }
}

fn run(db: Db, cfg: &Config, out: &OutputArgs) -> Result<()> {
    let _timer = cpu::TimerResolution::one_ms()?;
    let topo = cpu::detect_topology()?;
    let plan = cpu::plan(cfg, &topo)?;
    plan.print();
    println!("  worker thread priority: {:?}", cfg.cpu.worker_priority);
    let level = out.level.clone().unwrap_or_else(|| match cfg.run.mode {
        config::Mode::Closed => cfg.run.concurrency.to_string(),
        config::Mode::Open => cfg.run.rate.to_string(),
    });
    let id = output::RunId {
        scenario: cfg.workload.scenario.as_str().to_string(),
        db: db.name().to_string(),
        mode: cfg.run.mode.as_str().to_string(),
        level,
        run: out.run,
    };
    let dir = id.dir(&out.out);
    if dir.join("summary.csv").exists() && !out.force {
        bail!(
            "{} already has results; pass --force to overwrite",
            dir.display()
        );
    }
    let mut sys = output::SysInfo::collect();
    let affinity_before = sys.vm_affinity_in_effect();
    println!("  docker VM affinity in effect: {affinity_before}");
    if out.require_vm_affinity && !affinity_before {
        bail!(
            "Docker VM CPU affinity is not in effect (running VM PIDs are not all covered by \
             .vm_affinity.json); re-run scripts\\set_vm_affinity.ps1 elevated"
        );
    }
    let start = SystemTime::now();
    let res = match db {
        Db::Noop => worker::run_engine::<adapters::noop::Noop>(cfg, &plan)?,
        Db::RedisPing => worker::run_engine::<adapters::redis_ping::RedisPing>(cfg, &plan)?,
        Db::Redis => worker::run_engine::<adapters::redis::RedisAdapter>(cfg, &plan)?,
        other => bail!("adapter `{}` is not implemented yet", other.name()),
    };
    // A VM restart during the run would drop the affinity: re-check the PIDs.
    let affinity_after = sys.recheck_vm_affinity();
    let rows = output::write_run(&dir, &id, cfg, &plan, &res, &sys, start, true)?;
    output::print_table(&id, &rows, &res, cfg.run.mode == config::Mode::Open);
    println!("results: {}", dir.display());
    if out.require_vm_affinity && !affinity_after {
        bail!("Docker VM processes changed during the run (affinity lost): run is invalid");
    }
    if res.dropped > 0 {
        bail!("{} telemetry records dropped: run is invalid", res.dropped);
    }
    Ok(())
}

fn load(db: Db, cfg: &Config) -> Result<()> {
    let t = Instant::now();
    let ds = dataset::Dataset::generate(cfg);
    eprintln!(
        "generated dataset (fingerprint {:#018x}) in {:.1} s; loading into {}",
        ds.fingerprint(),
        t.elapsed().as_secs_f64(),
        db.name()
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let t = Instant::now();
    match db {
        Db::Redis => rt.block_on(loaders::redis::load(cfg, &ds))?,
        Db::Noop | Db::RedisPing => bail!("`{}` has no dataset to load", db.name()),
        other => bail!("loader for `{}` is not implemented yet", other.name()),
    }
    eprintln!("load complete in {:.1} s", t.elapsed().as_secs_f64());
    print_fanout_stats(cfg, &ds);
    Ok(())
}

fn dry_run(db: Db, cfg: &Config) -> Result<()> {
    println!("db: {}", db.name());
    println!("--- resolved config ---");
    print!("{}", toml::to_string_pretty(cfg)?);
    println!("--- normalised op mix ---");
    for (k, w) in workload::OpKind::ALL.iter().zip(cfg.normalised_weights()) {
        println!("  {:<18} {:>6.2}%", k.name(), w * 100.0);
    }
    println!("---");
    let topo = cpu::detect_topology()?;
    let plan = cpu::plan(cfg, &topo)?;
    plan.print();
    let n = plan.worker_cpus.len() as u32;
    match cfg.run.mode {
        config::Mode::Closed => println!(
            "closed mode: {} workers x {} loops = {} in flight",
            n,
            cfg.run.concurrency / n,
            (cfg.run.concurrency / n) * n
        ),
        config::Mode::Open => println!(
            "open mode: {} workers, {} ops/s each, {} loops each",
            n,
            cfg.run.rate / n as u64,
            cfg.run.max_inflight / n
        ),
    }
    let t = Instant::now();
    let ds = dataset::Dataset::generate(cfg);
    println!(
        "dataset: {} users, {} follows, {} posts, fingerprint {:#018x} ({:.0} ms)",
        ds.users.len(),
        ds.follow_targets.len(),
        ds.posts.len(),
        ds.fingerprint(),
        t.elapsed().as_secs_f64() * 1e3
    );
    print_fanout_stats(cfg, &ds);
    Ok(())
}

/// Timeline size and the fan-out that POST_CREATE will cause (from worker 0's
/// op stream), for the configured scenario.
fn print_fanout_stats(cfg: &Config, ds: &dataset::Dataset) {
    let (off, tl) = ds.timelines();
    let (fo, _) = ds.followers();
    let followers = |u: u64| (fo[u as usize + 1] - fo[u as usize]) as u64;
    let cap = cfg.dataset.fanout_cap as u64;
    let untrimmed: u64 = ds.posts.iter().map(|p| followers(p.author).min(cap)).sum();
    println!(
        "scenario {}: timelines {} entries (newest {} per user; untrimmed {}), longest timeline {}",
        cfg.workload.scenario.as_str(),
        tl.len(),
        cfg.dataset.timeline_len,
        untrimmed,
        off.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(0)
    );
    let top = ds.activity[0];
    let posts_by = |u: u64| ds.posts.iter().filter(|p| p.author == u).count();
    println!(
        "most active user {top}: {} followers, {} posts; most followed user 0: {} followers, {} posts",
        followers(top),
        posts_by(top),
        followers(0),
        posts_by(0)
    );
    let stream = workload::OpStream::generate(cfg, 0);
    let (mut n, mut sum) = (0u64, 0u64);
    for op in &stream.ops {
        if op.kind == workload::OpKind::PostCreate {
            n += 1;
            sum += followers(op.user).min(cap);
        }
    }
    println!(
        "expected fan-out per POST_CREATE (worker 0 stream, loaded follow graph): {:.1} over {n} ops",
        sum as f64 / n.max(1) as f64
    );
}
