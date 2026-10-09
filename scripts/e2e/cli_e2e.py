#!/usr/bin/env python3
"""WP-7: CLI E2E checks against the frozen binary (no credentials, tmp HOME).

    python3 scripts/e2e/cli_e2e.py [-k SUBSTR]

Sections: args (flag matrix), list (600-session --list timing), headless
(exit codes per failure class, stdout cleanliness, SIGINT), control (control
socket: idempotency, stale socket, concurrency, 1 MiB, journal replay),
install (upgrade with modified plugins).
PASS/FAIL/XFAIL per check; exit code = FAIL count. XFAIL = known product bug
(documented in e2e-findings/wp-0.md), XPASS = it now behaves as expected.
"""
import argparse
import concurrent.futures
import json
import os
import signal
import socket
import stat
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from harness import (BIN, REPO, Checks, alive, kill9, start_provider, tmp_dir, wait_until,  # noqa: E402
                     with_rness)
from fixtures.gen_session import generate  # noqa: E402
import perf  # noqa: E402

WP = 0


class Suite(Checks):
    def xcheck(self, name, ok, bug, detail=""):
        self.results.append((name, True, detail))
        print(("XPASS " if ok else "XFAIL ") + name + f"  -- {bug}" + (f" ({str(detail)[:200]})" if detail else ""),
              flush=True)


def short(s, n=300):
    return (s or "")[-n:].replace("\n", " | ")


# -- args ------------------------------------------------------------------------

