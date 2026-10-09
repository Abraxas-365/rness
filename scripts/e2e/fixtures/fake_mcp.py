#!/usr/bin/env python3
"""Tiny adversarial MCP stdio server (WP-5). Standard library only.

    python3 fake_mcp.py [--mode MODE] [--tools N] [--state DIR] [--sleep S]

Modes (behaviour of the server as a whole):
  normal         initialize + tools/list (N tools: echo0..) + tools/call
  never-answer   reads everything, answers nothing (not even initialize)
  garbage        answers initialize, then writes a non-JSON line on tools/list
  huge-frame     answers initialize, then a 17 MiB line (no newline until the end)
  dup-tools      tools/list returns two tools with the same name
  many-tools     tools/list returns --tools N tools (use 4097)
  exit-mid-call  tools/call -> exit(3) without answering
  grandchild     spawns a grandchild (sleep 300) that inherits nothing but lives,
                 ignores SIGTERM; writes pids to --state DIR
  storm          after initialized, emits notifications/tools/list_changed at
                 --rate per second (default 100) for --sleep seconds
  flap           exits right after answering initialize+tools/list once
                 (counts starts in --state/starts)

Per-call methods on tools/call (tool name):
  echo*          returns {"content":[{"type":"text","text": json(args)}]}
  slow           sleeps args.ms (default 2000) then answers
  big            returns args.mb MiB of text
  hang           never answers this call
  crash          exits(4) without answering
Every start appends a line to --state/starts (pid) when --state is given.
"""
import argparse
import json
import os
import signal
import subprocess
import sys
import threading
import time

p = argparse.ArgumentParser()
p.add_argument("--mode", default="normal")
p.add_argument("--tools", type=int, default=2)
p.add_argument("--state")
p.add_argument("--sleep", type=float, default=2.0)
p.add_argument("--rate", type=float, default=100.0)
a = p.parse_args()

out = sys.stdout.buffer
lock = threading.Lock()


def send(obj):
    with lock:
        out.write((json.dumps(obj) + "\n").encode())
        out.flush()


def state(name, text):
    if a.state:
        os.makedirs(a.state, exist_ok=True)
        with open(os.path.join(a.state, name), "a") as f:
            f.write(text + "\n")


state("starts", str(os.getpid()))

if a.mode == "grandchild":
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    gc = subprocess.Popen([sys.executable, "-c",
                           "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(300)"],
                          stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    state("grandchild", str(gc.pid))


def tools_list():
    if a.mode == "dup-tools":
        return [{"name": "same", "inputSchema": {"type": "object"}}] * 2
    n = a.tools
    return [{"name": f"echo{i}", "description": f"echo tool {i}", "inputSchema": {"type": "object"}}
            for i in range(n)] + [{"name": x, "inputSchema": {"type": "object"}} for x in ("slow", "big", "hang", "crash")]


def storm():
    end = time.monotonic() + a.sleep
    gap = 1.0 / a.rate
    while time.monotonic() < end:
        send({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
        time.sleep(gap)


def call(mid, params):
    name = params.get("name", "")
    args = params.get("arguments") or {}
    if name == "hang":
        return
    if name == "crash" or a.mode == "exit-mid-call":
        os._exit(4)
    if name == "slow":
        time.sleep(args.get("ms", 2000) / 1000)
        text = "slow done"
    elif name == "big":
        text = "B" * int(float(args.get("mb", 1)) * 1024 * 1024)
    else:
        text = json.dumps(args)
    send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": text}]}})


listed = 0
for raw in sys.stdin.buffer:
    try:
        m = json.loads(raw)
    except ValueError:
        continue
    method, mid = m.get("method"), m.get("id")
    state("methods", f"{method} {mid}")
    if a.mode == "never-answer":
        continue
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": "2024-11-05", "capabilities": {"tools": {"listChanged": True}},
            "serverInfo": {"name": "fake-mcp", "version": "0"}}})
        continue
    if method == "notifications/initialized":
        if a.mode == "storm":
            threading.Thread(target=storm, daemon=True).start()
        continue
    if method and method.startswith("notifications/"):
        continue
    if method == "tools/list":
        if a.mode == "garbage":
            with lock:
                out.write(b"this is not json {{{\n")
                out.flush()
            continue
        if a.mode == "huge-frame":
            with lock:
                out.write(b'{"jsonrpc":"2.0","id":%d,"result":{"pad":"' % mid)
                chunk = b"x" * (1 << 20)
                for _ in range(17):
                    out.write(chunk)
                out.write(b'"}}\n')
                out.flush()
            continue
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": tools_list()}})
        listed += 1
        if a.mode == "flap":
            time.sleep(0.2)
            os._exit(0)
        continue
    if method == "tools/call":
        threading.Thread(target=call, args=(mid, m.get("params") or {}), daemon=True).start()
        continue
    if method == "ping":
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
        continue
    send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": f"unknown {method}"}})
