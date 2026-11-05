#!/usr/bin/env bash
# ext_proc contract test with a real Envoy: Envoy routes only on x-selected-model, so every
# 200 below proves the adapter decided the model, rewrote the body and made Envoy re-match.
# Needs the local stack (scripts/local-up.sh) and Docker.
set -euo pipefail
cd "$(dirname "$0")/.."
IMAGE=${ENVOY_IMAGE:-envoyproxy/envoy:v1.36-latest}
docker rm -f sy-envoy >/dev/null 2>&1 || true
docker run -d --name sy-envoy --network host \
  -v "$PWD/deploy/envoy/envoy-extproc.yaml:/etc/envoy/envoy.yaml:ro" \
  "$IMAGE" -c /etc/envoy/envoy.yaml --disable-hot-restart >/dev/null
trap 'docker rm -f sy-envoy >/dev/null 2>&1' EXIT
for _ in $(seq 1 50); do
  curl -fsS -o /dev/null http://127.0.0.1:9471/ready 2>/dev/null && break
  sleep 0.2
done
python3 - <<'PY'
import json, sys, urllib.request, urllib.error
fails = []
def call(model, text, stream=False, tools=False):
    body = {"model": model, "stream": stream, "messages": [{"role": "user", "content": text}]}
    if tools:
        body["tools"] = [{"type": "function", "function": {"name": "search", "parameters": {"type": "object"}}}]
    req = urllib.request.Request("http://127.0.0.1:9470/v1/chat/completions", data=json.dumps(body).encode(),
                                 headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, r.headers.get("x-served-by") or "", r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, "", e.read().decode()
def check(name, ok):
    print(("PASS  " if ok else "FAIL  ") + name)
    if not ok:
        fails.append(name)
s, pod, _ = call("auto", "Write a one-line commit message for a typo fix.")
check("auto, easy step -> small pool through Envoy", s == 200 and pod.startswith("small"))
s, pod, _ = call("auto", "Debug this race condition between two workers.")
check("auto, hard step -> large pool through Envoy", s == 200 and pod.startswith("large"))
s, pod, raw = call("auto", "Design a distributed lock and prove it is correct.", stream=True)
check("streaming survives the body rewrite", s == 200 and raw.rstrip().endswith("data: [DONE]"))
s, _, raw = call("auto", "look it up [call-tool]", tools=True)
check("tool definitions survive the body rewrite", s == 200 and '"tool_calls"' in raw)
s, pod, _ = call("large", "hi")
check("explicit model name is labelled for the gateway", s == 200 and pod.startswith("large"))
s, _, _ = call("not-a-model", "hi")
check("unknown name passes through undecided (404 from Envoy)", s == 404)
print("\nALL PASSED" if not fails else f"\nFAILED: {len(fails)}")
sys.exit(1 if fails else 0)
PY
