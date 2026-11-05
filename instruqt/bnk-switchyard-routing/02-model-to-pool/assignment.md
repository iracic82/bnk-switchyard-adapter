---
slug: model-to-pool
id: mzmzxrxe7w7c
type: challenge
title: From model name to GPU pod
teaser: A model name selects nothing in Gateway API. Follow one request from the adapter's
  choice to the pod that answered it.
notes:
- type: text
  contents: |-
    Choosing a model does not choose an InferencePool. In Gateway API the pool is picked by
    whichever HTTPRoute matches the request. This challenge follows that chain link by link,
    then shows the F5 Endpoint Picker spreading load inside one pool.
tabs:
- id: v3tab7shghdv
  title: Terminal
  type: terminal
  hostname: lab
- id: v9gcy3ip5jed
  title: Switchyard live view
  type: service
  hostname: lab
  path: /live
  port: 8090
- id: olb5ldm6xyie
  title: AI Gateway console
  type: service
  hostname: lab
  port: 3000
difficulty: intermediate
timelimit: 1500
enhanced_loading: null
---

The adapter's registry binds each logical model to one URL. Look at the binding for
`large`:

```bash,run
kubectl -n switchyard get cm -o jsonpath='{.items[0].data.adapter\.toml}' | sed -n '/\[models.large\]/,/^cost/p'
```

`base_url` is the VIP of the internal Gateway `inference-large`. There are three BNK
Gateways: one for clients, one per model pool.

```bash,run
kubectl get gateway -n f5-bnk switchyard-external inference-small inference-large
```

Each internal Gateway has exactly one HTTPRoute, and that route's only backend is an
InferencePool:

```bash,run
kubectl -n sim get httproute small-pool large-pool \
  -o custom-columns=ROUTE:.metadata.name,GATEWAY:.spec.parentRefs[0].name,POOL:.spec.rules[0].backendRefs[0].name
```

Each pool selects only its own model's pods and names its own F5 Endpoint Picker. BNK
rendered each picker from a single `F5EPP` resource:

```bash,run
kubectl -n sim get inferencepool small-pool large-pool \
  -o custom-columns=POOL:.metadata.name,SELECTOR:.spec.selector.matchLabels,PICKER:.spec.endpointPickerRef.name
kubectl -n sim get f5epp small-pool-epp large-pool-epp
```

So a request routed to `large` cannot land on a small-model pod: the pool's selector
doesn't include them.

Now watch the picker. Send 40 requests to the small model and count which pod served each:

```bash,run
for i in $(seq 1 40); do
  curl -s -o /dev/null -D - http://172.16.201.110/v1/chat/completions \
    -H 'Authorization: Bearer sk-demo-eng-agents' -H 'content-type: application/json' \
    -d '{"model":"small","messages":[{"role":"user","content":"hi"}]}' | grep -i x-served-by
done | sort | uniq -c
```

Both pods take traffic. The picker reads each pod's `vllm:num_requests_waiting` and
`vllm:num_requests_running` and sends each request to the least loaded one. These mock pods
report simulated numbers (slots in use), so prefix-cache affinity is not part of this demo.

One more detail. The adapter marks every request it sends to BNK with
`x-switchyard-adapter-hop: 1`. If a misconfigured route ever sent that traffic back to the
adapter, it would refuse it rather than loop:

```bash,run
curl -s -w '\n%{http_code}\n' http://172.16.201.110/v1/chat/completions -H 'x-switchyard-adapter-hop: 1' \
  -H 'Authorization: Bearer sk-demo-eng-agents' -d '{"model":"small","messages":[]}'
```

Click **Check** when both small-model pods have served requests.
