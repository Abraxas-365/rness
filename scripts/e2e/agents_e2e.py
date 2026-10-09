#!/usr/bin/env python3
"""WP-6 process-level E2E: subagents, workflows, background jobs.

Runs the frozen binary (~/.cache/rness-e2e/bin/rness) against an in-process
fake provider whose handler (subclassed here, fake_provider.py untouched)
routes on the session's LAST user message containing a `WP6:<tag>` marker,
so parents and children never share a request counter.

    python3 scripts/e2e/agents_e2e.py            # all groups
    python3 scripts/e2e/agents_e2e.py fanout crash failures workflows abuse
    RNESS_E2E_FANOUT=20 python3 scripts/e2e/agents_e2e.py fanout

Exit code = number of failed checks. Perf samples -> perf.Reporter("agents").
"""
import json
import os
import re
import subprocess
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import fake_provider as fpmod  # noqa: E402
from harness import (REPO, Checks, alive, descendants, fd_count, kill9, rss_kb, start_provider,  # noqa: E402
                     wait_until, with_rness)
from perf import Reporter  # noqa: E402

WP = 6
FANOUT = int(os.environ.get("RNESS_E2E_FANOUT", "50"))
RUN = uuid.uuid4().hex[:8]
SLEEP_TAG = f"wp6tag{RUN}"
INIT = """
rness.workflow = {}
rness.agents.declare("worker", {
  description = "Implements a bounded task.", subagent = true,
  tools = { "Glob", "Grep", "Read", "Edit", "Write", "Bash" },
  instructions = "Implement carefully.",
})
"""

# -- prompt-routed fake model ------------------------------------------------

WF_ABUSE = {
    "busy": "while true do end",
    "big": "return 1 --" + "x" * (64 * 1024),
    "items": "local t = {} for i = 1, 4097 do t[i] = function() return i end end return #parallel(t)",
    "cycle": "local t = {} t.self = t return t",
    "deep": "local t = {} local c = t for i = 1, 200 do c.n = {} c = c.n end return t",
    "error": "error('boom from script')",
}


def _texts(content):
    if isinstance(content, str):
        return [content]
    out = []
    for b in content or []:
        if not isinstance(b, dict):
            continue
        if b.get("type") == "text":
            out.append(b.get("text", ""))
    return out


def _tool_result_texts(content):
    out = []
    for b in content if isinstance(content, list) else []:
        if isinstance(b, dict) and b.get("type") == "tool_result":
            c = b.get("content")
            out.extend(_texts(c) if isinstance(c, list) else [c or ""])
    return out


def _workflow_args(name):
    src = (REPO / "examples/workflows" / f"{name}.lua").read_text()
    meta = json.loads(re.search(r"^-- meta: (.*)$", src, re.M).group(1))
    args = json.loads(re.search(r"^-- args: (.*)$", src, re.M).group(1))
    return {"meta": meta, "script": src, "args": args}


def _sub(prompt, mode="fg", provider="spawn", agent=None):
    a = {"provider": provider, "prompt": prompt}
    if mode in ("bg", "cont"):
        a["run_in_background"] = True
    if mode == "cont":
        a["background_mode"] = "continuable"
    if agent:
        a["agent"] = agent
    return ("subagent", a)


