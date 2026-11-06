#!/usr/bin/env bash
# A real judge for Switchyard: NVIDIA Nemotron on NVIDIA's hosted API (build.nvidia.com). Run as root on
# the lab VM after instruqt-kind-layer.sh (and instruqt-dynamo-layer.sh, which rewrites the router config).
#
#   LLM_API_KEY   an NVIDIA API key (nvapi-...). Anything else, or a key the API refuses, leaves the
#                 lab on its stand-in judge, and says so. The learner can read it (root on the VM,
#                 cluster access): use a key made for the track, with a small quota.
#   JUDGE_MODEL   default nvidia/nemotron-3.5-lightning-30b-a3b
#
# What it adds: Secret f5-bnk/switchyard-judge with the key, the router's NVIDIA_API_KEY from it, and
# model "nemotron" in the router config. The routes keep the stand-in judge until `sy judge nemotron`:
# the hosted API is rate-limited, and a benchmark sends hundreds of judge calls.
set -uo pipefail
LAB_ROOT=${LAB_ROOT:-/root/lab}
export KUBECONFIG=${KUBECONFIG:-$LAB_ROOT/clab/kubeconfig}
# shellcheck disable=SC1091
source "$LAB_ROOT/install/lib/common.sh"
NS=f5-bnk; CM=switchyard-router-config
JUDGE_MODEL=${JUDGE_MODEL:-nvidia/nemotron-3.5-lightning-30b-a3b}
API=https://integrate.api.nvidia.com/v1
KEY=${LLM_API_KEY:-${NVIDIA_API_KEY:-}}

step "Judge: NVIDIA Nemotron on NVIDIA's API ($JUDGE_MODEL)"
case "$KEY" in
  nvapi-*) ;;
  "") warn "no LLM_API_KEY: the lab keeps its stand-in judge"; exit 0 ;;
  *) warn "LLM_API_KEY is not an NVIDIA key (nvapi-...): the lab keeps its stand-in judge"; exit 0 ;;
esac

# One verdict with Switchyard's own contract (strict JSON schema, its rule and boundary enums), timed.
probe=$(jq -n --arg m "$JUDGE_MODEL" '{
  model: $m, max_tokens: 200, temperature: 0,
  messages: [
    {role: "system", content: "You judge whether a small language model can solve a task. Reply with the JSON verdict only."},
    {role: "user", content: "Task: Translate good morning into Spanish."}],
  response_format: {type: "json_schema", json_schema: {name: "CapabilityClassifierDecision", strict: true, schema: {
    type: "object", additionalProperties: false, required: ["crux", "primary_rule", "capability_boundary", "p_solve"],
    properties: {
      crux: {type: "string"},
      primary_rule: {type: "string", enum: ["SUP-1", "SUP-2", "SUP-3", "SUP-4", "SUP-5", "UNC-1", "UNC-2", "LIM-1", "LIM-2", "none"]},
      capability_boundary: {type: "string", enum: ["supported", "uncertain", "unsupported", "unmatched"]},
      p_solve: {type: "number", minimum: 0, maximum: 1}}}}}}')
# The key goes to curl and kubectl on stdin or from a 0600 file, never on a command line (ps shows those).
umask 077; HDR=$(mktemp); trap 'rm -f "$HDR"' EXIT
printf 'Authorization: Bearer %s\n' "$KEY" > "$HDR"
t0=$(date +%s%N)
code=$(curl -sS -m 30 -o /tmp/judge-probe.json -w '%{http_code}' "$API/chat/completions" \
  -H @"$HDR" -H 'Content-Type: application/json' -d "$probe" 2>/dev/null || echo 000)
ms=$(( ($(date +%s%N) - t0) / 1000000 ))
verdict=$(jq -r '.choices[0].message.content // empty' /tmp/judge-probe.json 2>/dev/null)
if [ "$code" != 200 ] || ! echo "$verdict" | jq -e '.p_solve | numbers' >/dev/null 2>&1; then
  warn "NVIDIA's API answered $code in ${ms} ms without a usable verdict: the lab keeps its stand-in judge"
  head -c 300 /tmp/judge-probe.json | sed 's/^/   /'; echo
  rm -f /tmp/judge-probe.json; exit 0
fi
rm -f /tmp/judge-probe.json
ok "Nemotron answered a verdict in ${ms} ms: $verdict"

# the key reaches the router as an environment variable from a Secret, never through the ConfigMap
# Anyone with this cluster's credentials can read it, as with any Secret: on a lab that is the learner.
# Give the track a key of its own with a small quota, not a personal or production one.
printf '%s' "$KEY" | kubectl -n $NS create secret generic switchyard-judge --from-file=NVIDIA_API_KEY=/dev/stdin \
  --dry-run=client -o yaml | kubectl apply -f - >/dev/null || fail "Secret switchyard-judge"
kubectl -n $NS set env deploy/switchyard-router --from=secret/switchyard-judge >/dev/null || fail "router env"

cfg=$(kubectl -n $NS get cm $CM -o jsonpath='{.data.adapter\.toml}')
if ! grep -q '^\[models.nemotron\]' <<<"$cfg"; then
  cfg="$cfg
# --- Appended by scripts/instruqt-judge.sh: a real judge, NVIDIA Nemotron on NVIDIA's hosted API. ---
# Used by a route when its judge is \"nemotron\" (sy judge nemotron). Never answers a request itself.
[models.nemotron]
provider_model = \"$JUDGE_MODEL\"
base_url = \"$API\"
api_key_env = \"NVIDIA_API_KEY\"
timeout_secs = 20
max_retries = 1
cost = { input_per_1m_usd = 0.10, output_per_1m_usd = 0.40 }
"
  kubectl -n $NS patch cm $CM --type merge -p "$(jq -n --arg v "$cfg" '{data:{"adapter.toml":$v}}')" >/dev/null \
    || fail "router config"
fi
kubectl -n $NS rollout restart deploy/switchyard-router >/dev/null
kubectl -n $NS rollout status deploy/switchyard-router --timeout=180s >/dev/null || fail "router not ready with the judge model"
ok "judge \"nemotron\" available; 'sy judge nemotron' switches Switchyard to it, 'sy judge mock' back"
