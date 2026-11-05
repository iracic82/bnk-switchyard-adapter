---
slug: two-decisions
type: challenge
title: Two decisions per request
teaser: Switchyard picks the model. BNK picks the GPU replica. Watch both happen on every request.
notes:
- type: text
  contents: |-
    Every request an agent sends needs two answers. Which model should do this work? And
    which copy of that model, on which GPU, should take it right now?

    In this lab NVIDIA Switchyard answers the first one, inside a small adapter that sits
    behind a BNK Gateway. BNK answers the second, with the F5 Endpoint Picker in front of
    each model's pool.
tabs:
- title: Terminal
  type: terminal
  hostname: bnk
- title: Switchyard Dashboard
  type: service
  hostname: bnk
  path: /d/switchyard-bnk
  port: 3000
difficulty: basic
timelimit: 1200
---

The adapter is behind BNK's external Gateway at `172.16.201.110`. Clients send OpenAI
Chat Completions requests. Instead of naming a model, they send `model: auto` and let
Switchyard choose.

Run the walkthrough:

```bash,run
ADAPTER_URL=http://172.16.201.110 $SY_ROOT/scripts/demo.sh
```

Look at steps 1 and 2. The same coding agent sent an easy step and a hard step:

- `Switchyard chose small` / `large` is the model decision. The algorithm is
  `llm_task_classifier`: before answering, it asked a judge model (the small model, through
  its own BNK pool) whether the small model can solve this.
- `decided in ... ms` is the cost of that decision, the judge call.
- `BNK pool ... -> endpoint ...` is the second decision. The pod name is the replica the F5
  Endpoint Picker chose.

Send one hard request yourself and read the headers:

```bash,run
curl -s -D - -o /dev/null http://172.16.201.110/v1/chat/completions \
  -H 'Authorization: Bearer sk-demo-eng-agents' -H 'content-type: application/json' \
  -d '{"model":"auto","messages":[{"role":"user","content":"Find the root cause of this deadlock between the queue and the lock service."}]}' \
  | grep -iE 'x-switchyard|x-served-by'
```

Then a streaming one. The answer arrives as server-sent events, as with any OpenAI client:

```bash,run
curl -sN http://172.16.201.110/v1/chat/completions \
  -H 'Authorization: Bearer sk-demo-eng-agents' -H 'content-type: application/json' \
  -d '{"model":"auto","stream":true,"messages":[{"role":"user","content":"Write a commit message for a typo fix."}]}' | head -5
```

Open the **Switchyard Dashboard** tab. The top row is the model decision (requests per
model, decision latency, judge calls). The second row is the inference path through BNK.
The bottom row is the load each pod reports; with the mock model servers it is simulated.

Click **Check** when you have sent a few requests.
