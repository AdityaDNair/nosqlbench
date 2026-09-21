// Temporary: M1 defines items that the engine (M2) and loaders (M3+) consume.
#![allow(dead_code)]

mod config;
mod cpu;
mod dataset;
mod workload;

use std::path::PathBuf;
use std::time::Instant;

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
    /// No-op calibration: engine overhead and ceiling throughput.
    Calibrate(CommonArgs),
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

#[derive(Args)]
struct RunArgs {
    #[arg(long, value_enum)]
    db: Db,
    /// Print the resolved config and pinning plan, then exit.
    #[arg(long)]
    dry_run: bool,
    #[command(flatten)]
    common: CommonArgs,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Load(a) => {
            let _cfg = a.common.load_config()?;
            bail!("`bench load --db {}` is not implemented yet", a.db.name())
        }
        Cmd::Run(a) => {
            let cfg = a.common.load_config()?;
            if a.dry_run {
                return dry_run(a.db, &cfg);
            }
            bail!("`bench run` is not implemented yet (M2)")
        }
        Cmd::Calibrate(_) => bail!("`bench calibrate` is not implemented yet (M2)"),
    }
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
    Ok(())
}