def route(tag, step, tools, last_results, last_user, all_results=()):
    """-> dict(text=, calls=[(name,args)], error=status, scenario=, delay=)."""
    parts = tag.split(":") if tag else []
    head = parts[0] if parts else None
    joined = "\n".join(last_results)
    if head in ("bg", "cont", "fg", "mix"):
        n, kind = int(parts[1]), parts[2]
        if step == 0:
            modes = [head] * n if head != "mix" else ["bg"] * n + ["cont"] * n
            return {"calls": [_sub(f"WP6:c:{kind}:{i}", m) for i, m in enumerate(modes)]}
        return {"text": "ack"}
    if head == "c":
        kind, i = parts[1], parts[2]
        if kind == "quick":
            return {"text": f"ans-{i}", "delay": 0.05}
        if kind == "slow":
            return {"text": f"slow-{i}", "delay": 3}
        if kind == "sleep":
            if step == 0:
                return {"calls": [("Bash", {"command": f"sleep 300 && echo {SLEEP_TAG}", "description": "sleep",
                                            "timeout_ms": 600000})]}
            return {"text": "slept"}
        if kind == "fail500":
            return {"error": 500}
        if kind == "overflow":
            return {"scenario": "context-overflow"}
        if kind == "nest":
            return {"calls": [_sub("WP6:c:nest:0")]} if step == 0 else {"text": "nest-done"}
    if head == "msgjob":
        if step == 0:
            return {"calls": [_sub("WP6:c:quick:0", "bg")]}
        if step == 1:
            job = re.search(r"as job (j\S+)", joined)
            return {"calls": [("send_message", {"agent_id": job.group(1) if job else "jnone", "message": "hi"})]}
        return {"text": "ack"}
    if head == "ctl":
        if step == 0:
            return {"calls": [_sub("WP6:c:slow:0", "cont")]}
        everything = "\n".join(all_results)
        agent = re.search(r"agent_id: (\S+?)\)", everything)
        aid = agent.group(1) if agent else "none"
        if step == 1:
            return {"calls": [("send_message", {"agent_id": aid, "message": "steer while running"})], "agent": aid}
        if step == 2:
            return {"calls": [("interrupt_agent", {"agent_id": aid})]}
        return {"text": "ack"}
    if head == "forkfresh":
        return {"calls": [_sub("WP6:c:quick:0", "fg", provider="fork")]} if step == 0 else {"text": "done"}
    if head == "wf":
        return {"calls": [("workflow", _workflow_args(parts[1]))]} if step == 0 else {"text": "done"}
    if head == "wfraw":
        return ({"calls": [("workflow", {"meta": {"name": f"abuse-{parts[1]}", "description": "x"},
                                         "script": WF_ABUSE[parts[1]]})]} if step == 0 else {"text": "done"})
    # Workflow members (no tag): structured output by prompt keyword.
    if "structured_output" in tools:
        if step > 0:
            return {"text": "reported"}
        p = last_user
        if "Report every occurrence" in p or "Verify each finding" in p:
            v = {"findings": [{"file": "a.rs", "line": 3, "why": "unwrap on io"}]}
        elif "Find up to" in p:
            v = {"candidates": [{"claim": f"c{i}", "location": f"x.rs:{i}"} for i in range(3)]}
        elif "DISPROVE" in p:
            v = {"holds": "c1" not in p, "reason": "checked"}
        else:
            v = {"ok": True, "note": "fine"}
        return {"calls": [("structured_output", v)]}
    if step > 0:
        return {"text": "ack"}
    return {"text": "member text answer"}


class WP6Handler(fpmod.Provider):
    def _scenario(self, name, args, n, summary, in_tokens):
        if name != "wp6":
            return super()._scenario(name, args, n, summary, in_tokens)
        msgs = self.body.get("messages", [])
        tag_idx, tag = None, None
        for i, m in enumerate(msgs):
            if m.get("role") != "user":
                continue
            for t in _texts(m.get("content")):
                mt = re.search(r"WP6:(\S+)", t)
                if mt:
                    tag_idx, tag = i, mt.group(1)
        start = tag_idx if tag_idx is not None else 0
        step = sum(1 for m in msgs[start:] if m.get("role") == "assistant")
        all_results = [x for m in msgs if m.get("role") == "user" for x in _tool_result_texts(m.get("content"))]
        last_results = []
        for m in reversed(msgs):
            if m.get("role") == "user" and _tool_result_texts(m.get("content")):
                last_results = _tool_result_texts(m.get("content"))
                break
        last_user = ""
        for m in reversed(msgs):
            if m.get("role") == "user" and "".join(_texts(m.get("content"))).strip():
                last_user = "".join(_texts(m.get("content")))
                break
        plan = route(tag, step, summary["tool_names"], last_results, last_user, all_results)
        if plan.get("delay"):
            time.sleep(plan["delay"])
        if plan.get("error"):
            return self._error(plan["error"], "api_error", "Simulated HTTP 500")
        if plan.get("scenario"):
            nm, *a = plan["scenario"].split(":")
            return super()._scenario(nm, a, n, summary, in_tokens)
        blocks = []
        if plan.get("text"):
            blocks.append(("text", plan["text"]))
        for i, (tool, targs) in enumerate(plan.get("calls", [])):
            blocks.append(("tool", f"toolu_wp6_{time.time_ns()}_{i}", tool, json.dumps(targs)))
        self._emit(fpmod.Plan(blocks or [("text", "")]), in_tokens)


