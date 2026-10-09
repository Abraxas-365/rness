#!/usr/bin/env python3
"""WP-1 P2: inbox races and approvals through `rness --serve` (frozen binary).

    python3 scripts/e2e/session_p2_e2e.py [-k inbox|approvals] [--turns 200]

inbox: fire followup/steer/inject via POST /api/request at ~10 ms intervals
across turn boundaries (`script` scenario with a short per-step delay);
assert every accepted message is in the log exactly once, in submission
order, with the intent the server reported. Then cancel with queued
followups and document what happens to them.
approvals: `--approval ask` + a scripted Bash call; answer twice, unknown
call, cancel with a pending approval, SSE reconnect then resolve.
PASS/FAIL/XFAIL per check; exit code = FAIL count.
"""
import argparse
import json
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from harness import Checks, read_log, wait_until, with_rness  # noqa: E402

WP = 1


class Suite(Checks):
    def xcheck(self, name, ok, bug, detail=""):
        self.results.append((name, True, detail))
        print(("XPASS " if ok else "XFAIL ") + name + f"  -- {bug}" + (f" ({str(detail)[:300]})" if detail else ""),
              flush=True)


def send(r, sid, intent, text):
    return r.api("POST", "/api/request", {"type": "send", "session": sid, "intent": intent,
                                          "content": [{"kind": "text", "text": text}]}, raw=True)


def phase(r, sid):
    st, text = r.api("GET", f"/api/sessions/{sid}/phase", raw=True)
    try:
        return json.loads(text)
    except Exception:
        return text


def user_msgs(r, sid):
    out = []
    for e in read_log(r.root, sid):
        if e["type"] == "user/message":
            t = "".join(p.get("text", "") for p in e["content"] if p.get("kind") == "text")
            out.append((t, e["intent"], e["id"]))
    return out


def idle(r, sid):
    p = phase(r, sid)
    return "idle" in json.dumps(p).lower()


