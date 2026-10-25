#!/usr/bin/env python3
"""E2E: `rness --acp` driven as an ACP client over real stdio.

Fake Anthropic provider scripted to call Bash, then answer. Checks the
handshake, session/new, a prompt with committed updates and a permission
round-trip (allow), session/close, session/list, session/load replay and a
clean exit on stdin EOF.

    RNESS_E2E_BIN=target/debug/rness python3 scripts/e2e/acp_e2e.py
"""
import json
import os
import subprocess
import sys
import threading
import queue
import shutil
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from harness import Checks, make_home, provider_flags, tmp_dir  # noqa: E402
from fake_provider import FakeProvider  # noqa: E402


MCP_SERVER = r'''
import json, os, sys
for line in sys.stdin:
    msg = json.loads(line)
    mid, method = msg.get("id"), msg.get("method")
    def send(result):
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
        sys.stdout.flush()
    if method == "initialize":
        send({"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "e2e", "version": "0"}})
    elif method == "tools/list":
        send({"tools": [{"name": "pwd", "description": "cwd", "inputSchema": {"type": "object"}}]})
    elif method == "tools/call":
        send({"content": [{"type": "text", "text": os.getcwd() + " " + os.environ.get("GREETING", "")}]})
'''


def wait_gone(script_name, timeout=10):
    """True once no process runs `script_name` (the MCP child was stopped)."""
    import time
    deadline = time.time() + timeout
    while time.time() < deadline:
        if subprocess.run(["pgrep", "-f", script_name], capture_output=True).returncode != 0:
            return True
        time.sleep(0.1)
    return False


def http_mcp():
    """Streamable-HTTP MCP server on loopback requiring the e2e bearer token."""
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def reply(self, code, body=None):
            data = json.dumps(body).encode() if body is not None else b""
            self.send_response(code)
            if body is not None:
                self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_DELETE(self):
            self.reply(204)

        def do_POST(self):
            msg = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.headers.get("Authorization") != "Bearer e2e-secret":
                return self.reply(401)
            method = msg.get("method")
            result = {
                "initialize": {"protocolVersion": "2025-03-26", "capabilities": {"tools": {}}},
                "tools/list": {"tools": [{"name": "ping", "inputSchema": {"type": "object"}}]},
                "tools/call": {"content": [{"type": "text", "text": "pong"}]},
            }.get(method)
            if result is None or "id" not in msg:
                return self.reply(202)
            self.reply(200, {"jsonrpc": "2.0", "id": msg["id"], "result": result})

    httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd


class Acp:
    def __init__(self, argv, env, cwd):
        self.proc = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.q = queue.Queue()
        self.raw = []
        threading.Thread(target=self._read, daemon=True).start()
        self.next = 0

    def _read(self):
        for line in self.proc.stdout:
            self.raw.append(line)
            self.q.put(json.loads(line))
        self.q.put(None)

    def send(self, msg):
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def request(self, method, params):
        self.next += 1
        self.send({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params})
        return self.next

    def recv(self, timeout=30):
        msg = self.q.get(timeout=timeout)
        if msg is None:
            raise EOFError(self.proc.stderr.read())
        return msg

    def until(self, rid, on_request=None):
        seen = []
        while True:
            msg = self.recv()
            if msg.get("id") == rid and "method" not in msg:
                return seen, msg
            if "method" in msg and "id" in msg and on_request:
                self.send({"jsonrpc": "2.0", "id": msg["id"], "result": on_request(msg)})
            seen.append(msg)


def main():
    c = Checks()
    base = tmp_dir(0, "acp")
    try:
        run(c, base)
    finally:
        shutil.rmtree(base, ignore_errors=True)
    sys.exit(c.done())


