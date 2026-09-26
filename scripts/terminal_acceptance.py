#!/usr/bin/env python3
"""Live acceptance for persistent terminals: the release (or debug) binary in
tmux, the default flavor in a throwaway HOME, and a scripted mock model.

    cargo build --release -p rness-cli && python3 scripts/terminal_acceptance.py

Needs tmux, bash and python3 (standard library only). No credentials. Prints
PASS/FAIL per check and exits with the failure count.

The mock model maps each user prompt to one tool call:
    TO            -> terminal_open {name: "dev"}
    TS[ms] <cmd>  -> terminal_send {session_id: "term-1", text: <cmd>, wait_ms: ms}
    SUB <cmd>     -> subagent (spawn) whose child opens a terminal, starts
                     <cmd> there, and finishes while it still runs
and answers a tool result with "TOOL RESULT: <text>".
"""
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REPO = Path(__file__).resolve().parents[1]
TMUX = "rness-terminal-acceptance"


def text_of(msg):
    c = msg.get("content")
    if isinstance(c, list):
        return "".join(p.get("text", "") for p in c if isinstance(p, dict))
    return c or ""


def decide(messages):
    last = messages[-1]
    users = [m for m in messages if m.get("role") == "user"]
    prompt = text_of(users[-1]).strip() if users else ""
    child = re.match(r"CHILD (.*)", prompt, re.S)
    if child:
        # The one-shot child: open a terminal, start the command, finish.
        results = [text_of(m) for m in messages if m.get("role") == "tool"]
        if not results:
            return {"tool": "terminal_open", "args": {"name": "child"}}
        if len(results) == 1:
            term = re.search(r"term-\d+", results[0])
            return {"tool": "terminal_send", "args": {
                "session_id": term.group(0) if term else "term-2",
                "text": child.group(1), "wait_ms": 1000}}
        return {"text": "child done"}
    if last.get("role") == "tool":
        return {"text": "TOOL RESULT: " + text_of(last)[:300].replace("\n", " | ")}
    if prompt == "TO":
        return {"tool": "terminal_open", "args": {"name": "dev"}}
    m = re.match(r"TS(\d*) (.*)", prompt, re.S)
    if m:
        return {"tool": "terminal_send", "args": {
            "session_id": "term-1", "text": m.group(2), "wait_ms": int(m.group(1) or 1500)}}
    sub = re.match(r"SUB (.*)", prompt, re.S)
    if sub:
        return {"tool": "subagent", "args": {"provider": "spawn", "prompt": "CHILD " + sub.group(1)}}
    return {"text": "ok"}


