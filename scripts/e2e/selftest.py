#!/usr/bin/env python3
"""WP-0 self-test: every fake-provider scenario once through headless rness
(all three wire shapes for the protocol-level ones), plus the harness modes
(TUI in tmux, --serve), fixture generator, perf reporter and process helpers.

    python3 scripts/e2e/selftest.py [--quick] [-k SUBSTR]

Prints PASS/FAIL/SKIP/XFAIL/XPASS per check; exit code = number of FAILs.
XFAIL = known product bug (see e2e-findings/wp-0.md), not counted as failure;
XPASS = that bug appears fixed (update the expectation).
"""
import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from harness import (Checks, Sampler, Timer, alive, children, fd_count, footprint_kb, kill9_when,  # noqa: E402
                     process_tree, rss_kb, start_provider, tmp_dir, wait_until, with_rness, SHAPES)
from fixtures.gen_session import generate  # noqa: E402
import perf  # noqa: E402

WP = 0
IDLE_MS = 1000
STALL_S = 3
ROUTE = {"anthropic": "anthropic", "openai": "fake", "responses": "chatgpt"}


def attempts(log):
    return [e["outcome"] for e in log if e["type"] == "assistant/attempt"]


def turn_outcome(log):
    ended = [e["outcome"] for e in log if e["type"] == "turn/ended"]
    return ended[-1] if ended else None


def tool_names(log):
    return [e["name"] for e in log if e["type"] == "tool/result"]


class Suite(Checks):
    def __init__(self, k=None):
        super().__init__()
        self.k = k

    def xcheck(self, name, ok, bug, detail=""):
        """Expected failure (known bug): ok=True means the bug is gone."""
        self.results.append((name, True, detail))
        print(("XPASS " if ok else "XFAIL ") + name + f"  -- {bug}" + (f" ({detail})" if detail else ""), flush=True)


