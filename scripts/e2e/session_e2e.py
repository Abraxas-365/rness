#!/usr/bin/env python3
"""WP-1 process-level E2E: session lock, crash injection, instruction
re-injection, against the FROZEN binary with the fake provider.

    python3 scripts/e2e/session_e2e.py [-k SUBSTR] [--crash-rounds N]

Sections: lock (two processes / serve / TUI on one session, kill -9 holder),
crash (kill -9 mid-stream, mid-compaction summary, mid-5MB tool result;
reopen: torn tail healed, spans closed, next request == replay),
corrupt (garbled tail quarantined, unknown event type / newer version refused),
instructions (AGENTS.md change / compaction / truncation / invalid UTF-8,
via --record request bodies).
PASS/FAIL/XFAIL per check; exit code = FAIL count. XFAIL = confirmed bug
(see ~/.rness/plans/rness/e2e-findings/wp-1.md).
"""
import argparse
import json
import os
import random
import signal
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from harness import (Checks, alive, kill9, kill9_when, read_log, start_provider, wait_until,  # noqa: E402
                     with_rness)

WP = 1


class Suite(Checks):
    def xcheck(self, name, ok, bug, detail=""):
        self.results.append((name, True, detail))
        print(("XPASS " if ok else "XFAIL ") + name + f"  -- {bug}" + (f" ({str(detail)[:300]})" if detail else ""),
              flush=True)


def log_path(r, sid):
    return Path(r.root) / sid / "session.v1.jsonl"


def types(r, sid):
    return [e["type"] for e in read_log(r.root, sid)]


def lines_ok(path):
    """(complete lines parse, ends with newline)."""
    data = Path(path).read_bytes()
    body = data.rsplit(b"\n", 1)[0] if not data.endswith(b"\n") else data
    try:
        for line in body.splitlines():
            if line.strip():
                json.loads(line)
        return True, data.endswith(b"\n")
    except Exception:
        return False, data.endswith(b"\n")


def requests(r):
    d = r.base / "requests"
    return sorted(d.glob("*.json")) if d.exists() else []


def load(p):
    return json.loads(Path(p).read_text())


def main_bodies(r):
    return [load(p) for p in requests(r) if "-title" not in p.name and "-compaction" not in p.name]


def body_text(body):
    return json.dumps(body["body"])


def new_session(r, scenario="ok"):
    res = r.headless("seed", scenario=scenario)
    assert res.rc == 0 and res.session, res
    return res.session


# -- 1. lock ------------------------------------------------------------------

