#!/usr/bin/env python3
"""Live acceptance for terminal cleanup when rness is ended by a signal.

    cargo build --release -p rness-cli && python3 scripts/terminal_signal_acceptance.py

For each of KILL, TERM, HUP and INT: start rness (tmux, default flavor,
throwaway HOME, the scripted mock model from terminal_acceptance.py), have a
terminal run a foreground command, an `&` job and a `nohup` job, send rness
the signal, and check that nothing survives, that the exit status names the
signal, and (for catchable signals) that the user's terminal settings were
restored. Needs tmux, bash and python3. Exits with the failure count.
"""
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from http.server import ThreadingHTTPServer

sys.path.insert(0, str(Path(__file__).resolve().parent))
import terminal_acceptance as A  # noqa: E402


def rness_pid(binary):
    out = subprocess.run(["ps", "-axo", "pid=,command="], capture_output=True, text=True).stdout
    for line in out.splitlines():
        pid, _, command = line.strip().partition(" ")
        if command.startswith(str(binary)) and "__rness-terminal-reaper" not in command:
            return int(pid)
    return None


def run(binary, port, sig, check):
    name = signal.Signals(sig).name
    home = Path(tempfile.mkdtemp(prefix="rness-signal-acceptance-"))
    (home / "work").mkdir()
    shutil.copytree(A.REPO / "flavors/default", home / ".rness")
    base = 600 + (os.getpid() + sig * 13) % 90
    sleeps = {"foreground": base, "& job": base + 100, "nohup job": base + 200}
    A.tmux("kill-session", "-t", A.TMUX)
    cmd = (f"stty -g > {home}/before; HOME={home} {binary} --route mock=http://127.0.0.1:{port}/v1,none "
           f"-m mock/test-model --instructions none --approval allow; echo $? > {home}/status; "
           f"stty -g > {home}/after; sleep 120")
    A.tmux("new-session", "-d", "-s", A.TMUX, "-x", "170", "-y", "50", "-c", str(home / "work"), cmd)
    pids = {}
    try:
        if not A.wait_for("mock/test-model", 150):
            check(f"{name}: rness starts", False)
            return
        time.sleep(1)
        A.say("TO")
        A.wait_for(r"term-1 · dev", 20)
        fg, bg, noh = sleeps.values()
        A.say(f"TS sleep {bg} & nohup sleep {noh} >/dev/null 2>&1 & sleep {fg}")
        A.wait_for(r"still running", 20)
        pids = {what: A.pid_of(f"sleep {n}") for what, n in sleeps.items()}
        check(f"{name}: processes found", all(pids.values()))
        pid = rness_pid(binary)
        os.kill(pid, sig)
        end = time.monotonic() + 15
        while A.alive(pid) and time.monotonic() < end:
            time.sleep(0.05)
        check(f"{name}: rness exits", not A.alive(pid))
        end = time.monotonic() + 5
        while any(A.alive(p) for p in pids.values()) and time.monotonic() < end:
            time.sleep(0.1)
        survivors = [what for what, p in pids.items() if A.alive(p)]
        check(f"{name}: nothing survives {survivors or ''}".rstrip(), not survivors)
        status = (home / "status").read_text().strip() if (home / "status").exists() else "?"
        check(f"{name}: exit status {128 + sig}", status == str(128 + sig))
        if sig != signal.SIGKILL:
            restored = (home / "after").exists() and \
                (home / "before").read_text() == (home / "after").read_text()
            check(f"{name}: terminal settings restored", restored)
    finally:
        A.tmux("kill-session", "-t", A.TMUX)
        for n in sleeps.values():
            p = A.pid_of(f"sleep {n}")
            if p:
                os.kill(p, signal.SIGKILL)
        shutil.rmtree(home, ignore_errors=True)


def main():
    for tool in ("tmux", "bash"):
        if not shutil.which(tool):
            print(f"SKIP  {tool} not found")
            return 0
    binary = next((p for p in (A.REPO / "target/release/rness", A.REPO / "target/debug/rness")
                   if p.exists()), None)
    if not binary:
        print("SKIP  build rness first: cargo build --release -p rness-cli")
        return 0
    server = ThreadingHTTPServer(("127.0.0.1", 0), A.Mock)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    failures = 0

    def check(name, ok):
        nonlocal failures
        print(("PASS  " if ok else "FAIL  ") + name)
        failures += 0 if ok else 1

    try:
        for sig in (signal.SIGKILL, signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
            run(binary, server.server_address[1], sig, check)
    finally:
        server.shutdown()
    print(f"failures: {failures}")
    return failures


if __name__ == "__main__":
    sys.exit(main())
