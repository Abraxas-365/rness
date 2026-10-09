#!/usr/bin/env python3
"""WP-5 HTTP/SSE server E2E against the frozen binary (stdlib only).

    python3 scripts/e2e/server_e2e.py [--perf] [--only g1,g2]

Groups: auth, upload, sse50, lag, approvals, cancel, concurrent, (perf: fanout).
PASS/FAIL/XFAIL per check; exit code = number of FAILs.
"""
import argparse
import http.client
import json
import os
import socket
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from harness import Checks, alive, fd_count, rss_kb, wait_until, with_rness  # noqa: E402
from wp5_fake import start_wp5_provider  # noqa: E402

WP = 5
TOKEN = "t" * 40


class Suite(Checks):
    def xcheck(self, name, ok, bug, detail=""):
        self.results.append((name, True, detail))
        print(("XPASS " if ok else "XFAIL ") + name + f"  -- {bug}" + (f" ({detail})" if detail else ""), flush=True)


def make_png():
    import struct
    import zlib

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    ihdr = struct.pack(">IIBBBBB", 1, 1, 8, 6, 0, 0, 0)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(b"\0\0\0\0\0"))
            + chunk(b"IEND", b""))


def req(addr, method, path, body=None, headers=None, timeout=30):
    host, port = addr.split(":")
    c = http.client.HTTPConnection(host, int(port), timeout=timeout)
    data = body if isinstance(body, (bytes, type(None))) else json.dumps(body).encode()
    h = dict(headers or {})
    if data is not None and "Content-Type" not in h:
        h["Content-Type"] = "application/json"
    c.request(method, path, body=data, headers=h)
    r = c.getresponse()
    text = r.read()
    c.close()
    return r.status, text


def send(r, sid, text, intent="followup"):
    return r.api("POST", "/api/request", {"type": "send", "session": sid, "intent": intent,
                                          "content": [{"kind": "text", "text": text}]})


def new_session(r):
    return r.api("POST", "/api/sessions", {"workspace": str(r.work)})["session"]


def idle(r, sid, timeout=30):
    return wait_until(lambda: r.api("GET", f"/api/sessions/{sid}/phase")["phase"] == "idle", timeout, 0.05)


class SseClient(threading.Thread):
    """Raw-socket SSE reader. `slow` = seconds to sleep per frame (lagging client);
    `nodrain` = never read after connecting (TCP back-pressure)."""

    def __init__(self, addr, path="/api/events", slow=0.0, nodrain=False, rcvbuf=None):
        super().__init__(daemon=True)
        self.addr, self.path, self.slow, self.nodrain, self.rcvbuf = addr, path, slow, nodrain, rcvbuf
        self.frames, self.ready, self.stop, self.error = [], threading.Event(), threading.Event(), None
        self.drain = threading.Event()
        self.first_at = None
        self.sock = None

    def run(self):
        host, port = self.addr.split(":")
        try:
            s = socket.socket()
            if self.rcvbuf:
                s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, self.rcvbuf)
            s.connect((host, int(port)))
            self.sock = s
            s.sendall(f"GET {self.path} HTTP/1.1\r\nHost: {self.addr}\r\nAccept: text/event-stream\r\n\r\n".encode())
            f = s.makefile("rb")
            while True:  # headers
                line = f.readline()
                if line in (b"\r\n", b"\n", b""):
                    break
            self.ready.set()
            if self.nodrain:
                while not self.drain.wait(0.1):
                    if self.stop.is_set():
                        return
            s.settimeout(1.0)
            data = []
            while not self.stop.is_set():
                try:
                    line = f.readline()
                except (socket.timeout, TimeoutError):
                    continue
                if not line:
                    break
                line = line.rstrip(b"\r\n")
                if not line:
                    if data:
                        self.frames.append((time.monotonic(), b"\n".join(data)))
                        self.first_at = self.first_at or time.monotonic()
                        data = []
                        if self.slow:
                            time.sleep(self.slow)
                elif line.startswith(b"data:"):
                    data.append(line[5:].lstrip())
        except Exception as e:  # noqa: BLE001
            self.error = e
            self.ready.set()

    def close(self):
        self.stop.set()
        if self.sock:
            try:
                self.sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            self.sock.close()

    def parsed(self):
        out = []
        for _, d in self.frames:
            try:
                out.append(json.loads(d))
            except ValueError:
                pass
        return out


