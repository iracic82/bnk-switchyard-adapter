---
slug: cost-of-a-decision
id: ilynvf4ctmbx
type: challenge
title: The cost of a decision
teaser: Run one agent workload three ways (small only, large only, Switchyard) and
  compare task success and cost.
notes:
- type: text
  contents: |-
    The CFO's question arrives on a Friday: "Is the router worth it, or should we just buy more
    GPUs?" Opinions won't answer that. Numbers will.

    Run the same agent workload three ways through the gateway: always small, always large, and
    Switchyard deciding. Then compare what each one costs per task that actually succeeded.
- type: text
  contents: |-
    Pin every agent step to the large model and tasks succeed, at the large model's price. Pin them
    to the small model and they are cheap, and the hard steps fail. Switchyard sits between the two.
    It also costs something: a judge call before every step.

    This lesson measures all three through the AI gateway, on the same workload.
tabs:
- id: nb7elqnxm4t2
  title: Terminal
  type: terminal
  hostname: lab
- id: uwwjn3fdjhfr
  title: Topology
  type: service
  hostname: lab
  path: /topology
  port: 8092
- id: wqljnemskesi
  title: Switchyard live view
  type: service
  hostname: lab
  path: /live
  port: 8090
- id: hzae9lbotvtz
  title: AI Gateway console
  type: service
  hostname: lab
  port: 3000
difficulty: intermediate
timelimit: 1800
enhanced_loading: null
---

The workload is 30 agent tasks of three to six steps. Most steps are easy; some are hard, and a
task succeeds only if every step does. Nothing here is a real model, so success is simulated:
the large model solves 90% of hard steps, the small one 30%. The small pool's mock writes its
verdict into the answer; the large pool runs llm-d's vLLM simulator, which answers with filler
text, so the bench draws those outcomes at the same rates and says how many it drew. Latency
and token counts are the simulators' own. Prices are the ones set on the
gateway's models: the small model at $0.10 and $0.40 per million input and output tokens, the
large one at $2 and $8.

Every request goes through the full chain: virtual key, gateway, Switchyard for `auto`, pool
bridge, endpoint picker, pod. It takes two to three minutes:

```bash,run
python3 $SY_ROOT/bench/polaris_bench.py --episodes 30 --json /root/bench.json
```

Read the table:

- **task success**: how many tasks finished with every step right.
- **served small/large**: where the steps went. For `auto` this is Switchyard's mix.
- **EPP picks**: requests whose pod the F5 Endpoint Picker chose (all of them, normally).
- **cost USD** comes from the token usage in each response. **+judge** is the judge call
  Switchyard makes for every `auto` step, estimated from its prompt size (about 1,150 tokens on
  the small model). It does not show in the response.
- **USD/success**: total cost divided by successful tasks, the number that matters.

Usually `auto` succeeds almost as often as `sy-large` and costs well under half as much per
successful task. Where it falls short, it is the judge's mistakes: hard steps sent to the small
model. A better judge, or a higher threshold in the router's config, trades cost for success.

Every number here is simulated. On real GPUs the shape holds; the values will not.

Click **Check** when the run has finished.

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