def scenario_expectations():
    """(scenario, shapes, checker(res, log, fp, shape) -> [(name, ok, detail[, xfail_bug])])"""
    P = "Partial response from fake provider."

    def completed(text=None):
        def chk(res, log, fp, shape):
            out = [("completed", turn_outcome(log) == "completed", turn_outcome(log))]
            if text:
                out.append(("text", f"rness: {text}" in res.stdout, res.stdout[-200:]))
            return out
        return chk

    def failed_retried(code, n=3, xfail=None):
        def chk(res, log, fp, shape):
            a = attempts(log)
            bug = xfail.get(shape) if xfail else None
            return [("turn failed", turn_outcome(log) == "failed", turn_outcome(log), bug),
                     (f"{n} attempts code={code}", len(a) == n and all(x.get("code") == code for x in a),
                      json.dumps(a)[:300], bug),
                     ("no partial text on stdout", "rness:" not in res.stdout, res.stdout[-200:],
                      bug if shape == "openai" else None)]
        return chk

    def recover(res, log, fp, shape):
        a = attempts(log)
        ended = [e["outcome"] for e in log if e["type"] == "turn/ended"]
        return [("first turn fails 401 (non-retryable)", ended[:1] == ["failed"] and len(a) == 1
                 and a[0].get("retryable") is False, f"{ended} {json.dumps(a)}"),
                ("retry on same session succeeds", res.followup and res.followup.rc == 0
                 and P in res.followup.stdout, repr(res.followup))]

    def heartbeat(res, log, fp, shape):
        return completed(P)(res, log, fp, shape) + [("took >= 2s", res.elapsed >= 1.9, f"{res.elapsed:.2f}")]

    def http_529_1(res, log, fp, shape):
        a = attempts(log)
        return completed("ok after 1 HTTP 529")(res, log, fp, shape) + [
            ("one retryable 529 attempt", len(a) == 1 and a[0].get("retryable") is True, json.dumps(a))]

    def retry_after(expect_ms_lo, expect_ms_hi):
        def chk(res, log, fp, shape):
            a = attempts(log)
            ms = a[0].get("retry_in_ms") if a else None
            return completed("retry ok after 1 rejection(s)")(res, log, fp, shape) + [
                (f"retry_in_ms in [{expect_ms_lo},{expect_ms_hi}]", ms is not None and expect_ms_lo <= ms <= expect_ms_hi, ms),
                ("waited before retry", res.elapsed >= expect_ms_lo / 1000 - 0.05, f"{res.elapsed:.2f}s")]
        return chk

    def overflow(res, log, fp, shape):
        a = attempts(log)
        return [("turn failed", turn_outcome(log) == "failed", turn_outcome(log)),
                ("single CONTEXT_OVERFLOW non-retryable", len(a) == 1 and a[0].get("code") == "CONTEXT_OVERFLOW"
                 and a[0].get("retryable") is False, json.dumps(a))]

    def huge(res, log, fp, shape):
        msgs = [e for e in log if e["type"] == "assistant/message" and any(c.get("kind") == "tool_use" for c in e["content"])]
        pad = len(msgs[0]["content"][-1]["args"].get("pad", "")) if msgs else 0
        return completed("huge-tool-json done")(res, log, fp, shape) + [
            ("4 MiB args parsed intact", pad == 4 * 1024 * 1024, pad)]

    def slow(res, log, fp, shape):
        return completed("drip ok")(res, log, fp, shape) + [("drip took > 0.5s", res.elapsed > 0.5, f"{res.elapsed:.2f}")]

    def drip_vs_idle(res, log, fp, shape):
        # Idle timeout counts complete SSE events/comments, not bytes: a 5 ms/byte
        # drip (>1 s per ~200-byte event) must time out under a 1 s idle timeout.
        a = attempts(log)
        return [("byte trickle is not progress: TIMEOUT x3", turn_outcome(log) == "failed" and len(a) == 3
                 and all(x.get("code") == "TIMEOUT" for x in a), json.dumps(a)[:300])]

    def dup(res, log, fp, shape):
        if shape == "anthropic":
            return completed()(res, log, fp, shape) + [
                ("both blocks kept", "rness: AAA" in res.stdout and "rness: BBB" in res.stdout, res.stdout[-200:])]
        return completed("dup-index done")(res, log, fp, shape) + [
            # OpenAI: two calls with distinct ids but the same index. Merging them silently is data loss.
            ("both same-index tool calls executed", tool_names(log).count("X") == 2, tool_names(log),
             {"openai": "openai: same-index tool_calls with distinct ids are merged into one call"}.get(shape))]

    def tool_loop(res, log, fp, shape):
        return completed("tool-loop done after 3 tool results")(res, log, fp, shape) + [
            ("3 Bash results", tool_names(log) == ["Bash"] * 3, tool_names(log))]

    def echo(res, log, fp, shape):
        m = re.search(r"ECHO (\{.*\})", res.stdout)
        st = json.loads(m.group(1)) if m else {}
        return completed()(res, log, fp, shape) + [
            ("echo stats", st.get("messages") == 1 and st.get("last_user") == "hello" and st.get("tools", 0) > 5
             and st.get("shape") == shape, json.dumps(st)[:300])]

    def script(res, log, fp, shape):
        return completed("script done 2")(res, log, fp, shape) + [("scripted Bash ran", tool_names(log) == ["Bash"], tool_names(log))]

    def truncated(res, log, fp, shape):
        bug = {"openai": "openai: stream closed without finish_reason/[DONE] is committed as a complete message",
               "responses": "responses: stream closed without response.completed is committed as a complete message"}.get(shape)
        return failed_retried("PROVIDER", xfail={shape: bug} if bug else None)(res, log, fp, shape)

    def malformed(res, log, fp, shape):
        bug = {"responses": "responses: invalid JSON events are silently skipped; empty message committed"}.get(shape)
        return failed_retried("PROVIDER", xfail={shape: bug} if bug else None)(res, log, fp, shape)

    all_shapes = SHAPES
    return [
        ("ok", all_shapes, completed(P)),
        ("plan", ("anthropic",), lambda res, log, fp, s: completed(P)(res, log, fp, s) + [
            ("exit_plan_mode called", tool_names(log) == ["exit_plan_mode"], tool_names(log))]),
        ("recover", ("anthropic",), recover),
        ("heartbeat", ("anthropic",), heartbeat),
        ("cut", all_shapes, truncated),
        ("disconnect", ("anthropic",), failed_retried("PROVIDER")),
        ("stall", ("anthropic",), failed_retried("TIMEOUT")),
        ("stall-headers", ("anthropic",), failed_retried("TIMEOUT")),
        ("malformed", all_shapes, malformed),
        ("stream-error", all_shapes, failed_retried("PROVIDER")),
        ("overloaded", ("anthropic",), failed_retried("PROVIDER")),
        ("http-529", ("anthropic",), failed_retried("HTTP")),
        ("http-529:1", all_shapes, http_529_1),
        ("retry-after-seconds", all_shapes, retry_after(1000, 1000)),
        ("retry-after-date:2", ("anthropic",), retry_after(1000, 2000)),
        ("context-overflow", all_shapes, overflow),
        ("huge-tool-json", all_shapes, huge),
        ("slow-drip:1", ("anthropic",), slow),
        ("slow-drip:5", ("anthropic",), drip_vs_idle),
        ("split-utf8", all_shapes, completed("split-utf8 ok: héllo wörld 日本語 🎉 ünïcödé ✓")),
        ("dup-index", all_shapes, dup),
        ("unknown-event", all_shapes, completed("unknown-event ok")),
        ("tool-loop:3:Bash", all_shapes, tool_loop),
        ("echo", all_shapes, echo),
        ("script:s1", all_shapes, script),
    ]


