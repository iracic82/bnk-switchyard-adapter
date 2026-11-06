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
purpose one time in ten; if the answer surprises you, send it again.) Nothing changed in the
gateway, the keys or the pools. Only the router's policy did. Put it back before going on:

```bash,run
sy threshold 0.6
```

## Ask a real judge

So far the judge was a stand-in. This lab can also ask a real one: NVIDIA Nemotron 3.5 Lightning,
the efficient model in NVIDIA's own Switchyard benchmark, on NVIDIA's hosted API. Switchyard sends
it the same classifier prompt and verdict schema, and decides from its `p_solve` the same way.
Switch to it (skip this part if the command says the lab has no Nemotron judge):

```bash,run
sy judge nemotron
sy ask "Convert 72 degrees Fahrenheit to Celsius."
sy ask "Optimize this SQL query that scans the whole orders table."
sy ask "Find the root cause of this distributed deadlock between the queue consumer and the lock service."
```

In the **Switchyard live view**, each decision now carries Nemotron's own `p_solve`, and the
decision time includes a call across the internet. The model answers are still simulated; the
judgement is not. A real judge disagrees with the stand-in on some steps, which is the point:
the threshold you choose depends on which judge you trust.

Switch back before the sweep. The hosted API is rate-limited and the sweep sends hundreds of
judge calls (the benchmark would switch to the stand-in for you anyway):

```bash,run
sy judge mock
```

## One decision per conversation

Judging every step costs a judge call every step. For a conversation that stays on one kind of
work, Switchyard can judge once and keep the answer: the route `auto-session` does that for a
session ID. `sy` sends one when you set `SY_SESSION`:

```bash,run
SY_SESSION=ticket-42 sy ask "Find the root cause of this distributed deadlock between the queue consumer and the lock service."
SY_SESSION=ticket-42 sy ask "Thanks. Now write the incident summary in one sentence."
SY_SESSION=ticket-43 sy ask "Thanks. Now write the incident summary in one sentence."
```

In the live view, the first request of `ticket-42` carries the judge's `p_solve` and the second
reuses its model without a judge call, so its decision takes a few milliseconds. `ticket-43` is
a new conversation and is judged on its own, so the same short request lands on `sy-small`. One
judgement per conversation saves judge calls; judging every step reacts when a conversation
changes. Which is better depends on how your agents work.

## Sweep it

Now measure the trade-off on the whole agent workload: 60 tasks, once on the small model,
once on the large one, then through Switchyard at three thresholds. Every request goes
through the gateway, the router, the pool bridge and F5's picker, exactly as before. It takes
five to seven minutes, and puts the original threshold back at the end:

```bash,run
python3 $SY_ROOT/bench/polaris_bench.py --episodes 60 --sweep 0.4,0.6,0.8 --json /root/sweep.json
```

What is real here and what is not: the routing decisions, the escalation share, the token counts
and the judge's cost are real. Whether a task succeeds is not: no model here solves anything, so
the bench applies fixed odds (the small model solves 30% of hard steps, the large one 90%). The
sweep shows the shape of the trade-off under those odds; it cannot measure quality. The sweep
also uses the stand-in judge, because the hosted one is rate-limited.

Read the `to large` column first: the share of steps that escalated. It climbs with the
threshold, and task success climbs with it.

Then decide what you are buying. **USD/success** (cost per completed task) on its own favours
`sy-small`: it is so cheap that it wins even though it fails two tasks in three, because the
number does not count what a failed task costs you. So start from a success target, say close
to `sy-large`'s, and find the cheapest setting that meets it. In a typical run `auto@0.8` finishes
about eight tasks in ten against `sy-large`'s nine, for 10 to 20 percent less per completed task;
`auto@0.4` is cheaper per step but fails too often to count.

Two more things in the table:

- `+judge` is the price of deciding, the same at every threshold: one call per step with about
  1,150 tokens of prompt. In this lab it costs as much as the answers do at the low thresholds.
  Judging once per conversation, a smaller judge or a cached prompt shrinks it.
- The gain is modest here because the lab's large model is cheap ($2 and $8 per million tokens).
  Against a frontier model at tens of dollars per million output tokens, the same escalation
  share saves far more.

With 60 simulated tasks two runs can differ by several points, so do not read too much into one
row. On real models the trade-off has the same parts; where it lands depends on your models, your
prices and your workload. That is why the threshold is a setting and not a constant.

Click **Check** when `/root/sweep.json` has a finished sweep. If this lab has the Nemotron judge, the
check also wants one step judged by it, and calls it once through the judge proxy.

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