def section_args(t, fp):
    with with_rness(WP, provider=fp, tag="args") as r:
        def bare(*args, **kw):
            return r.run(*args, provider=False, timeout=30, **kw)

        conflicts = [
            ("--profile with -m", ["-m", "anthropic/x", "--profile", "p", "-p", "hi"]),
            ("--fork with -s", ["--fork", "A", "-s", "B", "-p", "hi"]),
            ("--fork with --list", ["--fork", "A", "--list"]),
            ("--fork with --serve", ["--fork", "A", "--serve", "127.0.0.1:8708"]),
            ("--control-socket with -p", ["--control-socket", "/tmp/x.sock", "-p", "hi"]),
            ("--control-socket with --list", ["--control-socket", "/tmp/x.sock", "--list"]),
            ("--provider without --model", ["--provider", "anthropic", "-p", "hi"]),
            ("--reasoning with --effort", ["-m", "anthropic/x", "--reasoning", "low", "--effort", "high", "-p", "hi"]),
            ("--effort with --budget-tokens", ["-m", "anthropic/x", "--effort", "low", "--budget-tokens", "2048", "-p", "hi"]),
        ]
        for name, args in conflicts:
            res = bare(*args)
            t.check(f"args: {name} rejected (exit 2, usage error)", res.rc == 2 and ("cannot be used" in res.stderr
                    or "required" in res.stderr) and not res.stdout, f"rc={res.rc} {short(res.stderr)}")

        res = bare("-p", "hi")
        t.check("args: -p without -m fails clearly", res.rc != 0 and "model" in res.stderr.lower() and not res.stdout,
                f"rc={res.rc} {short(res.stderr)}")
        for sel in ("anthropic", "anthropic/", "/fake"):
            res = bare("-m", sel, "-p", "hi")
            t.check(f"args: -m {sel!r} rejected", res.rc != 0 and not res.stdout and not res.session,
                    f"rc={res.rc} {short(res.stderr)}")
        res = bare("-m", "nosuchprovider/x", "-p", "hi")
        t.check("args: -m unknown provider rejected", res.rc != 0 and "nosuchprovider" in res.stderr,
                f"rc={res.rc} {short(res.stderr)}")
        res = r.run("-m", "anthropic/fake", "-p", "hi", provider=False, env={"ANTHROPIC_API_KEY": ""})
        t.check("args: provider without credential fails before a session is created",
                res.rc != 0 and not r.sessions(), f"rc={res.rc} sessions={r.sessions()} {short(res.stderr)}")

        for spec, why in (("noeq", "no ="), ("a/b=http://h/v1", "slash in name"), ("x=ftp://h/v1", "non-http"),
                          ("x=http://u:p@h/v1", "credentials in URL"), ("x=http://h/v1?q=1", "query"),
                          ("x=", "empty url"), ("=http://h/v1", "empty name")):
            res = bare("--route", spec, "-m", "x/m", "-p", "hi")
            t.check(f"args: bad --route ({why}) rejected", res.rc != 0 and not res.stdout and "route" in res.stderr.lower(),
                    f"rc={res.rc} {short(res.stderr)}")

        # --root unwritable
        ro = r.base / "ro-root"
        ro.mkdir()
        os.chmod(ro, 0o500)
        try:
            res = r.headless("hi", "--root", str(ro))
            t.check("args: --root unwritable fails non-zero, nothing on stdout", res.rc != 0 and not res.stdout,
                    f"rc={res.rc} {short(res.stderr)}")
            t.check("args: --root unwritable error names the path", str(ro.name) in res.stderr, short(res.stderr))
            res = r.headless("hi", "--root", str(ro / "sub"))
            t.check("args: --root under unwritable parent fails non-zero", res.rc != 0, f"rc={res.rc} {short(res.stderr)}")
        finally:
            os.chmod(ro, 0o700)
        f = r.base / "a-file"
        f.write_text("x")
        res = r.headless("hi", "--root", str(f))
        t.check("args: --root pointing at a file fails non-zero", res.rc != 0, f"rc={res.rc} {short(res.stderr)}")

        # --instructions none vs default discovery
        (r.work / "AGENTS.md").write_text("WP0-INSTRUCTION-MARKER\n")
        res_default = r.run("-p", "hi", scenario="echo/instr")  # argv already carries --instructions none
        argv = [a for a in r.argv("-p", "hi", scenario="echo/instr2") if a not in ("--instructions", "none")]
        p = subprocess.run(argv, cwd=r.work, env=r.env, capture_output=True, text=True, timeout=30)
        reqs = {x["key"]: x for x in fp.requests if x["key"] in ("echo/instr", "echo/instr2") and not x["aux"]}
        log_none = r.log(res_default.session)
        t.check("args: --instructions none injects no instruction file",
                "WP0-INSTRUCTION-MARKER" not in json.dumps(log_none), "marker found in log")
        sid2 = next((l.split()[-1] for l in p.stderr.splitlines() if l.startswith("session:")), None)
        t.check("args: default --instructions picks up AGENTS.md",
                p.returncode == 0 and sid2 and "WP0-INSTRUCTION-MARKER" in json.dumps(r.log(sid2)),
                f"rc={p.returncode} {short(p.stderr)}")
        t.check("args: instruction file grows the request", reqs.get("echo/instr2", {}).get("bytes", 0)
                > reqs.get("echo/instr", {}).get("bytes", 0), {k: v["bytes"] for k, v in reqs.items()})
        (r.work / "AGENTS.md").unlink()

        # --fork
        res = r.run("--fork", "01NOSUCHSESSION0000000000", "-p", "hi")
        t.check("args: --fork of missing session fails, nothing created",
                res.rc != 0 and "fork" in res.stderr.lower() and not res.stdout, f"rc={res.rc} {short(res.stderr)}")
        res = r.run("--fork", "../../etc", "-p", "hi")
        t.check("args: --fork with path traversal rejected", res.rc != 0, f"rc={res.rc} {short(res.stderr)}")
        base = r.headless("seed")
        before = set(r.sessions())
        res = r.run("--fork", base.session, "-p", "child")
        new = set(r.sessions()) - before
        t.check("args: --fork existing session -> new session, 'forked A -> B' on stderr",
                res.rc == 0 and len(new) == 1 and f"forked {base.session} -> " in res.stderr
                and res.session in new, f"rc={res.rc} new={new} {short(res.stderr)}")
        t.check("args: --fork leaves parent log unchanged",
                [e["type"] for e in r.log(base.session)].count("user/message") == 1, "parent got new messages")
        res = r.headless("hi", session="01NOSUCHSESSION0000000000")
        t.check("args: -s missing session fails non-zero", res.rc != 0 and not res.stdout, f"rc={res.rc} {short(res.stderr)}")

        res = r.run("--serve", "0.0.0.0:8708")
        t.check("args: --serve non-loopback without token refused", res.rc != 0 and "TOKEN" in res.stderr.upper(),
                f"rc={res.rc} {short(res.stderr)}")
        t.check("args: refused --serve does not claim to be serving", "serving on" not in res.stderr, short(res.stderr))
        res = bare("--version")
        t.check("args: --version", res.rc == 0 and res.stdout.startswith("rness"), res.stdout)


