#!/usr/bin/env python3
"""WP-2 tool lifecycle E2E against the frozen rness binary + fake provider.

Standard library only; PASS/FAIL per check; exit code = number of failures.

    python3 scripts/e2e/tools_e2e.py            # all groups
    python3 scripts/e2e/tools_e2e.py kill9 read_zero

Groups:
  kill9      kill -9 rness while (a) a foreground Bash runs `cmd &` + a long
             command, (b) a terminal runs `yes`, (c) 20 background jobs run;
             report survivors after 5 s, then restart rness to see recovery.
  flood      200 MB `yes` through Bash in the real binary: rness RSS + time.
  read_zero  Read /dev/zero (plan §4 #12): RSS is watched and rness is killed
             at RNESS_E2E_READ_ZERO_MB (default 1500) MB to stay bounded.
Every spawned helper carries a unique marker in argv so pgrep finds exactly
our processes; all of them are killed at the end.
"""
import json
import os
import signal
import subprocess
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from harness import Checks, alive, kill_tree, rss_kb, start_provider, wait_until, with_rness  # noqa: E402

WP = 2
RESULTS = {}


def pgrep(marker):
    out = subprocess.run(["pgrep", "-f", marker], capture_output=True, text=True).stdout
    return sorted(int(p) for p in out.split() if int(p) != os.getpid())


def kill_marker(marker):
    for pid in pgrep(marker):
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def sleeper(marker, secs=1000):
    # perl keeps the marker in argv (pgrep -f) and costs ~2 MB.
    return f"perl -e 'sleep {secs}' {marker}"


def peak_rss_until(pid, done, limit_kb=None, every=0.05):
    """Sample rss of pid until done() or the process exits; kill if > limit."""
    peak, killed = 0, False
    while alive(pid) and not done():
        r = rss_kb(pid) or 0
        peak = max(peak, r)
        if limit_kb and r > limit_kb:
            os.kill(pid, signal.SIGKILL)
            killed = True
            break
        time.sleep(every)
    return peak, killed


def kill9_group(c, fp):
    m = f"wp2mark-{uuid.uuid4().hex[:10]}"
    scenarios = {
        "fg_bash_amp": [
            {"tool": "Bash", "args": {"command": f"{sleeper(m + '-bg')} & {sleeper(m + '-fg')}", "description": "x"}},
            {"text": "done"}],
        "terminal_yes": [
            {"tool": "terminal_open", "args": {"name": "t"}},
            {"tool": "terminal_send", "args": {"session_id": "term-1", "text": f"yes {m}-yes > /dev/null", "wait_ms": 60000}},
            {"text": "done"}],
        "bg_jobs_20": [
            {"tools": [{"name": "Bash", "args": {"command": sleeper(f"{m}-job{i}"), "description": "x",
                                                  "run_in_background": True}} for i in range(20)]},
            {"text": "waiting", "delay_ms": 60000},
            {"text": "done"}],
    }
    expect = {"fg_bash_amp": 2, "terminal_yes": 1, "bg_jobs_20": 20}
    for name, steps in scenarios.items():
        script = f"k9{name}{uuid.uuid4().hex[:6]}"
        fp.set_script(script, steps)
        with with_rness(wp=WP, provider=fp, tag=f"kill9-{name}", scenario=f"script:{script}/a") as r:
            p = r.spawn_headless("go")
            n = expect[name]
            prefix = f"{m}-{'bg' if name == 'fg_bash_amp' else 'yes' if name == 'terminal_yes' else 'job'}"
            got = wait_until(lambda: len(pgrep(prefix)) >= (1 if name != "bg_jobs_20" else n) or p.poll() is not None, 30)
            time.sleep(0.5)
            before = pgrep(m)
            os.kill(p.pid, signal.SIGKILL)
            p.wait()
            time.sleep(5)
            after = pgrep(m)
            # Also the terminal reaper helper: is it still around?
            reapers = [int(x) for x in subprocess.run(["pgrep", "-f", f"{r.binary} __rness-terminal-reaper"],
                                                      capture_output=True, text=True).stdout.split()]
            RESULTS[f"kill9_{name}"] = {"before": len(before), "survivors_after_5s": len(after),
                                        "reapers_matching_binary": len(reapers)}
            c.check(f"kill9/{name}: helpers started", got and len(before) >= 1,
                    f"before={len(before)} rc={p.returncode} err={Path(p.err_path).read_text()[-400:]}")
            c.check(f"kill9/{name}: no orphans 5 s after kill -9 (expected-to-fail = bug)", not after,
                    f"{len(after)} of {len(before)} survived")
            # Restart in the same HOME: does recovery mark the jobs interrupted?
            if name == "bg_jobs_20":
                kill_marker(m)
                fp.set_script(script + "r", [{"tool": "job_list", "args": {}}, {"text": "ok"}])
                res = r.headless("again", scenario=f"script:{script}r/b", timeout=60)
                states = {}
                for f in (r.root / "jobs").glob("*/*.json"):
                    try:
                        s = json.loads(f.read_text())
                    except Exception:
                        continue
                    if s.get("kind") == "bash":
                        st = s.get("status")
                        key = st if isinstance(st, str) else next(iter(st))
                        states[key] = states.get(key, 0) + 1
                RESULTS["kill9_bg_jobs_recovery_states"] = states
                c.check("kill9/bg_jobs_20: restart succeeds", res.rc == 0, res.stderr[-400:])
                c.check("kill9/bg_jobs_20: crashed jobs are not left 'Running' on disk",
                        states.get("Running", 0) == 0, json.dumps(states))
            kill_marker(m)


