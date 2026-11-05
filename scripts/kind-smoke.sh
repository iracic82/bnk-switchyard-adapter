#!/usr/bin/env bash
# Deploys the adapter and mock model servers into a kind cluster and runs the e2e checks.
# No BNK, no EPP: the adapter calls the model Services directly. This tests the images,
# probes, security context and config mounting, not BNK routing.
set -euo pipefail
cd "$(dirname "$0")/.."
CLUSTER=${KIND_CLUSTER:-sy-validate}
K="kubectl --context kind-$CLUSTER"
kind load docker-image bnk-switchyard-adapter:dev mock-llm:dev --name "$CLUSTER" >/dev/null
# Base minus the BNK-only kinds, plus plain Services and the smoke-test adapter config.
$K kustomize deploy/k8s/base | python3 -c '
import sys, yaml
drop = {"Gateway", "HTTPRoute", "InferencePool", "F5EPP", "ConfigMap"}
docs = [d for d in yaml.safe_load_all(sys.stdin) if d and d["kind"] not in drop]
for d in docs:
    if d["kind"] == "Deployment" and d["metadata"]["name"] == "switchyard-adapter":
        d["spec"]["template"]["spec"]["volumes"][0]["configMap"]["name"] = "switchyard-adapter-config"
    if d["kind"] == "Deployment" and d["metadata"]["name"] == "small-model":
        # Deterministic judge for assertions; the demo keeps the simulated error rate.
        for e in d["spec"]["template"]["spec"]["containers"][0]["env"]:
            if e["name"] == "MOCK_JUDGE_ERROR_RATE":
                e["value"] = "0"
print(yaml.safe_dump_all(docs))' | $K apply -f - >/dev/null
$K apply -f deploy/kind-smoke/services.yaml >/dev/null
$K -n switchyard create configmap switchyard-adapter-config --from-file=adapter.toml=deploy/kind-smoke/adapter.toml --dry-run=client -o yaml | $K apply -f - >/dev/null
$K -n switchyard rollout restart deploy/switchyard-adapter >/dev/null
for d in inference/small-model inference/large-model switchyard/switchyard-adapter; do
  $K -n "${d%/*}" rollout status "deploy/${d#*/}" --timeout=120s
done
# Run the checks from inside the cluster: kubectl port-forward drops its tunnel when the
# adapter refuses an oversized body early, which is the behaviour one check asserts.
$K -n switchyard delete pod e2e --ignore-not-found >/dev/null
$K -n switchyard create configmap e2e --from-file=tests/e2e_local.py --dry-run=client -o yaml | $K apply -f - >/dev/null
$K -n switchyard run e2e --restart=Never --image=python:3.12-alpine \
  --env=ADAPTER_URL=http://switchyard-adapter.switchyard.svc:8080 \
  --overrides='{"spec":{"containers":[{"name":"e2e","image":"python:3.12-alpine","command":["python3","/t/e2e_local.py"],"env":[{"name":"ADAPTER_URL","value":"http://switchyard-adapter.switchyard.svc:8080"}],"volumeMounts":[{"name":"t","mountPath":"/t"}]}],"volumes":[{"name":"t","configMap":{"name":"e2e"}}]}}' >/dev/null
for _ in $(seq 1 150); do
  phase=$($K -n switchyard get pod e2e -o jsonpath='{.status.phase}')
  [[ "$phase" == Succeeded || "$phase" == Failed ]] && break
  sleep 2
done
$K -n switchyard logs e2e
[[ "$($K -n switchyard get pod e2e -o jsonpath='{.status.phase}')" == Succeeded ]]
