#!/usr/bin/env python3
"""Local Anthropic fault injector. Run --help for scenarios. No credentials needed."""
import argparse
import json
import socket
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Provider(BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", "0")))
        scenario = self.path.strip("/").split("/")[0]
        if scenario == "disconnect":
            self.connection.shutdown(socket.SHUT_RDWR)
            self.connection.close()
            return
        if scenario.startswith("http-"):
            status = int(scenario[5:])
            payload = json.dumps({"error": {"message": f"Simulated HTTP {status}"}}).encode()
            self.send_response(status)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()

        def event(kind, data):
            self.wfile.write(f"event: {kind}\ndata: {json.dumps(data)}\n\n".encode())
            self.wfile.flush()

        try:
            if scenario == "malformed":
                self.wfile.write(b"event: message_start\ndata: {broken\n\n")
                return
            event("message_start", {"message": {"usage": {"input_tokens": 1}}})
            event("content_block_start", {"index": 0, "content_block": {"type": "text", "text": ""}})
            event("content_block_delta", {"index": 0, "delta": {"type": "text_delta", "text": "Partial response from fake provider."}})
            if scenario == "stall":
                time.sleep(self.server.stall_seconds)
                return
            if scenario == "cut":
                return
            if scenario == "stream-error":
                event("error", {"error": {"message": "Simulated overloaded stream"}})
                return
            event("content_block_stop", {"index": 0})
            event("message_delta", {"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}})
            event("message_stop", {})
        except (BrokenPipeError, ConnectionResetError):
            pass


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Use base URL http://127.0.0.1:PORT/SCENARIO. Scenarios: ok, cut, disconnect, stall, malformed, stream-error, http-401, http-429, http-500. Unknown scenarios return success.")
    parser.add_argument("--port", type=int, default=8769)
    parser.add_argument("--stall-seconds", type=float, default=30)
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Provider)
    server.stall_seconds = args.stall_seconds
    print(f"Fake provider on http://127.0.0.1:{server.server_port}", flush=True)
    server.serve_forever()