# -- --list ----------------------------------------------------------------------

def section_list(t, fp):
    with with_rness(WP, provider=fp, tag="list") as r:
        other = r.base / "other-ws"
        other.mkdir()
        t0 = time.perf_counter()
        mine = generate(r.root, r.work, turns=3, tools_per_turn=1, tool_result_bytes=512, sessions=600, seed=11)
        generate(r.root, other, turns=3, sessions=50, seed=12)
        gen_s = time.perf_counter() - t0
        bare = r.run("--list", provider=False)
        t.check("list: --list works without -m", bare.rc == 0 and len(bare.stdout.split()) == 600, short(bare.stderr))
        rep = perf.Reporter("cli-list-600", wp=WP, binary=BIN)
        out = None
        for _ in range(7):
            res = r.run("--list")
            rep.sample("list_600_ms", res.elapsed * 1000)
            out = res
        t.check("list: 600 sessions listed (only this workspace)", out.rc == 0 and set(out.stdout.split()) == set(mine),
                f"rc={out.rc} n={len(out.stdout.split())} {short(out.stderr)}")
        t.check("list: stdout only ids, stderr empty", all(len(l) == 26 for l in out.stdout.split()) and not out.stderr.strip(),
                short(out.stderr))
        # Large-session cost: --list must only read headers.
        big = generate(r.root, r.work, turns=800, tools_per_turn=2, tool_result_bytes=32768, sessions=2, seed=13)
        for _ in range(5):
            res = r.run("--list")
            rep.sample("list_600_plus_2x50MB_ms", res.elapsed * 1000)
        med = rep.summary()
        t.check("list: median < 1 s for 600 sessions", med["list_600_ms"]["median"] < 1000, med)
        t.check("list: two ~50 MB logs add < 50% (header-only reads)",
                med["list_600_plus_2x50MB_ms"]["median"] < med["list_600_ms"]["median"] * 1.5 + 20, med)
        # Empty / missing root
        res = r.run("--list", "--root", str(r.base / "empty-root"))
        t.check("list: missing root -> exit 0, empty output", res.rc == 0 and not res.stdout.strip(),
                f"rc={res.rc} {short(res.stderr)}")
        # A corrupt header must not break listing.
        bad = r.root / "01ZZZZZZZZZZZZZZZZZZZZZZZZ"
        bad.mkdir()
        (bad / "session.v1.jsonl").write_text("{not json\n")
        res = r.run("--list")
        t.check("list: corrupt header skipped, others still listed", res.rc == 0 and len(res.stdout.split()) == 602,
                f"rc={res.rc} n={len(res.stdout.split())} {short(res.stderr)}")
        cmp = perf.compare_baseline("cli-list-600", rep.medians())
        print(perf.format_comparison(cmp))
        print(f"      (generated 650 sessions in {gen_s:.1f}s; medians {json.dumps({k: round(v['median'], 1) for k, v in med.items()})})")
        if cmp["missing"]:
            perf.write_baseline("cli-list-600", rep.medians(), note="WP-0 initial baseline, frozen release binary 22490f1")
        rep.close()


# -- headless exit codes ----------------------------------------------------------

