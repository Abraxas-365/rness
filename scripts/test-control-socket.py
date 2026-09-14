#!/usr/bin/env python3
"""Native socket/TUI smoke test; local scripted provider, no credentials/model calls."""
import fcntl
import http.server
import json
import os
import pathlib
import pty
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


class Provider(http.server.BaseHTTPRequestHandler):
    calls = 0

    def do_POST(self):
        try:
            self.respond()
        except (BrokenPipeError, ConnectionResetError):
            pass  # Crash-recovery test intentionally kills the request owner.

    def respond(self):
        self.rfile.read(int(self.headers.get("Content-Length", "0")))
        Provider.calls += 1
        time.sleep(0.25)  # Leave time for a followup to queue.
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for delta, reason in [({"role": "assistant", "content": "SOCKET_TEST_REPLY"}, None), ({}, "stop")]:
            data = {"id": "test", "choices": [{"index": 0, "delta": delta, "finish_reason": reason}]}
            self.wfile.write(("data: " + json.dumps(data) + "\n\n").encode())
        self.wfile.write(b"data: [DONE]\n\n")

    def log_message(self, *_):
        pass


provider = http.server.HTTPServer(("127.0.0.1", 0), Provider)
threading.Thread(target=provider.serve_forever, daemon=True).start()
with tempfile.TemporaryDirectory(prefix="rn-", dir="/tmp") as root:
    os.chmod(root, 0o700)
    path = root + "/control.sock"
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))

    def child():
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

    args = [sys.argv[1] if len(sys.argv) > 1 else "target/debug/rness",
            "--control-socket", path, "--root", root + "/sessions",
            "--model", "socket-test/test", "--route",
            f"socket-test=http://127.0.0.1:{provider.server_port}/v1,none", "--instructions", "none"]
    process = subprocess.Popen(args, stdin=slave, stdout=slave, stderr=slave,
                               preexec_fn=child, env={**os.environ, "TERM": "xterm-256color"})
    os.close(slave)
    output = bytearray()

    def drain():
        while process.poll() is None:
            try:
                if select.select([master], [], [], 0.1)[0]:
                    output.extend(os.read(master, 65536))
            except OSError:
                return

    threading.Thread(target=drain, daemon=True).start()

    def wait(predicate):
        deadline = time.time() + 20
        while not predicate() and process.poll() is None and time.time() < deadline:
            time.sleep(0.05)
        assert predicate(), output.decode(errors="replace")[-6000:]

    def send(identity, text):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(5)
            connection.connect(path)
            stream = connection.makefile("rwb")
            hello = json.loads(stream.readline())
            stream.write((json.dumps({"id": identity, "session": hello["session"], "text": text}) + "\n").encode())
            stream.flush()
            return json.loads(stream.readline())

    try:
        wait(lambda: os.path.exists(path))
        reply = send("one", "Socket smoke test\nsecond line")
        assert reply.get("status") == "accepted" and reply.get("durable"), reply
        cli = subprocess.run([args[0], "send", "--socket", path, "--session", reply["session"],
                              "--id", "one"], input="Socket smoke test\nsecond line", text=True, capture_output=True)
        assert cli.returncode == 0, cli.stderr
        assert json.loads(cli.stdout) == reply, "CLI retry must return same durable acknowledgment"
        assert send("one", "Socket smoke test\nsecond line") == reply
        assert send("two", "Queued followup").get("status") == "accepted"
        assert "error" in send("command", "/plan on")
        wait(lambda: Provider.calls == 2 and b"SOCKET_TEST_REPLY" in output)
        time.sleep(0.5)
        assert Provider.calls == 2, "duplicate caused another model request"
        logs = "\n".join(p.read_text(errors="replace") for p in pathlib.Path(root, "sessions").rglob("*.jsonl"))
        assert "Socket smoke test" in logs and "Queued followup" in logs, "prompts not durable"
        # Stop with a durable queued request and resume with a fresh socket path.
        send("three", "Before restart")
        queued = send("four", "Survive restart")
        process.send_signal(signal.SIGSTOP)
        process.kill(); process.wait(timeout=10)
        os.close(master)
        path = root + "/resumed.sock"
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        resumed = args.copy(); resumed[2] = path
        resumed.extend(["--session", reply["session"]])
        process = subprocess.Popen(resumed, stdin=slave, stdout=slave, stderr=slave,
                                   preexec_fn=child, env={**os.environ, "TERM": "xterm-256color"})
        os.close(slave)
        threading.Thread(target=drain, daemon=True).start()
        wait(lambda: os.path.exists(path))
        assert send("four", "Survive restart") == queued, "restart retry must deduplicate"
        def persisted_four():
            history = pathlib.Path(root, "sessions", reply["session"], "session.v1.jsonl").read_text()
            return '"id":"four"' in history
        wait(persisted_four)
        history = pathlib.Path(root, "sessions", reply["session"], "session.v1.jsonl").read_text()
        assert history.count('"id":"four"') == 1
        print("PASS: native TUI, CLI stdin send, durable queue/restart replay, duplicate ID, slash rejection, visible response")
    finally:
        if process.poll() is None:
            process.send_signal(signal.SIGTERM)
        process.wait(timeout=10)
        os.close(master)
        provider.shutdown()
