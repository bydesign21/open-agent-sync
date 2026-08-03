#!/usr/bin/env python3
"""Deterministic local model stub for the OW-011 live E2E gate.

Speaks just enough of the OpenAI-compatible /v1/chat/completions surface
(streamed and non-streamed) for OpenCode's and Kilo's `@ai-sdk/openai-compatible`
custom provider to drive one real agentic turn: on the first request that
carries a `tools` array and no prior tool result, it answers with a tool call
(preferring a tool literally named "bash", so a real shell tool actually runs);
on every other request it answers with a plain final message. No network
access happens anywhere in this file — it only binds 127.0.0.1 and serves
canned, deterministic responses. This is what makes tool.execute.before/after
observable on a live host without any real model-provider credentials.
"""
import http.server
import json
import sys
import threading
import time
import uuid

LOGFILE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/agentsync-e2e-stub.log"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 0
CALL_COUNT = {"n": 0}
LOCK = threading.Lock()


def log(msg: str) -> None:
    with open(LOGFILE, "a") as f:
        f.write(msg + "\n")


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):  # noqa: A002 - stdlib signature
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        try:
            payload = json.loads(body)
        except Exception as e:  # noqa: BLE001 - deliberately broad for a stub
            payload = {"_parse_error": str(e), "raw": body.decode(errors="replace")}
        log("=== REQUEST %s ===" % self.path)
        log(json.dumps(payload)[:400])

        with LOCK:
            CALL_COUNT["n"] += 1
            call_n = CALL_COUNT["n"]

        tools = payload.get("tools") or []
        stream = bool(payload.get("stream"))
        messages = payload.get("messages") or []
        already_called_tool = any(m.get("role") == "tool" for m in messages)
        log(
            "call_n=%d tools=%d already_called_tool=%s"
            % (call_n, len(tools), already_called_tool)
        )

        if tools and not already_called_tool:
            self._respond_tool_call(stream, self._pick_tool_call(tools))
        else:
            self._respond_final(stream)

    def _pick_tool_call(self, tools):
        tool = None
        for candidate in tools:
            fn_name = candidate.get("function", candidate).get("name", "")
            if fn_name.lower() == "bash":
                tool = candidate
                break
        if tool is None:
            tool = tools[0]
        fn = tool.get("function", tool)
        name = fn.get("name", "unknown")
        params = fn.get("parameters", {}) or {}
        props = params.get("properties", {}) or {}
        required = params.get("required", []) or []
        args = {}
        for key in required:
            spec = props.get(key, {})
            t = spec.get("type")
            if t == "string":
                # "ls ." is a harmless, deterministic, side-effect-free
                # command — enough to exercise a real tool.execute cycle
                # without touching anything outside the project directory.
                args[key] = "ls ." if key == "command" else "x"
            elif t in ("number", "integer"):
                args[key] = 0
            elif t == "boolean":
                args[key] = False
            elif t == "array":
                args[key] = []
            elif t == "object":
                args[key] = {}
            else:
                args[key] = "."
        call_id = "call_" + uuid.uuid4().hex[:16]
        return name, call_id, args

    def _respond_tool_call(self, stream, picked):
        name, call_id, args = picked
        msg = {
            "id": "chatcmpl-" + uuid.uuid4().hex,
            "object": "chat.completion",
            "created": int(time.time()),
            "model": "stub-model",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": None,
                        "tool_calls": [
                            {
                                "id": call_id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": json.dumps(args),
                                },
                            }
                        ],
                    },
                    "finish_reason": "tool_calls",
                }
            ],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        }
        self._send(stream, msg, tool_call=True)

    def _respond_final(self, stream):
        msg = {
            "id": "chatcmpl-" + uuid.uuid4().hex,
            "object": "chat.completion",
            "created": int(time.time()),
            "model": "stub-model",
            "choices": [
                {
                    "index": 0,
                    "message": {"role": "assistant", "content": "AGENTSYNC_E2E_STUB_DONE"},
                    "finish_reason": "stop",
                }
            ],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        }
        self._send(stream, msg, tool_call=False)

    def _send(self, stream, msg, tool_call):
        if not stream:
            body = json.dumps(msg).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "keep-alive")
        self.end_headers()

        choice = msg["choices"][0]["message"]
        if tool_call:
            tc = choice["tool_calls"][0]
            chunks = [
                {
                    "id": msg["id"],
                    "object": "chat.completion.chunk",
                    "created": msg["created"],
                    "model": msg["model"],
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"role": "assistant", "content": None},
                            "finish_reason": None,
                        }
                    ],
                },
                {
                    "id": msg["id"],
                    "object": "chat.completion.chunk",
                    "created": msg["created"],
                    "model": msg["model"],
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": tc["id"],
                                        "type": "function",
                                        "function": {
                                            "name": tc["function"]["name"],
                                            "arguments": tc["function"]["arguments"],
                                        },
                                    }
                                ]
                            },
                            "finish_reason": None,
                        }
                    ],
                },
                {
                    "id": msg["id"],
                    "object": "chat.completion.chunk",
                    "created": msg["created"],
                    "model": msg["model"],
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                },
            ]
        else:
            chunks = [
                {
                    "id": msg["id"],
                    "object": "chat.completion.chunk",
                    "created": msg["created"],
                    "model": msg["model"],
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"role": "assistant", "content": choice["content"]},
                            "finish_reason": None,
                        }
                    ],
                },
                {
                    "id": msg["id"],
                    "object": "chat.completion.chunk",
                    "created": msg["created"],
                    "model": msg["model"],
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                },
            ]
        for c in chunks:
            self.wfile.write(("data: " + json.dumps(c) + "\n\n").encode())
        self.wfile.write(b"data: [DONE]\n\n")


class ReusableServer(http.server.ThreadingHTTPServer):
    allow_reuse_address = True


if __name__ == "__main__":
    srv = ReusableServer(("127.0.0.1", PORT), Handler)
    # Print the bound port so a caller that requested an ephemeral port (0)
    # can discover it deterministically instead of guessing or racing.
    log("stub server listening on %d" % srv.server_address[1])
    print(srv.server_address[1], flush=True)
    srv.serve_forever()