def sec_inbox(c, turns):
    with with_rness(wp=WP, tag="p2-inbox", mode="none", scenario="script:inbox", record=False) as r:
        # Each main request: 2 steps (tool then text), ~40 ms each, so a 10 ms
        # sender hits running, step boundaries, and idle.
        r.fp.set_script("inbox", {"steps": [{"text": "r{n}", "delay_ms": 40}], "then": "repeat-last"})
        r.serve()
        sid = r.api("POST", "/api/sessions", {"workspace": str(r.work)})["session"]
        sent = []  # (text, requested_intent, status)
        intents = ["followup", "steer", "inject", "followup", "steer"]
        errors = []
        for i in range(turns):
            intent = intents[i % len(intents)]
            text = f"M{i:04d}-{intent}"
            st, body = send(r, sid, intent, text)
            if st >= 400:
                errors.append((text, st, body[:120]))
                sent.append((text, intent, f"http-{st}"))
            else:
                sent.append((text, intent, json.loads(body).get("status")))
            time.sleep(0.01)
        ok = wait_until(lambda: idle(r, sid) and len([m for m in user_msgs(r, sid) if m[0].startswith("M")])
                        >= len([s for s in sent if not s[2].startswith("http")]), 120, 0.25)
        msgs = [m for m in user_msgs(r, sid) if m[0].startswith("M")]
        texts = [m[0] for m in msgs]
        accepted = [s[0] for s in sent if not s[2].startswith("http")]
        statuses = {}
        for s in sent:
            statuses[s[2]] = statuses.get(s[2], 0) + 1
        print(f"INFO  inbox: sent={len(sent)} statuses={statuses} logged={len(texts)} http_errors={len(errors)}")
        if errors:
            print(f"INFO  inbox: first errors {errors[:3]}")
        c.check("inbox: settles idle with all accepted messages logged", ok,
                f"logged {len(texts)}/{len(accepted)}")
        c.check("inbox: none lost", set(accepted) <= set(texts), sorted(set(accepted) - set(texts))[:10])
        c.check("inbox: none duplicated", len(texts) == len(set(texts)),
                [t for t in set(texts) if texts.count(t) > 1][:5])
        # Order: within each class (followups among followups, steers+injects among themselves).
        def order(sub):
            seq = [t for t in texts if t in sub]
            want = [t for t in accepted if t in sub]
            return seq == want
        fol = {s[0] for s in sent if s[2] in ("queued", "started") and s[1] == "followup"}
        si = {s[0] for s in sent if s[1] in ("steer", "inject")}
        c.check("inbox: followups logged in submission order", order(fol))
        c.check("inbox: steers/injects logged in submission order", order(si))
        # By design steers/injects jump ahead of queued followups (next step
        # boundary vs end of turn), so only report the global order.
        first_diff = next(((a, b) for a, b in zip(texts, accepted) if a != b), None)
        print(f"INFO  inbox: global order differs from submission (by design, steers first): first={first_diff}")
        # Each turn's request sees the steers committed before it: verify via log
        # that every logged steer/inject is followed (eventually) by an assistant message.
        log = read_log(r.root, sid)
        last_user = max(i for i, e in enumerate(log) if e["type"] == "user/message"
                        and any(p.get("text", "").startswith("M") for p in e["content"]))
        trailing = [e["type"] for e in log[last_user + 1:]]
        tail_text = next((m for m in msgs if m[2] == log[last_user]["id"]), None)
        c.check("inbox: last message was answered or is an idle inject",
                "assistant/message" in trailing or (tail_text and tail_text[1] == "inject"),
                f"tail={tail_text} after={trailing[:6]}")

        # Cancel with queued followups.
        r.fp.set_script("slow", {"steps": [{"text": "slow", "delay_ms": 3000}], "then": "repeat-last"})
        # restart serve not possible to change scenario: use a script whose name is fixed in URL;
        # swap the 'inbox' script contents instead.
        r.fp.set_script("inbox", {"steps": [{"text": "slow", "delay_ms": 3000}], "then": "repeat-last"})
        st0, b0 = send(r, sid, "followup", "C-start")
        time.sleep(0.5)
        queued = [send(r, sid, "followup", f"C-queued-{k}") for k in range(3)]
        st, body = r.api("POST", "/api/request", {"type": "cancel", "session": sid}, raw=True)
        settled = wait_until(lambda: idle(r, sid), 30, 0.2)
        time.sleep(1)
        texts = [m[0] for m in user_msgs(r, sid)]
        q_logged = [t for t in texts if t.startswith("C-queued")]
        print(f"INFO  inbox-cancel: start={st0}/{b0[:60]} queued={[q[1][:40] for q in queued]} cancel={st}/{body[:80]} "
              f"idle={bool(settled)} queued_logged={q_logged}")
        log = read_log(r.root, sid)
        ends = [e.get("outcome") for e in log if e["type"] == "turn/ended"][-3:]
        c.check("inbox-cancel: turn ended cancelled", "cancelled" in ends, ends)
        # Documented: "Cancelled bursts park queued input" (service.rs:1522, :2796).
        # Next send: are the parked followups delivered?
        r.fp.set_script("inbox", {"steps": [{"text": "after", "delay_ms": 10}], "then": "repeat-last"})
        send(r, sid, "followup", "C-after")
        wait_until(lambda: idle(r, sid) and "C-after" in [m[0] for m in user_msgs(r, sid)], 30, 0.2)
        time.sleep(1)
        texts = [m[0] for m in user_msgs(r, sid)]
        q_logged = [t for t in texts if t.startswith("C-queued")]
        print(f"INFO  inbox-cancel: after next send, queued logged={q_logged}, order tail={texts[-5:]}")
        c.check("inbox-cancel: queued followups are not silently lost (logged by the next turn or before)",
                len(q_logged) == 3, q_logged)
        c.xcheck("inbox-cancel: parked followups delivered before a newer followup",
                 texts.index("C-after") > max(texts.index(q) for q in q_logged) if len(q_logged) == 3 else False,
                 "B1-10 parked followups reordered after the next send", texts[-5:])


