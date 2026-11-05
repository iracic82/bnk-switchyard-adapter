---
slug: cost-of-a-decision
type: challenge
title: The cost of a decision
teaser: Run one agent workload three ways (small only, large only, Switchyard) and compare
  task success, cost and time to first token.
notes:
- type: text
  contents: |-
    Model routing is a trade. The small model is cheap and fast and fails hard steps. The
    large model succeeds and is slow and expensive. Switchyard spends a little on every
    decision to get most of the success for much less. This challenge measures that trade,
    with the numbers labelled for what they are.
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
timelimit: 1800
---

The workload is 30 synthetic agent episodes of 3 to 6 steps. About 30% of steps are hard.
An episode succeeds only if every step succeeds. All three runs go through BNK, so in the
Switchyard run the F5 Endpoint Picker chooses the replica for every request.

The cost numbers come from the adapter's own counters, so open a port to its metrics first:

```bash,run
kubectl -n switchyard port-forward svc/switchyard-adapter 19480:8080 >/dev/null 2>&1 &
```

Run the benchmark (about three minutes):

```bash,run
cd $SY_ROOT && python3 bench/run_bench.py --url http://172.16.201.110 --bnk-url http://172.16.201.110 \
  --metrics-url http://127.0.0.1:19480 --scenarios fixed-small,fixed-large,switchyard-bnk \
  --episodes 30 --concurrency 8 | xargs cat
```

Read the first table:

- **Task success.** Small-only fails most episodes because one hard step sinks the whole
  task. Large-only succeeds most often.
- **Cost per successful task.** This is the number that matters. Switchyard should be well
  below large-only.
- **TTFT.** Large-only queues everything on the four large-model slots. Switchyard sends
  the easy steps elsewhere, so its median time to first token drops.

Then the routing detail. *Selection cost* is what the judge calls cost. It is not small:
the classifier prompt is over a thousand tokens. *Hard steps on large* shows how many hard
steps the classifier caught; the misses are why Switchyard's success rate sits below
large-only. In a real deployment `base_threshold` is the knob for that trade.

Be clear about what these numbers are. The model servers are mocks: their latencies,
success rates per tier and the token prices are configured, and the judge is a keyword
heuristic with a 10% error rate. The comparison between strategies is meaningful. The
absolute values are not GPU measurements.

Click **Check** when the report has been written.
