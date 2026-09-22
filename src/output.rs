//! Run output: summary.csv, timeseries.csv, <op>.hgrm, meta.json, stdout table.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use hdrhistogram::Histogram;
use serde::Serialize;
use serde_json::json;

use crate::config::Config;
use crate::cpu::PinPlan;
use crate::telemetry::new_histogram;
use crate::worker::EngineResult;
use crate::workload::OpKind;

/// Identifies one run: `results\<scenario>\<db>\<mode>_<level>_run<n>\`.
pub struct RunId {
    pub scenario: String,
    pub db: String,
    pub mode: String,
    pub level: String,
    pub run: u32,
}

impl RunId {
    pub fn dir(&self, root: &Path) -> PathBuf {
        root.join(&self.scenario)
            .join(&self.db)
            .join(format!("{}_{}_run{}", self.mode, self.level, self.run))
    }
}

/// Host facts recorded in meta.json. Every field is best-effort.
#[derive(Debug, Serialize)]
pub struct SysInfo {
    pub cpu_model: Option<String>,
    pub windows_build: Option<String>,
    pub power_source: Option<String>,
    pub power_scheme: Option<String>,
    pub docker_version: Option<String>,
    /// Kernel of the Docker VM; "...-microsoft-standard-WSL2" means the
    /// containers run in the WSL2 VM (process vmmemWSL).
    pub docker_kernel: Option<String>,
    pub docker_images: Vec<String>,
    pub git_commit: Option<String>,
    pub git_dirty: Option<bool>,
    /// Docker VM CPU affinity: the state written by scripts\set_vm_affinity.ps1,
    /// the VM processes running now, and whether the affinity covers them.
    pub vm_affinity: serde_json::Value,
}

/// Combine `.vm_affinity.json` (if present) with the running VM processes.
/// `in_effect` is true only if every running VM PID was pinned by the script.
fn vm_affinity(running: serde_json::Value) -> serde_json::Value {
    let state: Option<serde_json::Value> = std::fs::read_to_string(".vm_affinity.json")
        .ok()
        // Windows PowerShell 5.1 writes UTF-8 with a BOM.
        .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok());
    let pids = |v: &serde_json::Value| -> Vec<u64> {
        // ConvertTo-Json writes one element as an object, several as an array.
        match v {
            serde_json::Value::Array(a) => {
                a.iter().filter_map(|p| p.get("pid")?.as_u64()).collect()
            }
            serde_json::Value::Object(_) => {
                v.get("pid").and_then(|p| p.as_u64()).into_iter().collect()
            }
            _ => Vec::new(),
        }
    };
    let pinned = state
        .as_ref()
        .and_then(|s| s.get("processes"))
        .map(pids)
        .unwrap_or_default();
    let running_pids = pids(&running);
    let in_effect = !running_pids.is_empty() && running_pids.iter().all(|p| pinned.contains(p));
    // The actual mask of each VM process, read directly (needs elevation).
    let actual: Vec<serde_json::Value> = running_pids
        .iter()
        .map(|&pid| match crate::cpu::process_affinity_mask(pid as u32) {
            Ok(m) => json!({ "pid": pid, "mask": format!("0x{m:X}") }),
            Err(e) => json!({ "pid": pid, "mask": null, "error": e.to_string() }),
        })
        .collect();
    json!({
        "policy": "no VM affinity (tested and rejected in M3; see CLAUDE.md section 11)",
        "actual_masks": actual,
        "in_effect": in_effect,
        "mask": if in_effect { state.as_ref().and_then(|s| s.get("mask")).cloned() } else { None },
        "state_file": state,
        "running_vm": running,
    })
}

