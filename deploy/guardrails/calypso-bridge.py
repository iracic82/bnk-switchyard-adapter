# The base lab's CalypsoAI bridge (manifests/guardrails/30-calypso-bridge.yaml), with one change: the iRule sends the
# whole OpenAI request body as the scan input, and CalypsoAI judges a prompt wrapped in that JSON differently from
# the prompt itself (it cleared an export of patient records it flags as plain text). So this bridge scans the text
# inside the request: every string value, in any field (messages, tools, tool-call arguments, prompt, input), minus
# fields that never carry prose. A body that is not JSON is scanned as it came. Everything else (fail closed,
# project id from this pod) is unchanged.
import json, os, ssl, urllib.request, urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

UPSTREAM = os.environ.get("CALYPSO_URL", "https://us1.calypsoai.app/backend/v1/scans")
TOKEN = open("/secrets/api-key").read().strip()
PROJECT = os.environ.get("CALYPSO_PROJECT_ID", "")
CTX = ssl.create_default_context()
# fields that never carry prose: identifiers, enums and media payloads (base64 would only drown the text)
SKIP = {"model", "role", "type", "id", "tool_call_id", "stream", "image_url", "input_audio", "file"}


def scan_text(raw):
    try:
        req = json.loads(raw)
    except ValueError:
        return raw
    if not isinstance(req, dict):
        return raw
    parts = []

    def walk(o):   # every string anywhere in the request, so no field the model reads escapes the scan
        if isinstance(o, str):
            if o.strip():
                parts.append(o)
        elif isinstance(o, list):
            for v in o:
                walk(v)
        elif isinstance(o, dict):
            for k, v in o.items():
                if k not in SKIP:
                    walk(v)

    walk(req)
    return "\n".join(parts) or raw


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _send(self, obj, code=200):
        raw = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw))); self.end_headers(); self.wfile.write(raw)

    def do_GET(self):
        self._send({"ok": True, "upstream": UPSTREAM}) if self.path == "/healthz" else self._send({"error": "not found"}, 404)

    def do_POST(self):
        # fail closed on anything odd: no or oversized body, bad JSON, no "input" string. Only {"input": str} is
        # forwarded (plus the project id from this pod's config, never from the caller).
        try:
            n = int(self.headers.get("Content-Length", ""))
        except ValueError:
            return self._send({"outcome": "flagged", "reason": "no Content-Length"})
        if n <= 0 or n > 1048576:
            return self._send({"outcome": "flagged", "reason": "body size"})
        try:
            got = json.loads(self.rfile.read(n))
        except ValueError:
            return self._send({"outcome": "flagged", "reason": "bad json"})
        if not isinstance(got, dict) or not isinstance(got.get("input"), str):
            return self._send({"outcome": "flagged", "reason": "expected {\"input\": str}"})
        body = {"input": scan_text(got["input"])}
        if PROJECT:
            body["project_id"] = PROJECT
        req = urllib.request.Request(UPSTREAM, data=json.dumps(body).encode(), method="POST",
                                     headers={"Content-Type": "application/json", "Authorization": f"Bearer {TOKEN}"})
        try:
            with urllib.request.urlopen(req, timeout=8, context=CTX) as r:
                res = json.loads(r.read() or b"{}")
        except urllib.error.HTTPError as e:
            return self._send({"outcome": "flagged", "reason": f"upstream {e.code}"})
        except Exception as e:
            return self._send({"outcome": "flagged", "reason": type(e).__name__})
        verdict = (res.get("result") or {}).get("outcome") or res.get("outcome") or "flagged"
        self._send({"outcome": "pass" if verdict in ("pass", "cleared") else "flagged", "upstream_outcome": verdict})

    def log_message(self, *a): return


print("calypso-bridge on :8080 ->", UPSTREAM, flush=True)
ThreadingHTTPServer(("0.0.0.0", 8080), H).serve_forever()
