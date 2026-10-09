#!/usr/bin/env python3
"""Local model-provider fault injector (Anthropic, OpenAI chat and ChatGPT
Responses wire shapes). Standard library only; binds to 127.0.0.1; no
credentials are checked. Backwards compatible with the original
Anthropic-only simulator (same scenarios, same CLI flags, same default text).

URL layout: http://127.0.0.1:PORT/<scenario>[/<free-form>...]<shape suffix>
  - <scenario> is `name` or `name:arg1:arg2` (see --help for the list).
  - The whole path prefix before the shape suffix is the *counter key*:
    stateful scenarios (recover, retry-after-*, http-NNN:K, script, ...)
    count requests per key, so `/recover/t17` gets its own counter.
  - Shape suffix (auto-detected unless --shape forces one):
      anthropic  POST <prefix>/v1/messages      rness: --base-url http://H:P/<scenario>
      openai     POST <prefix>/chat/completions rness: --route n=http://H:P/<scenario>/v1,none
      responses  POST <prefix>/codex/responses  rness: chatgpt route + --base-url http://H:P/<scenario>

Title and compaction requests ("aux", detected from their system prompt) are
answered with a fixed title / checkpoint summary and do not advance the
counters, so a scenario only sees the main turn's requests (--no-aux
disables this).

CLI:    python3 scripts/fake_provider.py --port 8700 [--record DIR] [--script-dir DIR]
Module: sys.path.insert(0, "scripts"); from fake_provider import FakeProvider
        with FakeProvider() as fp: fp.port, fp.anthropic_url("echo"), fp.requests
"""
import argparse
import email.utils
import json
import os
import re
import socket
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SCENARIOS = {
    "ok": "complete text response (also the default for unknown names)",
    "plan": "exit_plan_mode tool call first, text once a tool result is present",
    "recover": "first request on the key returns 401, later ones succeed",
    "heartbeat": "partial text, 2 s of SSE comments every 100 ms, then success",
    "cut": "partial text, then the stream closes without a terminal event",
    "disconnect": "connection closed before response headers",
    "stall": "partial text then silence for --stall-seconds",
    "stall-headers": "silence for --stall-seconds before any response header, then success",
    "malformed": "invalid JSON in the first SSE event",
    "stream-error": "partial text then an in-stream error event",
    "http-NNN[:K]": "HTTP NNN error (529 = overloaded_error); with K only the first K requests fail",
    "retry-after-seconds[:S[:K]]": "first K (1) requests: 429 + `Retry-After: S` (1), then success",
    "retry-after-date[:S[:K]]": "first K (1) requests: 429 + Retry-After HTTP-date S (3) s ahead, then success",
    "overloaded": "partial text then in-stream overloaded_error",
    "context-overflow[:K]": "HTTP 400 'prompt is too long' / context_length_exceeded; with K only the first K",
    "huge-tool-json[:MB[:TOOL]]": "tool call to TOOL (X) with MB (4) MiB of partial_json args; text after the result",
    "slow-drip[:MS]": "whole response body written 1 byte every MS (50) ms",
    "split-utf8": "multi-byte UTF-8 characters split across TCP writes",
    "dup-index": "two content blocks with the same index (OpenAI: two tool_call deltas, index 0, distinct ids)",
    "unknown-event": "unknown SSE events / ping / unknown delta types interleaved; text still completes",
    "tool-loop[:N[:TOOL]]": "tool call to TOOL (X) until N (3) tool results follow the last prompt, then end_turn",
    "echo": "reply `ECHO {json}` with request stats (messages, bytes, tools, tool_results, last_user, ...)",
    "script[:NAME]": "playbook --script-dir/NAME.json or FakeProvider.set_script(NAME, steps)",
}

PARTIAL = "Partial response from fake provider."
TITLE_TEXT = "Fake title"
SUMMARY_TEXT = ("## Primary Request and Intent\n- fake compaction summary\n\n## Key Technical Concepts\n- (none)\n\n"
                "## Files and Code\n- (none)\n\n## Errors and Fixes\n- (none)\n\n## Pending Jobs\n- (none)\n\n"
                "## Current Work\n- (none)\n\n## Next Step\n- (none)\n\n## Critical Context\n- (none)")
