"""WP-5 extensions to scripts/fake_provider.py (subclass; the original is
WP-0's and is not modified). Extra scenarios (anthropic + openai shapes):

  many-deltas:N[:BYTES]    N text deltas of BYTES bytes each, written in one go
  heartbeat-forever[:MS]   SSE comments every MS (default 200) for up to 10 min, no content
  gateway:STATUS:KIND      error with a non-provider body. KIND = html | message | text | string
  bigtext:MB               one text block of MB MiB, in 16-byte deltas

Usage:
    from wp5_fake import start_wp5_provider
    fp = start_wp5_provider(wp=5)        # FakeProvider with the extra scenarios
"""
import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import fake_provider as _fp  # noqa: E402


class WP5Handler(_fp.Provider):
    def _scenario(self, name, args, n, summary, in_tokens):
        if name == "many-deltas":
            count, size = self._arg(args, 0, 1000), self._arg(args, 1, 8)
            return self._many(count, size, in_tokens)
        if name == "bigtext":
            mb = self._arg(args, 0, 10, float)
            return self._many(int(mb * 1024 * 1024 / 16), 16, in_tokens)
        if name == "heartbeat-forever":
            ms = self._arg(args, 0, 200)
            self._start_stream()
            end = time.time() + 600
            while time.time() < end:
                self._write(b": keep-alive\n\n")
                self.wfile.flush()
                time.sleep(ms / 1000)
            return
        if name == "gateway":
            status, kind = self._arg(args, 0, 502), self._arg(args, 1, "html", str)
            body, ctype = {
                "html": (f"<html><body><h1>{status} Bad Gateway</h1>upstream connect error</body></html>", "text/html"),
                "message": (json.dumps({"message": "upstream overloaded, try later"}), "application/json"),
                "text": ("upstream request timeout", "text/plain"),
                "string": (json.dumps({"error": "model 'fake' not found"}), "application/json"),
            }[kind]
            payload = body.encode()
            self.send_response(status)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        return super()._scenario(name, args, n, summary, in_tokens)

    def _many(self, count, size, in_tokens):
        """Write N deltas as one buffer (full speed, no per-event syscalls)."""
        word = ("abcdefghijklmnopqrstuvwxyz0123456789" * (size // 36 + 1))[:size]
        self._start_stream()
        out = []
        if self.shape == "openai":
            def ev(_k, d):
                out.append(f"data: {json.dumps(d)}\n\n")
            ev("", {"choices": [{"index": 0, "delta": {"role": "assistant"}}]})
            delta = f"data: {json.dumps({'choices': [{'index': 0, 'delta': {'content': word}}]})}\n\n"
            out.append(delta * count)
            ev("", {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": in_tokens, "completion_tokens": count}})
            out.append("data: [DONE]\n\n")
        else:
            def ev(k, d):
                out.append(f"event: {k}\ndata: {json.dumps(d)}\n\n")
            ev("message_start", {"type": "message_start", "message": {
                "id": "msg_wp5", "type": "message", "role": "assistant", "model": self.body.get("model"),
                "content": [], "usage": {"input_tokens": in_tokens, "output_tokens": 1}}})
            ev("content_block_start", {"type": "content_block_start", "index": 0,
                                       "content_block": {"type": "text", "text": ""}})
            d = {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": word}}
            out.append(f"event: content_block_delta\ndata: {json.dumps(d)}\n\n" * count)
            ev("content_block_stop", {"type": "content_block_stop", "index": 0})
            ev("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                                 "usage": {"output_tokens": count}})
            ev("message_stop", {"type": "message_stop"})
        data = "".join(out).encode()
        for i in range(0, len(data), 1 << 20):
            self.wfile.write(data[i:i + (1 << 20)])
        self.wfile.flush()


def start_wp5_provider(wp=5, port=None, **kw):
    """Like harness.start_provider, but with WP5Handler."""
    last = None
    ports = [port] if port is not None else [8700 + 10 * wp + s for s in range(10)]
    for p in ports:
        try:
            fp = _fp.FakeProvider(port=p, **kw)
        except OSError as e:
            last = e
            continue
        fp.server.RequestHandlerClass = WP5Handler
        return fp.start()
    raise last