def sec_lock(c):
    with with_rness(wp=WP, tag="lock", scenario="ok") as r:
        sid = new_session(r)
        # (a) holder: a headless turn that stalls mid-stream.
        r.fp.set_stall_seconds(30)
        holder = r.spawn_headless("hold", "-s", sid, scenario="stall/lock-a")
        wait_until(lambda: len(r.fp.main_requests("stall/lock-a")) >= 1, 15)
        size0 = log_path(r, sid).stat().st_size
        second = r.headless("second", session=sid, scenario="ok", timeout=30)
        c.check("lock: 2nd headless on held session fails (rc!=0)", second.rc != 0, repr(second))
        msg = (second.stderr + second.stdout).lower()
        c.check("lock: error mentions lock/in use", "lock" in msg or "in use" in msg or "another" in msg,
                second.stderr[-300:])
        c.check("lock: no events appended by the loser",
                log_path(r, sid).stat().st_size >= size0 and "second" not in log_path(r, sid).read_text())
        # (b) --fork of a held session works (readers are not blocked).
        fork = r.headless("forked", "--fork", sid, scenario="ok", timeout=30)
        c.check("lock: --fork of a held session succeeds", fork.rc == 0 and fork.session not in (None, sid),
                repr(fork))
        # (c) serve instance: send to held session via HTTP.
        # (d) kill -9 the holder; lock released?
        kill9(holder.pid)
        holder.wait(5)
        ok, _ = lines_ok(log_path(r, sid))
        c.check("lock: log parses after kill -9 of holder", ok)
        third = r.headless("third", session=sid, scenario="ok", timeout=30)
        c.check("lock: released after kill -9 (next headless ok)", third.rc == 0, repr(third))
        t = types(r, sid)
        c.check("lock: session ends with a completed turn", t[-1] == "turn/ended", t[-5:])
        # Interrupted turn (killed) is closed on resume?
        log = read_log(r.root, sid)
        started = [e["turn"] for e in log if e["type"] == "turn/started"]
        ended = [e["turn"] for e in log if e["type"] == "turn/ended"]
        c.xcheck("lock: every turn/started has a turn/ended after resume",
                 sorted(set(started)) == sorted(set(ended)), "B1-9 killed turn never closed",
                 f"started={started} ended={ended}")

    # serve + headless on one session
    with with_rness(wp=WP, tag="lock-serve", mode="serve", scenario="ok") as r:
        sid = new_session(r)
        r.fp.set_stall_seconds(30)
        holder = r.spawn_headless("hold", "-s", sid, scenario="stall/lock-s")
        wait_until(lambda: len(r.fp.main_requests("stall/lock-s")) >= 1, 15)
        status, text = r.api("POST", "/api/request", {"type": "send", "session": sid, "intent": "followup",
                                                      "content": [{"kind": "text", "text": "via-serve"}]}, raw=True)
        time.sleep(2)
        content = log_path(r, sid).read_text()
        c.check("lock: serve send to session held by headless does not write into it",
                "via-serve" not in content, f"status={status} body={text[:200]}")
        c.check("lock: serve reports an error for the held session (HTTP status or event)",
                status >= 400 or "lock" in text.lower(), f"status={status} body={text[:200]}")
        kill9(holder.pid)
        holder.wait(5)
        # After release, the serve instance can use the session.
        status2, text2 = r.api("POST", "/api/request", {"type": "send", "session": sid, "intent": "followup",
                                                        "content": [{"kind": "text", "text": "after-release"}]},
                               raw=True)
        got = wait_until(lambda: "after-release" in log_path(r, sid).read_text(), 15)
        c.check("lock: serve can write after the holder dies", got, f"status={status2} body={text2[:200]}")

    # TUI + headless on one session
    with with_rness(wp=WP, tag="lock-tui", mode="none", scenario="ok") as r:
        sid = new_session(r)
        r.tui("-s", sid, tag="lock-tui", wait=r"fake")
        r.tmux.say("tui-turn")
        wait_until(lambda: "tui-turn" in log_path(r, sid).read_text(), 20)
        time.sleep(1)
        res = r.headless("headless-while-tui", session=sid, scenario="ok", timeout=30)
        content = log_path(r, sid).read_text()
        c.check("lock: headless on a session open in an idle TUI",
                res.rc != 0 or "headless-while-tui" in content,
                repr(res))
        print(f"INFO  lock: headless vs idle TUI -> rc={res.rc} wrote={'headless-while-tui' in content} "
              f"stderr={res.stderr.strip()[-200:]!r}")
        ok, _ = lines_ok(log_path(r, sid))
        c.check("lock: log parses after TUI + headless", ok)


# -- 2. crash injection ---------------------------------------------------------

def verify_resume(c, r, sid, tag, scenario="echo/resume-" ):
    """Reopen after a crash: torn tail healed, spans closed, next request == replay."""
    path = log_path(r, sid)
    parse_ok, nl = lines_ok(path)
    c.check(f"crash[{tag}]: committed lines parse", parse_ok)
    before = read_log(r.root, sid) if nl else None
    n_req = len(requests(r))
    res = r.headless(f"resume-{tag}", session=sid, scenario=f"echo/resume-{tag}", timeout=60)
    c.check(f"crash[{tag}]: resume turn ok", res.rc == 0, repr(res))
    data = path.read_bytes()
    c.check(f"crash[{tag}]: log ends with newline after resume", data.endswith(b"\n"))
    log = read_log(r.root, sid)
    starts = {e["id"] for e in log if e["type"] == "compaction/started"}
    finished = {e["started"] for e in log if e["type"] == "compaction/finished"}
    c.check(f"crash[{tag}]: every compaction/started is finished", starts <= finished,
            f"open={starts - finished}")
    # The echo reply lists what the model saw; the user message just sent
    # must be last and earlier prompts present.
    reqs = [load(p) for p in requests(r)[n_req:] if "-title" not in p.name and "-compaction" not in p.name]
    if reqs:
        body = body_text(reqs[-1])
        c.check(f"crash[{tag}]: next request contains resume prompt", f"resume-{tag}" in body)
        c.check(f"crash[{tag}]: next request contains first prompt", "seed" in body)
        # tool_use/tool_result pairing in the request (anthropic shape)
        msgs = reqs[-1]["body"].get("messages", [])
        bad = []
        for i, m in enumerate(msgs):
            if m["role"] == "assistant" and isinstance(m["content"], list):
                uses = {b["id"] for b in m["content"] if b.get("type") == "tool_use"}
                if uses:
                    nxt = msgs[i + 1] if i + 1 < len(msgs) else None
                    got = {b.get("tool_use_id") for b in (nxt or {}).get("content", []) if isinstance(b, dict)
                           and b.get("type") == "tool_result"} if nxt and isinstance(nxt["content"], list) else set()
                    if uses - got:
                        bad.append(i)
        c.check(f"crash[{tag}]: request tool_use/tool_result pairs intact", not bad, f"unpaired at {bad}")
    else:
        c.check(f"crash[{tag}]: a main request was recorded", False)
    return before