def run_scenarios(t, fp, quick):
    fp.set_script("s1", [{"tool": "Bash", "args": {"command": "echo hi", "description": "x"}}, {"text": "script done {n}"}])
    for shape in SHAPES:
        lua = f"rness.providers.set_stream_idle_timeout('{ROUTE[shape]}', {IDLE_MS})"
        with with_rness(WP, provider=fp, shape=shape, init_append=lua, tag=f"sc-{shape}") as r:
            for scenario, shapes, checker in scenario_expectations():
                if shape not in shapes or (quick and shape != "anthropic"):
                    continue
                name = f"{shape}/{scenario}"
                if t.k and t.k not in name:
                    continue
                key = f"{scenario}/{shape}"  # private counter per shape
                res = r.headless("hello", scenario=key, timeout=120)
                res.followup = r.headless("again", session=res.session, scenario=key) if scenario == "recover" else None
                if not t.check(f"{name}: rness ran (rc=0, session)", res.rc == 0 and res.session, repr(res)):
                    continue
                log = r.log(res.session)
                for item in checker(res, log, fp, shape):
                    label, ok, detail = item[0], item[1], item[2]
                    bug = item[3] if len(item) > 3 else None
                    if bug:
                        t.xcheck(f"{name}: {label}", ok, bug, str(detail)[:200])
                    else:
                        t.check(f"{name}: {label}", ok, str(detail)[:400])
                t.check(f"{name}: stdout is transcript only",
                        all(l.startswith(("you: ", "rness: ")) or not l.strip() or not re.match(r"^\S+:", l)
                            for l in res.stdout.splitlines()), res.stdout[-300:])


def check_overflow_compaction(t, fp):
    with with_rness(WP, provider=fp, tag="overflow") as r:
        sid = generate(r.root, r.work, turns=40, tools_per_turn=1, tool_result_bytes=6000, seed=3)[0]
        res = r.headless("go", session=sid, scenario="context-overflow:1/compact")
        log = r.log(sid)
        types = [e["type"] for e in log]
        fin = [e["outcome"] for e in log if e["type"] == "compaction/finished"]
        t.check("context-overflow:1 with history: compaction committed then turn completes",
                fin[-1:] == ["committed"] and turn_outcome(log) == "completed" and "after overflow ok" in res.stdout,
                f"{fin} {turn_outcome(log)} {types[-8:]}")
        aux = [x["aux"] for x in fp.requests if x["key"] == "context-overflow:1/compact"]
        t.check("fake provider saw main, compaction, main", aux == [None, "compaction", None], aux)


