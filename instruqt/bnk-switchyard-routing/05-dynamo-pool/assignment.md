---
slug: dynamo-pool
id: 8rmttdtyehwa
type: challenge
title: A Dynamo pool behind the same picker
teaser: Add NVIDIA Dynamo as a third model. Switchyard picks the model, F5's picker
  reads Dynamo's KV events and names the worker.
notes:
- type: text
  contents: |-
    Kestrel's inference team has moved its frontier model to NVIDIA Dynamo. Dynamo brings its own
    frontend, its own KV-aware router and an event plane, and the team wants to keep all of it.
    The platform team wants one gateway, one set of keys and one router in front of every model.

    Both can have what they want. Switchyard decides when a step deserves the Dynamo pool, and F5's
    picker reads Dynamo's own KV-cache events to choose the worker.
- type: text
  contents: |-
    The small and large pools are single servers behind F5's picker. NVIDIA Dynamo is a whole
    serving stack: a frontend with its own KV-aware router, workers, and an event plane (NATS)
    on which each worker publishes which prompt prefixes it holds in KV cache.

    This lab runs Dynamo's Mocker workers: Dynamo's GPU-free engine, which models vLLM's
    scheduling, KV cache and timing. The nodes they run on report simulated H100s.
tabs:
- id: qyr3pcpxswbk
  title: Terminal
  type: terminal
  hostname: lab
- title: Topology
  type: service
  hostname: lab
  path: /topology
  port: 8092
- id: 7phcr55x74vk
  title: Switchyard live view
  type: service
  hostname: lab
  path: /live
  port: 8090
- id: ecurgrcymuzn
  title: AI Gateway console
  type: service
  hostname: lab
  port: 3000
difficulty: advanced
timelimit: 1500
enhanced_loading: null
---

Start with what the cluster thinks it has. The fake GPU operator gives each worker node four
H100s, and the Dynamo operator runs the `mocker` graph: one frontend and two workers.

```bash,run
kubectl get nodes -o custom-columns='NODE:.metadata.name,GPU:.status.allocatable.nvidia\.com/gpu,PRODUCT:.metadata.labels.nvidia\.com/gpu\.product'
kubectl -n aigw-models get dynamographdeployment mocker
kubectl -n aigw-models get pods -l nvidia.com/dynamo-graph-deployment-name=mocker -o wide
```

The pool and its picker. The F5EPP runs with `engine: dynamo`: it subscribes to the workers' KV
events on NATS and names the worker it picks in `x-dynamo-worker-instance-id`. Each worker pod
has a Dynamo frontend sidecar in direct mode, which serves exactly the worker named in that
header instead of routing again.

```bash,run
kubectl -n aigw-models get inferencepool model-pool -o jsonpath='{.spec.endpointPickerRef}{"\n"}'
kubectl -n aigw-models get f5-epp model-pool-epp -o jsonpath='{.spec.engine}  {.spec.natsURL}{"\n"}'
```

Polaris knows the pool as model `sy-dynamo`. Its provider is the router's pool bridge, as for the
other two models; the bridge passes the picker's worker header on to the pod. Send one request:

```bash,run
sy ask sy-dynamo "Summarize this support ticket in one sentence: customer cannot log in after a password reset."
```

`x-pool-pick: epp` says F5's picker chose, and `x-pool-endpoint` is the worker pod's address.
The answer is Mocker text: Dynamo's engine is simulated, its routing is not.

Now let Switchyard decide. Model `auto-dynamo` runs the same judge as `auto`, with the Dynamo
pool as the capable model:

```bash,run
sy ask auto-dynamo "Translate 'good morning' into Spanish."
sy ask auto-dynamo "Find the root cause of this distributed deadlock between the queue consumer and the lock service."
```

The easy step should come back from `sy-small` and the hard one from `sy-dynamo`. The judge is
still wrong one time in ten.

Send the same long prompt several times and watch which worker serves it. A picker that reads KV
events prefers the worker that already holds the prompt's prefix:

```bash,run
sy burst 8 sy-dynamo "You are reviewing a payment service. Explain the retry and idempotency rules for at-least-once delivery, then list the failure cases."
```

Both decisions for the Dynamo pool are in the router's log:

```bash,run
kubectl -n f5-bnk logs deploy/switchyard-router --since=10m \
  | grep -E 'ext_proc routed|pool bridge forwarded' | grep -E 'dynamo' | tail -4 | jq -c '.fields'
```

Click **Check** when the log shows Switchyard sending a step to `sy-dynamo` and the picker
choosing its worker.

## Logins for this lab

The **AI Gateway console** tab asks for these:

```text
[[ Instruqt-Var key="AIGW_USER" hostname="lab" ]]
```

```text
[[ Instruqt-Var key="AIGW_PASSWORD" hostname="lab" ]]
```

The `sy` helper picks the right virtual key for you. To call the gateway yourself, this is the
engineering key (model scope `sy-small`, `sy-large`):

```text
[[ Instruqt-Var key="SY_ENG_KEY" hostname="lab" ]]
```
