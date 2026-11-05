#!/usr/bin/env bash
# Adds Switchyard to a one-VM BNK lab built by bnk-ai-factory-clab's base install (fabric, kind,
# BNK 2.4.2 with the AI gateway "Polaris", TMM on the fabric). Run as root on the lab VM.
#
#   LAB_ROOT      the base lab checkout (its install/lib helpers are sourced, not changed)
#   SY_ROOT       this repository
#   SECRETS_DIR   where virtual keys are written (default /root/.bnk)
#   SY_IMAGE_REPO, SY_IMAGE_TAG   public images (default docker.io/iracic82, 0.3.1)
#
# What it builds:
#   aigw-models   two mock model pools, an InferencePool and an F5EPP each
#   f5-bnk        internal Gateways inference-small (.26) and inference-large (.27), and the
#                 Switchyard router (HTTP 8080 for the live view, ext_proc 50051, pool bridge 8000)
#   Polaris       tenant "switchyard": models sy-small and sy-large on the pool bridge, an
#                 engineering key, a budget-limited pilot key, a route on 172.16.100.19, and
#                 Switchyard attached to the route's profile as its model router
#
# Polaris providers are URLs, so a model cannot name an InferencePool. The models point at the
# router's pool bridge (/pools/<pool>/v1), which asks the pool's F5 EPP for a pod the way TMM does
# and forwards there. BNK's VIP range on this lab is 172.16.100.10-29.
set -uo pipefail
LAB_ROOT=${LAB_ROOT:-/root/lab}; SY_ROOT=${SY_ROOT:-/root/switchyard}; SECRETS_DIR=${SECRETS_DIR:-/root/.bnk}
IMG=${SY_IMAGE_REPO:-docker.io/iracic82}; TAG=${SY_IMAGE_TAG:-0.3.1}
export KUBECONFIG=${KUBECONFIG:-$LAB_ROOT/clab/kubeconfig}
# shellcheck disable=SC1091
source "$LAB_ROOT/install/lib/common.sh"
# shellcheck disable=SC1091
source "$LAB_ROOT/install/lib/expose.sh"
NS=f5-bnk
T=switchyard; ROUTE_VIP=172.16.100.19; SMALL_VIP=172.16.100.26; LARGE_VIP=172.16.100.27

step "Switchyard: preconditions"
POOL_NS=$(kubectl get cneinstance -A -o jsonpath='{.items[0].spec.eppNamespaces[0]}')
[ -n "$POOL_NS" ] || fail "no eppNamespaces on the CNEInstance"
kubectl get ns "$POOL_NS" >/dev/null 2>&1 || kubectl create ns "$POOL_NS" >/dev/null
if ! kubectl -n "$POOL_NS" get secret far-pull-secret >/dev/null 2>&1; then
  kubectl -n $NS get secret far-pull-secret -o json \
    | jq --arg ns "$POOL_NS" 'del(.metadata.uid,.metadata.resourceVersion,.metadata.creationTimestamp,.metadata.managedFields,.metadata.ownerReferences) | .metadata.namespace=$ns' \
    | kubectl apply -f - >/dev/null || fail "could not copy far-pull-secret into $POOL_NS"
fi
ok "pools go to $POOL_NS (EPP-enabled), the router to $NS"

render() {
  kubectl kustomize "$SY_ROOT/deploy/k8s/base" | python3 "$SY_ROOT/scripts/render_lab.py" \
    --pool-ns "$POOL_NS" --bnk-ns "$NS" --image-repo "$IMG" --tag "$TAG" \
    --small-vip "$SMALL_VIP" --large-vip "$LARGE_VIP" --gateway-infra f5-bnkgateway --part-of switchyard-lab \
    --adapter-config "$SY_ROOT/deploy/instruqt/adapter.toml" --kinds "$1"
}
apply_retry() {
  for i in $(seq 1 20); do render "$1" | kubectl apply -f - >/dev/null && return 0; echo "   retry $i: $1"; sleep 6; done
  return 1
}

step "Switchyard: model pools, endpoint pickers, internal Gateways, router"
apply_retry ServiceAccount,ConfigMap,Service,Deployment || fail "router and model servers"
# F5's order: the F5EPP, then the InferencePool that names it, then Gateways and routes
apply_retry F5EPP || fail "F5EPP"
apply_retry InferencePool || fail "InferencePool"
apply_retry Gateway,HTTPRoute || fail "Gateways and HTTPRoutes"
kubectl -n $NS rollout status deploy/switchyard-router --timeout=300s >/dev/null || fail "router not ready"
for d in small-model large-model; do kubectl -n "$POOL_NS" rollout status "deploy/$d" --timeout=300s >/dev/null || fail "$d not ready"; done
for gw in inference-small inference-large; do
  kubectl -n $NS wait --for=condition=Programmed "gateway/$gw" --timeout=300s >/dev/null || fail "gateway $gw not Programmed"