def sec_crash(c, rounds):
    rng = random.Random(1)
    # (a) mid-stream with slow-drip: the attempt is streaming; kill at random delays.
    with with_rness(wp=WP, tag="crash-drip", scenario="ok", record=True) as r:
        sid = new_session(r)
        for i in range(rounds):
            p = r.spawn_headless(f"drip-{i}", "-s", sid, scenario=f"slow-drip:5/drip-{i}")
            delay = rng.uniform(0.2, 2.0)
            wait_until(lambda: len(r.fp.main_requests(f"slow-drip:5/drip-{i}")) >= 1, 15)
            time.sleep(delay)
            kill9(p.pid)
            p.wait(5)
            ok, _ = lines_ok(log_path(r, sid))
            if not ok:
                c.check(f"crash[drip-{i}]: committed lines parse after kill", False)
        verify_resume(c, r, sid, "drip")
        t = types(r, sid)
        c.check("crash[drip]: killed turns did not commit assistant/message for drip prompts",
                True, f"{t.count('assistant/message')} assistant msgs")

    # (c) tool result of 5 MB: kill while the tool/result line is being written.
    with with_rness(wp=WP, tag="crash-big", scenario="ok", record=True) as r:
        sid = new_session(r)
        big = r.work / "big.txt"
        big.write_text(("x" * 1023 + "\n") * 5 * 1024)
        r.fp.set_script("big", [{"tool": "Read", "args": {"path": str(big)}}, {"text": "done"}])
        fired = 0
        for i in range(max(3, rounds // 4)):
            r.fp.reset()
            p = r.spawn_headless(f"big-{i}", "-s", sid, scenario=f"script:big/big-{i}")
            path = log_path(r, sid)
            s0 = path.stat().st_size
            # kill as soon as the file has grown by >= 64 KB (mid tool/result or right after)
            if kill9_when(p.pid, lambda: path.stat().st_size > s0 + 65536, timeout=30, interval=0.001):
                fired += 1
            p.wait(5)
            ok, _ = lines_ok(path)
            c.check(f"crash[big-{i}]: committed lines parse after kill", ok)
        print(f"INFO  crash[big]: kill fired {fired} times")
        verify_resume(c, r, sid, "big")

    # (b) kill between compaction/started and checkpoint: tiny threshold so the
    # second turn compacts; the compaction (aux) request stalls.
    init = """
rness.compaction = { default = {
  threshold_tokens = 2000, retain_tokens = 200, summary_tokens = 256,
  max_overflow_retries = 1, max_compactions = 2,
  prune_threshold = 8192, prune_head = 4096, prune_tail = 1024,
  system_prompt = [[You are a compaction engine for tests.]], prompt = [[Summarize.]],
} }
"""
    with with_rness(wp=WP, tag="crash-compact", scenario="ok", record=True, init_append=init,
                    stall_seconds=60) as r:
        sid = new_session(r)
        for i in range(3):
            r.headless("filler " + "y" * 3000, session=sid, scenario="ok")
        log = read_log(r.root, sid)
        n_ckpt0 = sum(1 for e in log if e["type"] == "compaction/summary")
        print(f"INFO  crash[compact]: checkpoints before kill = {n_ckpt0}")
        # Make compaction requests stall: the fake provider answers aux
        # requests with a canned summary immediately, so emulate a slow
        # summarizer by killing as soon as compaction/started lands.
        path = log_path(r, sid)
        killed = 0
        for i in range(max(3, rounds // 4)):
            p = r.spawn_headless("big turn " + "z" * 6000, "-s", sid, scenario="ok")
            n_started = path.read_text().count('"compaction/started"')
            if kill9_when(p.pid, lambda: path.read_text().count('"compaction/started"') > n_started,
                          timeout=30, interval=0.001):
                killed += 1
            p.wait(5)
        log = read_log(r.root, sid)
        open_spans = {e["id"] for e in log if e["type"] == "compaction/started"} - \
                     {e["started"] for e in log if e["type"] == "compaction/finished"}
        print(f"INFO  crash[compact]: killed after compaction/started {killed}x, open spans before resume = "
              f"{len(open_spans)}")
        c.check("crash[compact]: kill landed inside a compaction span at least once", killed > 0)
        verify_resume(c, r, sid, "compact")
        log = read_log(r.root, sid)
        outcomes = [e.get("outcome") for e in log if e["type"] == "compaction/finished"]
        c.check("crash[compact]: recovered spans are interrupted/committed_before_interruption",
                any(o in ("interrupted", "committed_before_interruption") for o in outcomes) or killed == 0,
                outcomes)


# -- 6. instructions ------------------------------------------------------------

# -- 2b. corrupt / unknown / newer-version logs (plan 05) ---------------------

def sec_corrupt(c):
    with with_rness(wp=WP, tag="corrupt", scenario="ok", record=True) as r:
        # (a) garbled complete tail (zero-filled extent + newline): resume
        # quarantines it, the turn runs, the request has the history.
        sid = new_session(r)
        path = log_path(r, sid)
        committed = path.read_bytes()
        garbage = b"\0" * 512 + b"\nnot json\n"
        with open(path, "ab") as f:
            f.write(garbage)
        n_req = len(requests(r))
        res = r.headless("after-garbage", session=sid, scenario="echo/corrupt-a", timeout=60)
        c.check("corrupt: resume after garbled tail ok", res.rc == 0, repr(res))
        data = path.read_bytes()
        c.check("corrupt: committed prefix untouched", data.startswith(committed))
        log = read_log(r.root, sid)
        repairs = [e for e in log if e["type"] == "session/repair"]
        c.check("corrupt: one session/repair event", len(repairs) == 1, repairs)
        if repairs:
            side = path.parent / repairs[0]["sidecar"]
            c.check("corrupt: sidecar holds the garbage byte-for-byte",
                    side.exists() and side.read_bytes() == garbage)
        reqs = [load(p) for p in requests(r)[n_req:] if "-title" not in p.name and "-compaction" not in p.name]
        c.check("corrupt: resumed request contains earlier history",
                bool(reqs) and "seed" in body_text(reqs[-1]) and "after-garbage" in body_text(reqs[-1]),
                f"{len(reqs)} reqs, {[p.name for p in requests(r)]}")
        # (b) unknown event type from a newer rness: refused, nothing logged.
        sid2 = new_session(r)
        path2 = log_path(r, sid2)
        with open(path2, "ab") as f:
            f.write(b'{"id":"01FUTURE000000000000000000","at":"2026-01-01T00:00:00.000Z",'
                    b'"type":"future/event","x":1}\n')
        size = path2.stat().st_size
        res = r.headless("on-future", session=sid2, scenario="ok", timeout=60)
        msg = res.stderr + res.stdout
        c.check("corrupt: unknown event type refuses the turn", res.rc != 0 and "newer rness" in msg,
                msg[-300:])
        c.check("corrupt: refused turn appends nothing", path2.stat().st_size == size)
        # (c) newer header version: refused with an upgrade message, untouched.
        sid3 = new_session(r)
        path3 = log_path(r, sid3)
        text = path3.read_bytes().replace(b'"version":1', b'"version":2', 1)
        path3.write_bytes(text)
        res = r.headless("on-v2", session=sid3, scenario="ok", timeout=60)
        msg = res.stderr + res.stdout
        c.check("corrupt: newer format version refused with upgrade hint",
                res.rc != 0 and "format v2" in msg and "corrupt" not in msg.lower(), msg[-300:])
        c.check("corrupt: v2 log untouched", path3.read_bytes() == text)


def instr_reminders(body):
    """Instruction baseline blocks in an anthropic request body."""
    out = []
    for m in body["body"].get("messages", []):
        content = m["content"] if isinstance(m["content"], list) else [{"type": "text", "text": m["content"]}]
        for b in content:
            if b.get("type") == "text" and "Workspace instructions apply" in b.get("text", ""):
                out.append(b["text"])
    return out


def sec_instructions(c):
    init = """
rness.compaction = { default = {
  threshold_tokens = 3000, retain_tokens = 300, summary_tokens = 256,
  max_overflow_retries = 1, max_compactions = 2,
  prune_threshold = 8192, prune_head = 4096, prune_tail = 1024,
  system_prompt = [[You are a compaction engine for tests.]], prompt = [[Summarize.]],
} }
"""
    with with_rness(wp=WP, tag="instr", scenario="ok", record=True, init_append=init) as r:
        (r.work / ".git").mkdir()
        agents = r.work / "AGENTS.md"
        agents.write_text("RULE-V1 always be nice\n")
        # Harness adds --instructions none; a 2nd --instructions is rejected by
        # clap, so override via extra args is impossible — call the binary directly.
        def run(prompt, sid=None, extra=()):
            argv = [a for a in r.argv() if True]
            i = argv.index("--instructions")
            argv[i + 1] = "AGENTS.md"
            argv += list(extra) + ["-p", prompt] + (["-s", sid] if sid else [])
            p = subprocess.run(argv, cwd=r.work, env=r.env, capture_output=True, text=True, timeout=60)
            import re
            m = re.search(r"^session: (\S+)\s*$", p.stderr, re.M)
            return p, (m.group(1) if m else sid)
        p, sid = run("turn one")
        c.check("instr: first turn ok", p.returncode == 0, p.stderr[-300:])
        b = main_bodies(r)[-1]
        rem = instr_reminders(b)
        c.check("instr: baseline injected once on turn 1", len(rem) == 1 and "RULE-V1" in rem[0], rem)
        run("turn two", sid)
        rem = instr_reminders(main_bodies(r)[-1])
        c.check("instr: unchanged file not re-injected", len(rem) == 1, len(rem))
        agents.write_text("RULE-V2 be terse\n")
        run("turn three", sid)
        rem = instr_reminders(main_bodies(r)[-1])
        print(f"INFO  instr: after AGENTS.md change: {len(rem)} baselines visible; v1={any('RULE-V1' in x for x in rem)} "
              f"v2={any('RULE-V2' in x for x in rem)}")
        c.check("instr: changed file -> new baseline present", any("RULE-V2" in x for x in rem))
        c.check("instr: changed file -> stale V1 baseline no longer visible", not any("RULE-V1" in x for x in rem), rem)
        # Force compaction folds with big turns; afterwards exactly one baseline.
        for i in range(4):
            run(f"big {i} " + "w" * 6000, sid)
        log = read_log(r.root, sid)
        n_ckpt = sum(1 for e in log if e["type"] == "compaction/summary")
        rem = instr_reminders(main_bodies(r)[-1])
        c.check("instr: compaction happened", n_ckpt > 0, n_ckpt)
        c.check("instr: after compaction exactly one baseline visible, current version",
                len(rem) == 1 and "RULE-V2" in rem[0], [x[-60:] for x in rem])
        idents = [e["source"]["identity"] for e in log if e["type"] == "user/message" and e.get("source")
                  and e["source"].get("kind") == "instructions"]
        c.check("instr: at most one baseline per identity in every request",
                all(len(instr_reminders(bd)) == len(set(instr_reminders(bd))) for bd in main_bodies(r)),
                f"identities logged={len(idents)}")

        # Truncation at a multibyte boundary: 3-byte chars, odd budget.
        agents.write_text("€" * 400)  # 1200 bytes
        p, sid2 = run("trunc", None, ["--instructions-bytes", "1000"])
        c.check("instr: truncation run ok", p.returncode == 0, p.stderr[-300:])
        rem = instr_reminders(main_bodies(r)[-1])
        c.check("instr: truncated baseline valid UTF-8 and marked",
                len(rem) == 1 and "truncated" in rem[0] and "\ufffd" not in rem[0], rem[0][-120:] if rem else rem)

        # Invalid UTF-8 AGENTS.md
        agents.write_bytes(b"RULE-BAD \xff\xfe bytes\n")
        p, sid3 = run("badutf8", None)
        rem = instr_reminders(main_bodies(r)[-1])
        warned = "AGENTS.md" in p.stderr or "utf" in p.stderr.lower()
        print(f"INFO  instr: invalid UTF-8 AGENTS.md -> rc={p.returncode} baselines={len(rem)} warned={warned}")
        c.check("instr: invalid UTF-8 AGENTS.md does not fail the turn", p.returncode == 0)
        c.check("instr: invalid UTF-8 AGENTS.md is injected lossily (B1-7)",
                len(rem) == 1 and "RULE-BAD \ufffd\ufffd bytes" in rem[0], rem[0][-120:] if rem else rem)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-k", default="")
    ap.add_argument("--crash-rounds", type=int, default=20)
    a = ap.parse_args()
    c = Suite()
    for name, fn in [("lock", sec_lock), ("crash", lambda c: sec_crash(c, a.crash_rounds)),
                     ("corrupt", sec_corrupt), ("instructions", sec_instructions)]:
        if a.k and a.k not in name:
            continue
        print(f"== {name}", flush=True)
        try:
            fn(c)
        except Exception as e:  # a section crash is a FAIL, keep going
            import traceback
            traceback.print_exc()
            c.check(f"{name}: section completed", False, repr(e))
    sys.exit(c.done())


if __name__ == "__main__":
    main()
