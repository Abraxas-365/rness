#!/usr/bin/env python3
"""Perf reporting: JSON-lines records + baseline comparison. Stdlib only.

    from perf import Reporter, compare_baseline
    rep = Reporter("startup-list", wp=1)          # appends to $RNESS_E2E_PERF_OUT or
                                                  # $TMPDIR/rness-e2e-perf/<probe>.jsonl
    for _ in range(5):
        rep.sample("list_ms", 123.4)              # one JSON line per sample
    rep.summary()                                 # {"metric": {"n","median","min","max","p90"}}
    verdict = compare_baseline("startup-list", rep.medians())   # see below
    rep.close()

Record line: {"probe","metric","value","unit","wp","bin","commit","ts", **extra}

Baselines live in scripts/e2e/baselines/<probe>.json:
    {"probe": "startup-list", "commit": "22490f1", "metrics": {"list_ms": {"median": 120.0, "unit": "ms"}}}
compare_baseline(probe, medians, threshold=0.25) -> {"ok": bool, "rows": [...], "missing": bool}
A metric regresses when current median > baseline median * (1 + threshold)
(lower is better for every metric; record throughput as time-per-unit).
`write_baseline(probe, medians)` creates/overwrites the baseline file.
"""
import json
import os
import statistics
import subprocess
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
BASELINES = HERE / "baselines"
DEFAULT_OUT = Path(os.environ.get("TMPDIR", "/tmp")) / "rness-e2e-perf"
THRESHOLD = 0.25


def _commit():
    try:
        return subprocess.run(["git", "-C", str(HERE), "rev-parse", "--short", "HEAD"], capture_output=True,
                              text=True, timeout=5).stdout.strip() or None
    except Exception:
        return None


def stats(values):
    v = sorted(values)
    if not v:
        return {"n": 0}
    return {"n": len(v), "median": statistics.median(v), "min": v[0], "max": v[-1],
            "p90": v[min(len(v) - 1, int(round(0.9 * (len(v) - 1))))],
            "stdev": statistics.pstdev(v) if len(v) > 1 else 0.0}


class Reporter:
    def __init__(self, probe, wp=None, out=None, binary=None, echo=False):
        self.probe, self.wp, self.echo = probe, wp, echo
        self.binary = str(binary) if binary else None
        path = out or os.environ.get("RNESS_E2E_PERF_OUT") or (DEFAULT_OUT / f"{probe}.jsonl")
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.f = open(self.path, "a")
        self.commit = _commit()
        self.values = {}
        self.units = {}

    def sample(self, metric, value, unit="ms", **extra):
        rec = {"probe": self.probe, "metric": metric, "value": value, "unit": unit, "wp": self.wp,
               "bin": self.binary, "commit": self.commit, "ts": round(time.time(), 3), **extra}
        self.f.write(json.dumps(rec) + "\n")
        self.f.flush()
        self.values.setdefault(metric, []).append(value)
        self.units[metric] = unit
        if self.echo:
            print(json.dumps(rec), flush=True)
        return rec

    def time(self, metric, fn, *args, repeat=1, **kw):
        """Run fn `repeat` times, recording wall ms each time; returns last result."""
        result = None
        for _ in range(repeat):
            t0 = time.perf_counter()
            result = fn(*args, **kw)
            self.sample(metric, (time.perf_counter() - t0) * 1000)
        return result

    def summary(self):
        return {m: {**stats(v), "unit": self.units[m]} for m, v in self.values.items()}

    def medians(self):
        return {m: {"median": statistics.median(v), "unit": self.units[m]} for m, v in self.values.items() if v}

    def close(self):
        self.f.close()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


def load_baseline(probe):
    path = BASELINES / f"{probe}.json"
    return json.loads(path.read_text()) if path.exists() else None


def write_baseline(probe, medians, note=None):
    BASELINES.mkdir(parents=True, exist_ok=True)
    doc = {"probe": probe, "commit": _commit(), "written": time.strftime("%Y-%m-%dT%H:%M:%S"),
           "metrics": {m: {"median": _median_of(v), "unit": (v.get("unit") if isinstance(v, dict) else None) or "ms"}
                       for m, v in medians.items()}}
    if note:
        doc["note"] = note
    (BASELINES / f"{probe}.json").write_text(json.dumps(doc, indent=2) + "\n")
    return doc


def _median_of(v):
    if isinstance(v, dict):
        return v["median"]
    if isinstance(v, (list, tuple)):
        return statistics.median(v)
    return v


def compare_baseline(probe, medians, threshold=THRESHOLD):
    """Compare {metric: median | [values] | {"median":..}} with the stored baseline."""
    base = load_baseline(probe)
    rows = []
    if not base:
        return {"probe": probe, "ok": True, "missing": True, "rows": rows}
    ok = True
    for metric, v in medians.items():
        cur = _median_of(v)
        b = base.get("metrics", {}).get(metric)
        if b is None:
            rows.append({"metric": metric, "current": cur, "baseline": None, "ratio": None, "status": "new"})
            continue
        ratio = cur / b["median"] if b["median"] else float("inf") if cur else 1.0
        status = "regression" if ratio > 1 + threshold else ("improved" if ratio < 1 - threshold else "ok")
        ok &= status != "regression"
        rows.append({"metric": metric, "current": cur, "baseline": b["median"], "ratio": round(ratio, 3),
                     "status": status})
    return {"probe": probe, "ok": ok, "missing": False, "rows": rows}


def medians_from_jsonl(path, commit=None):
    """{probe: {metric: median}} from a reporter JSONL file (optionally one commit)."""
    vals = {}
    for line in Path(path).read_text().splitlines():
        if not line.strip():
            continue
        r = json.loads(line)
        if commit and r.get("commit") != commit:
            continue
        vals.setdefault(r["probe"], {}).setdefault(r["metric"], []).append(r["value"])
    return {p: {m: statistics.median(v) for m, v in ms.items()} for p, ms in vals.items()}


def format_comparison(result):
    if result["missing"]:
        return f"{result['probe']}: no baseline (scripts/e2e/baselines/{result['probe']}.json)"
    lines = [f"{result['probe']}: {'OK' if result['ok'] else 'REGRESSION'}"]
    for r in result["rows"]:
        lines.append(f"  {r['status']:10} {r['metric']:30} current={r['current']:.3f} baseline={r['baseline']}"
                     f" ratio={r['ratio']}" if r["baseline"] is not None else f"  new        {r['metric']:30} current={r['current']:.3f}")
    return "\n".join(lines)