def provider():
    fp = start_provider(WP)
    fp.server.RequestHandlerClass = WP6Handler
    return fp


# -- log helpers ----------------------------------------------------------------

def header(r, sid):
    """First envelope of the session log (carries the delegation stamp)."""
    return r.log(sid)[0]


def delegation(r, sid):
    h = header(r, sid)
    return h.get("delegation") or (h.get("header") or {}).get("delegation")


def children_of(r, parent):
    out = []
    for s in r.sessions():
        try:
            d = delegation(r, s)
        except Exception:
            continue
        if d and d.get("parent") == parent:
            out.append((s, d))
    return out


def user_texts(log):
    out = []
    for e in log:
        if e["type"] == "user/message":
            out.append("".join(p.get("text", "") for p in e.get("content", []) if isinstance(p, dict)))
    return out


def outcomes(log):
    return [e["outcome"] for e in log if e["type"] == "turn/ended"]


def tool_results(log, name=None):
    return [e for e in log if e["type"] == "tool/result" and (name is None or e.get("name") == name)]


def job_notice_counts(log):
    counts = {}
    for t in user_texts(log):
        for m in re.finditer(r"^Job (j\S+?):", t, re.M):
            counts[m.group(1)] = counts.get(m.group(1), 0) + 1
    return counts


def settle_counts(log):
    counts = {}
    for t in user_texts(log):
        for m in re.finditer(r"\[subagent (\S+) settled", t):
            counts[m.group(1)] = counts.get(m.group(1), 0) + 1
    return counts


def phase(r, sid):
    return r.api("GET", f"/api/sessions/{sid}/phase")["phase"]


def send(r, sid, text):
    return r.api("POST", "/api/request", {"type": "send", "session": sid, "intent": "followup",
                                          "content": [{"kind": "text", "text": text}]})


def new_session(r):
    return r.api("POST", "/api/sessions", {"workspace": str(r.work)})["session"]


def sleepers():
    out = subprocess.run(["pgrep", "-f", SLEEP_TAG], capture_output=True, text=True).stdout.split()
    return [int(p) for p in out]


def kill_sleepers():
    for pid in sleepers():
        for c in descendants(pid):
            kill9(c)
        kill9(pid)
    subprocess.run(["pkill", "-9", "-f", SLEEP_TAG], capture_output=True)


# -- groups -------------------------------------------------------------------------

