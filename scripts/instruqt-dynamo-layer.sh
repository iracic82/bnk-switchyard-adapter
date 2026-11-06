#!/usr/bin/env bash
# Adds NVIDIA Dynamo to the Switchyard lab as a third model pool. Run as root on the lab VM after
# scripts/instruqt-kind-layer.sh. Uses the base lab's own manifests (bnk-ai-factory-clab, manifests/ai),
# the ones its install/40-ai.sh applies, which this track does not run as a whole.
#
# What it adds, without changing the small and large pools:
#   dynamo-system   NVIDIA Dynamo platform (operator) with NATS, Dynamo's event plane
#   aigw-models     DynamoGraphDeployment "mocker": Dynamo's frontend and two Mocker workers (GPU-free
#                   engines that model vLLM's scheduling, KV cache and timing), each with a frontend
#                   sidecar in direct mode; InferencePool model-pool and F5EPP model-pool-epp (engine
#                   dynamo: picks the worker from Dynamo's KV events on NATS); Gateway pool-vip (.12),
#                   as in the base lab
#   gpu-operator    run.ai's fake GPU operator: the two workers show 4 x H100 each; KAI scheduler queues
#   router          pool "dynamo" on the pool bridge, model "dynamo" (sy-dynamo) and route "auto-dynamo"
#   Polaris         provider switchyard-dynamo-bridge, model sy-dynamo, key switchyard-dynamo-key
#
#   LAB_ROOT, SY_ROOT, SECRETS_DIR as for instruqt-kind-layer.sh
#   DYN_STAGE=pre    what needs no BNK (Dynamo, NATS, Mocker, GPU simulators), so it can run while BNK
#                    installs; post: the picker, the router and Polaris; unset or "all": both in order
set -uo pipefail
LAB_ROOT=${LAB_ROOT:-/root/lab}; SY_ROOT=${SY_ROOT:-/root/switchyard}; SECRETS_DIR=${SECRETS_DIR:-/root/.bnk}
IMG=${SY_IMAGE_REPO:-docker.io/iracic82}; TAG=${SY_IMAGE_TAG:-0.3.3}
# 0.3.3 is the first router that serves the Dynamo pool: it passes the picker's worker header on, adds an
# output limit Dynamo's Mocker needs, and takes the route header. An older tag (a lab started before it)
# is raised.
case "$TAG" in 0.1.*|0.2.*|0.3.0|0.3.1|0.3.2) echo "   router tag $TAG predates the Dynamo bridge; using 0.3.3"; TAG=0.3.3 ;; esac
export KUBECONFIG=${KUBECONFIG:-$LAB_ROOT/clab/kubeconfig}
# shellcheck disable=SC1091
source "$LAB_ROOT/install/lib/common.sh"
NS=f5-bnk; T=switchyard; ROUTE_VIP=172.16.100.19; SMALL_VIP=172.16.100.26; LARGE_VIP=172.16.100.27
M="$LAB_ROOT/manifests/ai"
DYNAMO=1.5.1; FGO=0.2.1; KAI=v0.18.6
DYN_STAGE=${DYN_STAGE:-all}
stage() { [ "$DYN_STAGE" = all ] || [ "$DYN_STAGE" = "$1" ]; }
# the base lab's Dynamo manifests name aigw-models, the namespace BNK's CNEInstance lists for pickers
POOL_NS=aigw-models
if stage post; then
  p=$(kubectl get cneinstance -A -o jsonpath='{.items[0].spec.eppNamespaces[0]}')
  [ "$p" = "$POOL_NS" ] || fail "the CNEInstance's picker namespace is '$p', the Dynamo manifests need $POOL_NS"
fi
for f in 10-dynamo-mocker.yaml 11-nats-policy.yaml 20-pool-vip.yaml 40-fake-gpu-values.yaml 41-kai-values.yaml 42-queues.yaml; do
  [ -f "$M/$f" ] || fail "$M/$f missing: is $LAB_ROOT the base lab checkout?"
done

if stage pre; then
step "Dynamo: Gateway API Inference Extension CRDs $GAIE_VERSION (BNK applies the same; idempotent)"
kubectl apply -f "https://github.com/kubernetes-sigs/gateway-api-inference-extension/releases/download/$GAIE_VERSION/manifests.yaml" >/dev/null \
  || fail "GAIE CRDs"