def run(c, base):
    home, work = make_home(base)
    work = work.resolve()
    with FakeProvider(port=0) as fp:
        fp.set_script("acp", [
            {"tool": "Bash", "args": {"command": "echo acp-ran", "description": "probe"}},
            {"text": "listo"},
            {"tool": "mcp__e2e__pwd", "args": {}},
            {"text": "mcp listo"},
            {"tool": "mcp__web__ping", "args": {}},
            {"text": "http listo"},
        ])
        flags, env_extra = provider_flags(fp, "anthropic", "script:acp/t1")
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("ANTHROPIC_", "OPENAI_", "OPENROUTER_", "RNESS_"))}
        env.update(env_extra, HOME=str(home))
        binary = os.environ.get("RNESS_E2E_BIN", str(Path.home() / ".cache/rness-e2e/bin/rness"))
        acp = Acp([binary, "--acp", "--approval", "ask", *flags], env, work)

        _, init = acp.until(acp.request("initialize", {"protocolVersion": 1, "clientCapabilities": {}}))
        c.check("initialize", init.get("result", {}).get("protocolVersion") == 1, init)

        _, new = acp.until(acp.request("session/new", {"cwd": str(work), "mcpServers": []}))
        sid = new.get("result", {}).get("sessionId")
        c.check("session/new", bool(sid), new)
        opts = new["result"].get("configOptions", [])
        c.check("model config option", opts and opts[0]["id"] == "model", opts)

        asked = []

        def allow(msg):
            asked.append(msg)
            return {"outcome": {"outcome": "selected", "optionId": "allow-once"}}

        seen, resp = acp.until(acp.request("session/prompt", {
            "sessionId": sid, "prompt": [{"type": "text", "text": "corre algo"}]}), allow)
        c.check("prompt end_turn", resp.get("result", {}).get("stopReason") == "end_turn", resp)
        c.check("permission asked", len(asked) == 1 and asked[0]["params"]["toolCall"]["kind"] == "execute",
                asked)
        ups = [m["params"]["update"] for m in seen if m.get("method") == "session/update"]
        kinds = [u["sessionUpdate"] for u in ups]
        c.check("tool_call before result", "tool_call" in kinds and "tool_call_update" in kinds
                and kinds.index("tool_call") < kinds.index("tool_call_update"), kinds)
        result = next((u for u in ups if u["sessionUpdate"] == "tool_call_update"), {})
        text = json.dumps(result.get("content"))
        c.check("tool ran", result.get("status") == "completed" and "acp-ran" in text, result)
        c.check("final text", any(u["sessionUpdate"] == "agent_message_chunk"
                                  and u["content"].get("text") == "listo" for u in ups), kinds)

        # A session open on this connection is neither loadable nor listed.
        _, busy = acp.until(acp.request("session/load", {"sessionId": sid, "cwd": str(work),
                                                         "mcpServers": []}))
        c.check("load refuses open session", busy.get("error", {}).get("code") == -32602, busy)
        acp.until(acp.request("session/close", {"sessionId": sid}))

        _, lst = acp.until(acp.request("session/list", {"cwd": str(work)}))
        c.check("list", [s["sessionId"] for s in lst["result"]["sessions"]] == [sid], lst)

        seen, load = acp.until(acp.request("session/load", {"sessionId": sid, "cwd": str(work),
                                                            "mcpServers": []}))
        kinds = [m["params"]["update"]["sessionUpdate"] for m in seen if m.get("method") == "session/update"]
        c.check("load replays", kinds[:1] == ["user_message_chunk"] and "tool_call" in kinds, kinds)

        # Client-attached MCP server (stdio): runs in the session folder with
        # the given env, and only that session sees its tool.
        server = base / "mcp_server.py"
        server.write_text(MCP_SERVER)
        _, bad = acp.until(acp.request("session/new", {"cwd": str(work), "mcpServers": [
            {"name": "e2e", "command": "python3", "args": [str(server)]}]}))
        c.check("mcp relative command refused", bad.get("error", {}).get("code") == -32602, bad)
        _, new = acp.until(acp.request("session/new", {"cwd": str(work), "mcpServers": [
            {"name": "e2e", "command": sys.executable, "args": [str(server)],
             "env": [{"name": "GREETING", "value": "hola"}]}]}))
        msid = new.get("result", {}).get("sessionId")
        c.check("mcp session/new", bool(msid), new)
        seen, resp = acp.until(acp.request("session/prompt", {
            "sessionId": msid, "prompt": [{"type": "text", "text": "usa mcp"}]}), allow)
        ups = [m["params"]["update"] for m in seen if m.get("method") == "session/update"]
        result = next((u for u in ups if u["sessionUpdate"] == "tool_call_update"), {})
        c.check("mcp tool ran in workspace", resp.get("result", {}).get("stopReason") == "end_turn"
                and result.get("status") == "completed"
                and f"{work} hola" in json.dumps(result.get("content")), [resp, result])
        acp.until(acp.request("session/close", {"sessionId": msid}))
        c.check("mcp server stopped on close", wait_gone(server.name), server.name)

        # Client-attached MCP server (streamable HTTP) with an auth header.
        httpd = http_mcp()
        url = f"http://127.0.0.1:{httpd.server_port}/mcp"
        entry = {"type": "http", "name": "web", "url": url,
                 "headers": [{"name": "Authorization", "value": "Bearer e2e-secret"}]}
        _, new = acp.until(acp.request("session/new", {"cwd": str(work), "mcpServers": [entry]}))
        hsid = new.get("result", {}).get("sessionId")
        c.check("http mcp session/new", bool(hsid), new)
        seen, resp = acp.until(acp.request("session/prompt", {
            "sessionId": hsid, "prompt": [{"type": "text", "text": "usa http"}]}), allow)
        ups = [m["params"]["update"] for m in seen if m.get("method") == "session/update"]
        result = next((u for u in ups if u["sessionUpdate"] == "tool_call_update"), {})
        c.check("http mcp tool ran", result.get("status") == "completed"
                and "pong" in json.dumps(result.get("content")), [resp, result])
        insecure = dict(entry, url="http://example.com/mcp")
        _, bad = acp.until(acp.request("session/new", {"cwd": str(work), "mcpServers": [insecure]}))
        c.check("http mcp refuses plain http off loopback", bad.get("error", {}).get("code") == -32602, bad)
        acp.until(acp.request("session/close", {"sessionId": hsid}))
        httpd.shutdown()

        acp.proc.stdin.close()
        try:
            code = acp.proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            acp.proc.kill()
            code = "timeout"
        c.check("clean exit on EOF", code == 0, code)
        bad = [line for line in acp.raw if not line.startswith("{")]
        c.check("stdout is pure JSON-RPC", not bad, bad[:3])


if __name__ == "__main__":
    main()
