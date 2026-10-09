#!/usr/bin/env python3
"""WP-5 provider E2E against the frozen binary (stdlib only).

    python3 scripts/e2e/providers_e2e.py [--quick] [--perf]

Covers what WP-0's selftest does not: huge tool JSON RSS bound, split-UTF8
rendering in the TUI, heartbeat-forever vs the idle timer, black-holed
connect, gateway error shapes, streaming throughput/RSS (--perf).
PASS/FAIL/XFAIL per check; exit code = number of FAILs (XFAIL = known bug, see wp-5.md).
"""
import argparse
import contextlib
import os
import signal
import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from harness import Checks, Sampler, with_rness, wait_until  # noqa: E402
from wp5_fake import start_wp5_provider  # noqa: E402

WP = 5
IDLE = "rness.providers.set_stream_idle_timeout('{route}', {ms})"
ROUTE = {"anthropic": "anthropic", "openai": "fake"}


class Suite(Checks):
    def xcheck(self, name, ok, bug, detail=""):
        self.results.append((name, True, detail))
        print(("XPASS " if ok else "XFAIL ") + name + f"  -- {bug}" + (f" ({detail})" if detail else ""), flush=True)


def attempts(log):
    return [e["outcome"] for e in log if e["type"] == "assistant/attempt"]


def turn_outcome(log):
    ended = [e["outcome"] for e in log if e["type"] == "turn/ended"]
    return ended[-1] if ended else None


def gateway_shapes(t, fp):
    for shape in ("anthropic", "openai"):
        with with_rness(WP, provider=fp, shape=shape, tag=f"gw-{shape}") as r:
            for status, kind, needle in ((502, "html", "upstream"), (503, "message", "upstream overloaded"),
                                         (400, "string", "not found"), (504, "text", "upstream request")):
                res = r.headless("hi", scenario=f"gateway:{status}:{kind}", timeout=90)
                a = attempts(r.log(res.session)) if res.session else []
                msg = a[0].get("message", "") if a else ""
                retry_ok = all(x.get("retryable") == (status >= 500) for x in a)
                t.check(f"{shape} gateway {status}/{kind}: retryable={status >= 500}", a and retry_ok, json.dumps(a)[:200])
                t.check(f"{shape} gateway {status}/{kind}: message carries body", needle in msg.lower(), msg)


def heartbeat_forever(t, fp, seconds):
    for shape in ("anthropic", "openai"):
        init = IDLE.format(route=ROUTE[shape], ms=1000)
        with with_rness(WP, provider=fp, shape=shape, init_append=init, tag=f"hb-{shape}") as r:
            p = r.spawn_headless("hi", scenario="heartbeat-forever:200")
            done = wait_until(lambda: p.poll() is not None, seconds, 0.2)
            t.xcheck(f"{shape} heartbeat-only stream ends (idle=1s) within {seconds}s", done,
                     "B5-6 comments reset the idle timer forever; no first-content/total deadline",
                     f"still running after {seconds}s" if not done else "")
            if not done:
                with contextlib.suppress(OSError):
                    os.killpg(p.pid, signal.SIGKILL)
                p.wait(10)


def blackhole(t, fp):
    # 10.255.255.1 drops SYNs here (no RST). Idle 2 s bounds connect; default 300 s.
    init = "rness.providers.set_stream_idle_timeout('anthropic', 2000)"
    with with_rness(WP, provider=fp, init_append=init, tag="bh") as r:
        res = r.run("-m", "anthropic/fake", "--base-url", "http://10.255.255.1:81", "-p", "hi",
                    provider=False, timeout=60, env={"ANTHROPIC_API_KEY": "fake"})
        a = attempts(r.log(res.session)) if res.session else []
        t.check("blackhole connect: fails with TIMEOUT x3 (idle=2s)",
                len(a) == 3 and all(x.get("code") == "TIMEOUT" for x in a), json.dumps(a)[:300])
        t.check("blackhole connect: total < 15s (3x2s + backoff)", res.elapsed < 15, f"{res.elapsed:.1f}s")
        t.check("blackhole connect: message names the connect phase",
                any("connect timeout to" in x.get("message", "").lower() for x in a), a[0].get("message") if a else "")
        print(f"      blackhole elapsed={res.elapsed:.1f}s (connect bound: min(10s, idle/2) per attempt)")


