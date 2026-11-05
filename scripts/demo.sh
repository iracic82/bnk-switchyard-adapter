#!/usr/bin/env bash
# Narrated walkthrough. Works against the local stack (default) or a BNK external VIP:
#   ADAPTER_URL=http://172.16.201.110 scripts/demo.sh
set -euo pipefail
URL=${ADAPTER_URL:-http://127.0.0.1:9480}
ENG=sk-demo-eng-agents
SUPPORT=sk-demo-support-agents
RESEARCH=sk-demo-research

bold=$'\e[1m'; dim=$'\e[2m'; reset=$'\e[0m'

ask() { # title, key, model, prompt
  local title=$1 key=$2 model=$3 prompt=$4
  printf '\n%s%s%s\n%s  model=%s  prompt: %s%s\n' "$bold" "$title" "$reset" "$dim" "$model" "$prompt" "$reset"
  python3 - "$URL" "$key" "$model" "$prompt" <<'EOF'
import json, sys, time, urllib.request, urllib.error
url, key, model, prompt = sys.argv[1:]
body = json.dumps({"model": model, "messages": [{"role": "user", "content": prompt}]}).encode()
req = urllib.request.Request(f"{url}/v1/chat/completions", data=body, method="POST",
                             headers={"authorization": f"Bearer {key}", "content-type": "application/json"})
start = time.perf_counter()
try:
    with urllib.request.urlopen(req, timeout=120) as r:
        h = r.headers; data = json.load(r)
        usage = data.get("usage", {})
        print(f"  \033[32m{r.status}\033[0m  Switchyard chose \033[1m{h.get('x-switchyard-model')}\033[0m"
              f" ({h.get('x-switchyard-algorithm')}, decided in {h.get('x-switchyard-decision-ms')} ms)")
        print(f"       BNK pool {h.get('x-switchyard-inference-pool')} -> endpoint {h.get('x-served-by', 'n/a')}"
              f"  model {data.get('model')}")
        extra = f"  policy={h['x-switchyard-policy']}" if h.get("x-switchyard-policy") else ""
        print(f"       tokens in/out {usage.get('prompt_tokens')}/{usage.get('completion_tokens')}"
              f"  total {time.perf_counter() - start:.2f}s{extra}")
except urllib.error.HTTPError as e:
    err = json.load(e).get("error", {})
    print(f"  \033[31m{e.code}\033[0m  {err.get('code')}: {err.get('message')}")
EOF
}

echo "${bold}Right model, right GPU, right tenant.${reset}  Target: $URL"
echo "${dim}Switchyard picks the model. BNK's endpoint picker picks the GPU replica in that model's pool.${reset}"

ask "1. A coding agent asks something easy" "$ENG" auto "Write a one-line commit message for a typo fix."
ask "2. The same agent hits a hard step" "$ENG" auto "Debug this race condition: two workers update the same balance."
ask "3. A support bot asks for the frontier model directly" "$SUPPORT" large "Help me with my order."
ask "4. The support bot uses auto: it may only use the small model" "$SUPPORT" auto "Design a migration plan for our ticket system."
ask "5. Research may call the large model by name, but not auto" "$RESEARCH" auto "Summarize this paper."
ask "   ...and by name it works" "$RESEARCH" large "Summarize this paper."
ask "6. Nobody gets in without a key" "" auto "hello"

printf '\n%sWhere it is measured:%s %s/metrics (sy_adapter_*), traces via OTLP, pools via vllm:* on the model servers.\n' "$bold" "$reset" "$URL"
