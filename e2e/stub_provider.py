#!/usr/bin/env python3
import argparse
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def load_script(path):
    with open(path) as f:
        script = json.load(f)
    return {"lead": list(script.get("lead", [])), "side": list(script.get("side", []))}


def msg_head(msg):
    role = msg.get("role", "?")
    parts = []
    content = msg.get("content")
    if isinstance(content, str):
        parts.append(content)
    elif isinstance(content, list):
        for p in content:
            if isinstance(p, dict):
                t = p.get("text") or p.get("content") or ""
                parts.append(str(t))
            else:
                parts.append(str(p))
    calls = msg.get("tool_calls") or []
    for c in calls:
        fn = (c.get("function") or {})
        parts.append("<call %s(%s)>" % (fn.get("name", "?"), (fn.get("arguments") or "")[:80]))
    head = " | ".join(p for p in parts if p)
    return {"role": role, "head": head[:400]}


def chunk(model, delta=None, finish=None, usage=None):
    c = {
        "id": "chatcmpl-stub",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": model,
        "choices": [],
    }
    if delta is not None or finish is not None:
        c["choices"] = [{"index": 0, "delta": delta or {}, "finish_reason": finish}]
    if usage is not None:
        c["usage"] = usage
    return c


def sse_body(model, entry, gseq):
    usage = {
        "prompt_tokens": 200 + 13 * gseq,
        "completion_tokens": 17 + 3 * gseq,
    }
    usage["total_tokens"] = usage["prompt_tokens"] + usage["completion_tokens"]
    lines = []
    calls = entry.get("tool_calls")
    if calls is None and "tool_call" in entry:
        calls = [entry["tool_call"]]
    if calls:
        deltas = []
        for i, call in enumerate(calls):
            deltas.append(
                {
                    "index": i,
                    "id": "call_%d_%d" % (gseq, i),
                    "type": "function",
                    "function": {
                        "name": call["name"],
                        "arguments": json.dumps(call.get("args", {})),
                    },
                }
            )
        lines.append(chunk(model, delta={"tool_calls": deltas}))
        lines.append(chunk(model, finish="tool_calls"))
    else:
        text = entry.get("text", "")
        lines.append(chunk(model, delta={"content": text}))
        lines.append(chunk(model, finish="stop"))
    lines.append(chunk(model, usage=usage))
    out = "".join("data: %s\n\n" % json.dumps(c) for c in lines)
    out += "data: [DONE]\n\n"
    return out.encode()


class State:
    def __init__(self, script_path, log_path):
        self.script = load_script(script_path)
        self.log_path = log_path
        self.lock = threading.Lock()
        self.gseq = 0

    def next_entry(self, role):
        with self.lock:
            self.gseq += 1
            q = self.script[role]
            if q:
                return q.pop(0), self.gseq
            return {"text": "STUB-SCRIPT-EXHAUSTED"}, self.gseq

    def log(self, row):
        line = json.dumps(row)
        with self.lock:
            with open(self.log_path, "a") as f:
                f.write(line + "\n")


def make_handler(state):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def _send(self, code, body, ctype="application/json"):
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.path.endswith("/models"):
                body = json.dumps(
                    {
                        "object": "list",
                        "data": [
                            {"id": "stub-lead", "object": "model", "created": 1, "owned_by": "stub"},
                            {"id": "stub-side", "object": "model", "created": 1, "owned_by": "stub"},
                        ],
                    }
                ).encode()
                self._send(200, body)
            elif self.path.endswith("/health"):
                self._send(200, b"ok", "text/plain")
            else:
                self._send(404, b"{}")

        def do_POST(self):
            if not self.path.endswith("/chat/completions"):
                self._send(404, b"{}")
                return
            length = int(self.headers.get("Content-Length") or 0)
            try:
                body = json.loads(self.rfile.read(length) or b"{}")
            except Exception as e:
                self._send(400, json.dumps({"error": str(e)}).encode())
                return
            model = body.get("model", "")
            role = "side" if model == "stub-side" else "lead"
            messages = body.get("messages", [])
            tools = [
                (t.get("function") or {}).get("name", "?")
                for t in (body.get("tools") or [])
            ]
            entry, gseq = state.next_entry(role)
            state.log(
                {
                    "seq": gseq,
                    "ts": round(time.time(), 3),
                    "role": role,
                    "model": model,
                    "n_messages": len(messages),
                    "tools": tools,
                    "msgs": [msg_head(m) for m in messages],
                }
            )
            delay = entry.get("delay", 0)
            if delay:
                time.sleep(delay)
            payload = sse_body(model, entry, gseq)
            self._send(200, payload, "text/event-stream")

    return Handler


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=0)
    args = ap.parse_args()
    state = State(os.environ["STUB_SCRIPT"], os.environ["STUB_LOG"])
    srv = ThreadingHTTPServer(("127.0.0.1", args.port), make_handler(state))
    port = srv.server_address[1]
    port_file = os.environ.get("STUB_PORT_FILE")
    if port_file:
        with open(port_file, "w") as f:
            f.write(str(port))
    print("stub-provider listening on 127.0.0.1:%d" % port, flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