def check_fixtures(t, fp):
    with with_rness(WP, provider=fp, scenario="echo", tag="fixtures") as r:
        ids = generate(r.root, r.work, turns=30, tools_per_turn=2, tool_result_bytes=8192, compactions=2, prunes=3,
                       forks=1, attempts=1, seed=1)
        plain = generate(r.root, r.work, turns=30, tools_per_turn=2, tool_result_bytes=8192, seed=1)
        res = r.run("--list")
        t.check("fixtures: --list shows generated roots + fork", res.rc == 0 and set(res.stdout.split()) == set(ids + plain),
                f"{res.stdout.split()} vs {ids + plain} {res.stderr[-300:]}")
        stats = {}
        for sid in ids + plain:
            res = r.headless("continue", session=sid)
            m = re.search(r"ECHO (\{.*\})", res.stdout)
            stats[sid] = json.loads(m.group(1)) if m else None
            t.check(f"fixtures: headless turn on {sid[-6:]} completes", res.rc == 0 and m, repr(res))
        a, b = stats.get(ids[0]), stats.get(plain[0])
        if a and b:
            t.check("fixtures: compaction checkpoints shrink replayed context", a["messages"] < b["messages"] / 2,
                    f"{a['messages']} vs {b['messages']}")
            t.check("fixtures: fork replays parent prefix", stats[ids[1]]["messages"] > 10, stats[ids[1]]["messages"])
        out = subprocess.run([sys.executable, str(Path(__file__).parent / "fixtures/gen_session.py"), "gen", str(r.base / "cli"),
                              "--workspace", str(r.work), "--turns", "3", "--sessions", "2"], capture_output=True, text=True)
        t.check("fixtures: gen_session.py CLI", out.returncode == 0 and len(out.stdout.splitlines()) == 2, out.stderr)


def check_tui(t, fp):
    if not shutil.which("tmux"):
        return t.skip("tui", "tmux not installed")
    with with_rness(WP, provider=fp, mode="tui", tag="selftest-tui", tui_wait=None) as r:
        ready = r.tmux.wait_for(r"fake", 20)
        t.check("tui: started in tmux, shows model", ready, r.tmux.capture()[-800:])
        t.check("tui: rness pid tracked", r.pid and alive(r.pid), r.pid)
        r.tmux.say("hello tui")
        t.check("tui: reply rendered", r.tmux.wait_for(r"Partial response from fake provider", 20), r.tmux.capture()[-1500:])
        t.check("tui: rss/fd sampling", (rss_kb(r.pid) or 0) > 1000 and (fd_count(r.pid) or 0) > 3,
                f"rss={rss_kb(r.pid)} fds={fd_count(r.pid)} footprint={footprint_kb(r.pid)}")
        sessions = r.sessions()
        t.check("tui: one session persisted", len(sessions) == 1, sessions)
        pid = r.pid
    t.check("tui: cleaned up (process + tmux session)", not alive(pid) and subprocess.run(
        ["tmux", "has-session", "-t", "=rness-e2e-wp0-selftest-tui"], capture_output=True).returncode != 0, pid)


def check_serve(t, fp):
    with with_rness(WP, provider=fp, mode="serve", tag="serve") as r:
        sid = r.api("POST", "/api/sessions", {"workspace": str(r.work)})["session"]
        t.check("serve: create session", sid, sid)
        resp = r.api("POST", "/api/request", {"type": "send", "session": sid, "intent": "followup",
                                              "content": [{"kind": "text", "text": "hi"}]})
        t.check("serve: send started", resp.get("status") == "started", resp)
        idle = wait_until(lambda: r.api("GET", f"/api/sessions/{sid}/phase")["phase"] == "idle", 20, 0.1)
        hist = r.api("GET", f"/api/sessions/{sid}")["envelopes"]
        t.check("serve: turn completed via API", idle and turn_outcome(hist) == "completed",
                [e["type"] for e in hist][-6:])
        status, _ = r.api("GET", "/api/sessions", raw=True)
        t.check("serve: list sessions 200", status == 200, status)
        pid = r.pid
    t.check("serve: process gone after context exit", not alive(pid), pid)