step "Dynamo: NVIDIA Dynamo platform $DYNAMO with NATS"
kubectl get ns "$POOL_NS" >/dev/null 2>&1 || kubectl create ns "$POOL_NS" >/dev/null
kubectl label ns "$POOL_NS" aigw.lab/engine=dynamo --overwrite >/dev/null   # NATS admits this namespace
helm repo add prometheus-community https://prometheus-community.github.io/helm-charts --force-update >/dev/null 2>&1 || true
helm repo update prometheus-community >/dev/null 2>&1 || warn "helm repo update prometheus-community failed"
helm upgrade --install dynamo-platform "https://helm.ngc.nvidia.com/nvidia/ai-dynamo/charts/dynamo-platform-$DYNAMO.tgz" \
  -n dynamo-system --create-namespace --set global.nats.install=true --wait --timeout "$(tmo 15m 5m)" >/dev/null \
  || fail "dynamo-platform did not install"
kubectl -n dynamo-system rollout status statefulset/dynamo-platform-nats --timeout=300s >/dev/null || fail "NATS not ready"
kubectl apply -f "$M/11-nats-policy.yaml" >/dev/null
kubectl -n "$POOL_NS" create secret generic hf-token-secret --from-literal=HF_TOKEN= --dry-run=client -o yaml | kubectl apply -f - >/dev/null
ok "Dynamo platform and NATS in dynamo-system"

step "Dynamo: Mocker workers (DynamoGraphDeployment mocker)"
kubectl apply -f "$M/10-dynamo-mocker.yaml" >/dev/null || fail "DynamoGraphDeployment"
kubectl -n "$POOL_NS" wait dynamographdeployment/mocker --for=condition=Ready --timeout="$(tmo 1200s 300s)" >/dev/null \
  && ok "mocker Ready: frontend and two workers" || warn "mocker not Ready yet (continuing)"

step "Dynamo: GPU simulators, fake GPU operator $FGO (4 x H100 per worker) and KAI scheduler $KAI"
for n in bnk-worker bnk-worker2; do kubectl label node $n run.ai/simulated-gpu-node-pool=h100 --overwrite >/dev/null; done
helm upgrade -i gpu-operator oci://ghcr.io/run-ai/fake-gpu-operator/fake-gpu-operator --version $FGO -n gpu-operator --create-namespace \
  -f "$M/40-fake-gpu-values.yaml" --wait --timeout 5m >/dev/null || warn "fake GPU operator did not install"
helm upgrade -i kai-scheduler oci://ghcr.io/kai-scheduler/kai-scheduler/kai-scheduler --version $KAI -n kai-scheduler --create-namespace \
  -f "$M/41-kai-values.yaml" --wait --timeout 5m >/dev/null || warn "KAI scheduler did not install"
for _ in $(seq 1 40); do kubectl apply -f "$M/42-queues.yaml" >/dev/null 2>&1 && break; sleep 5; done
kubectl get nodes -o custom-columns='NODE:.metadata.name,GPU:.status.allocatable.nvidia\.com/gpu,PRODUCT:.metadata.labels.nvidia\.com/gpu\.product'
fi

stage post || exit 0
step "Dynamo: InferencePool model-pool and F5's picker (engine dynamo)"
kubectl apply -f "$M/20-pool-vip.yaml" >/dev/null || fail "pool, picker and VIP"
for _ in $(seq 1 "$(tmo 120 60)"); do kubectl -n "$POOL_NS" get deploy model-pool-epp >/dev/null 2>&1 && break; sleep 5; done
kubectl -n "$POOL_NS" rollout status deploy/model-pool-epp --timeout="$(tmo 600s 180s)" >/dev/null 2>&1 \
  && ok "picker model-pool-epp running" || warn "picker not up yet (the bridge fails open to Dynamo's frontend)"

step "Dynamo: the router learns pool, model and route \"dynamo\""
CFG=$(mktemp)
sed 's/allowed_models = \["small", "large"\]/allowed_models = ["small", "large", "dynamo"]/' "$SY_ROOT/deploy/instruqt/adapter.toml" > "$CFG"
cat "$SY_ROOT/deploy/instruqt/adapter-dynamo.toml" >> "$CFG"
grep -q '"dynamo"\]' "$CFG" || fail "could not add dynamo to the tenant's models"
kubectl kustomize "$SY_ROOT/deploy/k8s/base" | python3 "$SY_ROOT/scripts/render_lab.py" \
  --pool-ns "$POOL_NS" --bnk-ns "$NS" --image-repo "$IMG" --tag "$TAG" \
  --small-vip "$SMALL_VIP" --large-vip "$LARGE_VIP" --gateway-infra f5-bnkgateway --part-of switchyard-lab \
  --adapter-config "$CFG" --kinds ConfigMap | kubectl apply -f - >/dev/null || fail "router config"