# -- groups --------------------------------------------------------------------

def g_auth(t, fp):
    with with_rness(WP, provider=fp, mode="serve", tag="srv-auth") as r:
        a = r.serve_addr
        st, _ = req(a, "GET", "/api/sessions")
        t.check("no token, loopback: 200", st == 200, st)
        st, _ = req(a, "GET", "/api/sessions", headers={"Origin": "http://evil.example"})
        t.check("Origin header -> 403", st == 403, st)
        st, _ = req(a, "GET", "/api/sessions", headers={"Sec-Fetch-Site": "cross-site"})
        t.check("Sec-Fetch-Site: cross-site -> 403", st == 403, st)
        st, _ = req(a, "GET", "/api/sessions", headers={"Sec-Fetch-Site": "none"})
        t.check("Sec-Fetch-Site: none -> 200", st == 200, st)
        st, _ = req(a, "GET", "/api/sessions", headers={"Host": "evil.example:80"})
        t.check("DNS rebinding Host: evil.example -> 403", st == 403, st)
        st, _ = req(a, "GET", "/api/sessions", headers={"Host": "localhost:1"})
        t.check("Host: localhost -> 200", st == 200, st)
        st, _ = req(a, "POST", "/api/request", body={"type": "cancel", "session": "x"}, headers={"Origin": "null"})
        t.check("Origin: null on mutation -> 403", st == 403, st)
    # Non-loopback without token must refuse to start.
    with with_rness(WP, provider=fp, mode="none", tag="srv-auth2") as r:
        port = 8758
        res = r.run("--serve", f"0.0.0.0:{port}", timeout=20)
        t.check("0.0.0.0 without RNESS_SERVER_TOKEN refused", res.rc != 0 and "RNESS_SERVER_TOKEN" in res.stderr,
                f"rc={res.rc} {res.stderr[-200:]}")
        res = r.run("--serve", f"0.0.0.0:{port}", timeout=20, env={"RNESS_SERVER_TOKEN": "short"})
        t.check("short token refused", res.rc != 0, f"rc={res.rc}")
    # Token mode (loopback + token).
    with with_rness(WP, provider=fp, mode="none", tag="srv-auth3", env={"RNESS_SERVER_TOKEN": TOKEN}) as r:
        r.env["RNESS_SERVER_TOKEN"] = TOKEN
        r.serve()
        a = r.serve_addr
        st, _ = req(a, "GET", "/api/sessions")
        t.check("token mode: no Authorization -> 401", st == 401, st)
        st, _ = req(a, "GET", "/api/sessions", headers={"Authorization": f"Bearer {TOKEN}"})
        t.check("token mode: correct bearer -> 200", st == 200, st)
        st, _ = req(a, "GET", "/api/sessions", headers={"Authorization": f"Bearer {TOKEN}x"})
        t.check("token mode: token+suffix -> 401", st == 401, st)
        st, _ = req(a, "GET", "/api/sessions", headers={"Authorization": f"Bearer {TOKEN[:-1]}"})
        t.check("token mode: truncated token -> 401", st == 401, st)
        st, _ = req(a, "GET", "/api/sessions", headers={"Authorization": f"bearer {TOKEN}"})
        t.check("token mode: lowercase scheme -> 401 (strict)", st == 401, st)
        st, _ = req(a, "GET", "/api/sessions", headers={"Authorization": f"Bearer {TOKEN}", "Host": "evil.example"})
        t.check("token mode: foreign Host allowed with token (by design)", st == 200, st)
        # Timing: compare is constant-time in code; measure first-byte-wrong vs last-byte-wrong.
        def timeit(tok, n=300):
            ts = []
            for _ in range(n):
                t0 = time.perf_counter()
                req(a, "GET", "/api/sessions/none/phase", headers={"Authorization": f"Bearer {tok}"})
                ts.append(time.perf_counter() - t0)
            ts.sort()
            return ts[n // 2] * 1e6
        first = timeit("X" + TOKEN[1:])
        last = timeit(TOKEN[:-1] + "X")
        print(f"      token compare median us: first-byte-wrong={first:.0f} last-byte-wrong={last:.0f}")
        t.check("token compare: no measurable early-exit (|diff| < 15%)", abs(first - last) / max(first, last) < 0.15,
                f"{first:.0f} vs {last:.0f}")


def g_upload(t, fp):
    from harness import tmp_dir  # noqa: F401
    with with_rness(WP, provider=fp, mode="serve", tag="srv-up") as r:
        sid = new_session(r)
        a = r.serve_addr
        # A valid tiny PNG.
        png = make_png()
        st, body = req(a, "POST", f"/api/sessions/{sid}/images", body=png, headers={"Content-Type": "image/png"})
        t.check("upload tiny png -> 201", st == 201, f"{st} {body[:200]}")
        limit = 20 * 1024 * 1024
        pad = png + b"\0" * (limit - len(png))
        st, body = req(a, "POST", f"/api/sessions/{sid}/images", body=pad, headers={"Content-Type": "image/png"}, timeout=60)
        t.check("upload exactly 20 MiB -> not 413", st != 413, f"{st} {body[:200]}")
        st, body = req(a, "POST", f"/api/sessions/{sid}/images", body=pad + b"\0", headers={"Content-Type": "image/png"}, timeout=60)
        t.check("upload 20 MiB + 1 -> 413", st == 413, f"{st} {body[:200]}")
        st, body = req(a, "POST", f"/api/sessions/{sid}/images", body=b"x" * 100, headers={"Content-Type": "text/html"})
        t.check("upload non-image -> 400", st == 400, f"{st} {body[:100]}")
        st, body = req(a, "POST", "/api/sessions/../../etc/images", body=png, headers={"Content-Type": "image/png"})
        t.check("upload with traversal session id -> 4xx", 400 <= st < 500, f"{st} {body[:100]}")
        st, body = req(a, "POST", "/api/sessions/NOPE/images", body=png, headers={"Content-Type": "image/png"})
        t.check("upload to unknown session -> 4xx", 400 <= st < 500, f"{st} {body[:100]}")
        # Other JSON endpoints: default axum limit is 2 MiB.
        big = {"type": "send", "session": sid, "intent": "inject", "content": [{"kind": "text", "text": "x" * (3 * 1024 * 1024)}]}
        try:
            st, body = req(a, "POST", "/api/request", body=big, timeout=60)
        except ConnectionResetError:
            st, body = "reset", b""
        t.check("POST /api/request 3 MiB rejected (413 or reset; axum default 2 MiB)", st in (413, "reset"), f"{st} {body[:120]}")
        st, _ = req(a, "GET", f"/api/sessions/{sid}/phase")
        t.check("server healthy after oversized body", st == 200, st)
        st, _ = req(a, "GET", "/api/sessions/../../../etc/passwd")
        t.check("history with traversal id -> 4xx", 400 <= st < 500, st)
        rss = rss_kb(r.pid)
        print(f"      serve RSS after uploads: {rss / 1024:.0f} MiB")


def g_sse50(t, fp, n=50):
    with with_rness(WP, provider=fp, mode="serve", tag="srv-sse", scenario="many-deltas:200:8") as r:
        sid = new_session(r)
        fds0 = fd_count(r.pid)
        clients = [SseClient(r.serve_addr) for _ in range(n)]
        [c.start() for c in clients]
        t.check(f"{n} SSE clients connected", all(c.ready.wait(10) for c in clients) and not any(c.error for c in clients),
                [repr(c.error) for c in clients if c.error][:3])
        time.sleep(0.3)
        t0 = time.monotonic()
        send(r, sid, "go")
        done = idle(r, sid, 60)
        time.sleep(0.5)
        counts = [len(c.parsed()) for c in clients]
        deltas = [sum(1 for f in c.parsed() if f.get("type") == "delta") for c in clients]
        t.check("turn completed", done)
        t.check(f"all {n} clients got the 200 deltas", min(deltas) == 200, f"min={min(deltas)} max={max(deltas)}")
        t.check("all clients got turn_idle", all(any(f.get("type") == "turn_idle" for f in c.parsed()) for c in clients))
        lat = sorted(c.frames[-1][0] - t0 for c in clients if c.frames)
        print(f"      frames/client min={min(counts)} max={max(counts)}; last frame at p50={lat[len(lat)//2]*1000:.0f}ms max={lat[-1]*1000:.0f}ms")
        fds1 = fd_count(r.pid)
        for c in clients:
            c.close()
        time.sleep(1.0)
        # Server notices closed SSE only when it next writes: trigger a turn.
        send(r, sid, "again")
        idle(r, sid, 30)
        time.sleep(0.5)
        fds2 = fd_count(r.pid)
        print(f"      fds: before={fds0} with-clients={fds1} after-close={fds2}")
        t.check("SSE sockets released after clients close", fds2 is None or fds2 <= fds0 + 5, f"{fds0}->{fds1}->{fds2}")


def g_lag(t, fp):
    # N deltas >> broadcast capacity 1024 + socket buffers: a non-reading
    # client must lose frames (not stall the turn or others), then reconcile.
    n = int(os.environ.get("RNESS_BENCH_LAG_DELTAS", "20000"))
    with with_rness(WP, provider=fp, mode="serve", tag="srv-lag", scenario=f"many-deltas:{n}:64") as r:
        sid = new_session(r)
        fast = SseClient(r.serve_addr)
        stuck = SseClient(r.serve_addr, nodrain=True, rcvbuf=4096)
        for c in (fast, stuck):
            c.start()
            c.ready.wait(10)
        time.sleep(0.3)
        t0 = time.monotonic()
        send(r, sid, "go")
        done = idle(r, sid, 120)
        turn_s = time.monotonic() - t0
        t.check("turn completes despite a non-reading SSE client", done, f"{turn_s:.1f}s")
        wait_until(lambda: any(f.get("type") == "turn_idle" for f in fast.parsed()), 30, 0.2)
        fd = sum(1 for f in fast.parsed() if f.get("type") == "delta")
        t.check(f"fast client got all {n} deltas", fd == n, fd)
        stuck.drain.set()
        got_idle = wait_until(lambda: any(f.get("type") == "turn_idle" for f in stuck.parsed()), 30, 0.2)
        sd = sum(1 for f in stuck.parsed() if f.get("type") == "delta")
        print(f"      turn {turn_s:.2f}s; fast deltas={fd}; stuck-then-drained deltas={sd} (lost {n - sd})")
        t.check("stuck client lost frames (lagged) and still received turn_idle", sd < n and got_idle, f"{sd} idle={got_idle}")
        types = {f.get("type") for f in stuck.parsed()}
        t.xcheck("stuck client is told it lagged (resync hint)", "lagged" in types or "resync" in types,
                 "B5-9 Lagged is swallowed (sse.rs:27,58): no event tells the client to reconcile", f"lost={n - sd}")
        hist = r.api("GET", f"/api/sessions/{sid}")["envelopes"]
        text = "".join(c.get("text", "") for e in hist if e["type"] == "assistant/message" for c in e["content"])
        t.check("reconcile: history holds the full text", len(text) == n * 64, len(text))
        fast.close()
        stuck.close()


def g_approvals(t, fp):
    fp.set_script("wp5-approve", {"steps": [{"tool": "Bash", "args": {"command": "echo approved-ran", "description": "x"}},
                                            {"text": "after tool"},
                                            {"tool": "Bash", "args": {"command": "echo second", "description": "x"}},
                                            {"text": "after second"}], "then": "ok"})
    with with_rness(WP, provider=fp, mode="none", tag="srv-appr", scenario="script:wp5-approve") as r:
        r.serve("--approval", "ask")
        sid = new_session(r)
        send(r, sid, "run it")
        pend = wait_until(lambda: r.api("GET", "/api/approvals"), 20, 0.1)
        t.check("late client sees pending approval via GET /api/approvals", pend and pend[0]["tool"] == "Bash", pend)
        call = pend[0]["call"] if pend else "x"
        sse = SseClient(r.serve_addr, f"/api/events/{sid}")
        sse.start()
        sse.ready.wait(5)
        st, body = req(r.serve_addr, "POST", f"/api/approvals/{call}", body={"decision": "allowed"})
        t.check("approve -> 200", st == 200, f"{st} {body}")
        st, _ = req(r.serve_addr, "POST", f"/api/approvals/{call}", body={"decision": "allowed"})
        t.check("second answer -> 404", st == 404, st)
        t.check("turn completes after approval", idle(r, sid, 30))
        hist = r.api("GET", f"/api/sessions/{sid}")["envelopes"]
        out = json.dumps([e for e in hist if e["type"] == "tool/result"])
        t.check("approved tool actually ran", "approved-ran" in out, out[:200])
        st, body = req(r.serve_addr, "POST", "/api/approvals/nope", body={"decision": "maybe"})
        t.check("bad decision value -> 4xx", 400 <= st < 500, st)
        # Cancel while an approval is pending -> withdrawn.
        send(r, sid, "again")
        pend = wait_until(lambda: r.api("GET", "/api/approvals"), 20, 0.1)
        r.api("POST", "/api/request", {"type": "cancel", "session": sid})
        gone = wait_until(lambda: r.api("GET", "/api/approvals") == [], 10, 0.1)
        t.check("cancel withdraws the pending approval", pend and gone, pend)
        time.sleep(0.3)
        t.check("approval_resolved frame sent on withdrawal",
                any(f.get("type") == "approval_resolved" for f in sse.parsed()), [f.get("type") for f in sse.parsed()][-5:])
        sse.close()


def g_cancel(t, fp):
    fp.set_script("wp5-sleep", {"steps": [{"tool": "Bash", "args": {"command": "sleep 30; echo slept", "description": "x"}},
                                          {"text": "done"}], "then": "ok"})
    with with_rness(WP, provider=fp, mode="serve", tag="srv-cancel", scenario="script:wp5-sleep") as r:
        sid = new_session(r)
        send(r, sid, "sleep")
        started = wait_until(lambda: any(e["type"] == "tool/started" for e in r.api("GET", f"/api/sessions/{sid}")["envelopes"])
                             or r.api("GET", f"/api/sessions/{sid}/phase")["phase"] == "running" and time.sleep(1.0) is None, 20, 0.2)
        def sleeps():
            out = os.popen(f"pgrep -f 'sleep 30; echo slept'").read().split()
            return [int(x) for x in out]
        wait_until(lambda: sleeps(), 10, 0.1)
        before = sleeps()
        t0 = time.monotonic()
        r.api("POST", "/api/request", {"type": "cancel", "session": sid})
        ok = idle(r, sid, 10)
        took = time.monotonic() - t0
        t.check("cancel via API while tool runs: idle < 5s", ok and took < 5, f"{took:.2f}s")
        time.sleep(0.5)
        left = [p for p in before if alive(p)]
        t.check("cancelled tool process tree is gone", not left, left)
        for p in left:
            os.kill(p, 9)
        hist = r.api("GET", f"/api/sessions/{sid}")["envelopes"]
        ended = [e["outcome"] for e in hist if e["type"] == "turn/ended"]
        t.check("turn/ended = cancelled", ended[-1:] == ["cancelled"], ended)
        _ = started


def g_concurrent(t, fp):
    with with_rness(WP, provider=fp, mode="serve", tag="srv-conc", scenario="slow-drip:2") as r:
        sid = new_session(r)
        results, errs = [], []

        def one(i):
            try:
                results.append(send(r, sid, f"msg {i}").get("status"))
            except Exception as e:  # noqa: BLE001
                errs.append(repr(e))
        ths = [threading.Thread(target=one, args=(i,)) for i in range(20)]
        [th.start() for th in ths]
        [th.join() for th in ths]
        busy = sum("session is busy" in e for e in errs)
        print(f"      20 concurrent sends: ok={len(results)} busy-500={busy} other-errors={len(errs) - busy}")
        t.xcheck("20 concurrent sends: none rejected", not errs,
                 "B5-10 concurrent sends race on operation.try_lock -> 500 'session is busy' instead of queueing",
                 f"busy={busy} other={[e for e in errs if 'busy' not in e][:1]}")
        t.check("rejections are busy-500 only (no other errors)", len(errs) == busy, errs[:2])
        t.check("at most one 'started'", results.count("started") == 1, {s: results.count(s) for s in set(results)})
        done = wait_until(lambda: (r.api("GET", f"/api/sessions/{sid}/phase")["phase"] == "idle"
                                   and len([e for e in r.api("GET", f"/api/sessions/{sid}")["envelopes"]
                                            if e["type"] == "turn/ended"]) >= 1), 120, 0.3)
        hist = r.api("GET", f"/api/sessions/{sid}")["envelopes"]
        users = [e for e in hist if e["type"] == "user/message"]
        texts = sorted(json.dumps(e).split("msg ")[1].split('"')[0] for e in users if "msg " in json.dumps(e))
        t.check("every accepted message durable exactly once", len(texts) == len(results) and len(set(texts)) == len(texts),
                f"{len(texts)} unique={len(set(texts))} accepted={len(results)}")
        ended = [e for e in hist if e["type"] == "turn/ended"]
        print(f"      turns run for 20 queued sends: {len(ended)} (queued messages are batched)")
        t.check("session idle at the end", done)
        # Concurrent create + send on many sessions.
        sids = [new_session(r) for _ in range(10)]
        res2 = []
        ths = [threading.Thread(target=lambda s=s: res2.append(send(r, s, "x").get("status"))) for s in sids]
        [th.start() for th in ths]
        [th.join() for th in ths]
        t.check("10 sessions started in parallel", res2.count("started") == 10, res2)
        t.check("all 10 finish", all(idle(r, s, 60) for s in sids))


def g_fanout_perf(t, fp):
    from perf import Reporter
    rep = Reporter("wp5_sse_fanout", wp=WP, echo=True)
    n_deltas = int(os.environ.get("RNESS_BENCH_SSE_DELTAS", "1000"))
    for n in (1, 10, 50):
        with with_rness(WP, provider=fp, mode="serve", tag=f"srv-fan{n}", scenario=f"many-deltas:{n_deltas}:16") as r:
            sid = new_session(r)
            clients = [SseClient(r.serve_addr) for _ in range(n)]
            [c.start() for c in clients]
            [c.ready.wait(10) for c in clients]
            time.sleep(0.3)
            t0 = time.monotonic()
            send(r, sid, "go")
            wait_until(lambda: all(any(b'"turn_idle"' in d for _, d in c.frames[-3:]) for c in clients), 120, 0.02)
            wall = time.monotonic() - t0
            total = sum(sum(1 for f in c.parsed() if f.get("type") == "delta") for c in clients)
            rep.sample(f"clients{n}_frames_per_s", total / wall, unit="frames/s", clients=n, deltas=n_deltas)
            rep.sample(f"clients{n}_wall_ms", wall * 1000, clients=n)
            rep.sample(f"clients{n}_lost_frames", n * n_deltas - total, unit="frames", clients=n)
            rep.sample(f"clients{n}_rss_mb", (rss_kb(r.pid) or 0) / 1024, unit="MiB", clients=n)
            for c in clients:
                c.close()
    rep.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--perf", action="store_true")
    ap.add_argument("--only")
    a = ap.parse_args()
    t = Suite()
    fp = start_wp5_provider(WP)
    groups = {"auth": g_auth, "upload": g_upload, "sse50": g_sse50, "lag": g_lag, "approvals": g_approvals,
              "cancel": g_cancel, "concurrent": g_concurrent}
    if a.perf:
        groups["fanout"] = g_fanout_perf
    try:
        for name, fn in groups.items():
            if a.only and name not in a.only.split(","):
                continue
            print(f"== {name}", flush=True)
            try:
                fn(t, fp)
            except Exception as e:  # noqa: BLE001
                import traceback
                traceback.print_exc()
                t.check(f"{name}: group ran", False, repr(e))
    finally:
        fp.stop()
    sys.exit(t.done())


if __name__ == "__main__":
    main()
