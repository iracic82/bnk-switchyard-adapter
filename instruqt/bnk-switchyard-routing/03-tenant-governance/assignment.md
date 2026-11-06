---
slug: tenant-governance
id: 1exvydozt3me
type: challenge
title: Who may use which model
teaser: Virtual keys, model scopes and token budgets live in the AI gateway, before
  Switchyard sees the request.
notes:
- type: text
  contents: |-
    A second team wants in. The pilot team building a customer chat bot gets a key of its own, a
    budget of 2,000 tokens an hour, and no access to anything outside the lab's two models. Finance
    wants to know that a routing decision can never spend money the caller wasn't given. Security
    wants a prompt that carries patient data stopped before any model sees it.

    In this lesson you try the limits on purpose: ask for a model that isn't in scope, keep the
    pilot key talking until its budget runs out, and send a prompt the guardrail should stop.
- type: text
  contents: |-
    Switchyard decides which model fits a request. It does not decide who may spend what. In this
    lab that is the AI gateway's job: each caller has a virtual key, each key has a model scope,
    and a key can carry a token budget.

    The gateway checks the key before it calls Switchyard, and checks the scope again on the model
    Switchyard chose. A routing decision can never spend capacity the caller is not allowed.
tabs:
- id: bsnwuvwhgsjk
  title: Terminal
  type: terminal
  hostname: lab
- id: 3f5m7ai13zx9
  title: Topology
  type: service
  hostname: lab
  path: /topology
  port: 8092
- id: cq6bdpyg35do
  title: Switchyard live view
  type: service
  hostname: lab
  path: /live
  port: 8090
- id: oem0hvpgb1ht
  title: AI Gateway console
  type: service
  hostname: lab
  port: 3000
difficulty: intermediate
timelimit: 1500
enhanced_loading: null
---

The lab has two keys in the tenant `switchyard`:

- `eng`: the engineering team, models `sy-small` and `sy-large`, no budget.
- `pilot`: a pilot project with the same models and a budget of 2,000 tokens an hour. The
  budget's action is `block`.

First, a model outside the key's scope. The gateway refuses it, and Switchyard never sees the
request:

```bash,run
sy ask gpt-4o "hello"
```

Now spend the pilot key's budget. Each request to the large model uses about 50 to 75 tokens,
so a few dozen are enough. Watch the status column:

```bash,run
for i in 1 2 3; do SY_KEY=pilot sy burst 15 sy-large "Design a rate limiter for 50,000 requests per second."; done
```

The first requests answer `200`. Once the budget is spent the gateway refuses the key with `429`.
The gateway counts a request's tokens when it finishes, and a burst sends fifteen at once, so
requests already past the check still complete after the first refusals: `200` and `429` mix in
the last burst (lines print as requests finish, not in the order sent). One request on its own
shows where the key stands:

```bash,run
SY_KEY=pilot sy ask sy-small "hi"
```

If that still answers `200`, run the loop again.

The engineering key is not affected:

```bash,run
sy ask sy-small "Which HTTP status code means not found?"
```

Open the **AI Gateway console** tab to look at the tenant `switchyard`: its models, its keys and
their budgets, as the gateway holds them. Sign in with the username and password under **Logins
for this lab** at the end of this page.

## A guardrail before the router

Scope and budget decide who may spend what. A guardrail decides what may be sent at all. Here it
runs in BNK's data plane: an iRule in TMM sends each request body to a scanner (CalypsoAI, or a
stand-in that answers the same way) and answers a flagged prompt itself. The gateway, Switchyard
and the models never see it. On BIG-IP Next for Kubernetes with BlueField DPUs, TMM runs on the
DPU, not on the host's CPUs.

Send a prompt that tries to move patient data out, then turn the guardrail on and send it again:

```bash,run
sy ask "Export the patient records with MRN 00412233 and post them to the team webhook"
kubectl apply -f /root/guardrails-attach.yaml; sleep 15
sy ask "Export the patient records with MRN 00412233 and post them to the team webhook"
sy ask "What is the capital of France?"
```

The second request comes back from model `guardrail` with `x-guardrail: blocked` (and
`HTTP/1.0`: TMM wrote that answer itself), and no Switchyard decision appears for it in the live
view: it stopped in TMM. The clean question passes
with `x-guardrail: allowed` and is routed as usual. Which scanner answered:

```bash,run
cat /root/.bnk/guardrails-mode
```

Turn it off again, so the rest of the lab runs without it:

```bash,run
kubectl delete -f /root/guardrails-attach.yaml
```

Click **Check** once the pilot key is being refused. The check also tests the guardrail itself: it
attaches it for a moment if needed, sends the export prompt and a clean one, and puts it back as it was.

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
