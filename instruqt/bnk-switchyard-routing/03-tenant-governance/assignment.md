---
slug: tenant-governance
id: 1exvydozt3me
type: challenge
title: Who may use which model
teaser: Virtual keys, model scopes and token budgets live in the AI gateway, before Switchyard sees the request.
notes:
- type: text
  contents: |-
    Switchyard decides which model fits a request. It does not decide who may spend what. In this
    lab that is the AI gateway's job: each caller has a virtual key, each key has a model scope,
    and a key can carry a token budget.

    The gateway checks all of that before it calls Switchyard, so a routing decision can never
    spend capacity the caller is not allowed.
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

The lab has two keys in the tenant `switchyard`:

- `eng`: the engineering team, models `sy-small` and `sy-large`, no budget.
- `pilot`: a pilot project with the same models and a budget of 2,000 tokens an hour. The
  budget's action is `block`.

First, a model outside the key's scope. The gateway refuses it, and Switchyard never sees the
request:

```bash,run
sy ask gpt-4o "hello"
```

Now spend the pilot key's budget. Each hard request to the large model uses about 75 tokens,
so a few dozen are enough. Watch the status column:

```bash,run
for i in 1 2 3; do SY_KEY=pilot sy burst 15 sy-large "Design a rate limiter for 50,000 requests per second."; done
```

The first requests answer `200`. Once the budget is spent the gateway refuses the key. The
gateway counts usage as requests finish, so a few may get through after the line is crossed.
Run the loop again if every request still says `200`.

The engineering key is not affected:

```bash,run
sy ask sy-small "Which HTTP status code means not found?"
```

Open the **AI Gateway console** tab to see the tenant, its keys and their spend. The login is
in a Kubernetes Secret:

```bash,run
kubectl -n f5-bnk get secret f5-aigateway-admin -o json | jq -r '.data | map_values(@base64d) | "user: \(.username)  password: \(.password)"'
```

Click **Check** once the pilot key is being refused.
