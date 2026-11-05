#!/usr/bin/env bash
# Adds the Switchyard demo to a cluster built by the bnk-ai-demos setup (BNK 2.4, GAIE CRDs,
# TMM VLANs, Prometheus and Grafana in f5-analytics). Run on the control-plane host.
#
#   SY_IMAGE_REPO  registry holding bnk-switchyard-adapter and mock-llm (required)
#   SY_IMAGE_TAG   image tag (default 0.1.0)
#   SY_POOL_NS     namespace for the pools; must already be in CNEInstance.spec.eppNamespaces
#                  and hold far-pull-secret (default sim, which bnk-ai-demos sets up)
set -uo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMG=${SY_IMAGE_REPO:?set SY_IMAGE_REPO to the registry with the adapter and mock images}
TAG=${SY_IMAGE_TAG:-0.1.0}
NS=${SY_POOL_NS:-sim}

step() { printf '\n==> %s\n' "$*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }
render() { kubectl kustomize "$ROOT/deploy/k8s/base" | python3 "$ROOT/scripts/retarget.py" --pool-namespace "$NS" --image-repo "$IMG" --tag "$TAG" "$@"; }
apply_retry() { # kinds
  for i in $(seq 1 20); do
    render --kinds "$1" | kubectl apply -f - && return 0
    echo "  retry $i for $1"; sleep 6
  done
  return 1
}

step "preconditions: $NS is EPP-enabled and has the F5 pull secret"
kubectl get cneinstance -A -o jsonpath='{.items[*].spec.eppNamespaces}' | grep -qw "$NS" \
  || fail "$NS is not in CNEInstance.spec.eppNamespaces (set it at BNK install time)"
kubectl -n "$NS" get secret far-pull-secret >/dev/null || fail "far-pull-secret missing in $NS"

step "adapter namespace, config and deployment"
apply_retry Namespace,ServiceAccount,ConfigMap,Service,Deployment || fail "adapter and model servers"

# F5 docs order: F5EPP, then the InferencePool that names it, then Gateways and routes.
step "F5EPPs"
apply_retry F5EPP || fail "F5EPP"
step "InferencePools"
apply_retry InferencePool || fail "InferencePool"
step "Gateways and HTTPRoutes"
apply_retry Gateway,HTTPRoute || fail "Gateway/HTTPRoute"

step "waiting for the data path"
kubectl -n switchyard rollout status deploy/switchyard-adapter --timeout=300s || fail "adapter not ready"
kubectl -n "$NS" rollout status deploy/small-model --timeout=300s || fail "small-model not ready"
kubectl -n "$NS" rollout status deploy/large-model --timeout=300s || fail "large-model not ready"
for gw in switchyard-external inference-small inference-large; do
  kubectl -n f5-bnk wait --for=condition=Programmed "gateway/$gw" --timeout=300s || fail "gateway $gw not programmed"
done
for epp in small-pool-epp large-pool-epp; do
  kubectl -n "$NS" wait --for=condition=Available "f5epp/$epp" --timeout=300s || echo "  warning: $epp not Available yet"
done

step "Prometheus: scrape the adapter and the model servers"
CFG=$(kubectl -n f5-analytics get cm prometheus-config -o jsonpath='{.data.prometheus\.yaml}' 2>/dev/null || true)
if [[ -n "$CFG" ]] && ! grep -q switchyard-adapter <<<"$CFG"; then
  python3 - "$NS" <<'EOF' > /tmp/sy-prom.yaml
import sys, subprocess, yaml
ns = sys.argv[1]
cfg = yaml.safe_load(subprocess.check_output(["kubectl", "-n", "f5-analytics", "get", "cm", "prometheus-config",
                                              "-o", "jsonpath={.data.prometheus\\.yaml}"]))
cfg.setdefault("scrape_configs", []).extend([
    {"job_name": "switchyard-adapter", "scrape_interval": "5s",
     "static_configs": [{"targets": ["switchyard-adapter.switchyard.svc:8080"]}]},
    {"job_name": "model-servers", "scrape_interval": "5s",
     "kubernetes_sd_configs": [{"role": "pod", "namespaces": {"names": [ns]}}],
     "relabel_configs": [
         {"source_labels": ["__meta_kubernetes_pod_label_app"], "regex": "(small|large)-model", "action": "keep"},
         {"source_labels": ["__meta_kubernetes_pod_label_app"], "regex": "(small|large)-model", "replacement": "$1", "target_label": "pool"},
         {"source_labels": ["__meta_kubernetes_pod_ip"], "replacement": "$1:8000", "target_label": "__address__"},
         {"source_labels": ["__meta_kubernetes_pod_name"], "target_label": "instance"}]},
])
print(yaml.safe_dump({"apiVersion": "v1", "kind": "ConfigMap",
                      "metadata": {"name": "prometheus-config", "namespace": "f5-analytics"},
                      "data": {"prometheus.yaml": yaml.safe_dump(cfg, sort_keys=False)}}))
EOF
  kubectl apply -f /tmp/sy-prom.yaml && kubectl -n f5-analytics rollout restart deploy/prometheus \
    || echo "  warning: Prometheus not updated; dashboards will be empty"
fi

step "Grafana: add the Switchyard dashboard"
python3 - "$ROOT/observability/grafana/dashboards/switchyard.json" > /tmp/sy-dashboard.json <<'EOF'
import json, sys
d = json.load(open(sys.argv[1]))
for p in d["panels"]:
    p.pop("datasource", None)  # use the lab's default Prometheus datasource
json.dump(d, sys.stdout)
EOF
kubectl -n f5-analytics get cm grafana-dashboard -o json \
  | python3 -c 'import json,sys; c=json.load(sys.stdin); c["data"]["switchyard.json"]=open("/tmp/sy-dashboard.json").read(); [c["metadata"].pop(k,None) for k in ("resourceVersion","uid","creationTimestamp","managedFields")]; print(json.dumps(c))' \
  | kubectl apply -f - || echo "  warning: dashboard not added"

step "smoke test through BNK"
VIP=$(kubectl -n f5-bnk get gateway switchyard-external -o jsonpath='{.spec.addresses[0].value}')
for i in $(seq 1 30); do
  code=$(curl -s -o /dev/null -w '%{http_code}' -m 20 "http://$VIP/v1/chat/completions" \
    -H 'Authorization: Bearer sk-demo-eng-agents' -H 'content-type: application/json' \
    -d '{"model":"small","messages":[{"role":"user","content":"hi"}]}')
  [[ "$code" == 200 ]] && { echo "  200 from http://$VIP"; exit 0; }
  sleep 5
done
fail "no 200 from the external VIP http://$VIP (last status $code)"
