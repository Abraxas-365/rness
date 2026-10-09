#!/usr/bin/env python3
"""WP-4 TUI perf through tmux against the frozen release binary.

    RNESS_E2E_PERF_OUT=/tmp/wp4-tui.jsonl python3 scripts/e2e/tui_perf.py [--sizes 1k,10k,50k] [--real N]

Per session size (synthetic, 200 compactions; or COPIES of real sessions with --real):
  startup_to_first_frame_ms  spawn → first non-blank capture
  time_to_idle_ms            spawn → statusline "idle" + pane stable
  pageup_latency_ms          PageUp → pane changed (capture-pane polling, ~5 ms resolution)
  pageup_burst_ms            50 PageUps → pane stable
  rss_kb / footprint_kb      after idle, after walking to top
  idle_cpu_pct               5 s sample at idle (200 ms tick should cost ~0)
  post_open_cpu_s            CPU seconds spent in the 10 s after idle (background layout)
Records go to $RNESS_E2E_PERF_OUT (perf.py Reporter). Numbers are preliminary on a shared box.
"""
import argparse
import re
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from harness import BIN, alive, footprint_kb, rss_kb, wait_until, with_rness  # noqa: E402
from fixtures.gen_session import generate, sample_real  # noqa: E402
from perf import Reporter  # noqa: E402

WP = 4
STATUS = re.compile(r"· idle")
SIZES = {"1k": (170, 200), "10k": (1700, 200), "50k": (8400, 200)}


def cpu_seconds(pid):
    out = subprocess.run(["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    if not out:
        return None
    parts = out.replace("-", ":").split(":")
    secs = 0.0
    for p in parts:
        secs = secs * 60 + float(p)
    return secs


def pane_body(tm):
    return "\n".join(tm.capture().splitlines()[:-3])


def measure(rep, r, sid, label, extra):
    t0 = time.perf_counter()
    tm = r.tui("-s", sid, tag=f"perf-{label}", width=169, height=46, wait=None)
    first = wait_until(lambda: tm.capture().strip(), 120, 0.005)
    t_first = (time.perf_counter() - t0) * 1000
    idle = wait_until(lambda: STATUS.search(tm.capture()), 180, 0.01)
    tm.wait_stable(0.3, 30)
    t_idle = (time.perf_counter() - t0) * 1000 - 300
    assert first and idle, f"{label}: never idle: {tm.capture()[-300:]}"
    rep.sample("startup_to_first_frame_ms", t_first, unit="ms", **extra)
    rep.sample("time_to_idle_ms", t_idle, unit="ms", **extra)
    rep.sample("rss_after_open_kb", rss_kb(r.pid), unit="kB", **extra)
    fp = footprint_kb(r.pid)
    if fp:
        rep.sample("footprint_after_open_kb", fp, unit="kB", **extra)
    c0 = cpu_seconds(r.pid)
    time.sleep(10)
    c1 = cpu_seconds(r.pid)
    rep.sample("post_open_cpu_10s_s", c1 - c0, unit="s", **extra)
    c0 = cpu_seconds(r.pid)
    time.sleep(5)
    c1 = cpu_seconds(r.pid)
    rep.sample("idle_cpu_pct", (c1 - c0) / 5 * 100, unit="pct", **extra)
    # PageUp latency (each sample: one PageUp, wait for change)
    lat = []
    for _ in range(15):
        before = pane_body(tm)
        t = time.perf_counter()
        tm.keys("PageUp")
        changed = wait_until(lambda: pane_body(tm) != before, 5, 0.002)
        if changed:
            lat.append((time.perf_counter() - t) * 1000)
        time.sleep(0.25)
    for v in lat:
        rep.sample("pageup_latency_ms", v, unit="ms", **extra)
    assert lat, "PageUp never changed the pane"
    # Burst of 50
    t = time.perf_counter()
    tm.keys(*["PageUp"] * 50)
    tm.wait_stable(0.3, 30)
    rep.sample("pageup_burst50_ms", (time.perf_counter() - t) * 1000 - 300, unit="ms", **extra)
    # Walk to top (bounded) and record memory there.
    t, last, presses = time.perf_counter(), None, 0
    while presses < 6000 and time.perf_counter() - t < 120:
        tm.keys(*["PageUp"] * 50)
        presses += 50
        time.sleep(0.05)
        cur = pane_body(tm)
        if cur == last:
            time.sleep(0.4)
            if pane_body(tm) == cur:
                break
        last = cur
    rep.sample("walk_to_top_ms", (time.perf_counter() - t) * 1000, unit="ms", presses=presses, **extra)
    rep.sample("rss_at_top_kb", rss_kb(r.pid), unit="kB", **extra)
    # Resize cost while at top and at bottom.
    for where in ("top", "bottom"):
        if where == "bottom":
            tm.keys(*["PageDown"] * 3000)
            tm.wait_stable(0.5, 60)
        ts = []
        for i in range(6):
            before = pane_body(tm)
            t = time.perf_counter()
            tm.resize(110 if i % 2 == 0 else 169, 46)
            wait_until(lambda: pane_body(tm) != before, 5, 0.002)
            ts.append((time.perf_counter() - t) * 1000)
            time.sleep(0.3)
        for v in ts:
            rep.sample(f"resize_latency_{where}_ms", v, unit="ms", **extra)
    assert alive(r.pid)
    return tm


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sizes", default="1k,10k,50k")
    ap.add_argument("--real", type=int, default=0, help="also measure COPIES of the N largest real sessions")
    a = ap.parse_args()
    for label in [s for s in a.sizes.split(",") if s]:
        turns, compactions = SIZES[label]
        rep = Reporter(f"tui-tmux-{label}", wp=WP, binary=BIN)
        with with_rness(wp=WP, mode="none", tag=f"perf-{label}") as r:
            sid = generate(r.root, r.work, turns=turns, tools_per_turn=2, tool_result_bytes=1500,
                           compactions=compactions, seed=11)[0]
            size = (r.root / sid / "session.v1.jsonl").stat().st_size
            measure(rep, r, sid, label, {"turns": turns, "compactions": compactions, "log_bytes": size})
        print(f"{label}: " + ", ".join(f"{k}={v['median']:.1f}" for k, v in rep.medians().items()), flush=True)
        rep.close()
    if a.real:
        rep = Reporter("tui-tmux-real", wp=WP, binary=BIN)
        with with_rness(wp=WP, mode="none", tag="perf-real") as r:
            ids = sample_real(r.root, n=a.real, workspace=r.work)
            for sid in ids[: a.real]:
                path = r.root / sid / "session.v1.jsonl"
                extra = {"session_bytes": path.stat().st_size, "lines": sum(1 for _ in open(path))}
                tm = measure(rep, r, sid, "real", extra)
                tm.kill()
                if r.pid and alive(r.pid):
                    subprocess.run(["kill", "-9", str(r.pid)])
                print(f"real {sid[:8]} {extra}: " + ", ".join(f"{k}={v['median']:.1f}" for k, v in rep.medians().items()),
                      flush=True)
        rep.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
