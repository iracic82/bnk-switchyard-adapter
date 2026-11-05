#!/usr/bin/env python3
"""End-to-end checks against a running local stack (scripts/local-up.sh).

Standard library only. Exits non-zero if any check fails. Each check prints PASS or FAIL.
"""

import json
import os
import socket
import sys
import time
import urllib.error
import urllib.request

BASE = os.environ.get("ADAPTER_URL", "http://127.0.0.1:9480")
ENG = "sk-demo-eng-agents"
SUPPORT = "sk-demo-support-agents"
RESEARCH = "sk-demo-research"
TOOLS = [{"type": "function", "function": {"name": "search", "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}}}]

failures = []


def call(key, body, headers=None, raw=False):
    data = body if isinstance(body, bytes) else json.dumps(body).encode()
    req = urllib.request.Request(f"{BASE}/v1/chat/completions", data=data, method="POST")
    req.add_header("content-type", "application/json")
    if key:
        req.add_header("authorization", f"Bearer {key}")
    for name, value in (headers or {}).items():
        req.add_header(name, value)
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            payload = resp.read()
            return resp.status, {k.lower(): v for k, v in resp.headers.items()}, payload if raw else json.loads(payload)
    except urllib.error.HTTPError as err:
        payload = err.read()
        try:
            payload = json.loads(payload)
        except ValueError:
            pass
        return err.code, {k.lower(): v for k, v in err.headers.items()}, payload


def sse_events(payload):
    events = []
    for line in payload.decode().splitlines():
        if line.startswith("data: "):
            data = line[6:]
            events.append(data if data == "[DONE]" else json.loads(data))
    return events


def check(name, condition, detail=""):
    print(f"{'PASS' if condition else 'FAIL'}  {name}" + (f"  ({detail})" if detail and not condition else ""))
    if not condition:
        failures.append(name)


def user(text):
    return [{"role": "user", "content": text}]


# Routing: the task classifier picks the tier; the logical model names the pool.
status, h, body = call(ENG, {"model": "auto", "messages": user("What is the capital of France?")})
check("auto: easy prompt goes to small", status == 200 and h.get("x-switchyard-model") == "small", f"{status} {h.get('x-switchyard-model')}")
check("auto: reports algorithm and pool", h.get("x-switchyard-algorithm") == "llm_task_classifier" and h.get("x-switchyard-inference-pool") == "inference/small-pool")
check("auto: response model is the provider model", status == 200 and body.get("model") == h.get("x-switchyard-provider-model") and body.get("model") not in (None, "auto", "small"), str(body.get("model")))
check("auto: served by the small pool's endpoint", h.get("x-served-by", "").startswith("small-"))

status, h, body = call(ENG, {"model": "auto", "messages": user("Design a distributed rate limiter and prove it is linearizable.")})
check("auto: hard prompt goes to large", status == 200 and h.get("x-switchyard-model") == "large", f"{status} {h.get('x-switchyard-model')}")

status, h, _ = call(ENG, {"model": "passthrough", "messages": user("hi")})
check("passthrough: first model answers", status == 200 and h.get("x-switchyard-model") == "large" and h.get("x-switchyard-algorithm") == "passthrough")

seen = set()
for _ in range(20):
    status, h, _ = call(ENG, {"model": "weighted", "messages": user("hi")})
    seen.add(h.get("x-switchyard-model"))
check("weighted random: both models chosen over 20 requests", seen == {"small", "large"}, str(seen))

# Request ID: accepted when well-formed, generated otherwise, forwarded upstream.
status, h, _ = call(ENG, {"model": "small", "messages": user("hi")}, {"x-request-id": "demo-req-123"})
check("request id echoed", h.get("x-request-id") == "demo-req-123")
status, h, _ = call(ENG, {"model": "small", "messages": user("hi")}, {"x-request-id": "bad id with spaces"})
check("bad request id replaced", h.get("x-request-id") not in (None, "bad id with spaces"))

# Governance.
status, h, body = call(SUPPORT, {"model": "auto", "messages": user("Design a distributed system")})
check("support tenant: auto degraded to small", status == 200 and h.get("x-switchyard-model") == "small" and h.get("x-switchyard-policy") == "degraded")
status, _, body = call(SUPPORT, {"model": "large", "messages": user("hi")})
check("support tenant: direct large refused 403", status == 403 and body["error"]["code"] == "model_not_allowed")
status, _, body = call(RESEARCH, {"model": "auto", "messages": user("hi")})
check("research tenant: route refused 403", status == 403 and body["error"]["code"] == "route_not_allowed")
status, h, _ = call(RESEARCH, {"model": "large", "messages": user("hi")})
check("research tenant: direct large allowed", status == 200 and h.get("x-switchyard-model") == "large")
status, _, body = call(None, {"model": "auto", "messages": user("hi")}, {"x-tenant-id": "eng-agents", "x-switchyard-agent-id": "eng-agents"})
check("identity headers alone are not credentials", status == 401)
status, _, body = call(SUPPORT, {"model": "small", "max_tokens": 4096, "messages": user("hi")})
check("max_tokens above tenant cap refused 400", status == 400 and body["error"]["code"] == "max_tokens")

# Loop guard.
status, _, body = call(ENG, {"model": "auto", "messages": user("hi")}, {"x-switchyard-adapter-hop": "1"})
check("hop header refused with 508", status == 508)

# Bounded request size.
status, _, _ = call(ENG, json.dumps({"model": "small", "messages": user("x" * (2 * 1024 * 1024))}).encode())
check("oversized body refused 413", status == 413, str(status))
status, _, body = call(ENG, b"{not json")
check("invalid JSON refused 400", status == 400)
status, _, body = call(ENG, {"model": "nope", "messages": user("hi")})
check("unknown model 404", status == 404 and body["error"]["code"] == "model_not_found")

# Streaming.
status, h, payload = call(ENG, {"model": "small", "stream": True, "messages": user("hello")}, raw=True)
events = sse_events(payload)
chunks = [e for e in events if isinstance(e, dict)]
check("stream: SSE content type", h.get("content-type", "").startswith("text/event-stream"))
check("stream: ends with [DONE]", events and events[-1] == "[DONE]")
check("stream: has content deltas", any(c["choices"] and c["choices"][0]["delta"].get("content") for c in chunks))
check("stream: no usage chunk the client did not ask for", not any("usage" in c and c.get("usage") for c in chunks))
check("stream: finish_reason stop", any(c["choices"] and c["choices"][0].get("finish_reason") == "stop" for c in chunks))
status, _, payload = call(ENG, {"model": "small", "stream": True, "stream_options": {"include_usage": True}, "messages": user("hello")}, raw=True)
chunks = [e for e in sse_events(payload) if isinstance(e, dict)]
check("stream: usage chunk kept when the client asked", any(c.get("usage") for c in chunks))
status, h, payload = call(ENG, {"model": "auto", "stream": True, "messages": user("Refactor this module and debug the race condition")}, raw=True)
check("stream through auto routes hard work to large", status == 200 and h.get("x-switchyard-model") == "large")

# Tool calls, both directions, buffered and streamed.
status, _, body = call(ENG, {"model": "small", "messages": user("look it up [call-tool]"), "tools": TOOLS})
choice = body["choices"][0] if status == 200 else {}
calls = choice.get("message", {}).get("tool_calls") or []
check("tool call returned (buffered)", choice.get("finish_reason") == "tool_calls" and calls and calls[0]["function"]["name"] == "search", json.dumps(choice)[:200])
status, _, payload = call(ENG, {"model": "small", "stream": True, "messages": user("look it up [call-tool]"), "tools": TOOLS}, raw=True)
chunks = [e for e in sse_events(payload) if isinstance(e, dict)]
check("tool call returned (stream)", any(c["choices"] and c["choices"][0]["delta"].get("tool_calls") for c in chunks))
history = user("look it up") + [
    {"role": "assistant", "content": None, "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "search", "arguments": "{}"}}]},
    {"role": "tool", "tool_call_id": "call_1", "content": "42"},
]
status, h, body = call(ENG, {"model": "auto", "messages": history, "tools": TOOLS})
check("tool result turn accepted through auto", status == 200 and "tool returned" in body["choices"][0]["message"]["content"], str(body)[:200])

# Cancellation: close the socket mid-stream; the adapter must record a cancelled request.
def metric(name, labels):
    with urllib.request.urlopen(f"{BASE}/metrics", timeout=10) as resp:
        text = resp.read().decode()
    total = 0.0
    for line in text.splitlines():
        if line.startswith(name + "{") and all(f'{k}="{v}"' in line for k, v in labels.items()):
            total += float(line.rsplit(" ", 1)[1])
    return total

before = metric("sy_adapter_requests_total", {"status": "cancelled"})
host, port = BASE.removeprefix("http://").split(":")
sock = socket.create_connection((host, int(port)))
body = json.dumps({"model": "large", "stream": True, "messages": user("[task:c1:hard] long answer please")}).encode()
sock.sendall(b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n"
             + f"Authorization: Bearer {ENG}\r\nContent-Length: {len(body)}\r\n\r\n".encode() + body)
sock.recv(512)
sock.close()
time.sleep(1.5)
check("client disconnect is recorded as cancelled", metric("sy_adapter_requests_total", {"status": "cancelled"}) > before)

# Observability.
check("metrics: selection cost recorded", metric("sy_adapter_estimated_cost_usd_total", {"phase": "selection"}) > 0)
check("metrics: inference tokens recorded", metric("sy_adapter_tokens_total", {"phase": "inference", "kind": "output"}) > 0)
check("metrics: judge calls recorded", metric("sy_adapter_routing_calls_total", {"kind": "model", "outcome": "ok"}) > 0)
check("metrics: ttft recorded for streams", metric("sy_adapter_ttft_seconds_count", {}) > 0)

# Live view: the event stream replays recent events, including refusals.
def live_events(limit=200, timeout=5):
    out = []
    try:
        with urllib.request.urlopen(f"{BASE}/live/events", timeout=timeout) as resp:
            for raw in resp:
                line = raw.decode().strip()
                if line.startswith("data: "):
                    out.append(json.loads(line[6:]))
                    if len(out) >= limit:
                        break
    except (TimeoutError, OSError):
        pass
    return out

with urllib.request.urlopen(f"{BASE}/live", timeout=10) as resp:
    page = resp.read().decode()
check("live view page served", "Right model, right GPU" in page)
events = live_events()
served = [e for e in events if e["status"] == "ok"]
check("live events include served requests", len(served) > 0)
check("live events include refusals", any(e["status"] == "refused" and e["reason"] == "model_not_allowed" for e in events))
check("live events carry pod and frontier cost", any(e["served_by"] and e["frontier_cost_usd"] >= e["inference_cost_usd"] for e in served))
check("live events carry classifier score", any(e["algorithm"] == "llm_task_classifier" and e["score"] is not None for e in served))

print(f"\n{'FAILED' if failures else 'ALL PASSED'}: {len(failures)} failure(s)")
sys.exit(1 if failures else 0)