def section_headless(t, fp):
    lua = "rness.providers.set_stream_idle_timeout('anthropic', 1000)"
    with with_rness(WP, provider=fp, tag="headless", init_append=lua) as r:
        res = r.headless("hi", scenario="ok/h")
        t.check("headless: success -> exit 0", res.rc == 0, res)
        t.check("headless: stdout is exactly the transcript",
                res.stdout == "you: hi\nrness: Partial response from fake provider.\n", repr(res.stdout))
        t.check("headless: stderr carries only the session line", res.stderr.strip() == f"session: {res.session}",
                repr(res.stderr))
        fp.set_script("tools", [{"tool": "Bash", "args": {"command": "echo hi", "description": "x"}}, {"text": "done"}])
        res = r.headless("hi", scenario="script:tools/h")
        t.check("headless: tool notices on stderr, not stdout", "[tool Bash" in res.stderr and "[tool" not in res.stdout
                and res.stdout == "you: hi\nrness: done\n", repr(res.stdout))
        classes = [
            ("auth 401", "http-401/h"),
            ("rate limit exhausted (429 x3)", "http-429/h"),
            ("overloaded 529 exhausted", "http-529/h"),
            ("server 500 exhausted", "http-500/h"),
            ("truncated stream exhausted", "cut/h"),
            ("stream inactivity timeout", "stall/h"),
            ("context overflow (nothing to compact)", "context-overflow/h"),
            ("connection refused", None),
        ]
        for name, scen in classes:
            if scen is None:
                argv = [str(BIN), "-m", "anthropic/fake", "--base-url", "http://127.0.0.1:8707/x", "--instructions",
                        "none", "-p", "hi"]
                p = subprocess.run(argv, cwd=r.work, env=r.env, capture_output=True, text=True, timeout=60)
                rc, out, err = p.returncode, p.stdout, p.stderr
                sid = next((l.split()[-1] for l in err.splitlines() if l.startswith("session:")), None)
            else:
                res = r.headless("hi", scenario=scen, timeout=120)
                rc, out, err, sid = res.rc, res.stdout, res.stderr, res.session
            outcome = [e["outcome"] for e in r.log(sid) if e["type"] == "turn/ended"] if sid else None
            t.check(f"headless: {name}: turn recorded as failed", outcome == ["failed"], outcome)
            t.check(f"headless: {name}: exit code 1", rc == 1, f"rc={rc}")
            lines = err.strip().splitlines()
            t.check(f"headless: {name}: 'rness: turn failed' line precedes the final session line",
                    len(lines) >= 2 and lines[-1].startswith("session:") and sid
                    and any(l.startswith("rness: turn failed") for l in lines[-3:-1]), short(err))
            t.check(f"headless: {name}: stdout has no error text", out.strip() == "you: hi", repr(out))
            t.check(f"headless: {name}: error explained on stderr", "attempt: Error" in err, short(err))
        # SIGINT mid-turn
        fp.set_stall_seconds(20)
        p = r.spawn_headless("hi", scenario="stall/sigint")
        wait_until(lambda: fp.main_requests("stall/sigint"), 15)
        time.sleep(0.3)
        t0 = time.monotonic()
        p.send_signal(signal.SIGINT)
        try:
            p.wait(10)
        except subprocess.TimeoutExpired:
            pass
        dt = time.monotonic() - t0
        t.check("headless: SIGINT mid-turn exits within 2 s", p.returncode is not None and dt < 2, f"rc={p.returncode} {dt:.1f}s")
        t.check("headless: SIGINT exit code non-zero", p.returncode not in (0, None), p.returncode)
        sid = r.latest_session()
        log = r.log(sid) if sid else []
        types = [e["type"] for e in log]
        t.check("headless: SIGINT leaves a readable log with the user message", "user/message" in types, types)
        t.check("headless: session after SIGINT can be continued",
                r.headless("again", session=sid, scenario="ok/after-sigint").rc == 0, sid)
        print(f"      (SIGINT: rc={p.returncode} in {dt:.2f}s; log tail {types[-4:]}; "
              f"stdout={Path(p.out_path).read_text()!r})")
        fp.set_stall_seconds(3)


# -- control socket ----------------------------------------------------------------

def sock_send(path, ident, text, session=None, timeout=10, raw=None):
    with socket.socket(socket.AF_UNIX) as c:
        c.settimeout(timeout)
        c.connect(str(path))
        f = c.makefile("rwb")
        hello = json.loads(f.readline())
        line = raw if raw is not None else (json.dumps({"id": ident, "session": session or hello["session"], "text": text}) + "\n").encode()
        f.write(line)
        f.flush()
        reply = f.readline()
        return hello, (json.loads(reply) if reply else None)


def cli_send(r, path, session, ident, text=None, stdin=None):
    argv = [str(r.binary), "send", "--socket", str(path), "--session", session, "--id", ident]
    if text is not None:
        argv.append(text)
    return subprocess.run(argv, input=stdin, capture_output=True, text=True, timeout=30, env=r.env)