def check_helpers(t, fp):
    with with_rness(WP, provider=fp, scenario="slow-drip:20", tag="helpers") as r:
        p = r.spawn_headless("hello")
        with Sampler(p.pid, every=0.2) as s:
            fired = kill9_when(p.pid, lambda: fp.main_requests("slow-drip:20"), 15)
            p.wait(10)
        t.check("helpers: kill9_when fires on provider request", fired and p.returncode == -signal.SIGKILL, p.returncode)
        t.check("helpers: Sampler recorded rss", s.samples and s.peak() and s.peak() > 1000, s.samples[:2])
        sid = r.latest_session()
        t.check("helpers: killed session readable by next run", sid and r.headless("again", session=sid, scenario="ok").rc == 0, sid)
        tree = process_tree(os.getpid())
        t.check("helpers: process_tree/children", tree[0][0] == os.getpid(), tree[:2])
    with Timer() as tm:
        time.sleep(0.05)
    t.check("helpers: Timer", 45 <= tm.ms < 500, tm.ms)


def check_perf(t):
    base = tmp_dir(WP, "perf")
    try:
        out = base / "p.jsonl"
        perf.BASELINES = base / "baselines"
        rep = perf.Reporter("selftest-probe", wp=WP, out=out)
        for v in (10, 11, 12):
            rep.sample("x_ms", v)
        rep.close()
        perf.write_baseline("selftest-probe", rep.medians())
        same = perf.compare_baseline("selftest-probe", {"x_ms": 12.4})
        worse = perf.compare_baseline("selftest-probe", {"x_ms": 14})
        t.check("perf: reporter writes JSON lines", len(out.read_text().splitlines()) == 3)
        t.check("perf: +12% is ok, +27% is regression", same["ok"] and not worse["ok"], (same, worse))
        t.check("perf: medians_from_jsonl", perf.medians_from_jsonl(out) == {"selftest-probe": {"x_ms": 11}})
    finally:
        perf.BASELINES = perf.HERE / "baselines"
        shutil.rmtree(base, ignore_errors=True)


def check_provider_cli(t):
    """Backwards compatibility of the standalone CLI (old flags, old URL layout)."""
    import urllib.request
    p = subprocess.Popen([sys.executable, str(Path(__file__).resolve().parents[1] / "fake_provider.py"), "--port", "8709",
                          "--stall-seconds", "1"], stdout=subprocess.PIPE, text=True)
    try:
        line = p.stdout.readline()
        req = urllib.request.Request("http://127.0.0.1:8709/ok/v1/messages", data=b'{"model":"m","messages":[]}',
                                     method="POST", headers={"Content-Type": "application/json"})
        body = urllib.request.urlopen(req, timeout=5).read().decode()
        t.check("fake_provider CLI: starts and serves /ok/v1/messages", "8709" in line and "message_stop" in body
                and "Partial response from fake provider." in body, body[:200])
    finally:
        p.terminate()
        p.wait(5)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--quick", action="store_true", help="anthropic shape only, skip TUI")
    ap.add_argument("-k", help="only scenario checks whose name contains this")
    a = ap.parse_args()
    t = Suite(a.k)
    fp = start_provider(WP, stall_seconds=STALL_S)
    try:
        run_scenarios(t, fp, a.quick)
        if not a.k:
            check_overflow_compaction(t, fp)
            check_fixtures(t, fp)
            check_serve(t, fp)
            check_helpers(t, fp)
            check_perf(t)
            check_provider_cli(t)
            if not a.quick:
                check_tui(t, fp)
    finally:
        fp.stop()
    return t.done()


if __name__ == "__main__":
    sys.exit(main())
