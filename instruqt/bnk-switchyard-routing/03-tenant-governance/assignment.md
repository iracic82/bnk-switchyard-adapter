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
    wants to know that a routing decision can never spend money the caller wasn't given.

    In this lesson you try the limits on purpose: ask for a model that isn't in scope, and keep
    the pilot key talking until its budget runs out.
- type: text
  contents: |-
    Switchyard decides which model fits a request. It does not decide who may spend what. In this
    lab that is the AI gateway's job: each caller has a virtual key, each key has a model scope,
    and a key can carry a token budget.

    The gateway checks all of that before it calls Switchyard, so a routing decision can never
    spend capacity the caller is not allowed.
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

Open the **AI Gateway console** tab to see the tenant `switchyard`, its keys and their spend.
Sign in with the username and password under **Logins for this lab** at the end of this page.
Under the tenant, the pilot key shows the tokens it has used against its budget.

Click **Check** once the pilot key is being refused.

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
