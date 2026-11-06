---
slug: two-decisions
id: dpo8ay6qa0qc
type: challenge
title: Two decisions per request
teaser: Switchyard picks the model. The F5 Endpoint Picker picks the replica. Watch
  both happen on every request.
notes:
- type: text
  contents: |-
    Kestrel Freight runs a fleet of AI agents: they plan routes, read customs forms, answer
    drivers and fix their own tool calls. Last month the platform team pinned every agent to the
    biggest model they had. The agents got things right. The GPU bill tripled.

    Most of what an agent does is small. "Format this as JSON." "Which status code means not
    found?" A 0.6B model handles that. A few steps need the big one. Your job today is to put each
    step on the model it needs, on the BIG-IP Next for Kubernetes cluster the platform team already
    runs, without changing a line of agent code.
- type: text
  contents: |-
    Every request an agent sends needs two answers. Which model should do this work? And which copy
    of that model, on which GPU, should take it right now?

    In this lab a client calls BIG-IP Next for Kubernetes' AI gateway with a virtual key and
    `model: auto`. The gateway asks NVIDIA Switchyard which model to use. Then the request goes to
    that model's pool, where the F5 Endpoint Picker chooses the replica.

    The model servers are mocks with vLLM-style metrics, so the lab needs no GPUs.
tabs:
- id: ghzooumlox1o
  title: Terminal
  type: terminal
  hostname: lab
- id: xtkzcmka5xv5
  title: Topology
  type: service
  hostname: lab
  path: /topology
  port: 8092
- id: wtomcyysrnwj
  title: Switchyard live view
  type: service
  hostname: lab
  path: /live
  port: 8090
- id: pdnz2o11cwwz
  title: AI Gateway console
  type: service
  hostname: lab
  port: 3000
difficulty: basic
timelimit: 1200
enhanced_loading: null
---

Four tabs come with every lesson: **Terminal**, **Topology** (the request path, live),
**Switchyard live view** and **AI Gateway console**. The console's username and password are at
the end of this page, under **Logins for this lab**.

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

Each answer shows the response headers and the model that answered. The answer text itself is
simulated (no model here is real, so expect filler); the routing that produced it is real:

- The `model` in the answer (`sy-small` or `sy-large`) is **the first decision**. Switchyard's
  task classifier asked a judge model (the small one) whether the small model can solve this
  step, and the gateway sent the request to the model it picked.
- `x-served-by` and `x-pool-endpoint` are **the second decision**: the pod the F5 Endpoint
  Picker chose inside that model's pool. `x-pool-pick: epp` says the picker made the choice.
  The large pool runs llm-d's vLLM simulator, which sends no `x-served-by` and answers with
  filler text; `x-pool-endpoint` still names its pod.

The judge here is a stand-in that grades each step by its wording ("distributed", "root cause"
and the like) and is wrong one time in ten on purpose, either way: now and then an easy step
escalates to the large model, or a hard one stays small. Send the same step again and it usually
lands where you expect. Real judges make mistakes too; lessons 4 and 5 measure what they cost.

Both decisions are in the router's log. Each request leaves two lines: `ext_proc routed` (the
model, and how long the decision took) and `pool bridge forwarded` (the pod, and how long the
picker took):

```bash,run
kubectl -n f5-bnk logs deploy/switchyard-router --since=5m \
  | grep -E 'ext_proc routed|pool bridge forwarded' | tail -4 | jq -c '.fields'
```

Open the **Topology** tab. It draws this lab's request path and moves a dot along it for every
request, live: first to Switchyard for the model, then through the pool bridge and the picker to
the pod. Click any box to see what it is and the command that shows it. The buttons at the top
send requests too.

Open the **Switchyard live view** tab and send a few more requests. Each row is one request: the
prompt, the model Switchyard chose with the judge's `p_solve` (its estimate that the small model
can do the step), how long the decision took, and the pod F5's picker chose.

Click **Check** once both models have answered an `auto` request.

## Logins for this lab

The **AI Gateway console** tab asks for these. Each box has a copy button.

**Username**

```text
[[ Instruqt-Var key="AIGW_USER" hostname="lab" ]]
```

**Password**

```text
[[ Instruqt-Var key="AIGW_PASSWORD" hostname="lab" ]]
```

The `sy` helper picks the right virtual key for you. To call the gateway yourself, this is the
engineering key (model scope `sy-small`, `sy-large`):

**Engineering key**

```text
[[ Instruqt-Var key="SY_ENG_KEY" hostname="lab" ]]
```

If a box above says `undefined`, this lab started before it could store them. This prints the same logins:

```bash,run
sy logins
```
