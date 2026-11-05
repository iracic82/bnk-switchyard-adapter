---
slug: two-decisions
id: dpo8ay6qa0qc
type: challenge
title: Two decisions per request
teaser: Switchyard picks the model. The F5 Endpoint Picker picks the replica. Watch both happen on every request.
notes:
- type: text
  contents: |-
    Every request an agent sends needs two answers. Which model should do this work? And which copy
    of that model, on which GPU, should take it right now?

    In this lab a client calls BIG-IP Next for Kubernetes' AI gateway with a virtual key and
    `model: auto`. The gateway asks NVIDIA Switchyard which model to use. Then the request goes to
    that model's pool, where the F5 Endpoint Picker chooses the replica.

    The model servers are mocks with vLLM-style metrics, so the lab needs no GPUs.
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
difficulty: basic
timelimit: 1200
enhanced_loading: null
---

The AI gateway's route for this lab is `172.16.100.19`. The `sy` helper sends requests to it
from the `client` container, which sits outside the cluster behind the fabric's router, like a
real caller. It uses the engineering team's virtual key.

Send an easy agent step, with `model: auto`:

```bash,run
sy ask "Write a one-line commit message for a typo fix in the README."
```

Then a hard one:

```bash,run
sy ask "Find the root cause of this distributed deadlock between the queue consumer and the lock service."
```

Each answer shows the response headers and the model that answered:

- The `model` in the answer (`sy-small` or `sy-large`) is **the first decision**. Switchyard's
  task classifier asked a judge model (the small one) whether the small model can solve this
  step, and the gateway sent the request to the model it picked.
- `x-served-by` and `x-pool-endpoint` are **the second decision**: the pod the F5 Endpoint
  Picker chose inside that model's pool. `x-pool-pick: epp` says the picker made the choice.

The mock judge looks for words like "distributed" or "root cause" and is wrong one time in ten
on purpose, so a hard step now and then lands on the small model. Real judges make mistakes
too. Lesson 4 measures what that costs.

Both decisions are in the router's log. Each request leaves two lines: `ext_proc routed` (the
model, and how long the decision took) and `pool bridge forwarded` (the pod, and how long the
picker took):

```bash,run
kubectl -n f5-bnk logs deploy/switchyard-router --since=5m \
  | grep -E 'ext_proc routed|pool bridge forwarded' | tail -4 | jq -c '.fields'
```

Open the **Switchyard live view** tab and send a few more requests. Each row is one decision:
the prompt, the model chosen, and how long the judge took.

Click **Check** once both models have answered an `auto` request.
