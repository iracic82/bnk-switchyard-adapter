---
slug: tenant-governance
type: challenge
title: Who may use which model
teaser: The support bot may only use the small model. Switchyard never gets the chance to
  pick anything else for it.
notes:
- type: text
  contents: |-
    A routing algorithm makes mistakes. A tenant rule must not depend on it being right.
    Here, authorization happens before Switchyard runs: the algorithm is only ever shown the
    models the tenant may use.
tabs:
- title: Terminal
  type: terminal
  hostname: bnk
- title: Switchyard Dashboard
  type: service
  hostname: bnk
  path: /d/switchyard-bnk
  port: 3000
difficulty: intermediate
timelimit: 1500
---

Three tenants are configured. Each is identified by its API key; the adapter stores only
the keys' SHA-256 digests.

```bash,run
kubectl -n switchyard get cm -o jsonpath='{.items[0].data.adapter\.toml}' | sed -n '/\[\[tenants\]\]/,/^limits/p' | grep -E '^id|allowed|limits'
```

The support bot asks for the large model by name. It is refused:

```bash,run
curl -s http://172.16.201.110/v1/chat/completions -H 'Authorization: Bearer sk-demo-support-agents' \
  -H 'content-type: application/json' -d '{"model":"large","messages":[{"role":"user","content":"hi"}]}'; echo
```

It sends a hard task through `auto`. For the engineering tenant this would go to the large
model. The support tenant may only use the small one, and its policy says `degrade`, so
the request runs on the small model and the response says so:

```bash,run
curl -s -D - -o /dev/null http://172.16.201.110/v1/chat/completions -H 'Authorization: Bearer sk-demo-support-agents' \
  -H 'content-type: application/json' \
  -d '{"model":"auto","messages":[{"role":"user","content":"Design a migration plan for our ticket system."}]}' \
  | grep -iE 'x-switchyard-(model|policy)'
```

Identity comes from the key, never from a header the client can set:

```bash,run
curl -s -w '  %{http_code}\n' http://172.16.201.110/v1/chat/completions -H 'x-tenant-id: eng-agents' \
  -H 'content-type: application/json' -d '{"model":"large","messages":[{"role":"user","content":"hi"}]}'
```

Limits are per tenant too. The support bot may ask for at most 2,048 output tokens:

```bash,run
curl -s http://172.16.201.110/v1/chat/completions -H 'Authorization: Bearer sk-demo-support-agents' \
  -H 'content-type: application/json' -d '{"model":"small","max_tokens":8000,"messages":[{"role":"user","content":"hi"}]}'; echo
```

Its estimated spend budget is $2 per hour, computed from configured token prices. That is
spend control, not a GPU quota, and the lab says so on purpose. The dashboard's
**Policy rejections** panel now shows the refusals by tenant and reason.

Click **Check** when the support bot has been refused at least once and served at least once.
