#!/usr/bin/env bash
# Validates every manifest set with a strict server-side dry run on a kind cluster.
#   F5EPP_CRD=/path/to/f5epp-crd.yaml  also validates F5EPP resources. Export it from a BNK
#   cluster: kubectl get crd f5-epps.inference.k8s.f5.com -o yaml > f5epp-crd.yaml
#   (strip status and cluster metadata). It is F5's and is not shipped in this repository.
set -euo pipefail
cd "$(dirname "$0")/.."
CLUSTER=${KIND_CLUSTER:-sy-validate}
K="kubectl --context kind-$CLUSTER"
kind get clusters | grep -qx "$CLUSTER" || kind create cluster --name "$CLUSTER" --wait 90s
$K apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.3.0/standard-install.yaml >/dev/null
$K apply --server-side -f https://github.com/kubernetes-sigs/gateway-api-inference-extension/releases/download/v1.4.0/manifests.yaml >/dev/null
SKIP_F5=1
if [[ -n "${F5EPP_CRD:-}" ]]; then
  $K apply --server-side -f "$F5EPP_CRD" >/dev/null
  SKIP_F5=0
fi
for ns in switchyard inference f5-bnk; do $K create ns "$ns" >/dev/null 2>&1 || true; done
filter() { # drop F5EPP when its CRD is not installed
  python3 -c '
import sys, yaml
skip = sys.argv[1] == "1"
docs = [d for d in yaml.safe_load_all(sys.stdin) if d and not (skip and d["kind"] == "F5EPP")]
print(yaml.safe_dump_all(docs))' "$SKIP_F5"
}
status=0
for set in deploy/k8s/base deploy/k8s/overlays/single-internal-gateway; do
  echo "== $set"
  $K kustomize "$set" | filter | $K apply --dry-run=server --validate=strict -f - | sed 's/^/  /' || status=1
done
echo "== deploy/k8s/models-real"
cat deploy/k8s/models-real/*.yaml | filter | $K apply --dry-run=server --validate=strict -f - | sed 's/^/  /' || status=1
[[ $SKIP_F5 == 1 ]] && echo "F5EPP resources not validated (set F5EPP_CRD)."
exit $status