def section_control(t, fp):
    if "--control-socket" not in subprocess.run([str(BIN), "--help"], capture_output=True, text=True).stdout:
        return t.skip("control", "binary built without experimental-control")
    fp.set_script("ctl", {"steps": [{"text": "reply {n}", "delay_ms": 300}], "then": "repeat-last"})
    with with_rness(WP, provider=fp, scenario="script:ctl", tag="control") as r:
        sockdir = r.base / "ctl"
        sockdir.mkdir(mode=0o700)
        path = sockdir / "c.sock"
        r.tui("--control-socket", str(path), tag="ctl", wait=None)
        t.check("control: socket created", wait_until(lambda: path.exists(), 20), r.tmux.capture()[-500:])
        hello, rep = sock_send(path, "one", "first message")
        sid = hello["session"]
        t.check("control: handshake + accepted", hello.get("version") == 1 and rep.get("status") == "accepted"
                and rep.get("durable") is True, (hello, rep))
        mode = stat.S_IMODE(os.stat(path).st_mode)
        t.check("control: socket not group/world accessible", mode & 0o077 == 0, oct(mode))
        # idempotency
        cli = cli_send(r, path, sid, "one", "first message")
        t.check("control: CLI resend same id+text -> identical ack", cli.returncode == 0 and json.loads(cli.stdout) == rep,
                f"{cli.returncode} {cli.stdout} {cli.stderr}")
        _, conflict = sock_send(path, "one", "DIFFERENT text")
        t.check("control: same id different text rejected", conflict and "error" in conflict, conflict)
        cli = cli_send(r, path, sid, "stdin-1", stdin="from stdin\n")
        t.check("control: CLI reads text from stdin", cli.returncode == 0 and '"accepted"' in cli.stdout, cli.stderr)
        cli = cli_send(r, path, "01WRONGSESSION00000000000", "x1", "hi")
        t.check("control: wrong --session refused", cli.returncode != 0, f"{cli.returncode} {cli.stdout} {cli.stderr}")
        for bad_id, why in (("", "empty id"), ("x" * 129, "129-byte id")):
            cli = cli_send(r, path, sid, bad_id, "hi")
            t.check(f"control: {why} refused", cli.returncode != 0, f"{cli.returncode} {cli.stdout} {cli.stderr}")
        _, slash = sock_send(path, "slash", "/plan on")
        t.check("control: slash command refused", slash and "error" in slash, slash)
        # two concurrent senders
        with concurrent.futures.ThreadPoolExecutor(4) as ex:
            futs = [ex.submit(cli_send, r, path, sid, f"conc-{i}", f"concurrent {i}") for i in range(4)]
            outs = [f.result() for f in futs]
        t.check("control: 4 concurrent senders all accepted", all(o.returncode == 0 and '"accepted"' in o.stdout for o in outs),
                [(o.returncode, o.stderr[-100:]) for o in outs])
        # 1 MiB boundary
        big_ok = "a" * (1024 * 1024 - 200)
        cli = cli_send(r, path, sid, "big-ok", stdin=big_ok)
        t.check("control: ~1 MiB - 200 B message accepted", cli.returncode == 0 and '"accepted"' in cli.stdout,
                f"{cli.returncode} {cli.stderr[-200:]}")
        cli = cli_send(r, path, sid, "big-no", stdin="a" * (1024 * 1024 + 10))
        t.check("control: > 1 MiB rejected client-side with clear message",
                cli.returncode != 0 and "1 MiB" in cli.stderr, f"{cli.returncode} {cli.stderr[-200:]}")
        try:
            _, rawrep = sock_send(path, None, None, raw=b'{"id":"raw","session":"' + sid.encode() + b'","text":"'
                                  + b"a" * (1024 * 1024 + 100) + b'"}\n')
            t.check("control: raw > 1 MiB line refused by server", rawrep is None or "error" in rawrep, str(rawrep)[:200])
        except OSError as e:
            t.check("control: raw > 1 MiB line refused by server (connection closed)", True, str(e))
        # Everything durable exactly once.
        all_ids = ["one", "stdin-1", "conc-0", "conc-1", "conc-2", "conc-3", "big-ok"]
        logp = r.root / sid / "session.v1.jsonl"
        def all_persisted():
            text = logp.read_text() if logp.exists() else ""
            return all(f'"id":"{i}"' in text for i in all_ids)
        t.check("control: all accepted submissions persisted", wait_until(all_persisted, 60),
                [i for i in all_ids if f'"id":"{i}"' not in logp.read_text()])
        text = logp.read_text()
        t.check("control: each id persisted exactly once", all(text.count(f'"id":"{i}"') == 1 for i in all_ids),
                {i: text.count(f'"id":"{i}"') for i in all_ids})
        # journal replay after kill -9 with queued submissions
        fp.set_script("ctl", {"steps": [{"text": "slow reply", "delay_ms": 3000}], "then": "repeat-last"})
        sock_send(path, "q-busy", "make it busy")
        time.sleep(0.5)
        _, q1 = sock_send(path, "q-1", "queued survives kill")
        pid = r.pid
        kill9(pid)
        wait_until(lambda: not alive(pid), 5)
        t.check("control: kill -9 leaves stale socket file", path.exists(), "socket removed")
        r.tmux.kill()
        fp.set_script("ctl", {"steps": [{"text": "after restart {n}"}], "then": "repeat-last"})
        # restart on the same path
        r.tui("--control-socket", str(path), "-s", sid, tag="ctl2", wait=None)
        time.sleep(3)
        pane = r.tmux.capture(history=200)
        bound = alive(r.pid) and not r.tmux.pane_dead() and "RNESS-EXITED" not in pane
        t.xcheck("control: restart on stale socket path recovers (stale-socket detection)", bound,
                 "kill -9 leaves the socket file and the next start refuses to bind", pane.strip()[-200:])
        if not bound:
            r.tmux.kill()
            path.unlink()
            r.tui("--control-socket", str(path), "-s", sid, tag="ctl3", wait=None)
        t.check("control: restarted TUI binds after manual unlink", wait_until(lambda: path.exists() and alive(r.pid), 20),
                r.tmux.capture()[-300:])
        _, again = sock_send(path, "q-1", "queued survives kill")
        t.check("control: resend after restart dedupes to the same ack", again == q1, (again, q1))
        def replayed():
            tx = logp.read_text()
            return '"id":"q-1"' in tx and "after restart" in tx
        t.check("control: queued submission replayed from journal and answered", wait_until(replayed, 30),
                logp.read_text()[-600:])
        tx = logp.read_text()
        t.check("control: replayed submission persisted once", tx.count('"id":"q-1"') == 1, tx.count('"id":"q-1"'))
        journal = r.root / "control" / "submissions.jsonl"
        t.check("control: journal exists under <root>/control", journal.exists(), list((r.root).iterdir()))