def huge_tool_json_rss(t, fp, mb):
    # A tool call whose args are `mb` MiB. Measures peak RSS of the process.
    with with_rness(WP, provider=fp, tag="huge") as r:
        p = r.spawn_headless("hi", scenario=f"huge-tool-json:{mb}")
        with Sampler(p.pid, every=0.05) as s:
            wait_until(lambda: p.poll() is not None, 180, 0.1)
        peak = s.peak() or 0
        out = Path(p.out_path).read_text()
        sess = r.latest_session()
        log = r.log(sess) if sess else []
        t.check(f"huge tool JSON {mb} MiB: turn completes", turn_outcome(log) == "completed",
                f"rc={p.returncode} {out[-200:]}")
        ratio = peak / 1024 / mb if mb else 0
        print(f"      huge tool JSON {mb} MiB: peak RSS {peak / 1024:.0f} MiB ({ratio:.1f}x payload)")
        t.xcheck(f"huge tool JSON {mb} MiB: peak RSS < 6x payload", ratio < 6,
                 "B5-8 args held ~N times (args_json + chunks Vec + parsed Value + log line + request replay)",
                 f"{ratio:.1f}x")
        log_mb = (r.root / sess / "session.v1.jsonl").stat().st_size / 1048576 if sess else 0
        print(f"      session log {log_mb:.1f} MiB")
        return peak


def split_utf8_tui(t, fp):
    with with_rness(WP, provider=fp, mode="tui", scenario="split-utf8", tag="utf8") as r:
        r.tmux.type("hi")
        r.tmux.keys("Enter")
        m = r.tmux.wait_for("split-utf8 ok", timeout=20, history=200)
        screen = r.tmux.capture(history=200)
        t.check("TUI split-utf8: text rendered", m, screen[-500:])
        t.check("TUI split-utf8: no U+FFFD", "\ufffd" not in screen, "")
        t.check("TUI split-utf8: CJK + emoji intact", "日本語" in screen and "🎉" in screen, screen[-300:])


def stream_throughput(t, fp, mb):
    from perf import Reporter
    rep = Reporter("wp5_stream_throughput", wp=WP, echo=True)
    for shape in ("anthropic", "openai"):
        with with_rness(WP, provider=fp, shape=shape, tag=f"tp-{shape}") as r:
            p = r.spawn_headless("hi", scenario=f"bigtext:{mb}")
            t0 = time.perf_counter()
            with Sampler(p.pid, every=0.05) as s:
                wait_until(lambda: p.poll() is not None, 300, 0.05)
            ms = (time.perf_counter() - t0) * 1000
            n = int(mb * 1024 * 1024 / 16)
            out = Path(p.out_path).read_text()
            t.check(f"{shape} bigtext {mb} MiB delivered", len(out) >= mb * 1024 * 1024, len(out))
            rep.sample(f"{shape}_wall_ms", ms, deltas=n)
            rep.sample(f"{shape}_us_per_delta", ms * 1000 / n, unit="us")
            rep.sample(f"{shape}_peak_rss_mb", (s.peak() or 0) / 1024, unit="MiB")
    rep.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--quick", action="store_true")
    ap.add_argument("--perf", action="store_true")
    ap.add_argument("--only")
    a = ap.parse_args()
    t = Suite()
    fp = start_wp5_provider(WP)
    try:
        groups = {
            "gateway": lambda: gateway_shapes(t, fp),
            "heartbeat": lambda: heartbeat_forever(t, fp, 6 if a.quick else 12),
            "blackhole": lambda: blackhole(t, fp),
            "huge": lambda: huge_tool_json_rss(t, fp, 16 if a.quick else 64),
            "utf8": lambda: split_utf8_tui(t, fp),
        }
        if a.perf:
            groups["throughput"] = lambda: stream_throughput(t, fp, 10)
        for name, fn in groups.items():
            if a.only and name not in a.only.split(","):
                continue
            print(f"== {name}", flush=True)
            try:
                fn()
            except Exception as e:  # a crashed group is a failure, not a crash of the suite
                t.check(f"{name}: group ran", False, repr(e))
    finally:
        fp.stop()
    sys.exit(t.done())


if __name__ == "__main__":
    main()