class Mock(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        self.reply_json({"data": [{"id": "test-model", "object": "model"}]})

    def reply_json(self, obj):
        body = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        req = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        d = decide(req.get("messages", []))
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()

        def chunk(delta, finish=None):
            obj = {"id": "c", "object": "chat.completion.chunk", "model": "test-model",
                   "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
            self.wfile.write(f"data: {json.dumps(obj)}\n\n".encode())

        try:
            if "tool" in d:
                chunk({"role": "assistant", "tool_calls": [{"index": 0, "id": f"call_{time.time_ns()}",
                       "type": "function", "function": {"name": d["tool"], "arguments": json.dumps(d["args"])}}]})
                chunk({}, "tool_calls")
            else:
                chunk({"role": "assistant", "content": d["text"]})
                chunk({}, "stop")
            usage = {"id": "c", "object": "chat.completion.chunk", "model": "test-model", "choices": [],
                     "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}
            self.wfile.write(f"data: {json.dumps(usage)}\n\ndata: [DONE]\n\n".encode())
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


def tmux(*args):
    return subprocess.run(["tmux", *args], capture_output=True, text=True).stdout


def pane():
    return tmux("capture-pane", "-t", TMUX, "-p", "-S", "-400")


def keys(*k):
    tmux("send-keys", "-t", TMUX, *k)


def say(text):
    tmux("send-keys", "-t", TMUX, "-l", text)
    time.sleep(0.3)
    keys("Enter")


def wait_for(pattern, seconds=15):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if re.search(pattern, pane()):
            return True
        time.sleep(0.25)
    return False


def pid_of(cmdline):
    # Exact command-line match (pgrep's anchored -f patterns differ by OS).
    out = subprocess.run(["ps", "-axo", "pid=,command="], capture_output=True, text=True).stdout
    for line in out.splitlines():
        pid, _, command = line.strip().partition(" ")
        if command.strip() == cmdline:
            return int(pid)
    return None


def alive(pid):
    if not pid:
        return False
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def main():
    for tool in ("tmux", "bash"):
        if not shutil.which(tool):
            print(f"SKIP  {tool} not found")
            return 0
    binary = next((p for p in (REPO / "target/release/rness", REPO / "target/debug/rness") if p.exists()), None)
    if not binary:
        print("SKIP  build rness first: cargo build --release -p rness-cli")
        return 0

    server = ThreadingHTTPServer(("127.0.0.1", 0), Mock)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    port = server.server_address[1]
    home = Path(tempfile.mkdtemp(prefix="rness-terminal-acceptance-"))
    work = home / "work"
    work.mkdir()
    shutil.copytree(REPO / "flavors/default", home / ".rness")
    # Generic children, so the one-shot subagent gets the terminal tools.
    agents = home / ".rness/lua/agents.lua"
    agents.write_text(agents.read_text().replace(
        "rness.agents.allow_generic = false", "rness.agents.allow_generic = true"))
    # Unique sleep durations so the pids we look up are ours.
    stamp = 700 + os.getpid() % 200
    long_bg, long_fg, quit_fg, child_fg = stamp, stamp + 1, stamp + 2, stamp + 3

    failures = 0

    def check(name, ok):
        nonlocal failures
        print(("PASS  " if ok else "FAIL  ") + name)
        if not ok:
            failures += 1
            print("\n".join(l for l in pane().splitlines() if l.strip())[-2500:])

    tmux("kill-session", "-t", TMUX)
    cmd = (f"HOME={home} {binary} --route mock=http://127.0.0.1:{port}/v1,none -m mock/test-model "
           f"--instructions none --approval allow; echo RNESS-EXITED=$?; sleep 120")
    tmux("new-session", "-d", "-s", TMUX, "-x", "170", "-y", "50", "-c", str(work), cmd)
    try:
        # Startup can take long when macOS fseventsd is slow (plugin watcher).
        if not wait_for("mock/test-model", 150):
            check("rness starts", False)
            return 1
        time.sleep(1)

        say("TO")
        check("terminal_open card", wait_for(r"Terminal open .*term-1 · dev", 20))
        check("statusline counts the terminal", wait_for(r"1 term ·", 10))

        say("TS echo hi-$((6*7)); (exit 4)")
        check("exit code on the card", wait_for(r"exit 4 · \d", 20))
        check("clean output on the card", wait_for(r"│ hi-42 ", 5))
        check("exit code marker for the model", wait_for(r"TOOL RESULT: hi-42 \| \[exit code: 4\]", 5))

        # Input wait from a program (not a shell builtin, which reports "needs
        # more input"): Linux names the wait, macOS reports a quiet command.
        say("TS6000 printf 'name? '; head -n1 | sed 's/^/got-/'")
        check("input wait reported", wait_for(r"waiting for input|still running, no output for", 20))
        say("TS alice")
        check("reply completes the command", wait_for(r"got-alice", 20))

        say(f"TS sleep {long_bg} & echo bgpid=$!; sleep {long_fg}")
        check("long command left running", wait_for(r"still running after 1\.5s", 20))
        check("statusline shows it running", wait_for(r"1 term \(1 running\)", 10))
        bg = pid_of(f"sleep {long_bg}")
        fg = pid_of(f"sleep {long_fg}")
        check("processes found", bool(bg and fg))

        say("/terminals list")
        check("/terminals list", wait_for(rf"term-1 dev \[bash, controlled\] running .*sleep {long_fg}", 10))

        say("/terminals")
        check("monitor lists terminals", wait_for(r"Terminals \(1\)", 10))
        keys("Enter")
        check("monitor tails output", wait_for(r"FOLLOW", 10) and "bgpid=" in pane())
        keys("Escape")
        time.sleep(0.3)
        keys("Escape")
        time.sleep(0.5)

        say("/terminals stop term-1")
        check("/terminals stop", wait_for(r"Stopping the command in term-1", 10))
        time.sleep(2)
        check("stop ends the foreground command", not alive(fg))
        check("stop keeps the & job", alive(bg))
        check("statusline back to idle", wait_for(r"1 term ·", 10))

        # A one-shot subagent's terminals close when it finishes.
        say(f"SUB sleep {child_fg}")
        seen_child, end = None, time.monotonic() + 30
        while time.monotonic() < end and not re.search(r"TOOL RESULT: .*child done", pane()):
            seen_child = seen_child or pid_of(f"sleep {child_fg}")
            time.sleep(0.1)
        check("subagent ran its command", bool(seen_child) and "child done" in pane())
        end = time.monotonic() + 5
        while alive(seen_child) and time.monotonic() < end:
            time.sleep(0.2)
        check("subagent's terminal closed with it", seen_child and not alive(seen_child))
        check("parent's terminal kept", wait_for(r"1 term ·", 5))

        # Cancel stops waiting but leaves the command running.
        say(f"TS60000 sleep {quit_fg}")
        check("send is waiting", wait_for(r"1 term \(1 running\)", 20))
        time.sleep(1)
        keys("C-c")
        check("cancel returns", wait_for(r"cancelled", 10))
        survivor = pid_of(f"sleep {quit_fg}")
        check("cancel leaves the command running", alive(survivor))

        # Another session still sees, and can stop, this session's terminal.
        keys("C-s")
        time.sleep(0.8)
        keys("f")
        check("switched to a forked session", wait_for(r"\+1 term elsewhere \(1 running\)", 15))
        say("/terminals list")
        check("/terminals list shows other sessions",
              wait_for(rf"In other sessions \(still running\):.*\n.*term-1 dev .*sleep {quit_fg} \(session", 10))
        say("/terminals stop term-1")
        check("stop from another session", wait_for(r"Stopping the command in term-1", 10))
        end = time.monotonic() + 5
        while alive(survivor) and time.monotonic() < end:
            time.sleep(0.2)
        check("it ended the command", not alive(survivor))
        say(f"TS60000 sleep {quit_fg}")
        check("this session's model can't reach it",
              wait_for(r"TOOL RESULT: .*term-1.* belongs to another session", 15))
        # Back to the first session (the picker lists newest first).
        keys("C-s")
        time.sleep(0.8)
        keys("j")
        time.sleep(0.3)
        keys("Enter")
        check("switched back", wait_for(r"1 term ·", 15))
        say(f"TS60000 sleep {quit_fg}")
        check("send is waiting again", wait_for(r"1 term \(1 running\)", 20))
        time.sleep(1)
        keys("C-c")
        wait_for(r"cancelled", 10)
        survivor = pid_of(f"sleep {quit_fg}")

        # First quit asks; the second exits and ends everything.
        time.sleep(1)
        keys("C-d")
        check("quit prompt", wait_for(r"Quit again within 5s", 10))
        check("first quit keeps rness running", "RNESS-EXITED" not in pane())
        keys("C-d")
        check("second quit exits cleanly", wait_for(r"RNESS-EXITED=0", 15))
        time.sleep(1)
        check("no foreground command survives", not alive(survivor))
        check("no & job survives", not alive(bg))
    finally:
        tmux("kill-session", "-t", TMUX)
        server.shutdown()
        for n in (long_bg, long_fg, quit_fg, child_fg):
            pid = pid_of(f"sleep {n}")
            if pid:
                os.kill(pid, signal.SIGKILL)
        shutil.rmtree(home, ignore_errors=True)
    print(f"failures: {failures}")
    return failures


if __name__ == "__main__":
    sys.exit(main())