rm -f "$CFG"
# the router serves the Dynamo pool from 0.3.3 (see the tag check at the top)
kubectl -n $NS set image deploy/switchyard-router "adapter=$IMG/bnk-switchyard-adapter:$TAG" >/dev/null
kubectl -n $NS rollout restart deploy/switchyard-router >/dev/null
kubectl -n $NS rollout status deploy/switchyard-router --timeout=300s >/dev/null || fail "router not ready with the Dynamo config"
ok "router $TAG: pool dynamo -> model-pool-epp, route auto-dynamo"

step "Dynamo: Polaris provider, model sy-dynamo, and a key that may use it"
API=http://127.0.0.1:18443
curl -fsS -o /dev/null "$API/api/v1/admin/ready" || fail "the management API is not on $API (run instruqt-kind-layer.sh first)"
TOK=$(jq -n --arg u "$(kubectl -n $NS get secret f5-aigateway-admin -o jsonpath='{.data.username}' | base64 -d)" \
            --arg p "$(kubectl -n $NS get secret f5-aigateway-admin -o jsonpath='{.data.password}' | base64 -d)" '{username:$u,password:$p}' \
      | curl -sS "$API/api/v1/auth/login" -H 'Content-Type: application/json' --data-binary @- | jq -er .access_token) || fail "admin login failed"