fn cmd_out(prog: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(prog).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

const SYSINFO_PS: &str = r#"
$c = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name
$os = Get-CimInstance Win32_OperatingSystem
$ubr = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion').UBR
$b = @(Get-CimInstance Win32_Battery)
if ($b.Count -eq 0) { $p = 'AC (no battery)' }
elseif (@($b | Where-Object { $_.BatteryStatus -ne 2 }).Count -eq 0) { $p = 'AC' }
else { $p = 'battery' }
$vm = @(Get-Process -Name vmmem, vmmemWSL -ErrorAction SilentlyContinue | ForEach-Object { [pscustomobject]@{ name = $_.Name; pid = $_.Id } })
[pscustomobject]@{ cpu = $c.Trim(); build = "$($os.Caption) $($os.BuildNumber).$ubr"; power = $p; vm = $vm } | ConvertTo-Json -Compress -Depth 3
"#;

/// Running Docker VM processes as `[{name, pid}]`.
fn running_vm() -> serde_json::Value {
    cmd_out(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", RUNNING_VM_PS],
    )
    .and_then(|s| serde_json::from_str(&s).ok())
    .unwrap_or(serde_json::Value::Null)
}

const RUNNING_VM_PS: &str = r#"
ConvertTo-Json -Compress -InputObject @(Get-Process -Name vmmem, vmmemWSL -ErrorAction SilentlyContinue | ForEach-Object { [pscustomobject]@{ name = $_.Name; pid = $_.Id } })
"#;

impl SysInfo {
    pub fn vm_affinity_in_effect(&self) -> bool {
        self.vm_affinity["in_effect"].as_bool().unwrap_or(false)
    }

    /// Re-read the running VM processes after a run and record whether the
    /// affinity still covers them (`in_effect_after_run`).
    pub fn recheck_vm_affinity(&mut self) -> bool {
        let after = vm_affinity(running_vm());
        let ok = after["in_effect"].as_bool().unwrap_or(false);
        self.vm_affinity["in_effect_after_run"] = serde_json::Value::Bool(ok);
        self.vm_affinity["running_vm_after_run"] = after["running_vm"].clone();
        ok
    }

    pub fn collect() -> SysInfo {
        let ps: Option<serde_json::Value> = cmd_out(
            "powershell.exe",
            &["-NoProfile", "-NonInteractive", "-Command", SYSINFO_PS],
        )
        .and_then(|s| serde_json::from_str(&s).ok());
        let field = |k: &str| {
            ps.as_ref()
                .and_then(|v| v.get(k))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let docker_images = cmd_out("docker", &["ps", "--format", "{{.Image}}"])
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default();
        let running_vm = ps
            .as_ref()
            .and_then(|v| v.get("vm"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        SysInfo {
            vm_affinity: vm_affinity(running_vm),
            cpu_model: field("cpu"),
            windows_build: field("build"),
            power_source: field("power"),
            power_scheme: cmd_out("powercfg", &["/getactivescheme"]),
            docker_version: cmd_out("docker", &["version", "--format", "{{.Server.Version}}"]),
            docker_kernel: cmd_out("docker", &["info", "--format", "{{.KernelVersion}}"]),
            docker_images,
            git_commit: cmd_out("git", &["rev-parse", "HEAD"]),
            git_dirty: Command::new("git")
                .args(["status", "--porcelain"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| !o.stdout.is_empty()),
        }
    }
}

/// UTC ISO-8601 timestamp without external crates.
pub fn iso8601(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / 3600,
        (sod / 60) % 60,
        sod % 60
    )
}

fn us(ns: u64) -> f64 {
    ns as f64 / 1_000.0
}

fn pct(h: &Histogram<u64>, q: f64) -> Option<f64> {
    (!h.is_empty()).then(|| us(h.value_at_quantile(q)))
}

fn opt(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.3}")).unwrap_or_default()
}

/// HdrHistogram percentile-distribution text, values in microseconds.
pub fn hgrm(h: &Histogram<u64>) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{:>12} {:>14} {:>10} {:>14}\n",
        "Value", "Percentile", "TotalCount", "1/(1-Percentile)"
    );
    let mut total = 0u64;
    for v in h.iter_quantiles(5) {
        total += v.count_since_last_iteration();
        let q = v.quantile_iterated_to();
        if q < 1.0 {
            let _ = writeln!(
                s,
                "{:>12.3} {:>14.12} {:>10} {:>14.2}",
                us(v.value_iterated_to()),
                q,
                total,
                1.0 / (1.0 - q)
            );
        } else {
            let _ = writeln!(
                s,
                "{:>12.3} {:>14.12} {:>10}",
                us(v.value_iterated_to()),
                q,
                total
            );
        }
    }
    let _ = writeln!(
        s,
        "#[Mean    = {:>12.3}, StdDeviation   = {:>12.3}]",
        h.mean() / 1_000.0,
        h.stdev() / 1_000.0
    );
    let _ = writeln!(
        s,
        "#[Max     = {:>12.3}, Total count    = {:>12}]",
        us(h.max()),
        h.len()
    );
    let _ = writeln!(
        s,
        "#[Buckets = {:>12}, SubBuckets     = {:>12}]",
        h.buckets(),
        h.distinct_values()
    );
    s
}

pub struct OpRow {
    pub op: &'static str,
    pub count: u64,
    pub errors: u64,
    pub timeouts: u64,
    pub throughput: f64,
    pub hist: Histogram<u64>,
}

pub fn rows(res: &EngineResult, measure_secs: u64) -> Vec<OpRow> {
    let mut all = new_histogram();
    let mut out = Vec::new();
    let (mut e_all, mut t_all) = (0, 0);
    for k in OpKind::ALL {
        let i = k as usize;
        let h = res.agg.ok[i].clone();
        all.add(&h).expect("same bounds");
        e_all += res.agg.errors[i];
        t_all += res.agg.timeouts[i];
        out.push(OpRow {
            op: k.name(),
            count: h.len(),
            errors: res.agg.errors[i],
            timeouts: res.agg.timeouts[i],
            throughput: h.len() as f64 / measure_secs as f64,
            hist: h,
        });
    }
    out.push(OpRow {
        op: "ALL",
        count: all.len(),
        errors: e_all,
        timeouts: t_all,
        throughput: all.len() as f64 / measure_secs as f64,
        hist: all,
    });
    out
}

#[allow(clippy::too_many_arguments)]
pub fn write_run(
    dir: &Path,
    id: &RunId,
    cfg: &Config,
    plan: &PinPlan,
    res: &EngineResult,
    sys: &SysInfo,
    start: SystemTime,
    force: bool,
) -> Result<Vec<OpRow>> {
    if dir.join("summary.csv").exists() && !force {
        bail!(
            "{} already has results; pass --force to overwrite",
            dir.display()
        );
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let rows = rows(res, cfg.run.measure_secs);
    let open = cfg.run.mode == crate::config::Mode::Open;
    let lag_p99 = if open {
        pct(&res.agg.send_lag, 0.99)
    } else {
        None
    };

    let mut w = csv::Writer::from_path(dir.join("summary.csv"))?;
    w.write_record([
        "db",
        "mode",
        "level",
        "run",
        "op",
        "count",
        "errors",
        "timeouts",
        "throughput_ops_s",
        "p50_us",
        "p90_us",
        "p95_us",
        "p99_us",
        "p999_us",
        "max_us",
        "send_lag_p99_us",
        "dropped_records",
    ])?;
    for r in &rows {
        let h = &r.hist;
        w.write_record([
            id.db.clone(),
            id.mode.clone(),
            id.level.clone(),
            id.run.to_string(),
            r.op.to_string(),
            r.count.to_string(),
            r.errors.to_string(),
            r.timeouts.to_string(),
            format!("{:.1}", r.throughput),
            opt(pct(h, 0.50)),
            opt(pct(h, 0.90)),
            opt(pct(h, 0.95)),
            opt(pct(h, 0.99)),
            opt(pct(h, 0.999)),
            opt((!h.is_empty()).then(|| us(h.max()))),
            opt(lag_p99),
            res.dropped.to_string(),
        ])?;
    }
    w.flush()?;

    let mut w = csv::Writer::from_path(dir.join("timeseries.csv"))?;
    w.write_record(["second", "op", "ok", "errors"])?;
    for (s, per_op) in res.agg.timeseries.iter().enumerate() {
        for k in OpKind::ALL {
            let [ok, err] = per_op[k as usize];
            w.write_record([
                s.to_string(),
                k.name().to_string(),
                ok.to_string(),
                err.to_string(),
            ])?;
        }
    }
    w.flush()?;

    for r in &rows {
        std::fs::write(dir.join(format!("{}.hgrm", r.op)), hgrm(&r.hist))?;
    }
    if open {
        std::fs::write(dir.join("SEND_LAG.hgrm"), hgrm(&res.agg.send_lag))?;
    }

    let lag = &res.agg.send_lag;
    let first_errors: Vec<_> = res
        .workers
        .iter()
        .filter_map(|w| {
            w.first_error
                .as_ref()
                .map(|e| json!({"worker": w.id, "error": e}))
        })
        .collect();
    let meta = json!({
        "scenario": id.scenario,
        "db": id.db,
        "mode": id.mode,
        "level": id.level,
        "run": id.run,
        "start_time_utc": iso8601(start),
        "config": cfg,
        "pinning": plan,
        "workers": res.specs,
        "system": sys,
        "measure_elapsed_secs": res.measure_elapsed.as_secs_f64(),
        "dropped_records": res.dropped,
        "records_drained": res.agg.records_drained,
        "warmup_records_discarded": res.agg.warmup_discarded,
        "send_lag_us": if open { json!({
            "p50": pct(lag, 0.50), "p99": pct(lag, 0.99),
            "p999": pct(lag, 0.999), "max": (!lag.is_empty()).then(|| us(lag.max())),
        }) } else { serde_json::Value::Null },
        "first_errors": first_errors,
        "histogram": { "low_ns": crate::telemetry::HIST_LOW, "high_ns": crate::telemetry::HIST_HIGH, "sigfig": crate::telemetry::HIST_SIGFIG },
    });
    std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
    Ok(rows)
}

pub fn print_table(id: &RunId, rows: &[OpRow], res: &EngineResult, open: bool) {
    println!(
        "\n{} {} level={} run={} scenario={}",
        id.db, id.mode, id.level, id.run, id.scenario
    );
    println!(
        "{:<18} {:>11} {:>7} {:>7} {:>13} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "op", "count", "errors", "tmo", "ops/s", "p50_us", "p90_us", "p99_us", "p999_us", "max_us"
    );
    for r in rows {
        let h = &r.hist;
        println!(
            "{:<18} {:>11} {:>7} {:>7} {:>13.1} {:>10} {:>10} {:>10} {:>10} {:>10}",
            r.op,
            r.count,
            r.errors,
            r.timeouts,
            r.throughput,
            opt(pct(h, 0.50)),
            opt(pct(h, 0.90)),
            opt(pct(h, 0.99)),
            opt(pct(h, 0.999)),
            opt((!h.is_empty()).then(|| us(h.max()))),
        );
    }
    if open {
        let l = &res.agg.send_lag;
        println!(
            "send_lag_us: p50={} p99={} p999={} max={}",
            opt(pct(l, 0.50)),
            opt(pct(l, 0.99)),
            opt(pct(l, 0.999)),
            opt((!l.is_empty()).then(|| us(l.max())))
        );
    }
    println!("dropped_records: {}", res.dropped);
    if res.dropped > 0 {
        println!("!! RUN INVALID: telemetry records were dropped");
    }
    for w in &res.workers {
        if let Some(e) = &w.first_error {
            println!("worker {} first error: {e}", w.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_formats_known_instants() {
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_767_225_600);
        assert_eq!(iso8601(t), "2026-01-01T00:00:00Z");
        let t = UNIX_EPOCH + std::time::Duration::from_secs(951_782_400 + 3_723);
        assert_eq!(iso8601(t), "2000-02-29T01:02:03Z");
    }

    #[test]
    fn hgrm_has_header_and_footer() {
        let mut h = new_histogram();
        for v in 1..=1000u64 {
            h.record(v * 1000).unwrap();
        }
        let s = hgrm(&h);
        assert!(s.starts_with("       Value"));
        assert!(s.contains("#[Max     ="));
        assert!(s.contains("Total count    =         1000"));
    }
}
