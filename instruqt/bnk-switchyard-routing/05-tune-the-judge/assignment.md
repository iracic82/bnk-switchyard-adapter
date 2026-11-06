---
slug: tune-the-judge
id: 2aqjoebc2d9p
type: challenge
title: Tune the judge
teaser: One number decides how often a step escalates to the large model. Sweep it
  through the gateway and find where cost per completed task bottoms out.
notes:
- type: text
  contents: |-
    The CFO liked the numbers from lesson 4 and asked the obvious follow-up: "Can it be
    cheaper?" The platform team's answer is a single setting.

    Switchyard's judge reads each agent step and estimates how likely the small model is to
    solve it. A threshold turns that estimate into a decision. Set it low and almost every step
    stays on the small model. Set it high and almost every step escalates. Somewhere in between
    is the cheapest way to get the work done.
- type: text
  contents: |-
    Switchyard sends a step to the small model when the judge's solve probability `p_solve`
    is at least `base_threshold`. Below it, the step goes to the large model.

    The judge in this lab is a stand-in for a real one: it grades each step by its wording and
    is wrong about one step in ten, on purpose. The trade-off it exposes is the real one.
tabs:
- id: qgzegci7bekv
  title: Terminal
  type: terminal
  hostname: lab
- id: ygwjfublulkf
  title: Topology
  type: service
  hostname: lab
  path: /topology
  port: 8092
- id: r46ppdxdd8cx
  title: Switchyard live view
  type: service
  hostname: lab
  path: /live
  port: 8090
- id: rbksk0iixnub
  title: AI Gateway console
  type: service
  hostname: lab
  port: 3000
difficulty: intermediate
timelimit: 1800
enhanced_loading: null
---

Look at what the judge says before changing anything. Send two steps and read `p_solve` in the
**Switchyard live view** tab, under each decision:

```bash,run
sy ask "Convert 72 degrees Fahrenheit to Celsius."
sy ask "Optimize this SQL query that scans the whole orders table."
```

The first gets a high `p_solve` (around 0.9), so it stays on `sy-small`. The second lands in
the middle, around 0.73: above the default threshold of 0.6, so it stays small too. Whether a
middle step escalates depends on the threshold. See the current one:

```bash,run
sy threshold
```

Move it and send the same middle step again. `sy threshold` rewrites the router's config and
restarts it; the AI gateway reconnects on the next request:

```bash,run
sy threshold 0.8
sy ask "Optimize this SQL query that scans the whole orders table."
```

At 0.8 the same step, with the same `p_solve`, escalates to `sy-large`. (The judge is wrong on
purpose one time in ten; if the answer surprises you, send it again.) Nothing changed in the gateway, the keys or the pools.
Only the router's policy did.

## Sweep it

Now measure the trade-off on the whole agent workload: 60 tasks, once on the small model,
once on the large one, then through Switchyard at three thresholds. Every request goes
through the gateway, the router, the pool bridge and F5's picker, exactly as before. It takes
five to seven minutes, and puts the original threshold back at the end:

```bash,run
python3 $SY_ROOT/bench/polaris_bench.py --episodes 60 --sweep 0.4,0.6,0.8 --json /root/sweep.json
```

Read the `to large` column first: the share of steps that escalated. It climbs with the
threshold. Task success climbs with it, and so does spend. **USD/success** is cost per
completed task, the number to compare, and the one Kong's and NVIDIA's own benchmarks report.

Three things to look for:

- The lowest USD/success is usually at a middle threshold, below both fixed models.
- `+judge` is the price of deciding: one call to the small model per step, about 1,150 tokens
  of prompt. At this lab's prices it costs about a third of a large-model step. Routing pays
  only when the judge keeps enough steps on the small model. A cheaper or cached judge widens
  the margin.
- At a high threshold the router escalates nearly everything and pays for the judge as well.
  That is worse than calling the large model directly.

Prices and success rates are simulated, and with 60 tasks two runs can differ by several
points. On real models the curve keeps its shape; where it bottoms out depends on your models,
your prices and your workload. That is why the threshold is a setting and not a constant.

Click **Check** when `/root/sweep.json` has a finished sweep.

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
