"""Figures and a markdown summary from a nosqlbench results root.

Usage:
    python plots/plot.py results          # validation runs
    python plots/plot.py results/final    # the single-session final run

Reads <root>/<scenario>/<db>/<mode>_<level>_run<n>/{summary.csv, meta.json,
ALL.hgrm} and writes <root>/figures/*.png (300 dpi) + *.svg and
<root>/results_summary.md. Runs are summarised as the median across repeats,
with the min-max range shown as whiskers / in brackets.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
import numpy as np  # noqa: E402
import polars as pl  # noqa: E402

# --- Visual system ---------------------------------------------------------
# Categorical slots 1-4 of the reference palette, fixed per database in every
# figure (validated: CVD dE >= 9.1, normal-vision dE >= 22.9 on adjacent
# pairs). Aqua and yellow sit below 3:1 on the surface, so lines also carry
# distinct markers and direct labels, and every value is in the summary table.
DBS = [
    ("redis", "Redis", "#2a78d6", "o"),
    ("cassandra", "Cassandra", "#eb6834", "s"),
    ("mongo", "MongoDB", "#1baf7a", "^"),
    ("neo4j", "Neo4j", "#eda100", "D"),
]
DB_NAME = {k: n for k, n, _, _ in DBS}
DB_COLOR = {k: c for k, _, c, _ in DBS}
DB_MARKER = {k: m for k, _, _, m in DBS}
SURFACE = "#fcfcfb"
INK = "#0b0b0b"
INK2 = "#52514e"
MUTED = "#898781"
GRID = "#e1e0d9"
AXIS = "#c3c2b7"
REF = "#52514e"  # reference lines (redis-ping) use secondary ink, not a series hue

A4_TEXT_WIDTH_IN = 6.3  # 160 mm text block
OPS = ["TIMELINE_READ", "USER_PROFILE_READ", "POST_LIKE", "POST_CREATE", "HASHTAG_SEARCH", "USER_FOLLOW"]
OP_LABEL = {
    "TIMELINE_READ": "Timeline read",
    "USER_PROFILE_READ": "Profile read",
    "POST_LIKE": "Post like",
    "POST_CREATE": "Post create",
    "HASHTAG_SEARCH": "Hashtag search",
    "USER_FOLLOW": "User follow",
}
LEVELS = [1, 8, 32, 128]
OPEN_LEVELS = ["50pct", "80pct", "95pct"]

# --- Saturation episodes ---------------------------------------------------
# An open-mode level is a *saturation episode*, not a steady-state latency: the
# database cannot absorb the offered rate, requests queue in the client, and
# nearly all of the reported latency is the client's own lateness (send lag),
# which open mode charges to latency by design. Flagged when the median
# latency p99 is at least SAT_LAT_US and send-lag p99 is at least SAT_LAG_SHARE
# of it. Such cells are labelled everywhere they appear and must not be read as
# the latency the database serves at that rate.
SAT_LAT_US = 1_000_000.0
SAT_LAG_SHARE = 0.5
SAT_MARK = "▲"  # filled triangle, used in figures and tables

plt.rcParams.update(
    {
        "font.family": ["Segoe UI", "DejaVu Sans"],
        "font.size": 8,
        "axes.titlesize": 9,
        "axes.labelsize": 8,
        "axes.edgecolor": AXIS,
        "axes.linewidth": 0.8,
        "axes.labelcolor": INK2,
        "axes.titlecolor": INK,
        "axes.facecolor": SURFACE,
        "axes.grid": True,
        "axes.axisbelow": True,
        "axes.spines.top": False,
        "axes.spines.right": False,
        "grid.color": GRID,
        "grid.linewidth": 0.6,
        "grid.linestyle": "-",
        "xtick.color": MUTED,
        "ytick.color": MUTED,
        "xtick.labelcolor": INK2,
        "ytick.labelcolor": INK2,
        "legend.frameon": False,
        "legend.fontsize": 7.5,
        "figure.facecolor": SURFACE,
        "savefig.facecolor": SURFACE,
        "svg.fonttype": "none",
        "lines.linewidth": 1.6,
        "lines.solid_capstyle": "round",
        "lines.solid_joinstyle": "round",
    }
)

RUN_DIR = re.compile(r"^(closed|open)_(.+)_run(\d+)$")


# --- Loading ---------------------------------------------------------------
def load(root: Path) -> pl.DataFrame:
    rows = []
    for scen_dir in sorted(p for p in root.iterdir() if p.is_dir() and p.name in ("normal", "celebrity")):
        for db_dir in sorted(p for p in scen_dir.iterdir() if p.is_dir()):
            for run_dir in sorted(p for p in db_dir.iterdir() if p.is_dir()):
                m = RUN_DIR.match(run_dir.name)
                if not m or not (run_dir / "summary.csv").exists():
                    continue
                meta = json.loads((run_dir / "meta.json").read_text(encoding="utf-8"))
                lag = meta.get("send_lag_us") or {}
                first_err = "; ".join(
                    e.get("error", "") for e in (meta.get("first_errors") or []) if e.get("error")
                )
                summ = pl.read_csv(run_dir / "summary.csv", infer_schema_length=0)
                for r in summ.iter_rows(named=True):
                    rows.append(
                        {
                            "scenario": scen_dir.name,
                            "db": db_dir.name,
                            "mode": m.group(1),
                            "level": m.group(2),
                            "run": int(m.group(3)),
                            "op": r["op"],
                            "count": int(r["count"]),
                            "errors": int(r["errors"]),
                            "timeouts": int(r["timeouts"]),
                            "tput": float(r["throughput_ops_s"]),
                            "p50": _f(r["p50_us"]),
                            "p99": _f(r["p99_us"]),
                            "p999": _f(r["p999_us"]),
                            "max": _f(r["max_us"]),
                            "dropped": int(r["dropped_records"]),
                            "lag_p99": _f(lag.get("p99")),
                            "lag_p999": _f(lag.get("p999")),
                            "target_rate": float(meta["config"]["run"]["rate"]),
                            "start": meta.get("start_time_utc", ""),
                            "git": (meta.get("system", {}).get("git_commit") or "")[:7],
                            "first_error": first_err,
                            "dir": str(run_dir),
                        }
                    )
    if not rows:
        sys.exit(f"no runs found under {root}\\{{normal,celebrity}}")
    return pl.DataFrame(rows)


def _f(v) -> float:
    try:
        return float(v)
    except (TypeError, ValueError):
        return float("nan")


def agg(df: pl.DataFrame, value: str) -> pl.DataFrame:
    """Median and min-max of `value` per (scenario, db, mode, level, op)."""
    return (
        df.group_by(["scenario", "db", "mode", "level", "op"])
        .agg(
            pl.col(value).median().alias("med"),
            pl.col(value).min().alias("lo"),
            pl.col(value).max().alias("hi"),
            pl.len().alias("n"),
        )
    )


def pick(a: pl.DataFrame, **kw) -> dict | None:
    q = a
    for k, v in kw.items():
        q = q.filter(pl.col(k) == v)
    return q.row(0, named=True) if q.height else None


def saturated(df: pl.DataFrame) -> set[tuple[str, str, str]]:
    """{(scenario, db, level)} open-mode cells that are saturation episodes."""
    base = df.filter((pl.col("op") == "ALL") & (pl.col("mode") == "open"))
    if base.is_empty():
        return set()
    lat, lag = agg(base, "p99"), agg(base, "lag_p99")
    out = set()
    for r in lat.iter_rows(named=True):
        g = pick(lag, scenario=r["scenario"], db=r["db"], level=r["level"], op="ALL")
        if not g or g["med"] != g["med"]:
            continue
        if r["med"] >= SAT_LAT_US and g["med"] >= SAT_LAG_SHARE * r["med"]:
            out.add((r["scenario"], r["db"], r["level"]))
    return out


def ranking(a: pl.DataFrame, scenario: str, level: str) -> dict[str, int]:
    """{db: 1-based rank by median throughput} among the databases present."""
    vals = []
    for db, _, _, _ in DBS:
        r = pick(a, scenario=scenario, db=db, level=level)
        if r:
            vals.append((r["med"], db))
    vals.sort(reverse=True)
    return {db: i + 1 for i, (_, db) in enumerate(vals)}


def order_text(rank: dict[str, int]) -> str:
    return " > ".join(DB_NAME[db] for db, _ in sorted(rank.items(), key=lambda kv: kv[1]))


def read_hgrm(path: Path) -> tuple[np.ndarray, np.ndarray]:
    """(latency_us, percentile 0..1) rows of an HdrHistogram text export."""
    xs, ps = [], []
    for line in path.read_text(encoding="utf-8").splitlines():
        parts = line.split()
        if len(parts) >= 3 and not line.startswith("#"):
            try:
                v, p = float(parts[0]), float(parts[1])
            except ValueError:
                continue
            xs.append(v)
            ps.append(p)
    return np.array(xs), np.array(ps)


# --- Figure helpers --------------------------------------------------------
def save(fig, out: Path, name: str, made: list[str]) -> None:
    for ext in ("png", "svg"):
        fig.savefig(out / f"{name}.{ext}", dpi=300, bbox_inches="tight")
    plt.close(fig)
    made.append(name)


def ms(us: float) -> float:
    return us / 1000.0


def conc_axis(ax) -> None:
    ax.set_xscale("log", base=2)
    ax.set_xticks(LEVELS, [str(c) for c in LEVELS])
    ax.minorticks_off()
    ax.set_xlim(0.75, 175)
    ax.set_xlabel("Concurrency (requests in flight, closed mode)")


def end_labels(ax, points: list[tuple[str, float, float]]) -> None:
    """Direct labels at line ends; nudged apart in log space if they collide."""
    points = sorted(points, key=lambda t: t[2])
    placed: list[float] = []
    for text, x, y in points:
        ly = np.log10(y)
        if placed and ly - placed[-1] < 0.11:
            ly = placed[-1] + 0.11
        placed.append(ly)
        ax.annotate(
            text, (x, y), xytext=(x * 1.12, 10**ly), textcoords="data",
            va="center", ha="left", fontsize=7, color=INK2,
            arrowprops=dict(arrowstyle="-", color=AXIS, lw=0.6, shrinkA=0, shrinkB=2)
            if abs(ly - np.log10(y)) > 0.01 else None,
        )


# --- Figures ---------------------------------------------------------------
def fig_throughput(df, out, made):
    a = agg(df.filter((pl.col("op") == "ALL") & (pl.col("mode") == "closed") & (pl.col("scenario") == "normal")), "tput")
    fig, ax = plt.subplots(figsize=(A4_TEXT_WIDTH_IN, 3.4))
    labels = []
    for db, name, color, marker in DBS:
        pts = [(c, pick(a, db=db, level=str(c), op="ALL")) for c in LEVELS]
        pts = [(c, r) for c, r in pts if r]
        if not pts:
            continue
        x = [c for c, _ in pts]
        y = [r["med"] for _, r in pts]
        err = [[r["med"] - r["lo"] for _, r in pts], [r["hi"] - r["med"] for _, r in pts]]
        ax.errorbar(x, y, yerr=err, color=color, marker=marker, ms=5, mec=SURFACE, mew=1.2,
                    capsize=2, elinewidth=0.8, label=name, zorder=3)
        labels.append((f"{name} {y[-1]:,.0f}", x[-1], y[-1]))
    ping = [(c, pick(a, db="redis-ping", level=str(c), op="ALL")) for c in LEVELS]
    ping = [(c, r) for c, r in ping if r]
    if ping:
        ax.plot([c for c, _ in ping], [r["med"] for _, r in ping], color=REF, ls=(0, (4, 2)), lw=1.2,
                marker="x", ms=4, label="redis-ping (transport reference)", zorder=2)
        labels.append((f"redis-ping {ping[-1][1]['med']:,.0f}", ping[-1][0], ping[-1][1]["med"]))
    ax.set_yscale("log")
    conc_axis(ax)
    ax.set_ylabel("Throughput (ops/s, log scale)")
    ax.yaxis.set_major_formatter(matplotlib.ticker.FuncFormatter(lambda v, _: f"{v:,.0f}"))
    ax.set_title("Closed-mode throughput vs concurrency (normal scenario, median of runs, whiskers = min–max)", loc="left")
    end_labels(ax, labels)
    ax.set_xlim(0.75, 420)
    keys = [matplotlib.lines.Line2D([], [], color=c, marker=m, ms=5, mec=SURFACE, label=n) for _, n, c, m in DBS]
    keys.append(matplotlib.lines.Line2D([], [], color=REF, ls=(0, (4, 2)), marker="x", ms=4,
                                        label="redis-ping (transport reference)"))
    ax.legend(handles=keys, loc="upper left", ncols=2)
    save(fig, out, "01_throughput_vs_concurrency", made)


def fig_latency(df, out, made):
    base = df.filter((pl.col("op") == "ALL") & (pl.col("mode") == "closed") & (pl.col("scenario") == "normal"))
    fig, axes = plt.subplots(1, 3, figsize=(A4_TEXT_WIDTH_IN, 2.7), sharey=True)
    ping50 = agg(base, "p50")
    for ax, (col, title) in zip(axes, [("p50", "p50"), ("p99", "p99"), ("p999", "p99.9")]):
        a = agg(base, col)
        for db, name, color, marker in DBS:
            pts = [(c, pick(a, db=db, level=str(c), op="ALL")) for c in LEVELS]
            pts = [(c, r) for c, r in pts if r]
            if pts:
                ax.plot([c for c, _ in pts], [ms(r["med"]) for _, r in pts], color=color, marker=marker,
                        ms=4.5, mec=SURFACE, mew=1.0, label=name, zorder=3)
        ping = [(c, pick(ping50, db="redis-ping", level=str(c), op="ALL")) for c in LEVELS]
        ping = [(c, r) for c, r in ping if r]
        if ping:
            ax.plot([c for c, _ in ping], [ms(r["med"]) for _, r in ping], color=REF, ls=(0, (4, 2)),
                    lw=1.1, label="redis-ping p50 (floor)", zorder=2)
        ax.set_yscale("log")
        conc_axis(ax)
        ax.set_xlabel("Concurrency")
        ax.set_title(title, loc="left")
    axes[0].set_ylabel("Latency (ms, log scale)")
    axes[0].yaxis.set_major_formatter(matplotlib.ticker.FuncFormatter(lambda v, _: f"{v:g}"))
    h, lab = axes[0].get_legend_handles_labels()
    fig.legend(h, lab, loc="lower center", ncols=5, bbox_to_anchor=(0.5, -0.08))
    fig.suptitle("Closed-mode latency vs concurrency (normal scenario, all operations, median of runs)",
                 x=0.01, ha="left", fontsize=9, color=INK)
    fig.tight_layout()
    save(fig, out, "02_latency_vs_concurrency", made)


def fig_per_op(df, out, made):
    a = agg(df.filter((pl.col("mode") == "closed") & (pl.col("scenario") == "normal") & (pl.col("level") == "32")), "p99")
    fig, ax = plt.subplots(figsize=(A4_TEXT_WIDTH_IN, 3.0))
    n = len(DBS)
    width = 0.8 / n
    x = np.arange(len(OPS))
    for i, (db, name, color, _) in enumerate(DBS):
        vals, lo, hi = [], [], []
        for op in OPS:
            r = pick(a, db=db, op=op)
            vals.append(ms(r["med"]) if r else np.nan)
            lo.append(ms(r["med"] - r["lo"]) if r else 0)
            hi.append(ms(r["hi"] - r["med"]) if r else 0)
        ax.bar(x + (i - (n - 1) / 2) * width, vals, width, color=color, edgecolor=SURFACE, linewidth=1.2,
               yerr=[lo, hi], error_kw=dict(elinewidth=0.7, capsize=1.5, ecolor=INK2), label=name, zorder=3)
    ax.set_xticks(x, [OP_LABEL[o] for o in OPS])
    ax.grid(axis="x", visible=False)
    ax.set_ylabel("p99 latency (ms)")
    ax.set_title("Per-operation p99 at concurrency 32 (closed, normal scenario, median of runs, whiskers = min–max)", loc="left")
    ax.legend(loc="upper left", ncols=4)
    save(fig, out, "03_per_operation_p99_c32", made)


def fig_open(df, out, made):
    base = df.filter((pl.col("op") == "ALL") & (pl.col("mode") == "open") & (pl.col("scenario") == "normal"))
    if base.is_empty():
        return
    lat = agg(base, "p99")
    lag = agg(base, "lag_p99")
    sat = saturated(df)
    fig, ax = plt.subplots(figsize=(A4_TEXT_WIDTH_IN, 3.6))
    n = len(DBS)
    group = 0.84
    slot = group / n
    bw = slot * 0.46
    x = np.arange(len(OPEN_LEVELS))
    for i, (db, name, color, _) in enumerate(DBS):
        centre = x + (i - (n - 1) / 2) * slot
        lv = [pick(lat, db=db, level=l) for l in OPEN_LEVELS]
        gv = [pick(lag, db=db, level=l) for l in OPEN_LEVELS]
        ax.bar(centre - bw / 2, [ms(r["med"]) if r else np.nan for r in lv], bw, color=color,
               edgecolor=SURFACE, linewidth=1.0, zorder=3, label=f"{name} latency p99")
        ax.bar(centre + bw / 2, [ms(r["med"]) if r else np.nan for r in gv], bw, color=SURFACE,
               edgecolor=color, hatch="////", linewidth=0.9, zorder=3)
        for j, lvl in enumerate(OPEN_LEVELS):
            if ("normal", db, lvl) not in sat or not lv[j]:
                continue
            top = max(ms(lv[j]["med"]), ms(gv[j]["med"]) if gv[j] else 0.0)
            ax.annotate(SAT_MARK, (centre[j], top * 1.22), ha="center", va="bottom",
                        fontsize=7, color=INK2, zorder=4)
    ax.set_yscale("log")
    ax.set_xticks(x, ["50% of best closed", "80%", "95%"])
    ax.grid(axis="x", visible=False)
    ax.set_ylabel("ms (log scale)")
    ax.yaxis.set_major_formatter(matplotlib.ticker.FuncFormatter(lambda v, _: f"{v:g}"))
    ax.set_ylim(top=ax.get_ylim()[1] * 2.2)
    ax.set_title("Open-mode latency p99 (solid) with send-lag p99 beside it (hatched), normal scenario, median of runs", loc="left")
    handles, labels = ax.get_legend_handles_labels()
    handles.append(matplotlib.patches.Patch(facecolor=SURFACE, edgecolor=INK2, hatch="////", label="send-lag p99 (client lateness, same colour)"))
    handles.append(matplotlib.lines.Line2D([], [], linestyle="none", marker="^", color=INK2, markersize=4,
                                           label="saturation episode, not steady-state latency"))
    ax.legend(handles=handles, loc="upper center", bbox_to_anchor=(0.5, -0.14), ncols=3, fontsize=7)
    ax.text(0.0, -0.30,
            f"{SAT_MARK} the database could not absorb the offered rate: latency p99 above 1 s with send lag at least "
            "half of it, so almost all of the\nbar is requests queued in the client waiting to be sent. Read those bars "
            "as the onset of saturation, not as service latency.",
            fontsize=6.5, color=INK2, ha="left", va="top", transform=ax.transAxes)
    save(fig, out, "04_open_mode_latency_send_lag", made)


def fig_celebrity(df, out, made):
    base = df.filter((pl.col("op") == "ALL") & (pl.col("mode") == "closed") & pl.col("level").is_in(["8", "32"]))
    a = agg(base, "tput")
    if a.filter(pl.col("scenario") == "celebrity").is_empty():
        return
    fig, axes = plt.subplots(1, 2, figsize=(A4_TEXT_WIDTH_IN, 3.0), sharey=True)
    orders = []
    for ax, level in zip(axes, ["8", "32"]):
        x = np.arange(len(DBS))
        bw = 0.36
        rank = {s: ranking(a, s, level) for s in ("normal", "celebrity")}
        orders.append((level, rank))
        for i, (db, name, color, _) in enumerate(DBS):
            nr = pick(a, scenario="normal", db=db, level=level)
            cr = pick(a, scenario="celebrity", db=db, level=level)
            if nr:
                ax.bar(i - bw / 2 - 0.01, nr["med"], bw, color=color, edgecolor=SURFACE, linewidth=1.0, zorder=3)
                ax.annotate(f"#{rank['normal'][db]}", (i - bw / 2 - 0.01, nr["med"]), xytext=(0, 3),
                            textcoords="offset points", ha="center", va="bottom", fontsize=7, color=INK2)
            if cr:
                ax.bar(i + bw / 2 + 0.01, cr["med"], bw, color=SURFACE, edgecolor=color, hatch="////",
                       linewidth=0.9, zorder=3)
            if nr and cr:
                drop = (cr["med"] / nr["med"] - 1) * 100
                ax.annotate(f"#{rank['celebrity'][db]}\n{drop:+.0f}%", (i + bw / 2 + 0.01, cr["med"]), xytext=(0, 3),
                            textcoords="offset points", ha="center", va="bottom", fontsize=7, color=INK,
                            linespacing=1.15)
        ax.set_xticks(np.arange(len(DBS)), [n for _, n, _, _ in DBS])
        ax.grid(axis="x", visible=False)
        ax.set_title(f"Concurrency {level}", loc="left")
        ax.yaxis.set_major_formatter(matplotlib.ticker.FuncFormatter(lambda v, _: f"{v:,.0f}"))
    axes[0].set_ylabel("Throughput (ops/s)")
    handles = [
        matplotlib.patches.Patch(facecolor=INK2, edgecolor=SURFACE, label="normal (activity independent of popularity)"),
        matplotlib.patches.Patch(facecolor=SURFACE, edgecolor=INK2, hatch="////", label="celebrity (most-followed = most active)"),
    ]
    fig.legend(handles=handles, loc="lower center", ncols=2, bbox_to_anchor=(0.5, -0.06))
    fig.suptitle("Celebrity vs normal scenario, closed mode (labels: rank within the scenario, change vs normal)",
                 x=0.01, ha="left", fontsize=9, color=INK)
    caption = ["The ranking reverses: Redis and Cassandra (fan-out on write) take the top two places under normal "
               "activity, but when the most-followed users are\nalso the most active, MongoDB leads and Cassandra "
               "falls to last."]
    for level, rank in orders:
        caption.append(f"c={level}:  normal {order_text(rank['normal'])}   |   celebrity {order_text(rank['celebrity'])}")
    fig.text(0.0, -0.10, "\n".join(caption), fontsize=6.5, color=INK2, ha="left", va="top")
    fig.tight_layout()
    save(fig, out, "05_celebrity_vs_normal", made)


def fig_cdf(df, root, out, made):
    runs = df.filter((pl.col("op") == "ALL") & (pl.col("mode") == "open") & (pl.col("level") == "50pct") & (pl.col("scenario") == "normal"))
    if runs.is_empty():
        return
    fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(A4_TEXT_WIDTH_IN, 3.0))
    ping = agg(df.filter((pl.col("op") == "ALL") & (pl.col("db") == "redis-ping") & (pl.col("level") == "1")), "p50")
    rtt = pick(ping, db="redis-ping", level="1", op="ALL")
    for db, name, color, _ in DBS:
        sub = runs.filter(pl.col("db") == db).sort("p99")
        if sub.is_empty():
            continue
        median_run = sub.row(sub.height // 2, named=True)["run"]
        for r in sub.iter_rows(named=True):
            xs, ps = read_hgrm(Path(r["dir"]) / "ALL.hgrm")
            if xs.size == 0:
                continue
            main = r["run"] == median_run
            kw = dict(color=color, lw=1.6 if main else 0.7, alpha=1.0 if main else 0.45,
                      label=name if main else None, zorder=3 if main else 2)
            ax1.plot(xs / 1000, ps, **kw)
            keep = ps < 1.0
            ax2.plot(1 / (1 - ps[keep]), xs[keep] / 1000, **kw)
    if rtt:
        for ax, vertical in ((ax1, True), (ax2, False)):
            (ax.axvline if vertical else ax.axhline)(ms(rtt["med"]), color=REF, ls=(0, (4, 2)), lw=1.0,
                                                     label="redis-ping RTT (c=1 p50)", zorder=1)
    ax1.set_xscale("log")
    ax1.set_xlabel("Latency (ms, log scale)")
    ax1.set_ylabel("Cumulative fraction of requests")
    ax1.set_title("CDF", loc="left")
    ax1.xaxis.set_major_formatter(matplotlib.ticker.FuncFormatter(lambda v, _: f"{v:g}"))
    ax2.set_xscale("log")
    ax2.set_yscale("log")
    ticks = [2, 10, 100, 1000, 10000, 100000]
    ax2.set_xticks(ticks, ["50%", "90%", "99%", "99.9%", "99.99%", "99.999%"])
    ax2.minorticks_off()
    ax2.set_xlabel("Percentile")
    ax2.set_ylabel("Latency (ms, log scale)")
    ax2.set_title("Tail (percentile spectrum)", loc="left")
    ax2.yaxis.set_major_formatter(matplotlib.ticker.FuncFormatter(lambda v, _: f"{v:g}"))
    h, lab = ax1.get_legend_handles_labels()
    fig.legend(h, lab, loc="lower center", ncols=5, bbox_to_anchor=(0.5, -0.07))
    fig.suptitle("Open mode at 50% of best closed throughput: latency distribution (median-p99 run bold, other runs thin)",
                 x=0.01, ha="left", fontsize=9, color=INK)
    fig.tight_layout()
    save(fig, out, "06_open50_latency_cdf", made)


# --- Markdown summary ------------------------------------------------------
def fmt_ms(us: float) -> str:
    if us != us:  # NaN
        return "–"
    v = us / 1000
    return f"{v:.2f}" if v < 10 else f"{v:.1f}" if v < 100 else f"{v:,.0f}"


def fmt_ops(v: float) -> str:
    return f"{v:,.0f}"


def rng(r: dict, f) -> str:
    return f if r["n"] <= 1 else f"{f} [{r['_lo']}–{r['_hi']}]"


def error_seconds(run_dir: Path, op: str) -> list[int]:
    """Measure-phase seconds in which this op recorded errors."""
    ts = run_dir / "timeseries.csv"
    if not ts.exists():
        return []
    t = pl.read_csv(ts, infer_schema_length=0)
    t = t.filter((pl.col("op") == op) & (pl.col("errors").cast(pl.Int64) > 0))
    return sorted(int(v) for v in t["second"].to_list())


def summary(df: pl.DataFrame, root: Path) -> str:
    L = []
    allrows = df.filter(pl.col("op") == "ALL")
    starts = sorted(s for s in allrows["start"].to_list() if s)
    L.append(f"# nosqlbench results summary\n")
    L.append(f"Source: `{root}`. Generated by `plots/plot.py`.\n")
    L.append(
        f"- Runs: {allrows.height} run directories; errors {int(df.filter(pl.col('op') != 'ALL')['errors'].sum())}, "
        f"timeouts {int(df.filter(pl.col('op') != 'ALL')['timeouts'].sum())}, "
        f"runs with dropped telemetry {allrows.filter(pl.col('dropped') > 0).height}."
    )
    if starts:
        L.append(f"- Time span (UTC): {starts[0]} → {starts[-1]}.")
    L.append(f"- Git commits: {', '.join(sorted(set(allrows['git'].to_list()) - {''}))}.")
    L.append("- Values are the median across repeated runs; brackets give the min–max range. Latency in ms.\n")

    bad = df.filter((pl.col("op") != "ALL") & ((pl.col("errors") > 0) | (pl.col("timeouts") > 0)))
    L.append("## Errors and timeouts\n")
    if bad.is_empty():
        L.append("None: every run completed with 0 errors and 0 timeouts.\n")
    else:
        L.append("Called out per run instead of folded into a median: they are rare, localised, and each one is a "
                 "request the database did not serve.\n")
        L.append("| Run | Operation | errors | timeouts | of count | measure seconds | driver message |\n"
                 "|---|---|---|---|---|---|---|")
        for r in bad.sort(["scenario", "db", "level", "run", "op"]).iter_rows(named=True):
            msg = (r["first_error"] or "").replace("|", "/")
            secs = error_seconds(Path(r["dir"]), r["op"])
            when = ", ".join(str(s) for s in secs) if secs else "–"
            L.append(f"| {r['scenario']} / {r['db']} / {r['mode']} {r['level'].replace('pct', '%')} run {r['run']} "
                     f"| {OP_LABEL.get(r['op'], r['op'])} | {r['errors']} | {r['timeouts']} | {r['count']:,} "
                     f"| {when} | {msg} |")
        L.append("")

    def closed(db, level, col):
        return agg(allrows.filter((pl.col("mode") == "closed") & (pl.col("scenario") == "normal")), col).pipe(
            lambda a: pick(a, db=db, level=str(level), op="ALL"))

    ping_rows = [(c, closed("redis-ping", c, "tput"), closed("redis-ping", c, "p50"), closed("redis-ping", c, "p99")) for c in LEVELS]
    if any(r[1] for r in ping_rows):
        L.append("## Transport floor: redis-ping (closed)\n")
        L.append("| Concurrency | ops/s [range] | p50 | p99 |\n|---|---|---|---|")
        for c, t, p50, p99 in ping_rows:
            if t:
                L.append(f"| {c} | {fmt_ops(t['med'])} [{fmt_ops(t['lo'])}–{fmt_ops(t['hi'])}] | {fmt_ms(p50['med'])} | {fmt_ms(p99['med'])} |")
        L.append("")

    L.append("## Closed mode, normal scenario (all operations)\n")
    L.append("| Database | Concurrency | ops/s [range] | p50 | p99 | p99.9 | runs |\n|---|---|---|---|---|---|---|")
    for db, name, _, _ in DBS:
        for c in LEVELS:
            t = closed(db, c, "tput")
            if not t:
                continue
            p50, p99, p999 = closed(db, c, "p50"), closed(db, c, "p99"), closed(db, c, "p999")
            L.append(f"| {name} | {c} | {fmt_ops(t['med'])} [{fmt_ops(t['lo'])}–{fmt_ops(t['hi'])}] | "
                     f"{fmt_ms(p50['med'])} | {fmt_ms(p99['med'])} | {fmt_ms(p999['med'])} | {t['n']} |")
    L.append("")

    a = agg(df.filter((pl.col("mode") == "closed") & (pl.col("scenario") == "normal") & (pl.col("level") == "32")), "p99")
    L.append("## Per-operation p99 at concurrency 32 (closed, normal)\n")
    L.append("| Operation | " + " | ".join(n for _, n, _, _ in DBS) + " |\n|---|" + "---|" * len(DBS))
    for op in OPS:
        cells = []
        for db, _, _, _ in DBS:
            r = pick(a, db=db, op=op)
            cells.append(f"{fmt_ms(r['med'])} [{fmt_ms(r['lo'])}–{fmt_ms(r['hi'])}]" if r else "–")
        L.append(f"| {OP_LABEL[op]} | " + " | ".join(cells) + " |")
    L.append("")

    sat = saturated(df)
    op_rows = allrows.filter((pl.col("mode") == "open") & (pl.col("scenario") == "normal"))
    if op_rows.height:
        L.append("## Open mode, normal scenario (all operations)\n")
        L.append("Target rate = % of that database's best closed-mode median throughput. Send lag = how late the client "
                 "started requests (charged to latency); see the host-contention limitation in the README.\n")
        L.append(f"{SAT_MARK} marks a **saturation episode**, not a steady-state latency: latency p99 above 1 s with "
                 "send-lag p99 at least half of it, meaning the database could not absorb the offered rate and almost "
                 "all of the reported figure is requests queued in the client waiting to be sent. Read those rows as "
                 "the onset of saturation. They carry the same mark on figure "
                 "`04_open_mode_latency_send_lag`.\n")
        L.append("| Database | Load | target ops/s | achieved ops/s | p50 | p99 [range] | p99.9 | send-lag p99 [range] | send-lag p99.9 |\n"
                 "|---|---|---|---|---|---|---|---|---|")
        cols = {c: agg(op_rows, c) for c in ("tput", "p50", "p99", "p999", "lag_p99", "lag_p999", "target_rate")}
        for db, name, _, _ in DBS:
            for lvl in OPEN_LEVELS:
                g = {c: pick(v, db=db, level=lvl) for c, v in cols.items()}
                if not g["tput"]:
                    continue
                mark = f" {SAT_MARK}" if ("normal", db, lvl) in sat else ""
                L.append(f"| {name} | {lvl.replace('pct', '%')}{mark} | {fmt_ops(g['target_rate']['med'])} | {fmt_ops(g['tput']['med'])} | "
                         f"{fmt_ms(g['p50']['med'])} | {fmt_ms(g['p99']['med'])} [{fmt_ms(g['p99']['lo'])}–{fmt_ms(g['p99']['hi'])}] | "
                         f"{fmt_ms(g['p999']['med'])} | {fmt_ms(g['lag_p99']['med'])} [{fmt_ms(g['lag_p99']['lo'])}–{fmt_ms(g['lag_p99']['hi'])}] | "
                         f"{fmt_ms(g['lag_p999']['med'])} |")
        L.append("")

    cel = allrows.filter((pl.col("mode") == "closed") & pl.col("level").is_in(["8", "32"]))
    ct = agg(cel, "tput")
    if ct.filter(pl.col("scenario") == "celebrity").height:
        L.append("## Celebrity vs normal scenario (closed)\n")
        L.append("| Database | Concurrency | normal ops/s | rank | celebrity ops/s | rank | change |\n"
                 "|---|---|---|---|---|---|---|")
        ranks = {(s, lvl): ranking(ct, s, lvl) for s in ("normal", "celebrity") for lvl in ("8", "32")}
        for db, name, _, _ in DBS:
            for lvl in ("8", "32"):
                nr, cr = pick(ct, scenario="normal", db=db, level=lvl), pick(ct, scenario="celebrity", db=db, level=lvl)
                if nr and cr:
                    L.append(f"| {name} | {lvl} | {fmt_ops(nr['med'])} | #{ranks[('normal', lvl)][db]} "
                             f"| {fmt_ops(cr['med'])} | #{ranks[('celebrity', lvl)][db]} "
                             f"| {(cr['med'] / nr['med'] - 1) * 100:+.0f}% |")
        L.append("")
        L.append("The ranking reverses between the two scenarios:\n")
        for lvl in ("8", "32"):
            L.append(f"- c={lvl}: normal **{order_text(ranks[('normal', lvl)])}**; "
                     f"celebrity **{order_text(ranks[('celebrity', lvl)])}**.")
        L.append("")

    noop = allrows.filter(pl.col("db") == "noop")
    if noop.height:
        t = agg(noop, "tput")
        L.append("## Engine calibration (noop adapter, closed, c=128)\n")
        for r in t.iter_rows(named=True):
            L.append(f"- {fmt_ops(r['med'])} ops/s [{fmt_ops(r['lo'])}–{fmt_ops(r['hi'])}] over {r['n']} runs.")
        L.append("")
    return "\n".join(L)


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit("usage: python plots/plot.py <results_root>   (e.g. results or results\\final)")
    root = Path(sys.argv[1]).resolve()
    df = load(root)
    out = root / "figures"
    out.mkdir(exist_ok=True)
    made: list[str] = []
    fig_throughput(df, out, made)
    fig_latency(df, out, made)
    fig_per_op(df, out, made)
    fig_open(df, out, made)
    fig_celebrity(df, out, made)
    fig_cdf(df, root, out, made)
    (root / "results_summary.md").write_text(summary(df, root), encoding="utf-8")
    print(f"{df.filter(pl.col('op') == 'ALL').height} runs loaded from {root}")
    for name in made:
        print(f"  {out / name}.png  (+ .svg)")
    print(f"  {root / 'results_summary.md'}")


if __name__ == "__main__":
    main()