def group_fanout(t, fp, rep):
    for mode in ("bg", "cont"):
        with with_rness(WP, provider=fp, mode="serve", scenario="wp6", allow_generic_agents=True,
                        init_append=INIT, tag=f"fan-{mode}") as r:
            sid = new_session(r)
            rss0 = rss_kb(r.pid)
            t0 = time.perf_counter()
            send(r, sid, f"WP6:{mode}:{FANOUT}:quick")
            lat, peak_rss, peak_fd = [], 0, 0
            counter = job_notice_counts if mode == "bg" else settle_counts

            def done():
                a = time.perf_counter()
                ph = phase(r, sid)
                lat.append((time.perf_counter() - a) * 1000)
                nonlocal peak_rss, peak_fd
                peak_rss = max(peak_rss, rss_kb(r.pid) or 0)
                peak_fd = max(peak_fd, fd_count(r.pid) or 0)
                return ph == "idle" and len(counter(r.log(sid))) >= FANOUT

            ok = wait_until(done, 120, 0.25)
            settle_ms = (time.perf_counter() - t0) * 1000
            time.sleep(3)  # past the 1 s durable poller: catch duplicates
            log = r.log(sid)
            counts = counter(log)
            kids = children_of(r, sid)
            t.check(f"fanout {mode}: {FANOUT} children all noticed", ok and len(counts) == FANOUT,
                    f"notices={len(counts)} kids={len(kids)}")
            t.check(f"fanout {mode}: every notice exactly once", all(c == 1 for c in counts.values()),
                    {k: v for k, v in counts.items() if v != 1})
            t.check(f"fanout {mode}: children are normal sessions in root", len(kids) == FANOUT
                    and all(d.get("depth") == 1 for _, d in kids), len(kids))
            t.check(f"fanout {mode}: all child turns completed",
                    all(outcomes(r.log(k)) == ["completed"] for k, _ in kids),
                    [outcomes(r.log(k)) for k, _ in kids][:3])
            t.check(f"fanout {mode}: parent turns all completed",
                    set(outcomes(log)) <= {"completed"}, outcomes(log))
            t.check(f"fanout {mode}: API stays responsive (max phase GET < 500 ms)", max(lat) < 500,
                    f"max={max(lat):.0f}ms")
            lat.sort()
            rep.sample(f"fanout_{mode}_settle", settle_ms, n=FANOUT)
            rep.sample(f"fanout_{mode}_phase_get_p50", lat[len(lat) // 2], n=FANOUT)
            rep.sample(f"fanout_{mode}_phase_get_max", lat[-1], n=FANOUT)
            rep.sample(f"fanout_{mode}_rss_growth", peak_rss - (rss0 or 0), unit="KiB", n=FANOUT)
            rep.sample(f"fanout_{mode}_peak_fds", peak_fd, unit="count", n=FANOUT)
            print(f"  fanout {mode}: settle {settle_ms:.0f} ms, phase GET p50 {lat[len(lat)//2]:.1f} ms "
                  f"max {lat[-1]:.0f} ms, rss +{peak_rss - (rss0 or 0)} KiB, peak fds {peak_fd}, "
                  f"parent turns {len(outcomes(log))}", flush=True)
            pid = r.pid
        t.check(f"fanout {mode}: no processes left after shutdown", not alive(pid))


def group_crash(t, fp, rep):
    n = 5
    with with_rness(WP, provider=fp, mode="serve", scenario="wp6", allow_generic_agents=True,
                    init_append=INIT, tag="crash") as r:
        sid = new_session(r)
        send(r, sid, f"WP6:mix:{n}:sleep")
        ok = wait_until(lambda: len(sleepers()) >= 2 * n, 60, 0.25)
        t.check(f"crash: {2*n} children running Bash sleep", ok, len(sleepers()))
        tree_before = descendants(r.pid)
        reapers = [p for p in tree_before if "__rness-terminal-reaper" in
                   subprocess.run(["ps", "-o", "command=", "-p", str(p)], capture_output=True, text=True).stdout]
        kids = children_of(r, sid)
        jobs_before = sorted(re.findall(r"as job (j\S+)", json.dumps(r.log(sid))))
        kill9(r.pid)
        wait_until(lambda: not alive(r.pid), 5)
        time.sleep(1)
        orphans = [p for p in sleepers() if alive(p)]
        t.check("crash: Bash children of killed rness are orphaned (observation; expect 0)",
                len(orphans) == 0, f"{len(orphans)} sleep processes survive kill -9 of rness")
        rep.sample("crash_orphan_bash", len(orphans), unit="count")
        t.check("crash: terminal reapers gone", not any(alive(p) for p in reapers), reapers)
        open_turns = [k for k, _ in kids if "turn/ended" not in [e["type"] for e in r.log(k)]]
        print(f"  crash: {len(kids)} children, {len(open_turns)} with an open turn; jobs {len(set(jobs_before))}",
              flush=True)

        # Restart #1 (same HOME): recovery.
        r.serve()
        time.sleep(4)
        log = r.log(sid)
        jn = job_notice_counts(log)
        interrupted = [t_ for t_ in user_texts(log) if "interrupted" in t_]
        t.check("crash: restart delivers one 'interrupted' notice per bg job", len(jn) == n and
                all(v == 1 for v in jn.values()) and len(interrupted) >= 1, jn)
        cont = [k for k, d in kids if d.get("mode") == "continuable"]
        sc = settle_counts(log)
        print(f"  restart1: job notices {jn}, continuable settle notices {len(sc)}/{len(cont)}", flush=True)
        t.check("crash: continuable children with open turns get a settle notice after restart (observation)",
                len(sc) == len(cont), f"{len(sc)} of {len(cont)} — interrupted continuable children never settle")
        t.check("crash: open child turns closed after restart (observation)",
                all("turn/ended" in [e["type"] for e in r.log(k)] for k, _ in kids),
                f"{sum('turn/ended' not in [e['type'] for e in r.log(k)] for k, _ in kids)} children keep an "
                f"unterminated turn")
        st = r.api("POST", "/api/request", {"type": "send", "session": sid, "intent": "followup",
                                            "content": [{"kind": "text", "text": "after crash"}]})
        wait_until(lambda: phase(r, sid) == "idle", 20, 0.2)
        t.check("crash: parent usable after restart", outcomes(r.log(sid))[-1] == "completed", st)
        # A continuable child of the crashed host is still messageable by the parent? (via serve: user send)
        if cont:
            res = r.api("POST", "/api/request", {"type": "send", "session": cont[0], "intent": "followup",
                                                 "content": [{"kind": "text", "text": "WP6:c:quick:9"}]}, raw=True)
            wait_until(lambda: phase(r, cont[0]) == "idle", 20, 0.2)
            t.check("crash: interrupted continuable child accepts a new turn after restart",
                    res[0] == 200 and outcomes(r.log(cont[0]))[-1:] == ["completed"], res)
        r.cleanup()
        r.procs.clear()

        # Restart #2: no duplicate job notices.
        r.serve()
        time.sleep(4)
        jn2 = job_notice_counts(r.log(sid))
        t.check("crash: second restart adds no duplicate job notices", jn2 == jn, (jn, jn2))
        remaining = sleepers()
        t.check("crash: orphan Bash sleeps still alive after restarts (observation; expect reaped)",
                len(remaining) == 0, f"{len(remaining)} still running")
        kill_sleepers()


def group_failures(t, fp, rep):
    with with_rness(WP, provider=fp, scenario="wp6", allow_generic_agents=True, init_append=INIT,
                    tag="fail") as r:
        res = r.headless("WP6:fg:1:fail500", timeout=120)
        log = r.log(res.session)
        sub = tool_results(log, "subagent")
        t.check("fail: child provider 500 forever -> parent gets an error result",
                sub and sub[0].get("is_error") and outcomes(log) == ["completed"], (sub[:1], outcomes(log)))
        kid = children_of(r, res.session)
        t.check("fail: child turn failed", kid and outcomes(r.log(kid[0][0])) == ["failed"],
                kid and outcomes(r.log(kid[0][0])))

        res = r.headless("WP6:fg:1:overflow", timeout=120)
        log = r.log(res.session)
        sub = tool_results(log, "subagent")
        t.check("fail: child context overflow -> parent turn completes, child reported",
                sub and outcomes(log) == ["completed"], (sub[:1], outcomes(log)))
        print(f"  overflow child result: is_error={sub[0].get('is_error') if sub else None} "
              f"out={(sub[0].get('output') or '')[:160] if sub else None!r}", flush=True)
        # B6-5: the failed child's result carries the reason, not just "failed".
        out = (sub[0].get("output") or "") if sub else ""
        t.check("fail: child context overflow result includes the reason (B6-5)",
                "failed:" in out and ("overflow" in out.lower() or "too long" in out.lower()), out[:200])

        res = r.headless("WP6:c:nest:0", timeout=120)
        chain = [res.session]
        while True:
            k = children_of(r, chain[-1])
            if not k:
                break
            chain.append(k[0][0])
        deepest = r.log(chain[-1])
        refused = tool_results(deepest, "subagent")
        t.check("fail: nested chain stops at depth 3 with a clean refusal",
                len(chain) == 4 and refused and refused[0].get("is_error") and "depth" in refused[0]["output"],
                (len(chain), refused[:1]))

        res = r.headless("WP6:msgjob", timeout=60)
        log = r.log(res.session)
        sm = tool_results(log, "send_message")
        t.check("fail: send_message to a job id errors", sm and sm[0].get("is_error")
                and "background job ID" in sm[0]["output"], sm[:1])

        res = r.headless("WP6:forkfresh", timeout=60)
        kid = children_of(r, res.session)
        first = [q for q in fp.main_requests("wp6") if "WP6:c:quick:0" in json.dumps(q["summary"]["last_user"])]
        t.check("fail: fork of a turnless parent degrades to fresh", kid and first and
                first[-1]["summary"]["assistant_messages"] == 0
                and "WP6:forkfresh" not in json.dumps(r.log(kid[0][0])),
                first[-1]["summary"] if first else kid)

    with with_rness(WP, provider=fp, mode="serve", scenario="wp6", allow_generic_agents=True,
                    init_append=INIT, tag="ctl") as r:
        sid = new_session(r)
        send(r, sid, "WP6:ctl")
        ok = wait_until(lambda: len(tool_results(r.log(sid), "interrupt_agent")) == 1
                        and phase(r, sid) == "idle", 30, 0.2)
        log = r.log(sid)
        sm, ia = tool_results(log, "send_message"), tool_results(log, "interrupt_agent")
        t.check("ctl: send_message to running continuable child accepted",
                ok and sm and not sm[0].get("is_error"), sm[:1])
        t.check("ctl: interrupt_agent on running child accepted", ia and not ia[0].get("is_error"), ia[:1])
        kid = children_of(r, sid)
        wait_until(lambda: kid and phase(r, kid[0][0]) == "idle" and phase(r, sid) == "idle", 20, 0.2)
        time.sleep(1.5)
        ko = outcomes(r.log(kid[0][0])) if kid else None
        sc = settle_counts(r.log(sid))
        print(f"  ctl: child outcomes {ko}, settle notices {sc}", flush=True)
        t.check("ctl: interrupted child turn ends cancelled and parent hears each settle once",
                ko and "cancelled" in ko and all(v <= len(ko) for v in sc.values()), (ko, sc))


def group_workflows(t, fp, rep):
    with with_rness(WP, provider=fp, scenario="wp6", allow_generic_agents=True, init_append=INIT,
                    tag="wf") as r:
        for name in ("audit", "adversarial", "migrate", "review"):
            t0 = time.perf_counter()
            res = r.headless(f"WP6:wf:{name}", timeout=180)
            ms = (time.perf_counter() - t0) * 1000
            log = r.log(res.session) if res.session else []
            wf = tool_results(log, "workflow")
            out = wf[0]["output"] if wf else res.stderr[-400:]
            ok = bool(wf) and not wf[0].get("is_error") and "completed" in out.splitlines()[0]
            body = out.split("\n", 1)[1] if ok and "\n" in out else ""
            t.check(f"workflow example {name}: completes end to end", ok, out[:300])
            if name == "audit" and ok:
                v = json.loads(body)
                t.check("workflow audit: confirmed findings for both targets",
                        len(v["confirmed"]) == 2 and v["failed_targets"] == [], v)
            if name == "adversarial" and ok:
                v = json.loads(body)
                t.check("workflow adversarial: skeptic-disproved claim dropped",
                        sorted(c["claim"] for c in v) == ["c0", "c2"] if isinstance(v, list) else v, v)
            if name == "migrate" and ok:
                v = json.loads(body)
                t.check("workflow migrate: both files migrated + build ok",
                        len(v["migrated"]) == 2 and v["build"]["ok"], v)
            kids = children_of(r, res.session) if res.session else []
            rep.sample(f"workflow_example_{name}", ms, members=len(kids))
            print(f"  {name}: {ms:.0f} ms, {len(kids)} members", flush=True)


def group_abuse(t, fp, rep):
    expect = {"busy": ("budget", 4, 15), "big": ("65536", 0, 10), "items": ("4096", 0, 10),
              "cycle": ("cycl", 0, 10), "deep": ("plain json", 0, 10), "error": ("boom", 0, 10)}
    with with_rness(WP, provider=fp, scenario="wp6", allow_generic_agents=True, init_append=INIT,
                    tag="abuse") as r:
        for case, (needle, lo, hi) in expect.items():
            t0 = time.perf_counter()
            res = r.headless(f"WP6:wfraw:{case}", timeout=60)
            s = time.perf_counter() - t0
            log = r.log(res.session) if res.session else []
            wf = tool_results(log, "workflow")
            out = wf[0]["output"] if wf else res.stderr[-300:]
            t.check(f"workflow abuse {case}: error result mentioning '{needle}' in {lo}-{hi}s",
                    wf and wf[0].get("is_error") and needle in out.lower() and lo <= s <= hi
                    and outcomes(log) == ["completed"], f"{s:.1f}s {out[:240]!r}")
            rep.sample(f"workflow_abuse_{case}", s * 1000)
            print(f"  abuse {case}: {s:.2f}s, error message {len(out)} chars", flush=True)


GROUPS = {"fanout": group_fanout, "crash": group_crash, "failures": group_failures,
          "workflows": group_workflows, "abuse": group_abuse}


def main():
    names = sys.argv[1:] or list(GROUPS)
    t = Checks()
    rep = Reporter("agents", wp=WP)
    fp = provider()
    try:
        for name in names:
            print(f"== {name}", flush=True)
            try:
                GROUPS[name](t, fp, rep)
            except Exception as e:  # keep going; report as a failure
                import traceback
                traceback.print_exc()
                t.check(f"{name}: group raised", False, repr(e)[:300])
    finally:
        kill_sleepers()
        fp.stop()
    sys.exit(t.done())


if __name__ == "__main__":
    main()