j() { curl -sS "$@" -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json'; }
TA="$API/api/v1/tenants/$T"
pk_ref() { j "$TA/provider-keys" | jq -r --arg n "$1" '(.data // .items)[]? | select(.name==$n) | .ref' | head -1; }
BRIDGE=http://switchyard-router.$NS.svc.cluster.local:8000/pools
for spec in "dynamo-bridge|$BRIDGE/dynamo/v1" "dynamo-svc|http://mocker-frontend.$POOL_NS.svc.cluster.local:8000/v1"; do
  IFS='|' read -r n url <<<"$spec"
  [ -n "$(pk_ref "$T-$n")" ] || jq -n --arg n "$T-$n" --arg u "$url" '{name:$n,vendor:"Generic",value:"not-needed",enabled:true,generic:{baseURL:$u}}' \
    | j "$TA/provider-keys" --data-binary @- >/dev/null
  [ -n "$(pk_ref "$T-$n")" ] || fail "provider key $T-$n not created"
done
DREF=$(pk_ref "$T-dynamo-bridge")
has_model() { j "$TA/models" | jq -e --arg n "$1" '(.data // .items)[]? | select(.name==$n)' >/dev/null 2>&1; }
if ! has_model sy-dynamo; then
  jq -n --arg p "$DREF" '{name:"sy-dynamo",deployments:[{providerRef:$p,upstream:"Qwen/Qwen3-0.6B",dialects:["openai"]}]}' | j "$TA/models" --data-binary @- >/dev/null
  for _ in $(seq 1 30); do has_model sy-dynamo && break; sleep 2; done
fi
has_model sy-dynamo || fail "model sy-dynamo not created"
for _ in $(seq 1 30); do kubectl -n $NS get aimodels.inference.k8s.f5.com sy-dynamo >/dev/null 2>&1 && break; sleep 2; done
kubectl -n $NS patch aimodels.inference.k8s.f5.com sy-dynamo --type merge -p "$(jq -n --arg p "${DREF##*/}" \
  '{spec:{deployments:[{providerRef:{name:$p},upstream:"Qwen/Qwen3-0.6B",dialects:["openai"],weight:100,state:"Active",pricing:{inputPerMtok:2000000,outputPerMtok:8000000}}]}}')" >/dev/null \
  || warn "could not price sy-dynamo"
# A key is bound to the providers and scope it is made with, so the existing keys stay as they are and a
# new org, team, principal and key carry the three models.
SCOPE='["sy-small","sy-large","sy-dynamo"]'
PKEYS=$(j "$TA/provider-keys" | jq -c --arg t "$T-" '[(.data // .items)[]? | select(.name | startswith($t)) | .ref]')
O="$T-dynamo-org"
j "$TA/governance/orgs" | jq -e --arg i "$O" '(.data // .items)[]? | select(.id==$i)' >/dev/null 2>&1 \
  || jq -n --arg i "$O" --argjson s "$SCOPE" '{id:$i,name:$i,displayName:"Switchyard with Dynamo",modelScope:$s}' | j "$TA/governance/orgs" --data-binary @- >/dev/null
team_id() { j "$TA/governance/teams" | jq -r --arg n "$T-dynamo-apps" --arg o "$O" '(.data // .items)[]? | select(.name==$n and .orgId==$o) | .id' | head -1; }
TEAM=$(team_id)
[ -n "$TEAM" ] || { jq -n --arg n "$T-dynamo-apps" --arg o "$O" --argjson s "$SCOPE" '{name:$n,displayName:"Switchyard apps with Dynamo",orgId:$o,modelScope:$s}' | j "$TA/governance/teams" --data-binary @- >/dev/null; TEAM=$(team_id); }
[ -n "$TEAM" ] || fail "team not created"
WHO="$T-dynamo"; KN="$T-dynamo-key"
j "$TA/governance/principals" | jq -e --arg i "$WHO" '(.data // .items)[]? | select(.id==$i)' >/dev/null 2>&1 \
  || jq -n --arg i "$WHO" --arg t "$TEAM" --argjson s "$SCOPE" '{id:$i,name:$i,kind:"service",teamId:$t,role:"member",active:true,modelScope:$s}' | j "$TA/governance/principals" --data-binary @- >/dev/null
mkdir -p "$SECRETS_DIR/aigw"
if [ ! -s "$SECRETS_DIR/aigw/$KN.key" ]; then
  for id in $(j "$TA/virtual-keys" | jq -r --arg n "$KN" '(.data // .items)[]? | select(.name==$n) | (.id // .ref // empty)'); do j -X DELETE "$TA/virtual-keys/$id" >/dev/null 2>&1 || true; done
  jq -n --arg n "$KN" --arg p "$WHO" --argjson k "$PKEYS" --argjson s "$SCOPE" '{name:$n,principalId:$p,providerKeys:$k,modelScope:$s}' \
    | j "$TA/virtual-keys" --data-binary @- | jq -r '.data.token // .token // empty' > "$SECRETS_DIR/aigw/$KN.key"
  chmod 600 "$SECRETS_DIR/aigw/$KN.key"
fi
[ -s "$SECRETS_DIR/aigw/$KN.key" ] || fail "virtual key $KN not created"
unset TOK
ok "model sy-dynamo on the pool bridge; key in $SECRETS_DIR/aigw/$KN.key (sy key dynamo)"

step "Dynamo: end-to-end check from the client"
DK=$(cat "$SECRETS_DIR/aigw/$KN.key"); OUT=/tmp/sy-dynamo-check.json
ask() { # model prompt [switchyard route]
  docker exec client curl -s -m 60 -o /dev/stdout -w '\n%{http_code}' "http://$ROUTE_VIP/v1/chat/completions" \
    ${3:+-H "x-switchyard-use-route: $3"} \
    -H "Authorization: Bearer $DK" -H 'Content-Type: application/json' -d "{\"model\":\"$1\",\"messages\":[{\"role\":\"user\",\"content\":\"$2\"}]}" \
    > "$OUT" 2>/dev/null; tail -1 "$OUT"; }
code=000
for _ in $(seq 1 36); do code=$(ask sy-dynamo hi); [ "$code" = 200 ] && break; sleep 5; done
if [ "$code" != 200 ]; then
  echo "   last answer: $(head -c 300 "$OUT" | tr '\n' ' ')"
  kubectl -n $NS logs deploy/switchyard-router --tail=8 2>&1 | grep -i dynamo | sed 's/^/   router: /'
  fail "sy-dynamo answered $code through Polaris"
fi
kubectl -n $NS logs deploy/switchyard-router --since=5m | grep -F '"pool bridge forwarded"' | grep -F '"pool":"dynamo"' | tail -1 | jq -c '.fields | {pick, endpoint, status}' \
  | sed 's/^/   last dynamo request: /'
code=000
for _ in $(seq 1 12); do code=$(ask auto "Find the root cause of this distributed deadlock between the queue consumer and the lock service." auto-dynamo); [ "$code" = 200 ] && break; sleep 5; done
[ "$code" = 200 ] && ok "auto-dynamo answered 200: Switchyard chose, F5's picker named the Dynamo worker" \
  || warn "auto-dynamo answered $code (sy-dynamo works; check the router log)"
