---
slug: model-to-pool
id: mzmzxrxe7w7c
type: challenge
title: From model name to GPU pod
teaser: A Polaris model is a URL, not a pool. Follow one request from the AI gateway to the pod and see the picker spread load.
notes:
- type: text
  contents: |-
    The AI gateway (Polaris) knows models by name, and each model points at a provider, which is a
    URL. An InferencePool is not a URL: it is a set of pods plus an endpoint picker that chooses
    between them, and BNK's data plane consults that picker when an HTTPRoute sends traffic to the
    pool.

    In BNK 2.4.2 a Polaris provider cannot name an InferencePool. This lab bridges the two: the
    providers point at a small "pool bridge" in the Switchyard router, which asks the pool's F5
    Endpoint Picker for a pod the same way BNK's data plane does, and forwards there.
tabs:
- title: Terminal
  type: terminal
  hostname: lab
- title: Switchyard live view
  type: service
  hostname: lab
  path: /live
  port: 8090
- title: AI Gateway console
  type: service
  hostname: lab
  port: 3000
difficulty: intermediate
timelimit: 1500
enhanced_loading: null
---

Follow `sy-small` from the gateway's address to its pods. Run the block and read it top to
bottom:

```bash,run
NS=f5-bnk
echo "route address:      $(kubectl -n $NS get gateway switchyard-ai -o jsonpath='{.spec.addresses[0].value}')"
echo "model router:       $(kubectl -n $NS get aigatewayprofiles.inference.k8s.f5.com switchyard-ai -o jsonpath='{.spec.modelRouting.semanticRouter.serviceRef}')"
P=$(kubectl -n $NS get aimodels.inference.k8s.f5.com sy-small -o jsonpath='{.spec.deployments[0].providerRef.name}')
echo "sy-small provider:  $(kubectl -n $NS get aiproviders.inference.k8s.f5.com $P -o jsonpath='{.spec.generic.baseURL}')"
kubectl -n aigw-models get inferencepool small-pool -o json | jq -c '.spec | {selector, endpointPickerRef}'
kubectl -n aigw-models get pods -l app=small-model -o wide
```

1. The route on `172.16.100.19` belongs to the gateway profile `switchyard-ai`.
2. The profile's model router is the Switchyard router on port 50051. It rewrites `auto` to
   `sy-small` or `sy-large`.
3. `sy-small`'s provider is the pool bridge, `/pools/small/v1` on the same router.
4. The InferencePool `small-pool` selects the `small-model` pods and names its endpoint picker,
   `small-pool-epp`, with `failureMode: FailOpen`.

Now send twelve requests to `sy-small` at once. Each line shows the status, the pod that
answered, how it was chosen and the pod address:

```bash,run
sy burst 12 sy-small "Translate 'good morning' into Spanish."
```

Both pods should appear. The picker reads each pod's queue and cache metrics and places each
request; it is not round robin. The bridge logs every pick and how long it took:

```bash,run
kubectl -n f5-bnk logs deploy/switchyard-router --since=2m | grep 'pool bridge forwarded' \
  | jq -r '.fields | "\(.pool)  \(.pick)  \(.endpoint)  pick \(.pick_ms) ms, model \(.upstream_ms) ms"' | tail -12
```

The pick takes a few milliseconds. The model takes hundreds. If the picker did not answer
within 500 ms, the bridge would send the request to the pool's Service instead and mark it
`fail-open`, as the pool's `FailOpen` mode asks.

Click **Check** once the small pool has served requests on two different pods.