def flood_group(c, fp):
    script = f"flood{uuid.uuid4().hex[:6]}"
    fp.set_script(script, [
        {"tool": "Bash", "args": {"command": "yes | head -c 200000000", "description": "flood"}},
        {"tool": "Bash", "args": {"command": "head -c 50000000 /dev/urandom", "description": "bin"}},
        {"text": "done"}])
    with with_rness(wp=WP, provider=fp, tag="flood", scenario=f"script:{script}/a", record=True) as r:
        t = time.monotonic()
        p = r.spawn_headless("go")
        peak, _ = peak_rss_until(p.pid, lambda: p.poll() is not None)
        p.wait(timeout=120)
        el = time.monotonic() - t
        reqs = [q for q in fp.requests if q.get("key", "").startswith(f"script:{script}")]
        RESULTS["flood_real_binary"] = {"elapsed_s": round(el, 2), "rss_peak_mb": round(peak / 1024, 1),
                                        "rc": p.returncode, "request_bytes": [q.get("bytes") for q in reqs]}
        c.check("flood: headless completes", p.returncode == 0, Path(p.err_path).read_text()[-400:])
        c.check("flood: rness RSS stays < 300 MB with 250 MB of tool output", peak < 300 * 1024, f"peak={peak} KiB")
        c.check("flood: request bodies stay small (tail-bounded tool results)",
                all((q.get("bytes") or 0) < 1_000_000 for q in reqs), str([q.get("bytes") for q in reqs]))


def read_zero_group(c, fp):
    limit_mb = int(os.environ.get("RNESS_E2E_READ_ZERO_MB", "1500"))
    for path in ["/dev/zero", "/dev/urandom"]:
        script = f"rz{uuid.uuid4().hex[:6]}"
        fp.set_script(script, [{"tool": "Read", "args": {"path": path}}, {"text": "done"}])
        with with_rness(wp=WP, provider=fp, tag="readzero", scenario=f"script:{script}/a") as r:
            t = time.monotonic()
            p = r.spawn_headless("go")
            peak, killed = peak_rss_until(p.pid, lambda: p.poll() is not None or time.monotonic() - t > 60,
                                          limit_kb=limit_mb * 1024)
            el = time.monotonic() - t
            if p.poll() is None:
                kill_tree(p.pid)
            p.wait()
            RESULTS[f"read{path.replace('/', '_')}"] = {"rss_peak_mb": round(peak / 1024), "killed_at_limit": killed,
                                                         "seconds_to_limit": round(el, 2)}
            c.check(f"read {path}: rness does not exceed {limit_mb} MB (expected-to-fail = §4 #12)",
                    not killed, f"peak {peak // 1024} MB after {el:.1f}s")


def main():
    groups = sys.argv[1:] or ["kill9", "flood", "read_zero"]
    c = Checks()
    fp = start_provider(WP)
    try:
        for g in groups:
            {"kill9": kill9_group, "flood": flood_group, "read_zero": read_zero_group}[g](c, fp)
    finally:
        fp.stop()
        kill_marker("wp2mark-")
    print("WP2-RESULTS " + json.dumps(RESULTS))
    sys.exit(c.failures)


if __name__ == "__main__":
    main()