done
for e in small-pool-epp large-pool-epp; do
  kubectl -n "$POOL_NS" wait --for=condition=Available "f5epp/$e" --timeout=300s >/dev/null || warn "$e not Available yet"
done
ok "pools on $SMALL_VIP and $LARGE_VIP, router Service $NS/switchyard-router"

step "Switchyard: Polaris (AI gateway) Reconciled, then its management API"
# as the base lab's 40-ai.sh does, which this track does not run
r=""
for _ in $(seq 1 "$(tmo 90 18)"); do
  r=$(kubectl -n $NS get aigateways.k8s.f5.com -o jsonpath='{.items[0].status.conditions[?(@.type=="Reconciled")].status}' 2>/dev/null || true)
  [ "$r" = True ] && break; sleep 20
done
[ "$r" = True ] || fail "AIGateway not Reconciled: kubectl -n $NS get aigateways.k8s.f5.com -o yaml"
kubectl -n $NS wait --for=condition=Ready pod/f5-aigateway-clickhouse-0 --timeout="$(tmo 600s 180s)" >/dev/null || warn "ClickHouse not Ready"
# the controller checks ClickHouse once at start and may have lost that race: one restart after ClickHouse is up
kubectl -n $NS rollout restart deploy/f5-cne-controller >/dev/null 2>&1 || true
kubectl -n $NS rollout status deploy/f5-cne-controller --timeout=300s >/dev/null 2>&1 || true
ok "AIGateway Reconciled"
expose_service polaris $NS f5-aigateway-console 3000
LAB_PUBLIC=0 expose_service aigw-api $NS f5-cne-controller-api 18443
API=http://127.0.0.1:18443
for _ in $(seq 1 60); do curl -fsS -o /dev/null "$API/api/v1/admin/ready" && break; sleep 3; done
curl -fsS -o /dev/null "$API/api/v1/admin/ready" || fail "the management API is not ready on $API"
TOK=$(jq -n --arg u "$(kubectl -n $NS get secret f5-aigateway-admin -o jsonpath='{.data.username}' | base64 -d)" \
            --arg p "$(kubectl -n $NS get secret f5-aigateway-admin -o jsonpath='{.data.password}' | base64 -d)" '{username:$u,password:$p}' \
      | curl -sS "$API/api/v1/auth/login" -H 'Content-Type: application/json' --data-binary @- | jq -er .access_token) || fail "admin login failed"