# -- install upgrade ---------------------------------------------------------------

def section_install(t):
    base = tmp_dir(WP, "install")
    try:
        home = base / "home"
        home.mkdir()
        bindir = base / "bin"
        env = {"HOME": str(home), "PATH": os.environ["PATH"]}
        cmd = ["bash", str(REPO / "install.sh"), "--binary", str(BIN), "--bin-dir", str(bindir), "--experimental-control"]
        p1 = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=60)
        t.check("install: fresh install copies flavor", p1.returncode == 0 and (home / ".rness/init.lua").exists(),
                short(p1.stdout + p1.stderr))
        plug_dir = home / ".rness/plugins"
        plugin = sorted(plug_dir.glob("*.lua"))[0] if plug_dir.exists() else None
        if plugin:
            plugin.write_text("-- locally modified / stale copy\n" + plugin.read_text())
        p2 = subprocess.run(cmd + ["--replace-binary"], env=env, capture_output=True, text=True, timeout=60)
        out = p2.stdout + p2.stderr
        t.check("install: upgrade preserves user config", p2.returncode == 0 and "Preserved existing configuration" in out
                and plugin and plugin.read_text().startswith("-- locally modified"), short(out))
        warned = any(w in out.lower() for w in ("differ", "stale", "outdated", "newer", "plugins"))
        t.xcheck("install: upgrade warns that installed flavor plugins differ from the repo", warned,
                 "install.sh never compares ~/.rness/plugins with flavors/default/plugins", short(out))
    finally:
        import shutil
        shutil.rmtree(base, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-k", help="only sections whose name contains this (args,list,headless,control,install)")
    a = ap.parse_args()
    t = Suite()
    fp = start_provider(WP, stall_seconds=3)
    try:
        for name, fn in (("args", section_args), ("list", section_list), ("headless", section_headless),
                         ("control", section_control), ("install", section_install)):
            if a.k and a.k not in name:
                continue
            print(f"== {name}", flush=True)
            try:
                fn(t) if name == "install" else fn(t, fp)
            except Exception as e:  # keep going; a crash in one section is one failure
                import traceback
                traceback.print_exc()
                t.check(f"{name}: section completed without harness error", False, repr(e))
    finally:
        fp.stop()
    return t.done()


if __name__ == "__main__":
    sys.exit(main())