def sec_approvals(c):
    with with_rness(wp=WP, tag="p2-appr", mode="none", scenario="script:appr", args=["--approval", "ask"]) as r:
        steps = [{"tool": "Bash", "args": {"command": "echo approved-ran", "description": "t"}}, {"text": "done"}]
        r.fp.set_script("appr", {"steps": steps, "then": "ok"})
        r.serve()
        sid = r.api("POST", "/api/sessions", {"workspace": str(r.work)})["session"]

        def pending():
            st, text = r.api("GET", "/api/approvals", raw=True)
            return json.loads(text) if st == 200 else []

        # SSE listener collects approval frames.
        frames = []

        def listen():
            try:
                for ev, data in r.sse(f"/api/events/{sid}", timeout=60):
                    frames.append((ev, data))
            except Exception:
                pass
        t = threading.Thread(target=listen, daemon=True)
        t.start()
        send(r, sid, "followup", "please run")
        p = wait_until(lambda: pending(), 20, 0.1)
        c.check("approvals: pending approval listed", bool(p), p)
        if not p:
            return
        call = p[0]["call"]
        c.check("approvals: approval_requested frame on SSE",
                wait_until(lambda: any("approval" in (ev or "") or "approval_requested" in d for ev, d in frames), 5),
                [ev for ev, _ in frames][:10])
        s1 = r.api("POST", f"/api/approvals/{call}", {"decision": "allowed"}, raw=True)
        s2 = r.api("POST", f"/api/approvals/{call}", {"decision": "rejected"}, raw=True)
        c.check("approvals: first answer resolves", s1[0] == 200, s1)
        c.check("approvals: second answer 404 (one-shot)", s2[0] == 404, s2)
        s3 = r.api("POST", "/api/approvals/nope", {"decision": "allowed"}, raw=True)
        c.check("approvals: unknown call 404", s3[0] == 404, s3)
        wait_until(lambda: idle(r, sid), 20, 0.2)
        log = read_log(r.root, sid)
        res = [e for e in log if e["type"] == "tool/result"]
        c.check("approvals: tool ran once after allow", len(res) == 1 and "approved-ran" in res[0]["output"],
                [x["output"][:80] for x in res])

        # Cancel with a pending approval.
        r.fp.reset()
        send(r, sid, "followup", "run again")
        p = wait_until(lambda: pending(), 20, 0.1)
        c.check("approvals: second pending approval", bool(p))
        r.api("POST", "/api/request", {"type": "cancel", "session": sid}, raw=True)
        settled = wait_until(lambda: idle(r, sid), 20, 0.2)
        c.check("approvals: cancel with pending approval settles idle", bool(settled), phase(r, sid))
        left = pending()
        c.check("approvals: no stale pending approval after cancel", not left, left)
        if p:
            late = r.api("POST", f"/api/approvals/{p[0]['call']}", {"decision": "allowed"}, raw=True)
            c.check("approvals: late answer after cancel -> 404, nothing runs", late[0] == 404, late)
        time.sleep(0.5)
        log = read_log(r.root, sid)
        ran = [e for e in log if e["type"] == "tool/result" and "approved-ran" in e.get("output", "")]
        c.check("approvals: cancelled call did not run", len(ran) == 1, len(ran))
        log_tail = [e["type"] for e in log][-4:]
        print(f"INFO  approvals: after cancel tail={log_tail}")

        # Reconnect: approval requested while no SSE client; new client resolves.
        r.fp.reset()
        frames.clear()
        send(r, sid, "followup", "third")
        p = wait_until(lambda: pending(), 20, 0.1)
        got = []

        def listen2():
            try:
                for ev, data in r.sse(f"/api/events/{sid}", timeout=30):
                    got.append((ev, data))
            except Exception:
                pass
        threading.Thread(target=listen2, daemon=True).start()
        time.sleep(1)
        replayed = any("approval" in (ev or "") or "approval_requested" in d for ev, d in got)
        print(f"INFO  approvals: late SSE subscriber got approval frame replay={replayed}; GET /api/approvals={bool(p)}")
        if p:
            s = r.api("POST", f"/api/approvals/{p[0]['call']}", {"decision": "allowed"}, raw=True)
            c.check("approvals: resolve after SSE reconnect", s[0] == 200, s)
            c.check("approvals: turn completes after reconnect resolve", wait_until(lambda: idle(r, sid), 20, 0.2))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-k", default="")
    ap.add_argument("--turns", type=int, default=200)
    a = ap.parse_args()
    c = Suite()
    for name, fn in [("inbox", lambda c: sec_inbox(c, a.turns)), ("approvals", sec_approvals)]:
        if a.k and a.k not in name:
            continue
        print(f"== {name}", flush=True)
        try:
            fn(c)
        except Exception as e:
            import traceback
            traceback.print_exc()
            c.check(f"{name}: section completed", False, repr(e))
    sys.exit(c.done())


if __name__ == "__main__":
    main()