j() { curl -sS "$@" -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json'; }
TA="$API/api/v1/tenants/$T"

step "Switchyard: Polaris tenant, providers and models"
j "$API/api/v1/tenants" | jq -e --arg n "$T" '(.data // .items)[]? | select(.name==$n)' >/dev/null 2>&1 \
  || jq -n --arg n "$T" '{name:$n,displayName:"Switchyard demo"}' | j "$API/api/v1/tenants" --data-binary @- >/dev/null
kubectl -n $NS wait --for=condition=Ready "aitenants.inference.k8s.f5.com/$T" --timeout=180s >/dev/null || warn "tenant $T not Ready"
pk_ref() { j "$TA/provider-keys" | jq -r --arg n "$1" '(.data // .items)[]? | select(.name==$n) | .ref' | head -1; }
declare -A REF=()
# Two providers per pool: the router's pool bridge (it asks the pool's F5 EPP for the pod), and
# straight to the Service. The models start on the bridge; a check below falls back if it fails.
BRIDGE=http://switchyard-router.$NS.svc.cluster.local:8000/pools
for spec in "small-bridge|$BRIDGE/small/v1" "large-bridge|$BRIDGE/large/v1" \
            "small-svc|http://small-model.$POOL_NS.svc.cluster.local:8000/v1" "large-svc|http://large-model.$POOL_NS.svc.cluster.local:8000/v1"; do
  IFS='|' read -r n url <<<"$spec"; ref=$(pk_ref "$T-$n")
  if [ -z "$ref" ]; then
    jq -n --arg n "$T-$n" --arg u "$url" '{name:$n,vendor:"Generic",value:"not-needed",enabled:true,generic:{baseURL:$u}}' \
      | j "$TA/provider-keys" --data-binary @- >/dev/null
    ref=$(pk_ref "$T-$n")
  fi
  [ -n "$ref" ] || fail "provider key $T-$n not created"
  REF[$n]=$ref
done
has_model() { j "$TA/models" | jq -e --arg n "$1" '(.data // .items)[]? | select(.name==$n)' >/dev/null 2>&1; }
for spec in "sy-small|small-bridge|Qwen/Qwen3-0.6B|100000|400000" "sy-large|large-bridge|Qwen/Qwen3-32B|2000000|8000000"; do
  IFS='|' read -r name prov up in out <<<"$spec"
  if ! has_model "$name"; then
    jq -n --arg n "$name" --arg p "${REF[$prov]}" --arg u "$up" '{name:$n,deployments:[{providerRef:$p,upstream:$u,dialects:["openai"]}]}' \
      | j "$TA/models" --data-binary @- >/dev/null
    for _ in $(seq 1 30); do has_model "$name" && break; sleep 2; done
  fi
  has_model "$name" || fail "model $name not created"
  # prices (micro-USD per million tokens) so Polaris records a cost per request, as the adapter does
  for _ in $(seq 1 30); do kubectl -n $NS get aimodels.inference.k8s.f5.com "$name" >/dev/null 2>&1 && break; sleep 2; done
  kubectl -n $NS patch aimodels.inference.k8s.f5.com "$name" --type merge -p "$(jq -n --arg p "${REF[$prov]##*/}" --arg u "$up" --argjson i "$in" --argjson o "$out" \
    '{spec:{deployments:[{providerRef:{name:$p},upstream:$u,dialects:["openai"],weight:100,state:"Active",pricing:{inputPerMtok:$i,outputPerMtok:$o}}]}}')" >/dev/null \
    || warn "could not price $name"
done
ok "models sy-small and sy-large"

step "Switchyard: keys (engineering, and a pilot with a token budget)"
mkdir -p "$SECRETS_DIR/aigw"; chmod 700 "$SECRETS_DIR/aigw"
SCOPE='["sy-small","sy-large"]'
# a key is bound to the providers named when it is made: name all four, so the fallback keeps it valid
PKEYS=$(printf '%s\n' "${REF[@]}" | jq -Rsc 'split("\n") | map(select(length>0))')
j "$TA/governance/orgs" | jq -e --arg i "$T-org" '(.data // .items)[]? | select(.id==$i)' >/dev/null 2>&1 \
  || jq -n --arg i "$T-org" --argjson s "$SCOPE" '{id:$i,name:$i,displayName:"Switchyard demo",modelScope:$s}' | j "$TA/governance/orgs" --data-binary @- >/dev/null
team_id() { j "$TA/governance/teams" | jq -r --arg n "$T-apps" --arg o "$T-org" '(.data // .items)[]? | select(.name==$n and .orgId==$o) | .id' | head -1; }
TEAM=$(team_id)
[ -n "$TEAM" ] || { jq -n --arg n "$T-apps" --arg o "$T-org" --argjson s "$SCOPE" '{name:$n,displayName:"Switchyard apps",orgId:$o,modelScope:$s}' | j "$TA/governance/teams" --data-binary @- >/dev/null; TEAM=$(team_id); }
[ -n "$TEAM" ] || fail "team not created"
key() {   # principal keyname scope
  local who=$1 kn=$2 scope=$3 id
  j "$TA/governance/principals" | jq -e --arg i "$who" '(.data // .items)[]? | select(.id==$i)' >/dev/null 2>&1 \
    || jq -n --arg i "$who" --arg t "$TEAM" --argjson s "$scope" '{id:$i,name:$i,kind:"service",teamId:$t,role:"member",active:true,modelScope:$s}' | j "$TA/governance/principals" --data-binary @- >/dev/null
  if [ ! -s "$SECRETS_DIR/aigw/$kn.key" ]; then
    for id in $(j "$TA/virtual-keys" | jq -r --arg n "$kn" '(.data // .items)[]? | select(.name==$n) | (.id // .ref // empty)'); do j -X DELETE "$TA/virtual-keys/$id" >/dev/null 2>&1 || true; done
    jq -n --arg n "$kn" --arg p "$who" --argjson k "$PKEYS" --argjson s "$scope" '{name:$n,principalId:$p,providerKeys:$k,modelScope:$s}' \
      | j "$TA/virtual-keys" --data-binary @- | jq -r '.data.token // .token // empty' > "$SECRETS_DIR/aigw/$kn.key"
    chmod 600 "$SECRETS_DIR/aigw/$kn.key"
  fi
  [ -s "$SECRETS_DIR/aigw/$kn.key" ] || fail "virtual key $kn not created"
}
key "$T-eng" "$T-eng-key" "$SCOPE"
key "$T-pilot" "$T-pilot-key" "$SCOPE"
PILOT_VK=$(j "$TA/virtual-keys" | jq -r --arg n "$T-pilot-key" '(.data // .items)[]? | select(.name==$n) | .id' | head -1)
if [ -n "$PILOT_VK" ] && ! j "$TA/governance/vks/$PILOT_VK/budgets" | jq -e '(.items // .data // [])[0]' >/dev/null 2>&1; then
  jq -n --arg o "$PILOT_VK" '{ownerKind:"vk",ownerId:$o,resetPeriod:"hourly",maxTokens:2000,action:"block"}' | j "$TA/governance/budgets" --data-binary @- >/dev/null \
    || warn "pilot budget not created"
fi
ok "keys in $SECRETS_DIR/aigw/$T-eng-key.key and $T-pilot-key.key (2,000 tokens an hour)"

step "Switchyard: route on $ROUTE_VIP and the model router"
kubectl -n $NS get gateway "$T-ai" >/dev/null 2>&1 \
  || jq -n --arg n "$T-ai" --arg a "$ROUTE_VIP" '{name:$n,address:$a,gatewayClassName:"f5-gateway-class"}' | j "$TA/routes" --data-binary @- >/dev/null
kubectl -n $NS wait --for=condition=Programmed "gateway/$T-ai" --timeout=240s >/dev/null || fail "gateway $T-ai not Programmed"
for _ in $(seq 1 30); do kubectl -n $NS get aigatewayprofiles.inference.k8s.f5.com "$T-ai" >/dev/null 2>&1 && break; sleep 2; done
r=$(j -X PUT "$TA/profiles/$T-ai/semantic-router" --data-binary '{"name":"switchyard-router","port":50051,"timeoutMs":5000}')
echo "   router hook: $(echo "$r" | head -c 200)"
kubectl -n $NS wait --for=condition=Programmed "aigatewayprofiles.inference.k8s.f5.com/$T-ai" --timeout=180s >/dev/null \
  || fail "profile $T-ai not Programmed with the router"
ok "model \"auto\" on $ROUTE_VIP is decided by Switchyard"

step "Switchyard: end-to-end check from the client, and the upstream path"
ENG=$(cat "$SECRETS_DIR/aigw/$T-eng-key.key")
OUT=/tmp/sy-check.json
ask() { docker exec client curl -s -m 60 -o /dev/stdout -w '\n%{http_code}' "http://$ROUTE_VIP/v1/chat/completions" \
          -H "Authorization: Bearer $ENG" -H 'Content-Type: application/json' -d "{\"model\":\"$1\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}" \
          > "$OUT" 2>/dev/null; tail -1 "$OUT"; }
# Polaris takes a while to push new models, keys and the router hook to the data plane: retry before judging
ask_until_200() { local code=000 i; for i in $(seq 1 "${2:-36}"); do code=$(ask "$1"); [ "$code" = 200 ] && break; sleep 5; done; echo "$code"; }
diag() {
  echo "   last answer: $(head -c 300 "$OUT" | tr '\n' ' ')"
  kubectl -n $NS logs deploy/switchyard-router --tail=8 2>&1 | sed 's/^/   router: /'
}
code=$(ask_until_200 sy-small)
if [ "$code" != 200 ]; then
  warn "sy-small through the pool bridge answered $code for 3 minutes; trying the models on the Services (no EPP on this path)"; diag
  for spec in "sy-small|small-svc|Qwen/Qwen3-0.6B|100000|400000" "sy-large|large-svc|Qwen/Qwen3-32B|2000000|8000000"; do
    IFS='|' read -r name prov up in out <<<"$spec"
    kubectl -n $NS patch aimodels.inference.k8s.f5.com "$name" --type merge -p "$(jq -n --arg p "${REF[$prov]##*/}" --arg u "$up" --argjson i "$in" --argjson o "$out" \
      '{spec:{deployments:[{providerRef:{name:$p},upstream:$u,dialects:["openai"],weight:100,state:"Active",pricing:{inputPerMtok:$i,outputPerMtok:$o}}]}}')" >/dev/null
  done
  code=$(ask_until_200 sy-small 24)
  echo "service" > "$SECRETS_DIR/aigw/upstream-path"
else
  echo "bridge" > "$SECRETS_DIR/aigw/upstream-path"
fi
[ "$code" = 200 ] || { diag; fail "sy-small answered $code through Polaris"; }
code=$(ask_until_200 auto 12); [ "$code" = 200 ] || { diag; fail "model auto answered $code through Polaris and Switchyard"; }
ok "auto answered 200 through Polaris + Switchyard; upstream path: $(cat "$SECRETS_DIR/aigw/upstream-path")"
unset TOK

step "Switchyard: live view on port 8090, helper command"
expose_service switchyard-live $NS switchyard-router 8090 8080
install -m 0755 "$SY_ROOT/instruqt/bnk-switchyard-routing/scripts/sy" /usr/local/bin/sy
ok "live view on :8090, 'sy' helper installed"