SPLIT_TEXT = "split-utf8 ok: héllo wörld 日本語 🎉 ünïcödé ✓"
SHAPE_SUFFIXES = (("anthropic", "/v1/messages"), ("openai", "/chat/completions"), ("responses", "/codex/responses"))


# -- request normalisation ------------------------------------------------

def _text_blocks(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(b.get("text", "") for b in content
                       if isinstance(b, dict) and b.get("type") in ("text", "input_text", "output_text"))
    return ""


def summarize(shape, body):
    """Shape-independent view of a request: what the model would see."""
    tools, system, msgs = [], "", []  # msgs: (role, text, n_tool_results, n_tool_calls)
    if shape == "anthropic":
        s = body.get("system")
        system = "".join(b.get("text", "") for b in s if isinstance(b, dict)) if isinstance(s, list) else (s or "")
        tools = [t.get("name") for t in body.get("tools") or []]
        for m in body.get("messages", []):
            c = m.get("content")
            blocks = c if isinstance(c, list) else []
            msgs.append((m.get("role"), _text_blocks(c),
                         sum(1 for b in blocks if isinstance(b, dict) and b.get("type") == "tool_result"),
                         sum(1 for b in blocks if isinstance(b, dict) and b.get("type") == "tool_use")))
    elif shape == "openai":
        tools = [t.get("function", {}).get("name") for t in body.get("tools") or []]
        for m in body.get("messages", []):
            role = m.get("role")
            if role == "system":
                system += _text_blocks(m.get("content"))
            elif role == "tool":
                msgs.append(("user", "", 1, 0))
            else:
                msgs.append((role, _text_blocks(m.get("content")), 0, len(m.get("tool_calls") or [])))
    else:
        system = body.get("instructions") or ""
        tools = [t.get("name") for t in body.get("tools") or []]
        for item in body.get("input", []):
            kind = item.get("type")
            if kind == "message":
                msgs.append((item.get("role"), _text_blocks(item.get("content")), 0, 0))
            elif kind == "function_call_output":
                msgs.append(("user", "", 1, 0))
            elif kind == "function_call":
                msgs.append(("assistant", "", 0, 1))
    last_user, trailing = "", 0
    for role, text, nres, _ in reversed(msgs):
        if role == "user" and nres == 0 and text:
            last_user = text
            break
        trailing += nres
    return {
        "shape": shape,
        "model": body.get("model"),
        "messages": len(msgs),
        "user_messages": sum(1 for r, t, n, _ in msgs if r == "user" and n == 0),
        "assistant_messages": sum(1 for r, *_ in msgs if r == "assistant"),
        "tool_results": sum(m[2] for m in msgs),
        "tool_calls": sum(m[3] for m in msgs),
        "trailing_tool_results": trailing,
        "tools": len(tools),
        "tool_names": sorted(t for t in tools if t),
        "system_bytes": len(system.encode()),
        "last_user": last_user[:200],
        "system": system,
    }


def aux_kind(summary):
    s = summary["system"]
    if "concise title" in s:
        return "title"
    if "compaction engine" in s:
        return "compaction"
    return None


# -- response plans --------------------------------------------------------

class Plan:
    """Ordered blocks + an optional fault fired after the first text delta."""

    def __init__(self, blocks=None, stop=None, fault=None):
        self.blocks = blocks or []  # ("text", s) | ("thinking", s) | ("tool", id, name, args_json)
        self.stop = stop or ("tool_use" if any(b[0] == "tool" for b in self.blocks) else "end_turn")
        self.fault = fault  # None | heartbeat | stall | cut | stream-error | overloaded
        self.dup_index = False
        self.unknown_events = False
        self.json_chunk = 65536


def text_plan(text):
    return Plan([("text", text)])


def tool_plan(name, args, call_id=None, text=None):
    blocks = [("text", text)] if text else []
    blocks.append(("tool", call_id or f"toolu_fake_{time.time_ns()}", name,
                   args if isinstance(args, str) else json.dumps(args)))
    return Plan(blocks)


def default_tool_args(name, i=0):
    if name == "Bash":
        return {"command": f"echo tool-loop-{i}", "description": "fake tool loop"}
    if name == "Read":
        return {"path": "/dev/null"}
    return {}


# -- server ----------------------------------------------------------------

class _Server(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, addr, opts):
        super().__init__(addr, Provider)
        self.opts = opts
        self.lock = threading.Lock()
        self.counters = {}
        self.requests = []
        self.scripts = {}
        self.seq = 0

    def bump(self, key):
        with self.lock:
            self.counters[key] = self.counters.get(key, 0) + 1
            return self.counters[key]


class Provider(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"  # SSE bodies end at connection close

    def log_message(self, *a):
        if self.server.opts.get("verbose"):
            sys.stderr.write("fake_provider: " + (a[0] % a[1:]) + "\n")

    def do_GET(self):  # OpenAI-compatible model discovery
        self._json(200, {"object": "list", "data": [{"id": "fake", "object": "model"}]})

    def _json(self, status, obj, headers=None):
        payload = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(payload)

    def _error(self, status, kind, message, code=None, headers=None):
        if self.shape == "anthropic":
            body = {"type": "error", "error": {"type": kind, "message": message}}
        else:
            body = {"error": {"type": kind, "message": message, "code": code or kind}}
        self._json(status, body, headers)

    def _write(self, data):
        if self.write_mode == "drip":
            for i in range(len(data)):
                self.wfile.write(data[i:i + 1])
                time.sleep(self.drip_s)
            return
        if self.write_mode == "split":  # cut inside every multi-byte character
            start = 0
            for i in range(1, len(data)):
                if 0x80 <= data[i] < 0xC0 and data[i - 1] >= 0xC0:
                    self.wfile.write(data[start:i])
                    time.sleep(0.02)
                    start = i
            self.wfile.write(data[start:])
            return
        self.wfile.write(data)

    def _ev(self, kind, data):
        payload = data if isinstance(data, str) else json.dumps(data, ensure_ascii=False)
        if self.shape == "openai":
            self._write(f"data: {payload}\n\n".encode())
        else:
            self._write(f"event: {kind}\ndata: {payload}\n\n".encode())

    def _start_stream(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

    def _fault(self, plan):
        """Run the plan's mid-stream fault. True = the stream must end now."""
        f = plan.fault
        if f == "heartbeat":
            for _ in range(20):
                self._write(b": keepalive\n\n")
                time.sleep(0.1)
            return False
        if f == "stall":
            time.sleep(self.server.opts["stall_seconds"])
            return True
        if f == "cut":
            return True
        if f in ("stream-error", "overloaded"):
            msg = "Simulated overloaded stream" if f == "stream-error" else "Overloaded"
            if self.shape == "anthropic":
                self._ev("error", {"type": "error", "error": {"type": "overloaded_error", "message": msg}})
            elif self.shape == "openai":
                self._ev("", {"error": {"type": "overloaded_error", "message": msg, "code": "overloaded"}})
            else:
                self._ev("response.failed", {"type": "response.failed", "response": {
                    "status": "failed", "error": {"code": "server_is_overloaded", "message": msg}}})
            return True
        return False

    def _emit(self, plan, in_tokens):
        self._start_stream()
        getattr(self, "_emit_" + self.shape)(plan, in_tokens)

    @staticmethod
    def _pieces(s, size):
        return [s[i:i + size] for i in range(0, len(s), size)] or [""]

    def _emit_anthropic(self, plan, in_tokens):
        ev = self._ev
        ev("message_start", {"type": "message_start", "message": {
            "id": "msg_fake", "type": "message", "role": "assistant", "model": self.body.get("model"),
            "content": [], "usage": {"input_tokens": in_tokens, "output_tokens": 1}}})
        first = True
        for i, block in enumerate(plan.blocks):
            index = 0 if plan.dup_index else i
            if plan.unknown_events:
                ev("ping", {"type": "ping"})
                ev("future_event", {"type": "future_event", "payload": {"x": 1}})
            if block[0] == "text":
                ev("content_block_start", {"type": "content_block_start", "index": index,
                                           "content_block": {"type": "text", "text": ""}})
                if block[1]:
                    ev("content_block_delta", {"type": "content_block_delta", "index": index,
                                               "delta": {"type": "text_delta", "text": block[1]}})
                    if plan.unknown_events:
                        ev("content_block_delta", {"type": "content_block_delta", "index": index,
                                                   "delta": {"type": "future_delta", "x": 1}})
                if first:
                    first = False
                    if self._fault(plan):
                        return
            elif block[0] == "thinking":
                ev("content_block_start", {"type": "content_block_start", "index": index,
                                           "content_block": {"type": "thinking", "thinking": ""}})
                ev("content_block_delta", {"type": "content_block_delta", "index": index,
                                           "delta": {"type": "thinking_delta", "thinking": block[1]}})
                ev("content_block_delta", {"type": "content_block_delta", "index": index,
                                           "delta": {"type": "signature_delta", "signature": "fake-sig"}})
            else:
                _, cid, name, args = block
                ev("content_block_start", {"type": "content_block_start", "index": index,
                                           "content_block": {"type": "tool_use", "id": cid, "name": name, "input": {}}})
                for piece in self._pieces(args, plan.json_chunk):
                    ev("content_block_delta", {"type": "content_block_delta", "index": index,
                                               "delta": {"type": "input_json_delta", "partial_json": piece}})
            ev("content_block_stop", {"type": "content_block_stop", "index": index})
        ev("message_delta", {"type": "message_delta", "delta": {"stop_reason": plan.stop, "stop_sequence": None},
                             "usage": {"output_tokens": 5}})
        ev("message_stop", {"type": "message_stop"})

    def _emit_openai(self, plan, in_tokens):
        model = self.body.get("model")

        def chunk(delta, finish=None, usage=None):
            obj = {"id": "chatcmpl-fake", "object": "chat.completion.chunk", "model": model,
                   "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
            if usage:
                obj = {"id": "chatcmpl-fake", "object": "chat.completion.chunk", "model": model,
                       "choices": [], "usage": usage}
            self._ev("", obj)

        chunk({"role": "assistant"})
        first, tool_index = True, 0
        for block in plan.blocks:
            if plan.unknown_events:
                self._write(b"event: future_event\ndata: {\"object\":\"future\"}\n\n: comment\n\n")
            if block[0] == "text":
                chunk({"content": block[1]})
                if first:
                    first = False
                    if self._fault(plan):
                        return
            elif block[0] == "thinking":
                chunk({"reasoning_content": block[1]})
            else:
                _, cid, name, args = block
                idx = 0 if plan.dup_index else tool_index
                tool_index += 1
                pieces = self._pieces(args, plan.json_chunk)
                chunk({"tool_calls": [{"index": idx, "id": cid, "type": "function",
                                       "function": {"name": name, "arguments": pieces[0]}}]})
                for piece in pieces[1:]:
                    chunk({"tool_calls": [{"index": idx, "function": {"arguments": piece}}]})
        chunk({}, "tool_calls" if plan.stop == "tool_use" else "stop")
        chunk({}, usage={"prompt_tokens": in_tokens, "completion_tokens": 5, "total_tokens": in_tokens + 5})
        self._write(b"data: [DONE]\n\n")

    def _emit_responses(self, plan, in_tokens):
        ev = self._ev
        ev("response.created", {"type": "response.created", "response": {"id": "resp_fake", "status": "in_progress"}})
        first = True
        for i, block in enumerate(plan.blocks):
            index = 0 if plan.dup_index else i
            if plan.unknown_events:
                ev("response.future_event", {"type": "response.future_event", "x": 1})
            if block[0] == "text":
                item_id = f"msg_{i}"
                ev("response.output_item.added", {"type": "response.output_item.added", "output_index": index,
                                                  "item": {"id": item_id, "type": "message", "role": "assistant", "content": []}})
                ev("response.output_text.delta", {"type": "response.output_text.delta", "item_id": item_id,
                                                  "output_index": index, "delta": block[1]})
                if first:
                    first = False
                    if self._fault(plan):
                        return
                item = {"id": item_id, "type": "message", "role": "assistant", "status": "completed",
                        "content": [{"type": "output_text", "text": block[1]}]}
            elif block[0] == "thinking":
                item = {"id": f"rs_{i}", "type": "reasoning", "summary": [{"type": "summary_text", "text": block[1]}]}
                ev("response.reasoning_summary_text.delta", {"type": "response.reasoning_summary_text.delta",
                                                             "item_id": item["id"], "delta": block[1]})
            else:
                _, cid, name, args = block
                item_id = f"fc_{i}"
                ev("response.output_item.added", {"type": "response.output_item.added", "output_index": index, "item": {
                    "id": item_id, "type": "function_call", "call_id": cid, "name": name, "arguments": ""}})
                for piece in self._pieces(args, plan.json_chunk):
                    ev("response.function_call_arguments.delta", {"type": "response.function_call_arguments.delta",
                                                                  "item_id": item_id, "output_index": index, "delta": piece})
                item = {"id": item_id, "type": "function_call", "status": "completed", "call_id": cid,
                        "name": name, "arguments": args}
            ev("response.output_item.done", {"type": "response.output_item.done", "output_index": index, "item": item})
        ev("response.completed", {"type": "response.completed", "response": {
            "id": "resp_fake", "status": "completed", "output": [],
            "usage": {"input_tokens": in_tokens, "output_tokens": 5, "input_tokens_details": {"cached_tokens": 0}}}})

    # -- dispatch
    def do_POST(self):
        try:
            self._handle()
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            pass

    def _handle(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        try:
            self.body = json.loads(raw or b"{}")
        except ValueError:
            self.body = {}
        path = self.path.split("?", 1)[0]
        forced = self.server.opts.get("shape") or "auto"
        self.shape = "anthropic" if forced == "auto" else forced
        prefix = path
        for shape, suffix in SHAPE_SUFFIXES:
            if path.endswith(suffix):
                prefix = path[: -len(suffix)]
                if forced == "auto":
                    self.shape = shape
                break
        if prefix.endswith("/v1"):
            prefix = prefix[:-3]
        key = prefix.strip("/") or "ok"
        name, *args = key.split("/")[0].split(":")
        self.write_mode, self.drip_s = None, 0.05
        summary = summarize(self.shape, self.body)
        aux = None if self.server.opts.get("no_aux") else aux_kind(summary)
        n = 0 if aux else self.server.bump(key)
        rec = {"seq": None, "time": time.time(), "path": self.path, "shape": self.shape, "key": key,
               "scenario": name, "args": args, "aux": aux, "n": n, "bytes": len(raw),
               "summary": {k: v for k, v in summary.items() if k != "system"},
               "headers": {k: v for k, v in self.headers.items() if k.lower() not in ("authorization", "x-api-key")}}
        with self.server.lock:
            self.server.seq += 1
            rec["seq"] = self.server.seq
            self.server.requests.append(rec)
        record_dir = self.server.opts.get("record")
        if record_dir:
            os.makedirs(record_dir, exist_ok=True)
            fname = f"{rec['seq']:05d}-{self.shape}-{name}{'-' + aux if aux else ''}.json"
            with open(os.path.join(record_dir, fname), "w") as f:
                json.dump({**rec, "body": self.body}, f)
        in_tokens = max(1, len(raw) // 4)
        if aux == "title":
            return self._emit(text_plan(TITLE_TEXT), in_tokens)
        if aux == "compaction":
            return self._emit(text_plan(SUMMARY_TEXT), in_tokens)
        self._scenario(name, args, n, summary, in_tokens)

    @staticmethod
    def _arg(args, i, default, cast=int):
        try:
            return cast(args[i]) if len(args) > i and args[i] != "" else default
        except ValueError:
            return default

    def _scenario(self, name, args, n, summary, in_tokens):
        if name == "recover":
            name = "http-401" if n == 1 else "ok"
        if name == "script":
            return self._script(args[0] if args else "default", n, summary, in_tokens)
        if name == "disconnect":
            self.connection.shutdown(socket.SHUT_RDWR)
            self.connection.close()
            return
        if name == "stall-headers":
            time.sleep(self.server.opts["stall_seconds"])
            return self._emit(text_plan(PARTIAL), in_tokens)
        m = re.fullmatch(r"http-(\d{3})", name)
        if m:
            status, limit = int(m.group(1)), self._arg(args, 0, None)
            if limit is None or n <= limit:
                kinds = {529: "overloaded_error", 429: "rate_limit_error", 401: "authentication_error",
                         403: "permission_error", 500: "api_error", 400: "invalid_request_error"}
                return self._error(status, kinds.get(status, "api_error"), f"Simulated HTTP {status}")
            return self._emit(text_plan(f"ok after {limit} HTTP {status}"), in_tokens)
        if name in ("retry-after-seconds", "retry-after-date"):
            seconds = self._arg(args, 0, 1 if name.endswith("seconds") else 3)
            if n <= self._arg(args, 1, 1):
                value = str(seconds) if name.endswith("seconds") else email.utils.formatdate(time.time() + seconds, usegmt=True)
                return self._error(429, "rate_limit_error", "Simulated rate limit", headers={"Retry-After": value})
            return self._emit(text_plan(f"retry ok after {n - 1} rejection(s)"), in_tokens)
        if name == "context-overflow":
            limit = self._arg(args, 0, None)
            if limit is None or n <= limit:
                return self._error(400, "invalid_request_error", "prompt is too long: 250000 tokens > 200000 maximum",
                                   code="context_length_exceeded")
            return self._emit(text_plan("after overflow ok"), in_tokens)
        if name == "malformed":
            self._start_stream()
            self._write({"openai": b"data: {broken\n\n",
                         "anthropic": b"event: message_start\ndata: {broken\n\n"}.get(
                self.shape, b"event: response.output_text.delta\ndata: {broken\n\n"))
            return
        if name == "echo":
            stats = {k: v for k, v in summary.items() if k != "system"}
            stats.update(bytes=len(json.dumps(self.body)), n=n)
            return self._emit(text_plan("ECHO " + json.dumps(stats, ensure_ascii=False)), in_tokens)
        if name == "tool-loop":
            limit, tool = self._arg(args, 0, 3), self._arg(args, 1, "X", str)
            done = summary["trailing_tool_results"]
            if done < limit:
                return self._emit(tool_plan(tool, default_tool_args(tool, done), text=f"loop {done + 1}/{limit}"), in_tokens)
            return self._emit(text_plan(f"tool-loop done after {done} tool results"), in_tokens)
        if name == "huge-tool-json":
            mb, tool = self._arg(args, 0, 4, float), self._arg(args, 1, "X", str)
            if summary["trailing_tool_results"] == 0:
                payload = default_tool_args(tool)
                payload["pad"] = "A" * int(mb * 1024 * 1024)
                return self._emit(tool_plan(tool, payload), in_tokens)
            return self._emit(text_plan("huge-tool-json done"), in_tokens)
        if name == "plan":
            if summary["tool_results"] == 0:
                plan_text = ("# Implementation plan\n\n## Investigation\nReview the current implementation and preserve existing user changes.\n\n"
                             "## Implementation\nAdd the requested feature without changing permissions. Unicode: 日本語 café.\n\n"
                             "## Validation\nRun regression tests, verify narrow terminal presentation, and confirm cancellation behavior.\n\nEND-OF-PLAN")
                return self._emit(tool_plan("exit_plan_mode", {"plan": plan_text}, call_id="plan-review-call"), in_tokens)
            return self._emit(text_plan(PARTIAL), in_tokens)
        plan = text_plan(PARTIAL)
        if name in ("heartbeat", "stall", "cut", "stream-error", "overloaded"):
            plan.fault = name
        elif name == "slow-drip":
            self.write_mode, self.drip_s = "drip", self._arg(args, 0, 50, float) / 1000
            plan = text_plan("drip ok")
        elif name == "split-utf8":
            self.write_mode = "split"
            self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            plan = text_plan(SPLIT_TEXT)
        elif name == "dup-index":
            if self.shape == "anthropic":
                plan = Plan([("text", "AAA"), ("text", "BBB")])
            elif summary["trailing_tool_results"]:
                plan = text_plan("dup-index done")
            else:
                plan = Plan([("tool", "call_dup_a", "X", "{}"), ("tool", "call_dup_b", "X", "{}")])
            plan.dup_index = True
        elif name == "unknown-event":
            plan = text_plan("unknown-event ok")
            plan.unknown_events = True
        self._emit(plan, in_tokens)

    def _script(self, script_name, n, summary, in_tokens):
        steps = self.server.scripts.get(script_name)
        if steps is None:
            directory = self.server.opts.get("script_dir")
            path = os.path.join(directory, script_name + ".json") if directory else None
            if not path or not os.path.exists(path):
                return self._error(500, "api_error", f"fake_provider: no script '{script_name}'")
            with open(path) as f:
                steps = json.load(f)
        then = "repeat-last"
        if isinstance(steps, dict):
            then, steps = steps.get("then", then), steps["steps"]
        if n <= len(steps):
            step = steps[n - 1]
        elif then == "ok" or not steps:
            step = {"text": "script exhausted"}
        else:
            step = steps[-1]
        if isinstance(step, str):
            step = {"text": step}
        if step.get("delay_ms"):
            time.sleep(step["delay_ms"] / 1000)
        if "scenario" in step:
            name, *args = step["scenario"].split(":")
            return self._scenario(name, args, n, summary, in_tokens)
        blocks = []
        if step.get("thinking"):
            blocks.append(("thinking", step["thinking"]))
        if step.get("text"):
            blocks.append(("text", step["text"].replace("{n}", str(n))))
        calls = step.get("tools") or ([{"name": step["tool"], "args": step.get("args", {})}] if step.get("tool") else [])
        for i, call in enumerate(calls):
            args = call.get("args", {})
            blocks.append(("tool", call.get("id") or f"toolu_script_{n}_{i}_{time.time_ns()}", call["name"],
                           args if isinstance(args, str) else json.dumps(args)))
        plan = Plan(blocks or [("text", "")])
        plan.fault = step.get("fault")
        self._emit(plan, in_tokens)


class FakeProvider:
    """In-process fake provider on a daemon thread; also a context manager."""

    def __init__(self, port=0, record=None, stall_seconds=30.0, script_dir=None, shape="auto",
                 no_aux=False, verbose=False):
        self.opts = {"record": record, "stall_seconds": stall_seconds, "script_dir": script_dir,
                     "shape": shape, "no_aux": no_aux, "verbose": verbose}
        self.server = _Server(("127.0.0.1", port), self.opts)
        self.port = self.server.server_address[1]
        self.thread = None

    def start(self):
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True,
                                       name=f"fake-provider-{self.port}")
        self.thread.start()
        return self

    def stop(self):
        self.server.shutdown()
        self.server.server_close()

    def __enter__(self):
        return self.start()

    def __exit__(self, *exc):
        self.stop()

    def url(self, scenario="ok"):
        return f"http://127.0.0.1:{self.port}/{scenario.strip('/')}"

    anthropic_url = url  # --base-url (rness appends /v1/messages)
    responses_url = url  # chatgpt --base-url (rness appends /codex/responses)

    def openai_url(self, scenario="ok"):
        return self.url(scenario) + "/v1"  # --route name=<this>,none

    @property
    def requests(self):
        with self.server.lock:
            return list(self.server.requests)

    def main_requests(self, key=None):
        return [r for r in self.requests if not r["aux"] and (key is None or r["key"] == key)]

    def set_script(self, name, steps):
        self.server.scripts[name] = steps

    def set_stall_seconds(self, seconds):
        self.opts["stall_seconds"] = seconds

    def reset(self):
        with self.server.lock:
            self.server.counters.clear()
            self.server.requests.clear()


def main():
    lines = "\n".join(f"  {k:30} {v}" for k, v in SCENARIOS.items())
    parser = argparse.ArgumentParser(
        description="Use base URL http://127.0.0.1:PORT/SCENARIO (see module docstring for shapes).",
        epilog="Scenarios (first URL path segment, `name:arg:arg`):\n" + lines,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--port", type=int, default=8769, help="0 picks a free port (printed)")
    parser.add_argument("--stall-seconds", type=float, default=30)
    parser.add_argument("--shape", choices=["auto", "anthropic", "openai", "responses"], default="auto",
                        help="force a wire shape (default: detect from the request path)")
    parser.add_argument("--record", metavar="DIR", help="dump each request as DIR/<seq>-<shape>-<scenario>.json")
    parser.add_argument("--script-dir", metavar="DIR", help="playbooks for script:NAME (DIR/NAME.json)")
    parser.add_argument("--no-aux", action="store_true", help="treat title/compaction calls like main requests")
    parser.add_argument("--verbose", action="store_true")
    a = parser.parse_args()
    fp = FakeProvider(a.port, a.record, a.stall_seconds, a.script_dir, a.shape, a.no_aux, a.verbose)
    print(f"Fake provider on http://127.0.0.1:{fp.port}", flush=True)
    try:
        fp.server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
